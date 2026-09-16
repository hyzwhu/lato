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

use super::{WorkflowManager, list_workflows};
use lato_workspace::SessionTrust;

/// Wire name visible to the model (`builtin:workflow` canonical).
pub const WORKFLOW_TOOL_WIRE_NAME: &str = "workflow";

const MAX_NAME_BYTES: usize = 256;
const MAX_LIST_ENTRIES: usize = 64;
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
    /// `list` (spec §4.2): the current trust/plugin snapshot's named scripts
    /// in registry keep-first order, bounded to 64 entries and 64 KiB, with
    /// no script bodies or disk paths.
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
                })
            })
            .collect();
        bounded_output("list", "workflows", truncated_entries, entries)
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

fn output_error(message: &str) -> ToolError {
    ToolError::new("workflow.output_too_large", message, Retryability::Never)
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
    use lato_extensions::PluginSnapshot;
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
        let context = context();
        context.cancellation.cancel();
        let error = tool
            .invoke(context, json!({"action":"list"}))
            .await
            .expect_err("cancelled before dispatch");
        assert_eq!(error.code, "tool.cancelled");
    }

    struct ListFixture {
        _temp: tempfile::TempDir,
        cwd: PathBuf,
        home: PathBuf,
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
            }
        }

        fn write_rhai(dir: &std::path::Path, name: &str, description: &str) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join(format!("{name}.rhai")),
                format!(
                    "let meta = #{{\n    name: \"{name}\",\n    description: \"{description}\",\n}};\ncomplete(\"ok\");\n"
                ),
            )
            .unwrap();
        }

        fn write_user(&self, name: &str) {
            Self::write_rhai(&self.home.join("workflows"), name, "User workflow");
        }

        fn write_project(&self, name: &str) {
            Self::write_rhai(&self.cwd.join(".lato/workflows"), name, "Project workflow");
        }

        fn snapshot(&self, project_trusted: bool) -> Arc<PluginSnapshot> {
            lato_extensions::build_snapshot(
                1,
                lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
                    cwd: self.cwd.clone(),
                    lato_home: self.home.clone(),
                    cli_plugin_dirs: Vec::new(),
                    project_trusted,
                }),
                &lato_extensions::PluginConfig::default(),
            )
            .unwrap()
        }

        /// Tool with the manager installed and the snapshot mounted, mirroring
        /// the host wiring (attach_workflow_manager → install → stage plugins).
        fn tool(&self, project_trusted: bool) -> WorkflowTool {
            let trust = SessionTrust::for_interactive(&self.cwd, project_trusted);
            let handle =
                SessionWorkflowHandle::new(self.cwd.clone(), self.home.clone(), trust.clone());
            let manager = Arc::new(WorkflowManager::new(
                "s-tool-test",
                self.cwd.clone(),
                trust,
                Arc::new(lato_workspace::FileLocks::new()),
                crate::default_fake_stream(),
                None,
                None,
            ));
            manager.set_snapshot(self.snapshot(project_trusted));
            handle.install(manager);
            WorkflowTool::new(handle)
        }
    }

    fn output_json(output: &ToolOutput) -> Value {
        serde_json::from_str(&output.content).unwrap()
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
        assert!(listed["workflows"][0]["description"].is_string());
    }
}
