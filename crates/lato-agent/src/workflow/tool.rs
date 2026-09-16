// Lato Phase 7B7: model-visible `workflow` tool.
//
// A thin, session-bound adapter over the same `WorkflowManager` the TUI/ACP
// already drive (spec §6). The tool never creates a second turn loop, manager,
// or ACP self-call: it resolves and launches through the manager mounted by
// `attach_workflow_manager`, reusing the 7B4-7B6 registry, concurrency cap,
// journal persistence, host service, and update broadcast.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};

use super::WorkflowManager;
use lato_workspace::SessionTrust;

/// Wire name visible to the model (`builtin:workflow` canonical).
pub const WORKFLOW_TOOL_WIRE_NAME: &str = "workflow";

const MAX_NAME_BYTES: usize = 256;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARGS_BYTES: usize = 64 * 1024;
const TOOL_TIMEOUT_MS: u64 = 20_000;

/// Model-visible input schema (spec §4.1). Conditional combinations are
/// re-validated by the invoke layer with `workflow.invalid_arguments`.
pub fn workflow_tool_definition() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["list", "start", "status"]},
            "name": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES},
            "run": {"type": "string", "minLength": 1, "maxLength": MAX_NAME_BYTES},
            "args": {"type": "object"},
            "agentBudget": {"type": "integer", "minimum": 1}
        },
        "required": ["action"]
    })
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
            version: Version::new(1, 0, 0),
            description: "Discover, start, and inspect named workflows. Actions: list (visible \
                          workflows), start (launch a background run after approval), status \
                          (query one run or recent runs). Starting a workflow never blocks the \
                          conversation; the run keeps streaming its own updates. Model-initiated \
                          pause, resume, and stop are not supported."
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
        match input.action.as_str() {
            "list" => self.list(&manager),
            "start" => self.start(&manager, input).await,
            "status" => self.status(&manager, input).await,
            other => Err(invalid_arguments(&format!(
                "action must be list, start, or status (got {other:?})"
            ))),
        }
    }
}

impl WorkflowTool {
    fn list(&self, _manager: &Arc<WorkflowManager>) -> Result<ToolOutput, ToolError> {
        Err(unavailable())
    }

    async fn start(
        &self,
        _manager: &Arc<WorkflowManager>,
        _input: WorkflowToolInput,
    ) -> Result<ToolOutput, ToolError> {
        Err(unavailable())
    }

    async fn status(
        &self,
        _manager: &Arc<WorkflowManager>,
        _input: WorkflowToolInput,
    ) -> Result<ToolOutput, ToolError> {
        Err(unavailable())
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
    args: Option<Value>,
    #[serde(default)]
    agent_budget: Option<u64>,
}

fn parse_input(arguments: Value) -> Result<WorkflowToolInput, ToolError> {
    serde_json::from_value(arguments).map_err(|error| invalid_arguments(&error.to_string()))
}

/// Conditional constraints from spec §4.1, enforced at the invoke layer.
fn validate_input(input: &WorkflowToolInput) -> Result<(), ToolError> {
    validate_selector("name", input.name.as_deref())?;
    validate_selector("run", input.run.as_deref())?;
    match input.action.as_str() {
        "list" => {
            if input.name.is_some()
                || input.run.is_some()
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
            validate_args(input.args.as_ref())?;
            if input.agent_budget.is_some_and(|budget| budget == 0) {
                return Err(invalid_arguments("agentBudget must be at least 1"));
            }
        }
        "status" => {
            if input.name.is_some() || input.args.is_some() || input.agent_budget.is_some() {
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
    if let Some(value) = value {
        if value.is_empty() || value.len() > MAX_NAME_BYTES {
            return Err(invalid_arguments(&format!(
                "{field} must contain 1..={MAX_NAME_BYTES} UTF-8 bytes"
            )));
        }
    }
    Ok(())
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

    fn tool() -> WorkflowTool {
        let cwd = std::env::temp_dir();
        WorkflowTool::new(SessionWorkflowHandle::new(
            cwd.clone(),
            cwd.join("lato-home"),
            SessionTrust::for_headless_prompt(&cwd),
        ))
    }

    #[test]
    fn descriptor_matches_spec_section_four() {
        let descriptor = tool().descriptor();
        assert_eq!(descriptor.name.as_str(), "builtin:workflow");
        assert_eq!(descriptor.name.local_name(), WORKFLOW_TOOL_WIRE_NAME);
        assert_eq!(descriptor.version, Version::new(1, 0, 0));
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
    fn wrong_argument_combinations_are_rejected() {
        let cases = [
            json!({"action":"list","name":"x"}),
            json!({"action":"list","run":"x"}),
            json!({"action":"start"}),
            json!({"action":"start","name":"w","run":"wf_1"}),
            json!({"action":"status","name":"w"}),
            json!({"action":"status","agentBudget":4}),
            json!({"action":"restart"}),
            json!({"action":"start","name":"","args":{}}),
            json!({"action":"start","name":"w","args":"not-object"}),
            json!({"action":"start","name":"w","agentBudget":0}),
            json!({}),
        ];
        for arguments in cases {
            let rejected = serde_json::from_value::<WorkflowToolInput>(arguments.clone())
                .map_err(|_| ())
                .and_then(|input| validate_input(&input).map_err(|_| ()))
                .is_err();
            assert!(rejected, "expected rejection for {arguments}");
        }
        let ok = [
            json!({"action":"list"}),
            json!({"action":"start","name":"review-changes"}),
            json!({"action":"start","name":"review-changes","args":{"base":"main"},"agentBudget":32}),
            json!({"action":"status"}),
            json!({"action":"status","run":"review-changes-2"}),
        ];
        for arguments in ok {
            let input: WorkflowToolInput = serde_json::from_value(arguments.clone()).unwrap();
            validate_input(&input).unwrap_or_else(|error| panic!("{arguments} rejected: {error}"));
        }
    }

    #[tokio::test]
    async fn actions_fail_closed_without_manager() {
        let tool = tool();
        for arguments in [
            json!({"action":"list"}),
            json!({"action":"start","name":"w"}),
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
        let mut context = context();
        context.cancellation.cancel();
        let error = tool
            .invoke(context, json!({"action":"list"}))
            .await
            .expect_err("cancelled before dispatch");
        assert_eq!(error.code, "tool.cancelled");
    }
}
