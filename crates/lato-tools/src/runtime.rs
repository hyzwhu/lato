use crate::{
    BuiltinAdapterError, BuiltinToolEnvironment, CatalogError, RegistrationOutcome, ToolCatalog,
    builtin_tools,
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
        for descriptor in self.catalog.descriptors() {
            let wire_name = descriptor.name.local_name().to_owned();
            if let Some(first) = wire_names.insert(wire_name.clone(), descriptor.name.clone()) {
                return Err(RuntimeBuildError::AmbiguousWireName {
                    wire_name,
                    first,
                    second: descriptor.name,
                });
            }
        }
        Ok(ToolRuntime {
            catalog: self.catalog,
            wire_names,
            policy: self.policy,
            scope: self.scope,
            sink: self.sink,
        })
    }
}

impl ToolRuntime {
    pub fn model_definitions(&self) -> Vec<Value> {
        self.catalog
            .descriptors()
            .into_iter()
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
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            ));
        }

        let Some(canonical) = self.resolve_wire_name(wire_name) else {
            return Err(not_found(wire_name));
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
        let arguments = serde_json::from_slice(&canonical_arguments).map_err(|error| {
            ToolError::new(
                "tool.invalid_arguments",
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
}

pub fn builtin_tool_runtime(
    environment: BuiltinToolEnvironment,
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
    builder.register_builtin_tools(environment)?;
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

fn not_found(wire_name: &str) -> ToolError {
    ToolError::new(
        "tool.not_found",
        format!("tool {wire_name} was not found"),
        Retryability::Never,
    )
}
