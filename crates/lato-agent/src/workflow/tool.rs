// Lato Phase 7B7 (spec v1.2): model-visible `workflow` tool.
//
// A thin, session-bound adapter over the same `WorkflowManager` the TUI/ACP
// already drive (spec §6). The tool never creates a second turn loop, manager,
// or ACP self-call: it resolves and launches through the manager mounted by
// `attach_workflow_manager`, reusing the 7B4-7B6 registry, concurrency cap,
// journal persistence, host service, and update broadcast.
//
// Authorization contract (spec v1.2): policy behavior strictly follows the
// existing `PolicyMode` (Ask → human approval; Auto/Always → automatic
// one-shot grant). `PolicyDecision::Deny` is a decision result, not a fourth
// mode. The pre-policy fingerprint binds the model-submitted canonical
// arguments — qualified id, content `revision`, explicit `agentBudget`, args —
// and `invoke` re-resolves and constant-time-compares the revision before any
// launch. Policy rejection codes are never rewritten into `workflow.*` errors.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use lato_policy::canonical_arguments;
use lato_workflow::{WorkflowError, clamp_agent_budget};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    LaunchError, LaunchSpec, ResolvedWorkflow, WorkflowManager, WorkflowRunState,
    WorkflowRunStatus, list_workflows, resolve_workflow,
};
use lato_workspace::SessionTrust;

/// Wire name visible to the model (`builtin:workflow` canonical).
pub const WORKFLOW_TOOL_WIRE_NAME: &str = "workflow";

const MAX_NAME_BYTES: usize = 256;
const MAX_LIST_ENTRIES: usize = 64;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARGS_BYTES: usize = 64 * 1024;
const MAX_ARGS_SUMMARY_CHARS: usize = 200;
const TOOL_TIMEOUT_MS: u64 = 20_000;

/// Model-visible input schema (spec §4.1 v1.2). The action conditions are
/// encoded as `oneOf` so wrong combinations fail pre-policy validation; the
/// invoke layer re-checks them as defense-in-depth.
pub fn workflow_tool_definition() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["list", "start", "status"]},
            "name": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES},
            "run": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES},
            "revision": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
            "args": {"type": "object"},
            "agentBudget": {"type": "integer", "minimum": 1}
        },
        "required": ["action"],
        "oneOf": [
            { "properties": { "action": { "const": "list" } },
              "not": { "anyOf": [{"required":["name"]},{"required":["run"]},{"required":["revision"]},{"required":["args"]},{"required":["agentBudget"]}] } },
            { "properties": { "action": { "const": "start" } },
              "required": ["name", "revision", "agentBudget"],
              "not": { "required": ["run"] } },
            { "properties": { "action": { "const": "status" } },
              "not": { "anyOf": [{"required":["name"]},{"required":["revision"]},{"required":["args"]},{"required":["agentBudget"]}] } }
        ]
    })
}

