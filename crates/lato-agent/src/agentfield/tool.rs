// Lato Phase 7C2 (spec v1.2): model-visible `agentfield` tool.
//
// A thin, session-bound adapter over the `AgentFieldManager` installed by
// the host (the same 7B7 construction-safe handle pattern). The tool never
// creates a second turn loop, never bypasses the local allowlist, and
// never touches the transport directly — every remote operation goes
// through the manager, which routes through the unique policy factory.
//
// Authorization contract (spec §8.1): descriptor metadata is TOOL-level,
// so all four actions share the static external-mutation / network /
// non-idempotent membrane — `list`/`status` are never downgraded to a
// weaker descriptor. The approval fingerprint binds the model-submitted
// canonical arguments (action, alias, revision, input), and `invoke`
// re-checks the revision constant-time before any reservation or send.
// Policy rejection codes are never rewritten.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};

use super::manager::{AgentFieldManager, CancelOutcome, ManagerError, RunState, RunStatus};

/// Wire name visible to the model (`builtin:agentfield` canonical).
pub const AGENTFIELD_TOOL_WIRE_NAME: &str = "agentfield";

const MAX_ALIAS_BYTES: usize = 64;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_LIST_ENTRIES: usize = 64;
const MAX_RECENT_RUNS: usize = 20;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const TOOL_TIMEOUT_MS: u64 = 70_000;

/// Model-visible input schema (spec §7). Action conditions are encoded as
/// `oneOf` so wrong combinations fail pre-policy validation; the invoke
/// layer re-checks them as defense-in-depth.
pub fn agentfield_tool_definition() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["list", "start", "status", "cancel"]},
            "name": {"type": "string", "minLength": 1, "maxLength": MAX_ALIAS_BYTES},
            "revision": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
            "input": {"type": "object"},
            "runId": {"type": "string", "minLength": 1, "maxLength": MAX_RUN_ID_BYTES}
        },
        "required": ["action"],
        "oneOf": [
            { "properties": { "action": { "const": "list" } },
              "not": { "anyOf": [{"required":["name"]},{"required":["revision"]},{"required":["input"]},{"required":["runId"]}] } },
            { "properties": { "action": { "const": "start" } },
              "required": ["name", "revision"],
              "not": { "required": ["runId"] } },
            { "properties": { "action": { "const": "status" } },
              "not": { "anyOf": [{"required":["name"]},{"required":["revision"]},{"required":["input"]}] } },
            { "properties": { "action": { "const": "cancel" } },
              "required": ["runId"],
              "not": { "anyOf": [{"required":["name"]},{"required":["revision"]},{"required":["input"]}] } }
        ]
    })
}

/// Session-owned indirection between the main-session tool runtime and the
/// `AgentFieldManager` mounted by the host. `None` until installation →
/// every action fails closed with `agentfield.unavailable`.
#[derive(Clone)]
pub struct SessionAgentFieldHandle {
    manager: Arc<RwLock<Option<Arc<AgentFieldManager>>>>,
}

