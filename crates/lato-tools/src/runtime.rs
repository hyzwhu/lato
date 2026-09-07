use crate::{
    BuiltinAdapterError, BuiltinToolEnvironment, CatalogError, RegistrationOutcome, SkillToolScope,
    ToolCatalog, builtin_tools, task_tools,
};
use lato_core::{
    ApprovalFingerprint, ApprovalRequest, EnvironmentPolicy, ExecutionGrant, NetworkPolicy,
    PolicyAuditDecision, PolicyAuditRecord, PolicyAuditStage, PolicyDecision, PolicyMode,
    PolicyRequest, PreparedToolAudit, Retryability, SandboxObligation, SandboxProfile, Tool,
    ToolContext, ToolDescriptor, ToolError, ToolName, ToolOutput, journal_request_hash,
};
use lato_policy::{
    ApprovalLedger, NoopPolicyEventSink, PolicyEngine, PolicyEvent, PolicyEventKind,
    PolicyEventSink, approval_fingerprint, canonical_arguments,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub struct ToolRuntime {
    catalog: ToolCatalog,
    wire_names: BTreeMap<String, ToolName>,
    validators: BTreeMap<ToolName, jsonschema::Validator>,
    policy: Arc<PolicyEngine>,
    scope: PolicyScope,
    sink: Arc<dyn PolicyEventSink>,
}

pub struct ToolRuntimeBuilder {
    catalog: ToolCatalog,
    policy: Arc<PolicyEngine>,
    scope: PolicyScope,
    sink: Arc<dyn PolicyEventSink>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyScope {
    pub workspace_root: PathBuf,
    pub mode: PolicyMode,
    pub project_trusted: bool,
    pub sandbox_profile: SandboxProfile,
}

pub struct PreparedToolCall {
    context: ToolContext,
    tool: Arc<dyn Tool>,
    arguments: Value,
    request: PolicyRequest,
    fingerprint: ApprovalFingerprint,
    decision: PolicyDecision,
    audit: PreparedToolAudit,
    skill_scope_guard: Option<SkillScopeGuard>,
}

struct SkillScopeGuard {
    scope: SkillToolScope,
    cwd: PathBuf,
    original_arguments: Value,
}

impl PreparedToolCall {
    pub fn audit(&self) -> PreparedToolAudit {
        self.audit.clone()
    }

    pub fn policy_audit(
        &self,
        stage: PolicyAuditStage,
        decision: PolicyAuditDecision,
    ) -> PolicyAuditRecord {
        PolicyAuditRecord {
            stage,
            decision,
            call_id: self.audit.call_id.clone(),
            tool_name: self.audit.tool_name.clone(),
            request_hash: self.audit.request_hash.clone(),
            approval_fingerprint: self.audit.approval_fingerprint.clone(),
            capabilities: self.audit.capabilities.clone(),
            sandbox: self.audit.sandbox.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Builtin(#[from] BuiltinAdapterError),
    #[error("model-facing tool name {wire_name} is ambiguous between {first} and {second}")]
    AmbiguousWireName {
        wire_name: String,
        first: ToolName,
        second: ToolName,
    },
    #[error("invalid input schema for tool {name}: {message}")]
    InvalidSchema { name: ToolName, message: String },
}

impl ToolRuntimeBuilder {
    pub fn new(policy: Arc<PolicyEngine>, scope: PolicyScope) -> Self {
        Self {
            catalog: ToolCatalog::new(),
            policy,
            scope,
            sink: Arc::new(NoopPolicyEventSink),
        }
    }

    pub fn with_sink(mut self, sink: Arc<dyn PolicyEventSink>) -> Self {
        self.sink = sink;
        self
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError> {
        self.catalog.register(tool)
    }

    pub fn register_builtin_tools(
        &mut self,
        environment: BuiltinToolEnvironment,
    ) -> Result<(), RuntimeBuildError> {
        for tool in builtin_tools(environment)? {
            self.register(tool)?;
        }
        Ok(())
    }

    pub fn build(self) -> Result<ToolRuntime, RuntimeBuildError> {
        let mut wire_names = BTreeMap::new();
        let mut validators = BTreeMap::new();
        for descriptor in self.catalog.descriptors() {
            let wire_name = descriptor.name.local_name().to_owned();
            if let Some(first) = wire_names.insert(wire_name.clone(), descriptor.name.clone()) {
                return Err(RuntimeBuildError::AmbiguousWireName {
                    wire_name,
                    first,
                    second: descriptor.name,
                });
            }
            let validator = jsonschema::options()
                .offline()
                .build(&descriptor.input_schema)
                .map_err(|error| RuntimeBuildError::InvalidSchema {
                    name: descriptor.name.clone(),
                    message: error.to_string(),
                })?;
            validators.insert(descriptor.name, validator);
        }
        Ok(ToolRuntime {
            catalog: self.catalog,
            wire_names,
            validators,
            policy: self.policy,
            scope: self.scope,
            sink: self.sink,
        })
    }
}

impl ToolRuntime {
    pub fn model_definitions(&self) -> Vec<Value> {
        self.model_definitions_scoped(None)
    }

    pub fn model_definitions_scoped(&self, scope: Option<&SkillToolScope>) -> Vec<Value> {
        self.catalog
            .descriptors()
            .into_iter()
            .filter(|descriptor| {
                scope.is_none_or(|scope| scope.allows_name(descriptor.name.local_name()))
            })
            .map(|descriptor| {
                json!({
                    "type": "function",
                    "function": {
                        "name": descriptor.name.local_name(),
                        "description": descriptor.description,
                        "parameters": descriptor.input_schema,
                    }
                })
            })
            .collect()
    }

    pub async fn invoke(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let prepared = self.prepare(context, wire_name, arguments)?;
        match self.decision(&prepared).clone() {
            PolicyDecision::Allow(grant) => self.execute(prepared, grant).await,
            PolicyDecision::RequireApproval(_) => {
                let error = policy_error(
                    "policy.approval_required",
                    "this tool call requires explicit approval",
                );
                self.emit_tool(
                    PolicyEventKind::ToolFailed,
                    &prepared.request,
                    Some(error.code.clone()),
                    None,
                    None,
                );
                Err(error)
            }
            PolicyDecision::Deny(denial) => {
                let error = policy_error(denial.code.clone(), denial.message.clone());
                self.emit_tool(
                    PolicyEventKind::ToolFailed,
                    &prepared.request,
                    Some(error.code.clone()),
                    None,
                    None,
                );
                Err(error)
            }
        }
    }

    pub fn prepare(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: Value,
    ) -> Result<PreparedToolCall, ToolError> {
        self.prepare_scoped(context, wire_name, arguments, None)
    }

    pub fn resolve_and_validate(
        &self,
        wire_name: &str,
        arguments: Value,
    ) -> Result<ValidatedToolCall, ToolError> {
        let Some(canonical_name) = self.resolve_wire_name(wire_name) else {
            return Err(not_found(wire_name));
        };
        let canonical_arguments = canonical_arguments(&arguments).map_err(|error| {
            ToolError::new(
                "tool.invalid_arguments",
                error.to_string(),
                Retryability::Never,
            )
        })?;
        let arguments: Value = serde_json::from_slice(&canonical_arguments).map_err(|error| {
            ToolError::new(
                "tool.invalid_arguments",
                error.to_string(),
                Retryability::Never,
            )
        })?;
        self.validate_arguments(&canonical_name, &arguments, wire_name)?;
        Ok(ValidatedToolCall {
            wire_name: wire_name.to_owned(),
            canonical_name,
            arguments,
        })
    }

    pub fn prepare_scoped(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: Value,
        scope: Option<&SkillToolScope>,
    ) -> Result<PreparedToolCall, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            ));
        }

        let validated = self.resolve_and_validate(wire_name, arguments)?;
        let canonical = validated.canonical_name;
        let original_arguments = validated.arguments;
        let arguments = match scope {
            Some(scope) => {
                let arguments = scope
                    .canonicalize_call(
                        canonical.local_name(),
                        &original_arguments,
                        &self.scope.workspace_root,
                    )
                    .ok_or_else(|| not_allowed_by_skill(wire_name))?;
                self.validate_arguments(&canonical, &arguments, wire_name)?;
                arguments
            }
            None => original_arguments.clone(),
        };
        let Some(descriptor) = self.catalog.descriptor(&canonical).cloned() else {
            return Err(not_found(wire_name));
        };
        let Some(tool) = self.catalog.resolve(&canonical) else {
            return Err(not_found(wire_name));
        };
        let canonical_arguments = canonical_arguments(&arguments).map_err(|error| {
            ToolError::new(
                "policy.fingerprint_failed",
                error.to_string(),
                Retryability::Never,
            )
        })?;
        let sandbox = sandbox_obligation(&self.scope, &descriptor);
        let request = PolicyRequest {
            session_id: context.session_id.clone(),
            turn_id: context.turn_id.clone(),
            call_id: context.call_id.clone(),
            tool_name: canonical,
            arguments_digest: sha256_hex(&canonical_arguments),
            capabilities: descriptor.capabilities,
            side_effect: descriptor.side_effect,
            mode: self.scope.mode,
            project_trusted: self.scope.project_trusted,
            sandbox,
        };
        let fingerprint = approval_fingerprint(&request).map_err(|error| {
            ToolError::new(
                "policy.fingerprint_failed",
                error.to_string(),
                Retryability::Never,
            )
        })?;
        let decision = self.policy.evaluate(&request);
        let audit = PreparedToolAudit {
            call_id: context.call_id.clone(),
            tool_name: descriptor.name,
            request_hash: journal_request_hash(request.tool_name.as_str(), &arguments),
            approval_fingerprint: fingerprint.clone(),
            idempotency: descriptor.idempotency,
            side_effect: descriptor.side_effect,
            sandbox: request.sandbox.clone(),
            capabilities: request.capabilities.clone(),
        };
        Ok(PreparedToolCall {
            context,
            tool,
            arguments,
            request,
            fingerprint,
            decision,
            audit,
            skill_scope_guard: scope.cloned().map(|scope| SkillScopeGuard {
                scope,
                cwd: self.scope.workspace_root.clone(),
                original_arguments,
            }),
        })
    }

    pub fn authorize(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: Value,
    ) -> Result<PreparedToolCall, ToolError> {
        self.prepare(context, wire_name, arguments)
    }

    pub fn decision<'a>(&self, prepared: &'a PreparedToolCall) -> &'a PolicyDecision {
        &prepared.decision
    }

    pub fn approve(&self, approval: &ApprovalRequest) -> Result<ExecutionGrant, ToolError> {
        self.policy.approve(approval).map_err(policy_engine_error)
    }

    pub async fn execute(
        &self,
        prepared: PreparedToolCall,
        grant: ExecutionGrant,
    ) -> Result<ToolOutput, ToolError> {
        if let PolicyDecision::Deny(denial) = &prepared.decision {
            return Err(policy_error(denial.code.clone(), denial.message.clone()));
        }
        let recomputed = approval_fingerprint(&prepared.request).map_err(|error| {
            ToolError::new(
                "policy.fingerprint_failed",
                error.to_string(),
                Retryability::Never,
            )
        })?;
        if recomputed != prepared.fingerprint {
            return Err(policy_error(
                "policy.grant_mismatch",
                "the prepared tool call changed after authorization",
            ));
        }
        // Scope preparation fingerprints the resolved path instead of the
        // caller's symlink spelling. Re-resolve immediately before consuming
        // the grant so a target swapped in the meantime fails closed. This
        // narrows the remaining OS-level race; eliminating it entirely would
        // require descriptor-relative openat-style I/O in every path tool.
        if let Some(guard) = &prepared.skill_scope_guard {
            let original_still_resolves_to_prepared = guard
                .scope
                .canonicalize_call(
                    prepared.request.tool_name.local_name(),
                    &guard.original_arguments,
                    &guard.cwd,
                )
                .is_some_and(|arguments| arguments == prepared.arguments);
            let prepared_target_still_allowed = guard
                .scope
                .canonicalize_call(
                    prepared.request.tool_name.local_name(),
                    &prepared.arguments,
                    &guard.cwd,
                )
                .is_some_and(|arguments| arguments == prepared.arguments);
            if !original_still_resolves_to_prepared || !prepared_target_still_allowed {
                return Err(not_allowed_by_skill(
                    prepared.request.tool_name.local_name(),
                ));
            }
        }
        self.policy
            .consume(&grant, &prepared.fingerprint, &prepared.request)
            .map_err(policy_engine_error)?;
        if prepared.context.cancellation.is_cancelled() {
            let error = ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            );
            self.emit_tool(
                PolicyEventKind::ToolFailed,
                &prepared.request,
                Some(error.code.clone()),
                None,
                None,
            );
            return Err(error);
        }
        self.emit_tool(
            PolicyEventKind::ToolStarted,
            &prepared.request,
            None,
            None,
            None,
        );
        let started = Instant::now();
        let result = prepared
            .tool
            .invoke(
                prepared.context.with_execution_grant(grant),
                prepared.arguments,
            )
            .await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(output) => {
                self.emit_tool(
                    PolicyEventKind::ToolCompleted,
                    &prepared.request,
                    Some("ok".into()),
                    Some(elapsed_ms),
                    Some(output.content.len()),
                );
                Ok(output)
            }
            Err(error) => {
                self.emit_tool(
                    PolicyEventKind::ToolFailed,
                    &prepared.request,
                    Some(error.code.clone()),
                    Some(elapsed_ms),
                    None,
                );
                Err(error)
            }
        }
    }

    pub async fn execute_authorized(
        &self,
        prepared: PreparedToolCall,
        grant: ExecutionGrant,
    ) -> Result<ToolOutput, ToolError> {
        self.execute(prepared, grant).await
    }

    #[doc(hidden)]
    pub async fn execute_without_approval_for_test(
        &self,
        prepared: PreparedToolCall,
    ) -> Result<ToolOutput, ToolError> {
        match prepared.decision.clone() {
            PolicyDecision::Allow(grant) => self.execute(prepared, grant).await,
            PolicyDecision::RequireApproval(_) => Err(policy_error(
                "policy.grant_missing",
                "the prepared tool call has no approval grant",
            )),
            PolicyDecision::Deny(ref denial) => {
                Err(policy_error(denial.code.clone(), denial.message.clone()))
            }
        }
    }

    pub fn descriptor_for_wire_name(&self, wire_name: &str) -> Option<ToolDescriptor> {
        let canonical = self.resolve_wire_name(wire_name)?;
        self.catalog.descriptor(&canonical).cloned()
    }

    pub(crate) fn registered_local_names(&self) -> Vec<String> {
        self.catalog
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name.local_name().to_owned())
            .collect()
    }

    pub(crate) fn has_local_name(&self, wire_name: &str) -> bool {
        self.wire_names.contains_key(wire_name)
    }

    pub(crate) fn resolve_registered_name(&self, wire_name: &str) -> Option<ToolName> {
        self.resolve_wire_name(wire_name)
            .filter(|name| self.catalog.descriptor(name).is_some())
    }

    fn emit_tool(
        &self,
        kind: PolicyEventKind,
        request: &lato_core::PolicyRequest,
        code: Option<String>,
        elapsed_ms: Option<u64>,
        output_bytes: Option<usize>,
    ) {
        self.sink.emit(PolicyEvent {
            kind,
            session_id: Some(request.session_id.clone()),
            turn_id: Some(request.turn_id.clone()),
            call_id: Some(request.call_id.clone()),
            tool_name: Some(request.tool_name.clone()),
            argument_digest: Some(request.arguments_digest.clone()),
            code,
            elapsed_ms,
            output_bytes,
        });
    }

    fn resolve_wire_name(&self, wire_name: &str) -> Option<ToolName> {
        let normalized = wire_name.strip_prefix("Lato:").unwrap_or(wire_name);
        let normalized = if normalized == "write" {
            "write_file"
        } else {
            normalized
        };
        if normalized.contains(':') {
            ToolName::parse(normalized).ok()
        } else {
            self.wire_names.get(normalized).cloned()
        }
    }

    fn validate_arguments(
        &self,
        canonical_name: &ToolName,
        arguments: &Value,
        wire_name: &str,
    ) -> Result<(), ToolError> {
        let Some(validator) = self.validators.get(canonical_name) else {
            return Err(not_found(wire_name));
        };
        validator.validate(arguments).map_err(|error| {
            ToolError::new(
                "tool.invalid_arguments",
                error.to_string(),
                Retryability::Never,
            )
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedToolCall {
    pub wire_name: String,
    pub canonical_name: ToolName,
    pub arguments: Value,
}

pub fn builtin_tool_runtime(
    environment: BuiltinToolEnvironment,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    build_builtin_tool_runtime(environment, None, None)
}

pub fn builtin_tool_runtime_for_capabilities(
    environment: BuiltinToolEnvironment,
    capabilities: Option<&[lato_core::ToolCapability]>,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    build_builtin_tool_runtime(environment, capabilities, None)
}

pub fn builtin_tool_runtime_with_subagents(
    environment: BuiltinToolEnvironment,
    backend: lato_runtime::SubagentBackendResource,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    build_builtin_tool_runtime(environment, None, Some(backend))
}

pub fn builtin_tool_runtime_for_capabilities_with_subagents(
    environment: BuiltinToolEnvironment,
    capabilities: Option<&[lato_core::ToolCapability]>,
    backend: lato_runtime::SubagentBackendResource,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    build_builtin_tool_runtime(environment, capabilities, Some(backend))
}

fn build_builtin_tool_runtime(
    environment: BuiltinToolEnvironment,
    capabilities: Option<&[lato_core::ToolCapability]>,
    backend: Option<lato_runtime::SubagentBackendResource>,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    let scope = PolicyScope {
        workspace_root: environment.cwd.clone(),
        mode: match environment.trust.mode {
            lato_workspace::ApprovalMode::Ask => PolicyMode::Ask,
            lato_workspace::ApprovalMode::Auto => PolicyMode::Auto,
            lato_workspace::ApprovalMode::Always => PolicyMode::Always,
        },
        project_trusted: environment.trust.cwd_trusted(),
        sandbox_profile: match environment.trust.sandbox {
            lato_workspace::SandboxProfile::Off => SandboxProfile::Off,
            lato_workspace::SandboxProfile::Workspace => SandboxProfile::Workspace,
            lato_workspace::SandboxProfile::ReadOnly => SandboxProfile::ReadOnly,
        },
    };
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(policy, scope);
    for tool in builtin_tools(environment)? {
        let descriptor = tool.descriptor();
        if capabilities.is_none_or(|allowed| {
            descriptor
                .capabilities
                .iter()
                .all(|capability| allowed.contains(capability))
        }) {
            builder.register(tool)?;
        }
    }
    if let Some(backend) = backend {
        for tool in task_tools(backend) {
            let descriptor = tool.descriptor();
            if capabilities.is_none_or(|allowed| {
                descriptor
                    .capabilities
                    .iter()
                    .all(|capability| allowed.contains(capability))
            }) {
                builder.register(tool)?;
            }
        }
    }
    Ok(Arc::new(builder.build()?))
}

fn sandbox_obligation(scope: &PolicyScope, descriptor: &ToolDescriptor) -> SandboxObligation {
    let mut obligation =
        SandboxObligation::for_profile(scope.sandbox_profile, &scope.workspace_root);
    if descriptor
        .capabilities
        .contains(&lato_core::ToolCapability::NetworkRead)
    {
        obligation.network = NetworkPolicy::PublicHttpsRead;
    }
    obligation.environment = EnvironmentPolicy::default();
    obligation
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn policy_engine_error(error: lato_policy::PolicyError) -> ToolError {
    policy_error(error.code(), error.to_string())
}

fn policy_error(code: impl Into<String>, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message, Retryability::Never)
}

fn not_allowed_by_skill(wire_name: &str) -> ToolError {
    ToolError::new(
        "tool.not_allowed_by_skill",
        format!("tool {wire_name} is not allowed by the active skill"),
        Retryability::Never,
    )
}

fn not_found(wire_name: &str) -> ToolError {
    ToolError::new(
        "tool.not_found",
        format!("tool {wire_name} was not found"),
        Retryability::Never,
    )
}