/// Content identity of a resolved workflow: SHA-256 of the canonical JSON
/// `{id, source, script, declaredAgentBudget}` (spec §4.2 v1.2). Lowercase
/// hex; reuses the existing canonical serializer (sorted keys).
pub fn workflow_revision(resolved: &ResolvedWorkflow) -> String {
    let value = json!({
        "declaredAgentBudget": resolved.agent_budget,
        "id": resolved.id,
        "script": resolved.script,
        "source": resolved.source,
    });
    let canonical = canonical_arguments(&value).expect("canonical serialization cannot fail");
    let digest = Sha256::digest(&canonical);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

/// Constant-time comparison for revision digests.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in left.iter().zip(right.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Session-owned indirection between the main-session tool runtime and the
/// `WorkflowManager` mounted by the host (construction-safe pairing: the tool
/// runtime is built before the manager exists; the host installs it once the
/// session is assembled). `None` until installation → every action fails
/// closed with `workflow.unavailable`.
#[derive(Clone)]
pub struct SessionWorkflowHandle {
    cwd: PathBuf,
    lato_home: PathBuf,
    trust: SessionTrust,
    manager: Arc<RwLock<Option<Arc<WorkflowManager>>>>,
}

impl SessionWorkflowHandle {
    pub fn new(cwd: PathBuf, lato_home: PathBuf, trust: SessionTrust) -> Self {
        Self {
            cwd,
            lato_home,
            trust,
            manager: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the session manager (host-side, right after
    /// `attach_workflow_manager`). Idempotent: the first install wins.
    pub fn install(&self, manager: Arc<WorkflowManager>) {
        let mut slot = self
            .manager
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(manager);
        }
    }

    fn manager(&self) -> Option<Arc<WorkflowManager>> {
        self.manager
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

pub struct WorkflowTool {
    handle: SessionWorkflowHandle,
}

impl WorkflowTool {
    pub fn new(handle: SessionWorkflowHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:workflow").expect("static tool name"),
            version: Version::new(1, 2, 0),
            description: "Discover, start, and inspect named workflows. Actions: list (visible \
                          workflows with content revisions), start (launch a background run with \
                          the listed id + revision + explicit agentBudget, after policy \
                          approval), status (query one run or recent runs). Starting a workflow \
                          never blocks the conversation; the run keeps streaming its own \
                          updates. Model-initiated pause, resume, and stop are not supported."
                .into(),
            input_schema: workflow_tool_definition(),
            capabilities: vec![ToolCapability::TaskControl],
            side_effect: SideEffect::ExternalMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: TOOL_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "lato.builtin.workflow".into(),
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
            "list" => self.list(&manager),
            "start" => self.start(context, &manager, &input).await,
            "status" => self.status(&manager, &input).await,
            other => Err(invalid_arguments(&format!(
                "action must be list, start, or status (got {other:?})"
            ))),
        }
    }

    /// Tool-provided approval detail (spec §4.3): the Ask-mode summary shows
    /// the resolved workflow id, source, effective budget, and args digest.
    /// Purely informational — the binding contract is the revision argument,
    /// which is part of the pre-policy fingerprint.
    fn approval_detail(&self, arguments: &Value) -> Option<String> {
        let input = parse_input(arguments.clone()).ok()?;
        match input.action.as_str() {
            "list" => Some("list workflows visible to this session".into()),
            "status" => Some(match input.run.as_deref() {
                Some(run) => format!("query workflow run status for '{run}'"),
                None => "query recent workflow run statuses".into(),
            }),
            "start" => {
                let manager = self.handle.manager()?;
                let snapshot = manager.snapshot()?;
                let resolved = resolve_workflow(
                    &self.handle.cwd,
                    &self.handle.lato_home,
                    &snapshot,
                    self.handle.trust.cwd_trusted(),
                    input.name.as_deref()?,
                )
                .ok()?;
                let budget = effective_budget(&resolved, input.agent_budget).ok()?;
                Some(start_summary(&resolved, budget, input.args.as_ref()))
            }
            _ => None,
        }
    }
}

impl WorkflowTool {
    /// `list` (spec §4.2): the current trust/plugin snapshot's named scripts
    /// in registry keep-first order, bounded to 64 entries and 64 KiB, with
    /// content revisions and no script bodies or disk paths.
    fn list(&self, manager: &Arc<WorkflowManager>) -> Result<ToolOutput, ToolError> {
        let Some(snapshot) = manager.snapshot() else {
            return Err(unavailable());
        };
        let workflows = list_workflows(
            &self.handle.cwd,
            &self.handle.lato_home,
            &snapshot,
            self.handle.trust.cwd_trusted(),
        );
        let truncated_entries = workflows.len() > MAX_LIST_ENTRIES;
        let entries = workflows
            .iter()
            .take(MAX_LIST_ENTRIES)
            .map(|workflow| {
                json!({
                    "id": workflow.id,
                    "name": workflow.display_name,
                    "description": workflow.description,
                    "source": workflow.source,
                    "agentBudget": workflow.agent_budget,
                    "revision": workflow_revision(workflow),
                })
            })
            .collect();
        bounded_output("list", "workflows", truncated_entries, entries)
    }

    /// `start` (spec §4.3 v1.2): the model carries the listed qualified id,
    /// revision, and explicit budget; the pre-policy fingerprint already binds
    /// them. Invoke re-resolves and constant-time-compares the revision — any
    /// mismatch returns `workflow.catalog_changed` with zero side effects
    /// (the grant was consumed by `ToolRuntime::execute` before entering
    /// here and is never restored). Then launch through the same manager and
    /// return the initial snapshot immediately.
    async fn start(
        &self,
        context: ToolContext,
        manager: &Arc<WorkflowManager>,
        input: &WorkflowToolInput,
    ) -> Result<ToolOutput, ToolError> {
        let name = input
            .name
            .as_deref()
            .ok_or_else(|| invalid_arguments("start requires name"))?;
        let revision = input
            .revision
            .as_deref()
            .ok_or_else(|| invalid_arguments("start requires revision"))?;
        let budget = input
            .agent_budget
            .ok_or_else(|| invalid_arguments("start requires agentBudget"))?;
        let args = input.args.clone().unwrap_or_else(|| json!({}));
        let snapshot = manager.snapshot().ok_or_else(unavailable)?;
        let resolved = resolve_workflow(
            &self.handle.cwd,
            &self.handle.lato_home,
            &snapshot,
            self.handle.trust.cwd_trusted(),
            name,
        )
        .map_err(|error| resolve_error(&error))?;
        let budget = u64::from(
            clamp_agent_budget(Some(budget))
                .map_err(|error| invalid_arguments(&error.to_string()))?,
        );
        // TOCTOU guard: content identity must match what the model listed.
        if !constant_time_eq(revision.as_bytes(), workflow_revision(&resolved).as_bytes()) {
            return Err(catalog_changed());
        }
        // Cancellation before launch means no run is ever created.
        if context.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let state = manager
            .launch(
                resolved,
                LaunchSpec {
                    args,
                    agent_budget: Some(budget),
                    resume_display_name: None,
                },
            )
            .map_err(launch_error)?;
        // A cancellation arriving after a successful launch is NOT a stop
        // (spec §4.3): the run stays live and its updates keep streaming.
        single_run_output("start", &state)
    }

    /// `status` (spec §4.4): query the same manager's real runs. An exact
    /// `runId` match wins over a `displayName` match; without a selector the
    /// tracker's bounded recent-run list is returned. Read-only, but still
    /// behind the shared `external_mutation` approval membrane.
    async fn status(
        &self,
        manager: &Arc<WorkflowManager>,
        input: &WorkflowToolInput,
    ) -> Result<ToolOutput, ToolError> {
        let runs = manager.list();
        match input.run.as_deref() {
            Some(selector) => {
                let state = runs
                    .iter()
                    .find(|run| run.run_id == selector)
                    .or_else(|| runs.iter().find(|run| run.display_name == selector))
                    .ok_or_else(run_not_found)?;
                single_run_output("status", state)
            }
            None => {
                let truncated_entries = runs.len() > MAX_LIST_ENTRIES;
                let entries = runs.iter().take(MAX_LIST_ENTRIES).map(run_value).collect();
                bounded_output("status", "runs", truncated_entries, entries)
            }
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowToolInput {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    agent_budget: Option<u64>,
}

fn parse_input(arguments: Value) -> Result<WorkflowToolInput, ToolError> {
    serde_json::from_value(arguments).map_err(|error| invalid_arguments(&error.to_string()))
}

/// Defense-in-depth re-check of the §4.1 v1.2 `oneOf` conditions.
fn validate_input(input: &WorkflowToolInput) -> Result<(), ToolError> {
    validate_selector("name", input.name.as_deref())?;
    validate_selector("run", input.run.as_deref())?;
    if let Some(revision) = input.revision.as_deref()
        && (revision.len() != 64
            || !revision
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
    {
        return Err(invalid_arguments(
            "revision must be 64 lowercase hex characters",
        ));
    }
    match input.action.as_str() {
        "list" => {
            if input.name.is_some()
                || input.run.is_some()
                || input.revision.is_some()
                || input.args.is_some()
                || input.agent_budget.is_some()
            {
                return Err(invalid_arguments(
                    "list accepts no other fields besides action",
                ));
            }
        }
        "start" => {
            if input.run.is_some() {
                return Err(invalid_arguments("start does not accept run"));
            }
            if input.name.as_deref().is_none() {
                return Err(invalid_arguments("start requires name"));
            }
            if input.revision.is_none() {
                return Err(invalid_arguments("start requires revision"));
            }
            if input.agent_budget.is_none() {
                return Err(invalid_arguments("start requires agentBudget"));
            }
            validate_args(input.args.as_ref())?;
        }
        "status" => {
            if input.name.is_some()
                || input.revision.is_some()
                || input.args.is_some()
                || input.agent_budget.is_some()
            {
                return Err(invalid_arguments("status accepts only action and run"));
            }
        }
        other => {
            return Err(invalid_arguments(&format!(
                "action must be list, start, or status (got {other:?})"
            )));
        }
    }
    Ok(())
}

fn validate_args(args: Option<&Value>) -> Result<(), ToolError> {
    let Some(args) = args else {
        return Ok(());
    };
    if !args.is_object() {
        return Err(invalid_arguments("args must be a JSON object"));
    }
    let serialized =
        serde_json::to_vec(args).map_err(|error| invalid_arguments(&error.to_string()))?;
    if serialized.len() > MAX_ARGS_BYTES {
        return Err(invalid_arguments(&format!(
            "args must serialize to at most {MAX_ARGS_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_selector(field: &str, value: Option<&str>) -> Result<(), ToolError> {
    if let Some(value) = value
        && (value.is_empty() || value.len() > MAX_NAME_BYTES)
    {
        return Err(invalid_arguments(&format!(
            "{field} must contain 1..={MAX_NAME_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

/// Effective agent budget for a `start`: the explicit value is mandatory and
/// must sit inside the engine range (`workflow.invalid_arguments` otherwise),
/// so the pre-policy fingerprint binds the final effective budget (spec
/// §4.3 v1.2 step 4).
fn effective_budget(
    resolved: &ResolvedWorkflow,
    agent_budget: Option<u64>,
) -> Result<u64, WorkflowError> {
    match agent_budget {
        Some(raw) => Ok(u64::from(clamp_agent_budget(Some(raw))?)),
        None => Ok(u64::from(resolved.agent_budget)),
    }
}

/// Human-facing approval summary; args are JSON-encoded and truncated, never
/// interpolated into shell or paths (spec §7.1).
fn start_summary(resolved: &ResolvedWorkflow, budget: u64, args: Option<&Value>) -> String {
    let args_text = args.map(Value::to_string).unwrap_or_else(|| "{}".into());
    let args_summary = if args_text.chars().count() > MAX_ARGS_SUMMARY_CHARS {
        let truncated: String = args_text.chars().take(MAX_ARGS_SUMMARY_CHARS).collect();
        format!("{truncated}…")
    } else {
        args_text
    };
    format!(
        "start workflow '{}' (source: {}, agent budget: {}) with args: {}",
        resolved.id, resolved.source, budget, args_summary
    )
}

/// Stable model-facing status normalization (spec §5); `detailStatus` stays
/// lossless and matches the manager `WorkflowRunStatus` spelling.
fn model_status(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Active => "active",
        WorkflowRunStatus::UserPaused
        | WorkflowRunStatus::BackOffPaused
        | WorkflowRunStatus::NoProgressPaused
        | WorkflowRunStatus::InfraPaused
        | WorkflowRunStatus::Blocked
        | WorkflowRunStatus::BudgetLimited => "paused",
        WorkflowRunStatus::Complete => "completed",
        WorkflowRunStatus::Interrupted
        | WorkflowRunStatus::Failed
        | WorkflowRunStatus::Cancelled => "interrupted",
    }
}

fn detail_status(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Active => "active",
        WorkflowRunStatus::UserPaused => "user_paused",
        WorkflowRunStatus::BackOffPaused => "back_off_paused",
        WorkflowRunStatus::NoProgressPaused => "no_progress_paused",
        WorkflowRunStatus::InfraPaused => "infra_paused",
        WorkflowRunStatus::Blocked => "blocked",
        WorkflowRunStatus::BudgetLimited => "budget_limited",
        WorkflowRunStatus::Interrupted => "interrupted",
        WorkflowRunStatus::Complete => "complete",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
    }
}

/// Bounded, privacy-safe run projection (spec §4.3/§7.2): no script bodies,
/// journal contents, or absolute paths.
fn run_value(state: &WorkflowRunState) -> Value {
    json!({
        "runId": state.run_id,
        "displayName": state.display_name,
        "status": model_status(state.status),
        "detailStatus": detail_status(state.status),
        "phase": state.current_phase,
        "agentBudget": state.agent_budget,
        "agentsUsed": state.agents_used,
        "pauseMessage": state.pause_message,
        "elapsedMsFloor": state.elapsed_ms_floor,
    })
}

/// Single-run output with the same 64 KiB ceiling as list outputs; a snapshot
/// that cannot fit is a stable `workflow.output_too_large` error.
fn single_run_output(action: &str, state: &WorkflowRunState) -> Result<ToolOutput, ToolError> {
    let value = json!({"action": action, "run": run_value(state)});
    let size = serde_json::to_vec(&value)
        .map_err(|error| output_error(&error.to_string()))?
        .len();
    if size > MAX_OUTPUT_BYTES {
        return Err(output_error(
            "the workflow run snapshot exceeds the 64 KiB output limit",
        ));
    }
    Ok(ToolOutput {
        content: value.to_string(),
        metadata: json!({"workflowTool": action}),
        truncated: false,
        artifact_path: None,
    })
}

/// Entry-bounded output: when the payload exceeds the limit, drop trailing
/// entries and flag truncation (spec §7.3). A single entry that alone exceeds
/// the limit is a stable `workflow.output_too_large` error.
fn bounded_output(
    action: &str,
    entries_key: &str,
    already_truncated: bool,
    mut entries: Vec<Value>,
) -> Result<ToolOutput, ToolError> {
    let mut truncated = already_truncated;
    loop {
        let value = json!({
            "action": action,
            entries_key: entries,
            "truncated": truncated,
        });
        let size = serde_json::to_vec(&value)
            .map_err(|error| output_error(&error.to_string()))?
            .len();
        if size <= MAX_OUTPUT_BYTES {
            return Ok(ToolOutput {
                content: value.to_string(),
                metadata: json!({"workflowTool": action}),
                truncated,
                artifact_path: None,
            });
        }
        if entries.is_empty() {
            return Err(output_error(
                "a single workflow entry exceeds the 64 KiB output limit",
            ));
        }
        entries.pop();
        truncated = true;
    }
}

fn unavailable() -> ToolError {
    ToolError::new(
        "workflow.unavailable",
        "workflow runs are not available in this session",
        Retryability::Never,
    )
}

fn invalid_arguments(message: &str) -> ToolError {
    ToolError::new("workflow.invalid_arguments", message, Retryability::Never)
}

fn output_error(message: &str) -> ToolError {
    ToolError::new("workflow.output_too_large", message, Retryability::Never)
}

fn catalog_changed() -> ToolError {
    ToolError::new(
        "workflow.catalog_changed",
        "the workflow content changed since it was listed; re-list and retry",
        Retryability::AfterBackoff,
    )
}

fn run_not_found() -> ToolError {
    ToolError::new(
        "workflow.run_not_found",
        "no workflow run matched the requested id or display name",
        Retryability::Never,
    )
}

fn cancelled() -> ToolError {
    ToolError::new(
        "tool.cancelled",
        "tool call was cancelled",
        Retryability::Never,
    )
}

fn resolve_error(error: &WorkflowError) -> ToolError {
    match error {
        WorkflowError::NotFound(_) => {
            ToolError::new("workflow.not_found", error.to_string(), Retryability::Never)
        }
        WorkflowError::InvalidConfiguration(message) if message == "workflow.duplicate_name" => {
            ToolError::new(
                "workflow.duplicate_name",
                error.to_string(),
                Retryability::Never,
            )
        }
        _ => ToolError::new(
            "workflow.unavailable",
            error.to_string(),
            Retryability::Never,
        ),
    }
}

fn launch_error(error: LaunchError) -> ToolError {
    match &error {
        LaunchError::TooManyActiveRuns => ToolError::new(
            "workflow.too_many_active_runs",
            error.to_string(),
            Retryability::AfterBackoff,
        ),
        LaunchError::Persist(_) => ToolError::new(
            "workflow.persistence_failed",
            error.to_string(),
            Retryability::AfterBackoff,
        ),
        LaunchError::Resolve(workflow_error) => resolve_error(workflow_error),
        _ => ToolError::new(
            "workflow.unavailable",
            error.to_string(),
            Retryability::Never,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{SessionId, ToolCallId, TurnId};
    use lato_extensions::{
        DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot, discover_plugins,
    };
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

    fn tool() -> WorkflowTool {
        let cwd = std::env::temp_dir();
        WorkflowTool::new(SessionWorkflowHandle::new(
            cwd.clone(),
            cwd.join("lato-home"),
            SessionTrust::for_headless_prompt(&cwd),
        ))
    }

    fn user_script(name: &str, description: &str) -> String {
        format!(
            "let meta = #{{\n    name: \"{name}\",\n    description: \"{description}\",\n}};\ncomplete(\"ok\");\n"
        )
    }

    struct HangingStream;

    #[async_trait::async_trait]
    impl lato_ai::ModelStream for HangingStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            _tx: tokio::sync::mpsc::Sender<lato_ai::StreamPiece>,
        ) -> Result<(), lato_core::ModelError> {
            std::future::pending::<()>().await;
            unreachable!();
        }
    }

    struct ListFixture {
        _temp: tempfile::TempDir,
        cwd: PathBuf,
        home: PathBuf,
        plugins: Vec<PathBuf>,
    }

    impl ListFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let cwd = temp.path().join("workspace");
            let home = temp.path().join("home");
            std::fs::create_dir_all(home.join("workflows")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            Self {
                _temp: temp,
                cwd,
                home,
                plugins: Vec::new(),
            }
        }

        fn write_rhai(dir: &std::path::Path, name: &str, body: &str) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{name}.rhai")), body).unwrap();
        }

        fn write_user(&self, name: &str) {
            Self::write_rhai(
                &self.home.join("workflows"),
                name,
                &user_script(name, "User workflow"),
            );
        }

        fn rewrite_user(&self, name: &str, body: &str) {
            Self::write_rhai(&self.home.join("workflows"), name, body);
        }

        fn write_project(&self, name: &str) {
            Self::write_rhai(
                &self.cwd.join(".lato/workflows"),
                name,
                &user_script(name, "Project workflow"),
            );
        }

        fn cli_plugin(&mut self, name: &str, plugin_json: &str) {
            let root = self.home.join("plugins").join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("plugin.json"), plugin_json).unwrap();
            self.plugins.push(root);
        }

        fn snapshot(&self, project_trusted: bool) -> Arc<PluginSnapshot> {
            build_snapshot(
                1,
                discover_plugins(&DiscoveryConfig {
                    cwd: self.cwd.clone(),
                    lato_home: self.home.clone(),
                    cli_plugin_dirs: self.plugins.clone(),
                    project_trusted,
                }),
                &PluginConfig::default(),
            )
            .unwrap()
        }

        /// Tool plus its manager, mirroring the host wiring
        /// (attach_workflow_manager → install → stage plugins).
        fn tool_with_manager(
            &self,
            project_trusted: bool,
            stream: Arc<dyn lato_ai::ModelStream>,
        ) -> (WorkflowTool, Arc<WorkflowManager>) {
            let trust = SessionTrust::for_interactive(&self.cwd, project_trusted);
            let handle =
                SessionWorkflowHandle::new(self.cwd.clone(), self.home.clone(), trust.clone());
            let manager = Arc::new(WorkflowManager::new(
                "s-tool-test",
                self.cwd.clone(),
                trust,
                Arc::new(lato_workspace::FileLocks::new()),
                stream,
                None,
                None,
            ));
            manager.set_snapshot(self.snapshot(project_trusted));
            handle.install(manager.clone());
            (WorkflowTool::new(handle), manager)
        }

        fn tool_with_stream(
            &self,
            project_trusted: bool,
            stream: Arc<dyn lato_ai::ModelStream>,
        ) -> WorkflowTool {
            self.tool_with_manager(project_trusted, stream).0
        }

        fn tool(&self, project_trusted: bool) -> WorkflowTool {
            self.tool_with_stream(project_trusted, crate::default_fake_stream())
        }
    }

    fn output_json(output: &ToolOutput) -> Value {
        serde_json::from_str(&output.content).unwrap()
    }

    /// List and pull one workflow's (id, revision) pair.
    async fn listed_revision(tool: &WorkflowTool, id: &str) -> (String, String) {
        let output = tool
            .invoke(context(), json!({"action":"list"}))
            .await
            .unwrap();
        let listed = output_json(&output);
        let entry = listed["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("{id} not listed"))
            .clone();
        (
            entry["id"].as_str().unwrap().to_owned(),
            entry["revision"].as_str().unwrap().to_owned(),
        )
    }

    #[test]
    fn descriptor_matches_spec_section_four() {
        let descriptor = tool().descriptor();
        assert_eq!(descriptor.name.as_str(), "builtin:workflow");
        assert_eq!(descriptor.name.local_name(), WORKFLOW_TOOL_WIRE_NAME);
        assert_eq!(descriptor.version, Version::new(1, 2, 0));
        assert_eq!(descriptor.capabilities, vec![ToolCapability::TaskControl]);
        assert_eq!(descriptor.side_effect, SideEffect::ExternalMutation);
        assert_eq!(descriptor.concurrency, ToolConcurrency::Serial);
        assert_eq!(descriptor.idempotency, ToolIdempotency::NonIdempotent);
        assert_eq!(descriptor.cancellation, ToolCancellation::Cooperative);
        assert_eq!(descriptor.timeout_ms, 20_000);
        assert_eq!(descriptor.max_output_bytes, 64 * 1024);
        assert_eq!(descriptor.input_schema, workflow_tool_definition());
        descriptor.validate().expect("descriptor is valid");
    }

    #[test]
    fn schema_oneof_rejects_wrong_combinations_pre_policy() {
        // Mirror of the wire schema's oneOf: wrong combinations must fail
        // validation before any approval is requested.
        let schema = workflow_tool_definition();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let rejects = [
            json!({"action":"list","name":"x"}),
            json!({"action":"list","run":"x"}),
            json!({"action":"list","revision":"a"}),
            json!({"action":"start"}),
            json!({"action":"start","name":"w"}),
            json!({"action":"start","name":"w","revision":"a"}),
            json!({"action":"start","name":"w","agentBudget":8}),
            json!({"action":"start","name":"w","revision":"a","agentBudget":8,"run":"wf_1"}),
            json!({"action":"status","name":"w"}),
            json!({"action":"status","revision":"a"}),
            json!({"action":"status","agentBudget":4}),
            json!({"action":"restart"}),
            json!({"action":"start","name":"","revision":"a","agentBudget":8}),
            json!({"action":"start","name":"w","revision":"a","agentBudget":0}),
            json!({}),
        ];
        for arguments in rejects {
            assert!(
                !validator.is_valid(&arguments),
                "schema must reject {arguments}"
            );
        }
        let accepts = [
            json!({"action":"list"}),
            json!({"action":"start","name":"review-changes","revision":"0".repeat(64),"agentBudget":32}),
            json!({"action":"start","name":"review-changes","revision":"0".repeat(64),"agentBudget":32,"args":{"base":"main"}}),
            json!({"action":"status"}),
            json!({"action":"status","run":"review-changes-2"}),
        ];
        for arguments in accepts {
            assert!(
                validator.is_valid(&arguments),
                "schema must accept {arguments}"
            );
        }
    }

    #[test]
    fn revision_is_content_identity_and_stable() {
        let resolved = ResolvedWorkflow {
            id: "demo/review".into(),
            display_name: "review".into(),
            description: "d".into(),
            script: "complete(1)".into(),
            agent_budget: 32,
            source: "plugin",
            compiled: true,
        };
        let first = workflow_revision(&resolved);
        assert_eq!(first.len(), 64);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            workflow_revision(&resolved),
            first,
            "stable for identical content"
        );
        let mut tampered = resolved.clone();
        tampered.script = "complete(2)".into();
        assert_ne!(
            workflow_revision(&tampered),
            first,
            "script change ⇒ new revision"
        );
        let mut budgeted = resolved;
        budgeted.agent_budget = 33;
        assert_ne!(
            workflow_revision(&budgeted),
            first,
            "declared budget change ⇒ new revision"
        );
    }

    #[tokio::test]
    async fn actions_fail_closed_without_manager() {
        let tool = tool();
        for arguments in [
            json!({"action":"list"}),
            json!({"action":"start","name":"w","revision":"0".repeat(64),"agentBudget":8}),
            json!({"action":"status"}),
        ] {
            let error = tool
                .invoke(context(), arguments)
                .await
                .expect_err("no manager installed");
            assert_eq!(error.code, "workflow.unavailable");
        }
    }

    #[tokio::test]
    async fn cancelled_call_never_reaches_the_manager() {
        let tool = tool();
        let context = context();
        context.cancellation.cancel();
        let error = tool
            .invoke(context, json!({"action":"list"}))
            .await
            .expect_err("cancelled before dispatch");
        assert_eq!(error.code, "tool.cancelled");
    }

    #[test]
    fn section_five_status_normalization_is_exhaustive_and_lossless() {
        let cases = [
            (WorkflowRunStatus::Active, "active", "active"),
            (WorkflowRunStatus::UserPaused, "paused", "user_paused"),
            (
                WorkflowRunStatus::BackOffPaused,
                "paused",
                "back_off_paused",
            ),
            (
                WorkflowRunStatus::NoProgressPaused,
                "paused",
                "no_progress_paused",
            ),
            (WorkflowRunStatus::InfraPaused, "paused", "infra_paused"),
            (WorkflowRunStatus::Blocked, "paused", "blocked"),
            (WorkflowRunStatus::BudgetLimited, "paused", "budget_limited"),
            (WorkflowRunStatus::Complete, "completed", "complete"),
            (WorkflowRunStatus::Interrupted, "interrupted", "interrupted"),
            (WorkflowRunStatus::Failed, "interrupted", "failed"),
            (WorkflowRunStatus::Cancelled, "interrupted", "cancelled"),
        ];
        for (status, normalized, detail) in cases {
            assert_eq!(model_status(status), normalized, "{detail}");
            assert_eq!(detail_status(status), detail);
        }
    }

    #[tokio::test]
    async fn list_shows_user_and_hides_untrusted_project_workflows() {
        let fixture = ListFixture::new();
        fixture.write_user("user-greet");
        fixture.write_project("proj-secret");

        let untrusted = fixture.tool(false);
        let listed = output_json(
            &untrusted
                .invoke(context(), json!({"action":"list"}))
                .await
                .unwrap(),
        );
        assert_eq!(listed["truncated"], false);
        let ids: Vec<&str> = listed["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["user-greet"]);

        let trusted = fixture.tool(true);
        let listed = output_json(
            &trusted
                .invoke(context(), json!({"action":"list"}))
                .await
                .unwrap(),
        );
        let ids: Vec<&str> = listed["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"proj-secret"));
        assert!(ids.contains(&"user-greet"));
    }

    #[tokio::test]
    async fn list_entries_are_bounded_and_private() {
        let fixture = ListFixture::new();
        for seq in 0..70 {
            fixture.write_user(&format!("wf-{seq:03}"));
        }
        let tool = fixture.tool(false);
        let output = tool
            .invoke(context(), json!({"action":"list"}))
            .await
            .unwrap();
        assert!(output.truncated);
        let listed = output_json(&output);
        assert_eq!(listed["workflows"].as_array().unwrap().len(), 64);
        assert_eq!(listed["truncated"], true);
        let serialized = output.content;
        assert!(!serialized.contains("complete("));
        assert!(!serialized.contains(fixture.home.to_str().unwrap()));
        // Stable registry order: sorted file names, keep-first.
        assert_eq!(listed["workflows"][0]["id"], "wf-000");
        assert_eq!(listed["workflows"][0]["source"], "user");
        assert!(listed["workflows"][0]["agentBudget"].is_u64());
        assert_eq!(
            listed["workflows"][0]["revision"].as_str().unwrap().len(),
            64
        );
    }

    #[tokio::test]
    async fn start_with_listed_revision_launches_through_the_same_manager() {
        let fixture = ListFixture::new();
        fixture.write_user("review");
        let (tool, _manager) = fixture.tool_with_manager(false, crate::default_fake_stream());

        let (id, revision) = listed_revision(&tool, "review").await;
        let started = tokio::time::Instant::now();
        let output = tool
            .invoke(
                context(),
                json!({"action":"start","name":id,"revision":revision,"agentBudget":32,"args":{"base":"main"}}),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "start must return the initial snapshot quickly"
        );
        let value = output_json(&output);
        assert_eq!(value["action"], "start");
        assert_eq!(value["run"]["displayName"], "review");
        assert_eq!(value["run"]["status"], "active");
        assert_eq!(value["run"]["agentBudget"], 32);
        let run_id = value["run"]["runId"].as_str().unwrap().to_owned();
        assert!(run_id.starts_with("wf_"));
    }

    #[tokio::test]
    async fn start_maps_stable_error_codes() {
        let mut fixture = ListFixture::new();
        fixture.write_user("greet");
        fixture.cli_plugin(
            "alpha",
            r#"{"name":"alpha","workflows":{"shared":{"description":"Alpha"}}}"#,
        );
        fixture.cli_plugin(
            "beta",
            r#"{"name":"beta","workflows":{"shared":{"description":"Beta"}}}"#,
        );
        let tool = fixture.tool(true);
        let revision = "0".repeat(64);

        let missing = tool
            .invoke(
                context(),
                json!({"action":"start","name":"missing/none","revision":revision,"agentBudget":8}),
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code, "workflow.not_found");

        let duplicate = tool
            .invoke(
                context(),
                json!({"action":"start","name":"shared","revision":revision,"agentBudget":8}),
            )
            .await
            .unwrap_err();
        assert_eq!(duplicate.code, "workflow.duplicate_name");

        let budget = tool
            .invoke(
                context(),
                json!({"action":"start","name":"greet","revision":revision,"agentBudget":999_999_999}),
            )
            .await
            .unwrap_err();
        assert_eq!(budget.code, "workflow.invalid_arguments");
    }

    #[tokio::test]
    async fn stale_revision_fails_closed_with_zero_side_effects() {
        let fixture = ListFixture::new();
        fixture.write_user("review");
        let (tool, manager) = fixture.tool_with_manager(false, crate::default_fake_stream());

        // The model lists, then the script content changes under the same name
        // before `start` executes.
        let (id, stale_revision) = listed_revision(&tool, "review").await;
        fixture.rewrite_user(
            "review",
            &format!("{}\n// tampered", user_script("review", "User workflow")),
        );
        let error = tool
            .invoke(
                context(),
                json!({"action":"start","name":id,"revision":stale_revision,"agentBudget":32}),
            )
            .await
            .expect_err("stale revision must not launch");
        assert_eq!(error.code, "workflow.catalog_changed");
        assert!(
            manager.list().is_empty(),
            "catalog_changed must not launch a run"
        );

        // Re-listing yields the fresh revision, which launches cleanly.
        let (_, fresh_revision) = listed_revision(&tool, "review").await;
        let ok = tool
            .invoke(
                context(),
                json!({"action":"start","name":"review","revision":fresh_revision,"agentBudget":32}),
            )
            .await
            .unwrap();
        assert_eq!(output_json(&ok)["run"]["status"], "active");
    }

    #[tokio::test]
    async fn approval_summary_shows_resolution() {
        let fixture = ListFixture::new();
        fixture.write_user("review");
        let tool = fixture.tool(false);

        let arguments =
            json!({"action":"start","name":"review","revision":"0".repeat(64),"agentBudget":32});
        let detail = tool
            .approval_detail(&arguments)
            .expect("start produces an approval detail");
        assert!(detail.contains("'review'"), "detail: {detail}");
        assert!(detail.contains("source: user"), "detail: {detail}");
        assert!(detail.contains("agent budget: 32"), "detail: {detail}");
        assert!(detail.contains("args: {}"), "detail: {detail}");
    }

    #[tokio::test]
    async fn fifth_concurrent_start_fails_with_stable_code() {
        let fixture = ListFixture::new();
        fixture.write_user("hang");
        fixture.rewrite_user(
            "hang",
            "let meta = #{ name: \"hang\", description: \"d\" };\nlet r = agent(\"work\");\n",
        );
        let (tool, _manager) = fixture.tool_with_manager(false, Arc::new(HangingStream));
        let (_, revision) = listed_revision(&tool, "hang").await;

        for _ in 0..4 {
            let output = tool
                .invoke(
                    context(),
                    json!({"action":"start","name":"hang","revision":revision,"agentBudget":8}),
                )
                .await
                .unwrap();
            assert_eq!(output_json(&output)["run"]["status"], "active");
        }
        let fifth = tool
            .invoke(
                context(),
                json!({"action":"start","name":"hang","revision":revision,"agentBudget":8}),
            )
            .await
            .unwrap_err();
        assert_eq!(fifth.code, "workflow.too_many_active_runs");
        assert_eq!(fifth.retryability, Retryability::AfterBackoff);
    }

    #[tokio::test]
    async fn status_reports_real_manager_runs_and_misses_stably() {
        let fixture = ListFixture::new();
        fixture.write_user("hold");
        fixture.rewrite_user(
            "hold",
            "let meta = #{ name: \"hold\", description: \"d\" };\nlet r = agent(\"work\");\n",
        );
        let (tool, manager) = fixture.tool_with_manager(false, Arc::new(HangingStream));

        // Launch through the MANAGER (as TUI/ACP do), query through the tool.
        let snapshot = manager.snapshot().unwrap();
        let resolved =
            super::super::resolve_workflow(&fixture.cwd, &fixture.home, &snapshot, false, "hold")
                .unwrap();
        let state = manager
            .launch(
                resolved,
                super::super::LaunchSpec {
                    args: json!({}),
                    agent_budget: Some(8),
                    resume_display_name: None,
                },
            )
            .unwrap();
        let run_id = state.run_id;

        // By runId.
        let by_id = tool
            .invoke(context(), json!({"action":"status","run":run_id}))
            .await
            .unwrap();
        let run = output_json(&by_id)["run"].clone();
        assert_eq!(run["runId"], run_id.as_str());
        assert_eq!(run["status"], "active");
        assert_eq!(run["detailStatus"], "active");

        // Pause through the manager (user action); the run task observes the
        // pause intent asynchronously, so wait for the tracker to settle.
        manager.pause("hold").unwrap();
        for _ in 0..400 {
            if manager.list().iter().any(|run| {
                run.display_name == "hold" && run.status == WorkflowRunStatus::UserPaused
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let paused = tool
            .invoke(context(), json!({"action":"status","run":"hold"}))
            .await
            .unwrap();
        let run = output_json(&paused)["run"].clone();
        assert_eq!(run["status"], "paused");
        assert_eq!(run["detailStatus"], "user_paused");
        assert!(run["phase"].is_null());
        assert!(run["elapsedMsFloor"].is_u64());

        // Default listing includes the run; a miss is a stable error.
        let listed = tool
            .invoke(context(), json!({"action":"status"}))
            .await
            .unwrap();
        assert_eq!(output_json(&listed)["runs"].as_array().unwrap().len(), 1);
        let missing = tool
            .invoke(context(), json!({"action":"status","run":"nope"}))
            .await
            .unwrap_err();
        assert_eq!(missing.code, "workflow.run_not_found");
    }

    #[tokio::test]
    async fn completed_runs_normalize_to_completed() {
        let fixture = ListFixture::new();
        fixture.write_user("quick");
        let tool = fixture.tool(false);

        let (_, revision) = listed_revision(&tool, "quick").await;
        tool.invoke(
            context(),
            json!({"action":"start","name":"quick","revision":revision,"agentBudget":8}),
        )
        .await
        .unwrap();
        let mut normalized = None;
        for _ in 0..200 {
            let listed = tool
                .invoke(context(), json!({"action":"status","run":"quick"}))
                .await
                .unwrap();
            let run = output_json(&listed)["run"].clone();
            if run["status"] == "completed" {
                normalized = Some(run);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let run = normalized.expect("run never completed");
        assert_eq!(run["detailStatus"], "complete");
    }
}