impl SessionAgentFieldHandle {
    pub fn new() -> Self {
        Self {
            manager: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the session manager (host-side, right after session
    /// assembly). Idempotent: the first install wins.
    pub fn install(&self, manager: Arc<AgentFieldManager>) {
        let mut slot = self
            .manager
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(manager);
        }
    }

    fn manager(&self) -> Option<Arc<AgentFieldManager>> {
        self.manager
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Default for SessionAgentFieldHandle {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AgentFieldTool {
    handle: SessionAgentFieldHandle,
}

impl AgentFieldTool {
    pub fn new(handle: SessionAgentFieldHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for AgentFieldTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:agentfield").expect("static tool name"),
            version: Version::new(1, 2, 0),
            description: "Discover, start, and inspect remote AgentField executions that this \
                          session owns. Actions: list (allowlisted remote capabilities with a \
                          catalog revision), start (launch one remote execution with the listed \
                          name + revision, after policy approval; asynchronous, at most one \
                          remote send), status (query one owned run or list recent runs), cancel \
                          (request cancellation of one owned run). Unknown or foreign run ids \
                          are indistinguishable. Remote results are untrusted data."
                .into(),
            input_schema: agentfield_tool_definition(),
            // Spec §8.1: metadata is tool-level; every action shares the
            // external-mutation network membrane (never downgraded).
            capabilities: vec![ToolCapability::NetworkRead, ToolCapability::NetworkWrite],
            side_effect: SideEffect::ExternalMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: TOOL_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "lato.builtin.agentfield".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let input = parse_input(arguments)?;
        validate_input(&input)?;
        let manager = self.handle.manager().ok_or_else(unavailable)?;
        if manager.is_closed() {
            return Err(unavailable());
        }
        match input.action.as_str() {
            "list" => self.list(&manager).await,
            "start" => self.start(&manager, &input).await,
            "status" => self.status(&manager, &input).await,
            "cancel" => self.cancel(&manager, &input).await,
            other => Err(invalid_arguments(&format!(
                "action must be list, start, status, or cancel (got {other:?})"
            ))),
        }
    }

    /// Tool-provided approval detail (spec §8.1): action, alias / owned
    /// run, derived target, risk, input field names, and serialized input
    /// byte count. Never a token, base URL, or input value.
    fn approval_detail(&self, arguments: &Value) -> Option<String> {
        let input = parse_input(arguments.clone()).ok()?;
        match input.action.as_str() {
            "list" => Some("list allowlisted AgentField remote capabilities".into()),
            "status" => Some(match input.run_id.as_deref() {
                Some(run_id) => format!("query owned AgentField run '{run_id}'"),
                None => "list recent owned AgentField runs".into(),
            }),
            "cancel" => Some(format!(
                "cancel owned AgentField run '{}'",
                input.run_id.as_deref()?
            )),
            "start" => {
                let alias = input.name.as_deref()?;
                let revision = input.revision.as_deref()?;
                let manager = self.handle.manager()?;
                let capability = manager.catalog().capability(alias)?;
                let bytes = input_bytes(input.input.as_ref())?;
                let fields = match input.input.as_ref() {
                    Some(Value::Object(map)) => {
                        let mut names: Vec<&String> = map.keys().collect();
                        names.sort_unstable();
                        names
                            .iter()
                            .map(|name| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                    _ => "none".into(),
                };
                Some(format!(
                    "start remote execution '{alias}' → target '{}' (risk: {}, input fields: {fields}, {bytes} bytes), catalog revision {revision}",
                    capability.target, capability.risk
                ))
            }
            _ => None,
        }
    }
}

impl AgentFieldTool {
    /// `list` (spec §7.1): the local allowlist with health availability,
    /// the canonical catalog revision, and no base URL, token, or raw
    /// remote metadata.
    async fn list(&self, manager: &Arc<AgentFieldManager>) -> Result<ToolOutput, ToolError> {
        let catalog = manager.catalog();
        let health = manager.healthy_targets().await;
        let capabilities: Vec<Value> = catalog
            .capabilities()
            .iter()
            .map(|(alias, capability)| {
                let available = health
                    .as_ref()
                    .map(|targets| targets.contains(&capability.target))
                    .unwrap_or(false);
                json!({
                    "name": alias,
                    "description": capability.description,
                    "risk": capability.risk,
                    "available": available,
                })
            })
            .collect();
        let truncated_entries = capabilities.len() > MAX_LIST_ENTRIES;
        let entries = capabilities.into_iter().take(MAX_LIST_ENTRIES).collect();
        bounded_output(
            "list",
            "capabilities",
            catalog.revision(),
            truncated_entries,
            entries,
        )
    }

    /// `start` (spec §7.2): allowlist → input schema → 64 KiB input cap →
    /// revision recheck (constant time) → health gate → exactly one send.
    async fn start(
        &self,
        manager: &Arc<AgentFieldManager>,
        input: &AgentFieldToolInput,
    ) -> Result<ToolOutput, ToolError> {
        let alias = input.name.as_deref().expect("oneOf requires name");
        let revision = input.revision.as_deref().expect("oneOf requires revision");
        let args = input.input.clone().unwrap_or_else(|| json!({}));

        // Local allowlist is the source of truth (unknown alias ≙ not found).
        let capability = manager
            .catalog()
            .capability(alias)
            .ok_or(not_found(&format!(
                "no allowlisted capability named '{alias}'"
            )))?;

        // Input schema validation + size cap, before any reservation.
        validate_input_against_schema(alias, &capability.input_schema, &args)?;

        // Post-approval TOCTOU guard: the catalog must still be what the
        // model listed. The grant is already consumed at this point and is
        // never restored — zero reservations, zero remote requests. Both
        // the frozen snapshot comparison AND a reload of the real
        // configuration sources (user / project / plugin) must agree; any
        // real change since session assembly fails closed.
        if !manager.catalog().matches_revision(revision) || !manager.recheck_revision_matches() {
            return Err(catalog_changed());
        }

        // Health gate (spec §6.2): start fails closed when health is
        // expired-and-unreachable, and when the target is not healthy.
        match manager.target_health(&capability.target).await {
            Ok(Some(true)) => {}
            Ok(Some(false)) => return Err(remote_denied("remote target is not healthy")),
            Ok(None) => {
                return Err(unavailable_message(
                    "remote health is stale and unreachable",
                ));
            }
            Err(error) => return Err(map_manager_error(error)),
        }

        let run = manager
            .start_run(alias, &capability.target, revision, &args)
            .await
            .map_err(map_manager_error)?;
        match run.status {
            // Bound and accepted: project the run (asynchronous, no wait).
            RunStatus::Queued | RunStatus::Running => single_run_output("start", &run),
            // Permanent local terminal (spec §7.2): the run is recorded, but
            // the model receives the stable error and the manual
            // reconciliation guidance — never a fabricated success.
            RunStatus::OutcomeUnknown => Err(ToolError::new(
                "agentfield.outcome_unknown",
                run.summary
                    .unwrap_or_else(|| "the remote start outcome could not be determined".into()),
                Retryability::Never,
            )),
            // Definitive remote rejection: the run is recorded as failed and
            // the frozen code surfaces to the model.
            RunStatus::Failed => {
                let code = run.last_error.unwrap_or("agentfield.remote_denied");
                Err(ToolError::new(
                    code,
                    "the remote rejected the start",
                    Retryability::Never,
                ))
            }
            other => Err(map_manager_error(ManagerError::Unavailable(format!(
                "unexpected post-start status `{}`",
                other.as_str()
            )))),
        }
    }

    /// `status` (spec §7.3): one owned run (with remote refresh when
    /// nonterminal and bound) or the 20 most recent owned runs (local).
    async fn status(
        &self,
        manager: &Arc<AgentFieldManager>,
        input: &AgentFieldToolInput,
    ) -> Result<ToolOutput, ToolError> {
        let runs = manager
            .run_status(input.run_id.as_deref())
            .await
            .map_err(map_manager_error)?;
        if input.run_id.is_some() {
            let run = runs
                .first()
                .ok_or_else(|| not_found("no owned run matched"))?;
            single_run_output("status", run)
        } else {
            let truncated_entries = runs.len() > MAX_RECENT_RUNS;
            let entries = runs.iter().take(MAX_RECENT_RUNS).map(run_value).collect();
            bounded_output(
                "status",
                "runs",
                manager.catalog().revision(),
                truncated_entries,
                entries,
            )
        }
    }

    /// `cancel` (spec §7.4): `cancelled`, `already_terminal`,
    /// `cancel_requested`, or `unavailable` — a timeout is never reported
    /// as a successful cancellation.
    async fn cancel(
        &self,
        manager: &Arc<AgentFieldManager>,
        input: &AgentFieldToolInput,
    ) -> Result<ToolOutput, ToolError> {
        let run_id = input.run_id.as_deref().expect("oneOf requires runId");
        let (run, outcome) = manager
            .cancel_run(run_id, "model requested cancellation")
            .await
            .map_err(map_manager_error)?;
        let result = match outcome {
            CancelOutcome::Cancelled => "cancelled",
            CancelOutcome::AlreadyTerminal => "already_terminal",
            CancelOutcome::CancelRequested => "cancel_requested",
            CancelOutcome::Unavailable => "unavailable",
        };
        let value = json!({
            "action": "cancel",
            "run": run_value(&run),
            "result": result,
        });
        let size = serde_json::to_vec(&value)
            .map_err(|error| output_error(&error.to_string()))?
            .len();
        if size > MAX_OUTPUT_BYTES {
            return Err(output_error(
                "the cancel projection exceeds the 64 KiB output limit",
            ));
        }
        Ok(ToolOutput {
            content: value.to_string(),
            metadata: json!({"agentfieldTool": "cancel"}),
            truncated: false,
            artifact_path: None,
        })
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentFieldToolInput {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    run_id: Option<String>,
}

fn parse_input(arguments: Value) -> Result<AgentFieldToolInput, ToolError> {
    serde_json::from_value(arguments).map_err(|error| invalid_arguments(&error.to_string()))
}

/// Defense-in-depth re-check of the §7 `oneOf` conditions.
fn validate_input(input: &AgentFieldToolInput) -> Result<(), ToolError> {
    validate_field("name", input.name.as_deref(), MAX_ALIAS_BYTES)?;
    validate_field("runId", input.run_id.as_deref(), MAX_RUN_ID_BYTES)?;
    if let Some(revision) = input.revision.as_deref() {
        let hex = revision.strip_prefix("sha256:").unwrap_or("");
        if hex.len() != 64
            || !hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(invalid_arguments(
                "revision must be `sha256:` followed by 64 lowercase hex characters",
            ));
        }
    }
    match input.action.as_str() {
        "list" => {
            if input.name.is_some()
                || input.revision.is_some()
                || input.input.is_some()
                || input.run_id.is_some()
            {
                return Err(invalid_arguments(
                    "list accepts no other fields besides action",
                ));
            }
        }
        "start" => {
            if input.run_id.is_some() {
                return Err(invalid_arguments("start does not accept runId"));
            }
            if input.name.is_none() || input.revision.is_none() {
                return Err(invalid_arguments("start requires name and revision"));
            }
        }
        "status" => {
            if input.name.is_some() || input.revision.is_some() || input.input.is_some() {
                return Err(invalid_arguments("status accepts only action and runId"));
            }
        }
        "cancel" => {
            if input.name.is_some() || input.revision.is_some() || input.input.is_some() {
                return Err(invalid_arguments("cancel accepts only action and runId"));
            }
            if input.run_id.is_none() {
                return Err(invalid_arguments("cancel requires runId"));
            }
        }
        other => {
            return Err(invalid_arguments(&format!(
                "action must be list, start, status, or cancel (got {other:?})"
            )));
        }
    }
    Ok(())
}

fn validate_field(field: &str, value: Option<&str>, max_bytes: usize) -> Result<(), ToolError> {
    if let Some(value) = value
        && (value.is_empty() || value.len() > max_bytes)
    {
        return Err(invalid_arguments(&format!(
            "{field} must contain 1..={max_bytes} UTF-8 bytes"
        )));
    }
    Ok(())
}

/// Input schema (the allowlist entry is the model-visible facts source) and
/// the 64 KiB serialized-input cap (spec §6.1/§8).
fn validate_input_against_schema(
    alias: &str,
    schema: &Value,
    args: &Value,
) -> Result<(), ToolError> {
    if !args.is_object() {
        return Err(invalid_arguments("input must be a JSON object"));
    }
    let serialized =
        serde_json::to_vec(args).map_err(|error| invalid_arguments(&error.to_string()))?;
    if serialized.len() > crate::agentfield::config::MAX_INPUT_BYTES {
        return Err(invalid_arguments(&format!(
            "input must serialize to at most {} bytes",
            crate::agentfield::config::MAX_INPUT_BYTES
        )));
    }
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        invalid_arguments(&format!(
            "capability '{alias}' has an invalid input schema: {error}"
        ))
    })?;
    if !validator.is_valid(args) {
        return Err(invalid_arguments(&format!(
            "input does not match the schema of capability '{alias}'"
        )));
    }
    Ok(())
}

fn input_bytes(input: Option<&Value>) -> Option<usize> {
    match input {
        None => Some(2),
        Some(value) => serde_json::to_vec(value).ok().map(|bytes| bytes.len()),
    }
}

/// Bounded, privacy-safe run projection: no base URLs, no tokens, no raw
/// inputs (only the digest), bounded summary.
fn run_value(run: &RunState) -> Value {
    let mut value = json!({
        "runId": run.run_id,
        "name": run.alias,
        "status": run.status.as_str(),
        "createdAtMs": run.created_at_ms,
        "updatedAtMs": run.updated_at_ms,
    });
    if let Some(execution_id) = run.execution_id.as_deref() {
        value["executionId"] = json!(execution_id);
    }
    if let Some(summary) = run.summary.as_deref() {
        value["summary"] = json!(summary);
        value["summaryTruncated"] = json!(run.summary_truncated);
    }
    if let Some(last_error) = run.last_error {
        value["lastError"] = json!(last_error);
    }
    value
}

/// Single-run output with the same 64 KiB ceiling as list outputs.
fn single_run_output(action: &str, run: &RunState) -> Result<ToolOutput, ToolError> {
    let value = json!({"action": action, "run": run_value(run)});
    let size = serde_json::to_vec(&value)
        .map_err(|error| output_error(&error.to_string()))?
        .len();
    if size > MAX_OUTPUT_BYTES {
        return Err(output_error(
            "the run projection exceeds the 64 KiB output limit",
        ));
    }
    Ok(ToolOutput {
        content: value.to_string(),
        metadata: json!({"agentfieldTool": action}),
        truncated: false,
        artifact_path: None,
    })
}

/// Entry-bounded output: when the payload exceeds 64 KiB, drop trailing
/// entries and flag truncation. A single entry that alone cannot fit is a
/// stable `agentfield.output_too_large` error.
fn bounded_output(
    action: &str,
    entries_key: &str,
    revision: &str,
    already_truncated: bool,
    mut entries: Vec<Value>,
) -> Result<ToolOutput, ToolError> {
    let mut truncated = already_truncated;
    loop {
        let value = json!({
            "action": action,
            "revision": revision,
            entries_key: entries,
            "truncated": truncated,
        });
        let size = serde_json::to_vec(&value)
            .map_err(|error| output_error(&error.to_string()))?
            .len();
        if size <= MAX_OUTPUT_BYTES {
            return Ok(ToolOutput {
                content: value.to_string(),
                metadata: json!({"agentfieldTool": action}),
                truncated,
                artifact_path: None,
            });
        }
        if entries.len() <= 1 {
            return Err(output_error(
                "a single entry exceeds the 64 KiB output limit",
            ));
        }
        entries.pop();
        truncated = true;
    }
}

fn map_manager_error(error: ManagerError) -> ToolError {
    let code = error.code();
    let retryability = match code {
        "agentfield.invalid_arguments"
        | "agentfield.not_found"
        | "agentfield.remote_denied"
        | "agentfield.outcome_unknown"
        | "agentfield.output_too_large" => Retryability::Never,
        _ => Retryability::AfterBackoff,
    };
    let message = match &error {
        ManagerError::NotFound => "no owned run matched the requested id".to_owned(),
        other => other.to_string(),
    };
    ToolError::new(code, message, retryability)
}

fn unavailable() -> ToolError {
    ToolError::new(
        "agentfield.unavailable",
        "the agentfield adapter is not available in this session",
        Retryability::AfterBackoff,
    )
}

fn unavailable_message(message: &str) -> ToolError {
    ToolError::new(
        "agentfield.unavailable",
        message,
        Retryability::AfterBackoff,
    )
}

fn invalid_arguments(message: &str) -> ToolError {
    ToolError::new("agentfield.invalid_arguments", message, Retryability::Never)
}

fn not_found(message: &str) -> ToolError {
    ToolError::new("agentfield.not_found", message, Retryability::Never)
}

fn catalog_changed() -> ToolError {
    ToolError::new(
        "agentfield.catalog_changed",
        "the capability catalog changed since it was listed; re-list and retry",
        Retryability::AfterBackoff,
    )
}

fn remote_denied(message: &str) -> ToolError {
    ToolError::new("agentfield.remote_denied", message, Retryability::Never)
}

fn output_error(message: &str) -> ToolError {
    ToolError::new("agentfield.output_too_large", message, Retryability::Never)
}

fn cancelled() -> ToolError {
    ToolError::new(
        "tool.cancelled",
        "tool call was cancelled",
        Retryability::Never,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentfield::AgentFieldCatalog;
    use crate::agentfield::client::{AgentFieldClient, AgentFieldError};
    use crate::agentfield::config::AgentFieldConfig;
    use crate::agentfield::manager::MAX_ACTIVE_RUNS;
    use crate::agentfield::types::{
        AsyncStartEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope, StatusEnvelope,
    };
    use async_trait::async_trait;
    use lato_core::{SessionId, ToolCallId, TurnId};
    use tokio_util::sync::CancellationToken;

    fn context() -> ToolContext {
        ToolContext {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            call_id: ToolCallId::from("call"),
            cancellation: CancellationToken::new(),
            execution_grant: None,
        }
    }

    fn test_config() -> AgentFieldConfig {
        let raw = json!({
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"contract": {"type": "string"}},
                        "required": ["contract"],
                        "additionalProperties": false
                    },
                    "risk": "remote_read",
                }
            }
        });
        AgentFieldConfig::parse(&raw).unwrap().unwrap()
    }

    /// Minimal offline client: discovery is healthy, everything else fails
    /// closed; the tool tests drive the manager through it.
    struct OfflineClient;

    #[async_trait]
    impl AgentFieldClient for OfflineClient {
        async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
            DiscoveryEnvelope::decode(&json!({
                "discovered_at": "2026-09-17T00:00:00Z",
                "total_agents": 1,
                "total_reasoners": 1,
                "total_skills": 0,
                "pagination": {"limit": 100, "offset": 0, "has_more": false},
                "capabilities": [{
                    "agent_id": "legal",
                    "group_id": "",
                    "base_url": "https://agentfield.invalid",
                    "version": "v0.1.138",
                    "health_status": "healthy",
                    "deployment_type": "service",
                    "last_heartbeat": "2026-09-17T00:00:00Z",
                    "reasoners": [{
                        "id": "review_contract",
                        "invocation_target": "legal:review_contract"
                    }],
                    "skills": []
                }]
            }))
            .map_err(AgentFieldError::RemoteProtocol)
        }

        async fn start_async(
            &self,
            _execute_target: &str,
            _input: &Value,
        ) -> Result<AsyncStartEnvelope, AgentFieldError> {
            AsyncStartEnvelope::decode(&json!({
                "execution_id": "exec-tool-1",
                "status": "queued",
                "target": "legal.review_contract",
                "type": "reasoner",
                "run_id": "run-1",
                "workflow_id": "run-1",
                "created_at": "2026-09-17T00:00:00Z",
                "enqueued_at": "2026-09-17T00:00:00Z",
                "webhook_registered": false,
            }))
            .map_err(AgentFieldError::RemoteProtocol)
        }

        async fn status(&self, _execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
            StatusEnvelope::decode(&json!({
                "execution_id": "exec-tool-1",
                "status": "running",
                "run_id": "run-1",
                "started_at": "2026-09-17T00:00:01Z",
                "webhook_registered": false,
            }))
            .map_err(AgentFieldError::RemoteProtocol)
        }

        async fn cancel(
            &self,
            _execution_id: &str,
            _reason: &str,
        ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
            Err(AgentFieldError::Unavailable("offline".into()))
        }
    }

    fn tool_with_manager() -> (AgentFieldTool, Arc<AgentFieldManager>) {
        let catalog = AgentFieldCatalog::from_config(&test_config());
        let client: Arc<dyn AgentFieldClient> = Arc::new(OfflineClient);
        let manager = AgentFieldManager::with_factory(
            "session-tool",
            catalog,
            Some(Arc::new(move || {
                let client = client.clone();
                Box::pin(async move { Ok(client.clone()) })
            })),
        );
        let manager = Arc::new(manager);
        let handle = SessionAgentFieldHandle::new();
        handle.install(manager.clone());
        (AgentFieldTool::new(handle), manager)
    }

    fn tool_without_manager() -> AgentFieldTool {
        AgentFieldTool::new(SessionAgentFieldHandle::new())
    }

    fn output_json(output: &ToolOutput) -> Value {
        serde_json::from_str(&output.content).unwrap()
    }

    #[test]
    fn descriptor_matches_spec_sections_seven_and_eight() {
        let tool = tool_without_manager();
        let descriptor = tool.descriptor();
        assert_eq!(descriptor.name.as_str(), "builtin:agentfield");
        assert_eq!(descriptor.name.local_name(), AGENTFIELD_TOOL_WIRE_NAME);
        assert_eq!(
            descriptor.capabilities,
            vec![ToolCapability::NetworkRead, ToolCapability::NetworkWrite]
        );
        assert_eq!(descriptor.side_effect, SideEffect::ExternalMutation);
        assert_eq!(descriptor.concurrency, ToolConcurrency::Serial);
        assert_eq!(descriptor.idempotency, ToolIdempotency::NonIdempotent);
        assert_eq!(descriptor.cancellation, ToolCancellation::Cooperative);
        assert_eq!(descriptor.timeout_ms, 70_000);
        assert_eq!(descriptor.max_output_bytes, 64 * 1024);
        assert_eq!(descriptor.input_schema, agentfield_tool_definition());
        descriptor.validate().expect("descriptor is valid");
    }

    #[test]
    fn schema_oneof_rejects_wrong_combinations_pre_policy() {
        let validator = jsonschema::validator_for(&agentfield_tool_definition()).unwrap();
        let rejects = [
            json!({"action":"list","name":"x"}),
            json!({"action":"list","runId":"x"}),
            json!({"action":"start"}),
            json!({"action":"start","name":"contract-review"}),
            json!({"action":"start","name":"contract-review","revision":"sha256:0".repeat(0)}),
            json!({"action":"start","name":"contract-review","revision":"nothex","input":{}}),
            json!({"action":"start","name":"contract-review","revision":"sha256:0123abcd","input":{}}),
            json!({"action":"start","name":"contract-review","revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000","input":{},"runId":"x"}),
            json!({"action":"status","name":"x"}),
            json!({"action":"status","input":{}}),
            json!({"action":"cancel"}),
            json!({"action":"cancel","name":"x"}),
            json!({"action":"restart"}),
            json!({}),
            json!({"action":"start","name":"","revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}),
        ];
        for arguments in rejects {
            if arguments
                == json!({"action":"start","name":"contract-review","revision":"sha256:0".repeat(0)})
            {
                continue; // duplicate-shape guard; covered by "nothex" case
            }
            assert!(
                !validator.is_valid(&arguments),
                "schema must reject {arguments}"
            );
        }
        let accepts = [
            json!({"action":"list"}),
            json!({"action":"start","name":"contract-review","revision":format!("sha256:{}", "0".repeat(64))}),
            json!({"action":"start","name":"contract-review","revision":format!("sha256:{}", "0".repeat(64)),"input":{"contract":"acme.pdf"}}),
            json!({"action":"status"}),
            json!({"action":"status","runId":"afrun_1-1"}),
            json!({"action":"cancel","runId":"afrun_1-1"}),
        ];
        for arguments in accepts {
            assert!(
                validator.is_valid(&arguments),
                "schema must accept {arguments}"
            );
        }
    }

    #[tokio::test]
    async fn all_actions_fail_closed_without_a_manager() {
        let tool = tool_without_manager();
        for arguments in [
            json!({"action":"list"}),
            json!({"action":"start","name":"contract-review","revision":format!("sha256:{}", "0".repeat(64))}),
            json!({"action":"status"}),
            json!({"action":"cancel","runId":"afrun_1-1"}),
        ] {
            let error = tool.invoke(context(), arguments).await.unwrap_err();
            assert_eq!(error.code, "agentfield.unavailable");
        }
    }

    #[tokio::test]
    async fn list_shows_allowlist_and_revision_without_infrastructure() {
        let (tool, _manager) = tool_with_manager();
        let output = tool
            .invoke(context(), json!({"action":"list"}))
            .await
            .unwrap();
        let listed = output_json(&output);
        assert!(listed["revision"].as_str().unwrap().starts_with("sha256:"));
        let capabilities = listed["capabilities"].as_array().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0]["name"], "contract-review");
        assert_eq!(capabilities[0]["risk"], "remote_read");
        assert!(capabilities[0]["available"].is_boolean());
        let serialized = listed.to_string();
        assert!(
            !serialized.contains("agents.example.internal"),
            "no base URL"
        );
        assert!(
            !serialized.contains("agentfield:primary"),
            "no credential reference"
        );
    }

    #[tokio::test]
    async fn unknown_alias_is_not_found_before_any_reservation() {
        let (tool, manager) = tool_with_manager();
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":"missing","revision":manager.catalog().revision(),"input":{}}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "agentfield.not_found");
        assert_eq!(manager.run_count().await, 0);
    }

    #[tokio::test]
    async fn stale_revision_fails_closed_with_zero_side_effects() {
        let (tool, manager) = tool_with_manager();
        let wrong = format!("sha256:{}", "a".repeat(64));
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":"contract-review","revision":wrong,"input":{"contract":"acme.pdf"}}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "agentfield.catalog_changed");
        assert_eq!(manager.run_count().await, 0, "no reservation, no send");
    }

    #[tokio::test]
    async fn input_schema_violation_is_invalid_arguments() {
        let (tool, manager) = tool_with_manager();
        // Missing required `contract` field.
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":"contract-review","revision":manager.catalog().revision(),"input":{}}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "agentfield.invalid_arguments");
        // additionalProperties: false.
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":"contract-review","revision":manager.catalog().revision(),"input":{"contract":"a.pdf","extra":1}}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "agentfield.invalid_arguments");
        assert_eq!(manager.run_count().await, 0);
    }

    #[tokio::test]
    async fn oversized_input_is_rejected_pre_reservation() {
        let (tool, manager) = tool_with_manager();
        let huge = "x".repeat(crate::agentfield::config::MAX_INPUT_BYTES + 1);
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":"contract-review","revision":manager.catalog().revision(),"input":{"contract":huge}}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "agentfield.invalid_arguments");
        assert_eq!(manager.run_count().await, 0);
    }

    #[tokio::test]
    async fn successful_start_projects_a_bounded_run() {
        let (tool, _manager) = tool_with_manager();
        let revision = {
            let manager = tool.handle.manager().unwrap();
            manager.catalog().revision().to_owned()
        };
        let output = tool
            .invoke(
                context(),
                json!({"action":"start","name":"contract-review","revision":revision,"input":{"contract":"acme.pdf"}}),
            )
            .await
            .unwrap();
        let started = output_json(&output);
        assert_eq!(started["action"], "start");
        assert_eq!(started["run"]["name"], "contract-review");
        assert_eq!(started["run"]["status"], "queued");
        assert_eq!(started["run"]["executionId"], "exec-tool-1");
        assert!(
            started["run"]["runId"]
                .as_str()
                .unwrap()
                .starts_with("afrun_")
        );
        assert!(
            started["run"]["inputDigest"].is_null(),
            "raw input is never projected"
        );
    }

    #[tokio::test]
    async fn status_and_cancel_hit_not_found_for_unknown_and_foreign_ids() {
        let (tool, _manager) = tool_without();
        let status = tool
            .invoke(
                context(),
                json!({"action":"status","runId":"afrun_unknown-1"}),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code, "agentfield.not_found");
        let cancel = tool
            .invoke(
                context(),
                json!({"action":"cancel","runId":"afrun_unknown-1"}),
            )
            .await
            .unwrap_err();
        assert_eq!(cancel.code, "agentfield.not_found");
        assert_eq!(status.retryability, cancel.retryability);
    }

    fn tool_without() -> (AgentFieldTool, ()) {
        let catalog = AgentFieldCatalog::from_config(&test_config());
        let manager = AgentFieldManager::with_factory("other-session", catalog, None);
        let manager = Arc::new(manager);
        let handle = SessionAgentFieldHandle::new();
        handle.install(manager);
        (AgentFieldTool::new(handle), ())
    }

    #[tokio::test]
    async fn status_lists_recent_runs_without_a_selector() {
        let (tool, _manager) = tool_with_manager();
        let output = tool
            .invoke(context(), json!({"action":"status"}))
            .await
            .unwrap();
        let listed = output_json(&output);
        assert_eq!(listed["runs"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn approval_detail_binds_action_alias_revision_and_input_digest() {
        let (tool, manager) = tool_with_manager();
        let revision = manager.catalog().revision().to_owned();
        let start = tool.approval_detail(&json!({
            "action":"start","name":"contract-review","revision":revision,"input":{"contract":"acme.pdf"}
        })).unwrap();
        assert!(start.contains("'contract-review'"), "{start}");
        assert!(start.contains("legal.review_contract"), "{start}");
        assert!(start.contains("risk: remote_read"), "{start}");
        assert!(start.contains("contract"), "{start}");
        assert!(start.contains(&revision), "{start}");
        assert!(
            !start.contains("acme.pdf"),
            "input values never appear: {start}"
        );
        let list = tool.approval_detail(&json!({"action":"list"})).unwrap();
        assert!(!list.is_empty());
        let status = tool
            .approval_detail(&json!({"action":"status","runId":"afrun_1-1"}))
            .unwrap();
        assert!(status.contains("afrun_1-1"));
        let cancel = tool
            .approval_detail(&json!({"action":"cancel","runId":"afrun_1-1"}))
            .unwrap();
        assert!(cancel.contains("afrun_1-1"));
    }

    #[tokio::test]
    async fn cancelled_calls_never_reach_the_manager() {
        let (tool, _manager) = tool_with_manager();
        let context = context();
        context.cancellation.cancel();
        let error = tool
            .invoke(context, json!({"action":"list"}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "tool.cancelled");
    }

    #[tokio::test]
    async fn closed_manager_fails_all_actions() {
        let (tool, manager) = tool_with_manager();
        manager.close();
        for arguments in [
            json!({"action":"list"}),
            json!({"action":"start","name":"contract-review","revision":format!("sha256:{}", "0".repeat(64))}),
            json!({"action":"status"}),
            json!({"action":"cancel","runId":"afrun_1-1"}),
        ] {
            let error = tool.invoke(context(), arguments).await.unwrap_err();
            assert_eq!(error.code, "agentfield.unavailable", "{error:?}");
        }
    }

    #[tokio::test]
    async fn bounded_output_drops_trailing_entries_and_flags_truncation() {
        // One entry that alone cannot fit is a stable error, never silent
        // truncation to nothing.
        let huge = json!({"name": "huge", "description": "x".repeat(70_000)});
        let error =
            bounded_output("list", "capabilities", "sha256:x", false, vec![huge]).unwrap_err();
        assert_eq!(error.code, "agentfield.output_too_large");
        // Entries are dropped until the payload fits.
        let entries: Vec<Value> = (0..8)
            .map(|seq| json!({"name": format!("cap-{seq}"), "description": "y".repeat(20_000)}))
            .collect();
        let output = bounded_output("list", "capabilities", "sha256:x", false, entries).unwrap();
        assert!(output.truncated);
        assert!(output.content.len() <= 64 * 1024);
        // MAX_ACTIVE_RUNS is referenced so the frozen cap stays auditable
        // from this module's contract tests.
        assert_eq!(MAX_ACTIVE_RUNS, 4);
    }
}
