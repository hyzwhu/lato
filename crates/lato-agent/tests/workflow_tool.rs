// Phase 7B7: main-session registration of the model-visible `workflow` tool.

use std::sync::Arc;

use lato_agent::AcpHost;
use lato_ai::{ModelStream, StreamPiece};
use lato_core::{ModelError, ToolCapability};
use lato_protocol::JsonRpcReq;
use lato_workspace::{FileLocks, SessionTrust as Trust};
use tokio::sync::mpsc;

struct ScriptedStream {
    rounds: tokio::sync::Mutex<std::collections::VecDeque<Vec<StreamPiece>>>,
    contexts: tokio::sync::Mutex<Vec<serde_json::Value>>,
}

impl ScriptedStream {
    fn scripted(rounds: Vec<Vec<StreamPiece>>) -> Self {
        Self {
            rounds: tokio::sync::Mutex::new(rounds.into()),
            contexts: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ModelStream for ScriptedStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.contexts.lock().await.push(context);
        let pieces = self
            .rounds
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| vec![StreamPiece::Text("done".into())]);
        for piece in pieces {
            tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        }
        Ok(())
    }
}

fn host(
    stream: Arc<dyn ModelStream>,
) -> (
    AcpHost,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        AcpHost::new(
            cwd.clone(),
            Trust::for_headless_prompt(&cwd),
            updates_tx,
            stream,
        ),
        updates_rx,
    )
}

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

/// AC-01: the main-session model catalog carries exactly one `workflow` tool
/// with the §4 schema, and a scripted model turn can drive it end-to-end
/// through the ordinary ToolRuntime membrane.
#[tokio::test]
async fn main_session_turn_can_drive_the_workflow_tool() {
    let stream = Arc::new(ScriptedStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "call-1".into(),
            name: "workflow".into(),
            arguments: serde_json::json!({"action":"list"}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (mut acp, _updates) = host(stream.clone());
    let sid = acp
        .handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let response = acp
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "list workflows"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    // Round 2's model context must contain the tool result: a bounded,
    // structured `workflow` list payload produced by the session-bound tool.
    let contexts = stream.contexts.lock().await.clone();
    assert!(contexts.len() >= 2, "expected a second model round");
    let messages = contexts[1]["messages"].as_array().unwrap().clone();
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("workflow tool result reached the model");
    let result: serde_json::Value = serde_json::from_str(tool_message["content"].as_str().unwrap())
        .expect("tool result is structured JSON");
    assert_eq!(result["action"], "list");
    assert!(result["workflows"].is_array());
    assert_eq!(result["truncated"], false);
    // The catalog itself carries exactly one `workflow` wire name.
    let catalog = serde_json::to_string(&contexts[0]).unwrap();
    assert_eq!(catalog.matches("\"name\":\"workflow\"").count(), 1);
    assert!(!catalog.contains("script.rhai"));
}

/// AC-09: subagent/headless capability ceilings never see the `workflow` tool
/// — the extra-tool slot is only used by the main-session host wiring.
#[test]
fn capability_filtered_catalogs_exclude_the_workflow_tool() {
    let cwd = std::env::current_dir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime_for_capabilities(
        lato_tools::BuiltinToolEnvironment {
            cwd: cwd.clone(),
            locks: Arc::new(FileLocks::new()),
            trust: Trust::for_headless_prompt(&cwd),
            skill_resolver: None,
        },
        Some(&[ToolCapability::TaskControl, ToolCapability::FileRead]),
    )
    .unwrap();
    assert!(
        !runtime
            .model_definitions()
            .iter()
            .any(|definition| definition["function"]["name"] == "workflow")
    );
}

/// AC-09 (wiring guard): only the main-session host path consumes the
/// extra-tools slot; subagent and headless builders must stay plain.
#[test]
fn only_the_main_session_host_registers_the_workflow_tool() {
    let host_source = include_str!("../src/host.rs");
    assert!(host_source.contains("builtin_tool_runtime_with_subagents_and_mcp_extra"));
    assert!(host_source.contains("workflow_handle.install(manager)"));

    let runner_source = include_str!("../src/subagent/runner.rs");
    assert!(!runner_source.contains("subagents_and_mcp_extra"));
    let skills_source = include_str!("../src/skills.rs");
    assert!(!skills_source.contains("subagents_and_mcp_extra"));
}

// ---- permission / persistence / resume coverage (spec §9.1) ----

use lato_agent::workflow::{
    SessionWorkflowHandle, WorkflowManager, WorkflowRunStatus, WorkflowTool,
};
use lato_core::Tool;
use lato_core::{PolicyDecision, PolicyMode, SandboxProfile};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use std::path::PathBuf;
use std::time::Duration;

struct ToolFixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
}

impl ToolFixture {
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

    fn write_user(&self, name: &str, body: &str) {
        let dir = self.home.join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.rhai")), body).unwrap();
    }
}

fn hang_script(name: &str) -> String {
    format!("let meta = #{{ name: \"{name}\", description: \"d\" }};\nlet r = agent(\"work\");\n")
}

struct PendingStream;

#[async_trait::async_trait]
impl ModelStream for PendingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        _tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        std::future::pending::<()>().await;
        unreachable!();
    }
}

fn tool_manager(
    fixture: &ToolFixture,
    workflows_dir: Option<PathBuf>,
) -> (WorkflowTool, Arc<WorkflowManager>) {
    let trust = Trust::for_interactive(&fixture.cwd, false);
    let handle =
        SessionWorkflowHandle::new(fixture.cwd.clone(), fixture.home.clone(), trust.clone());
    let manager = Arc::new(WorkflowManager::new(
        "s-perm-test",
        fixture.cwd.clone(),
        trust,
        Arc::new(FileLocks::new()),
        Arc::new(PendingStream),
        None,
        workflows_dir,
    ));
    let snapshot = lato_extensions::build_snapshot(
        1,
        lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
            cwd: fixture.cwd.clone(),
            lato_home: fixture.home.clone(),
            cli_plugin_dirs: Vec::new(),
            project_trusted: false,
        }),
        &lato_extensions::PluginConfig::default(),
    )
    .unwrap();
    manager.set_snapshot(snapshot);
    handle.install(manager.clone());
    (WorkflowTool::new(handle), manager)
}

fn tool_context() -> lato_core::ToolContext {
    lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    }
}

/// §9.1 #5: in Ask mode the start requires approval; a denial (or a missing
/// approval) leaves zero runs, directories, or journals behind.
#[tokio::test]
async fn rejected_start_has_zero_side_effects() {
    let fixture = ToolFixture::new();
    fixture.write_user("review", &hang_script("review"));
    let (tool, manager) = tool_manager(&fixture, None);

    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: fixture.cwd.clone(),
            mode: PolicyMode::Ask,
            project_trusted: false,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder.register(Arc::new(tool)).unwrap();
    let runtime = builder.build().unwrap();

    // No grant → policy.approval_required, and no run exists afterwards.
    let error = runtime
        .invoke(
            tool_context(),
            "workflow",
            serde_json::json!({"action":"start","name":"review"}),
        )
        .await
        .expect_err("ask mode requires approval");
    assert_eq!(error.code, "policy.approval_required");
    assert!(
        manager.list().is_empty(),
        "denied start must not create runs"
    );

    // A full approval round-trip executes exactly once; replaying the spent
    // grant fails closed.
    let prepared = runtime
        .prepare(
            tool_context(),
            "workflow",
            serde_json::json!({"action":"start","name":"review"}),
        )
        .unwrap();
    let request = match runtime.decision(&prepared).clone() {
        PolicyDecision::RequireApproval(request) => request,
        other => panic!("expected RequireApproval, got {other:?}"),
    };
    assert!(request.summary.contains("'review'"));
    assert!(request.summary.contains("source: user"));
    let grant = runtime.approve(&request).unwrap();
    let output = runtime.execute(prepared, grant.clone()).await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output.content).unwrap()["run"]["status"],
        "active"
    );
    assert_eq!(manager.list().len(), 1);
    // The grant is single-use: a fresh prepare replayed with the old grant
    // must fail (consumed grant never authorizes a second launch).
    let prepared_again = runtime
        .prepare(
            tool_context(),
            "workflow",
            serde_json::json!({"action":"start","name":"review"}),
        )
        .unwrap();
    let replay = runtime.execute(prepared_again, grant).await;
    assert!(
        replay.is_err(),
        "spent grant must not authorize a second run"
    );
    assert_eq!(manager.list().len(), 1);
}

/// §9.1 #8: launch persists; after a process-style shutdown the restored run
/// is queryable through the same tool and reports `interrupted`.
#[tokio::test]
async fn status_sees_restored_runs_after_cross_process_resume() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let workflows_dir = fixture.home.join("sessions").join("s1").join("workflows");
    let (tool, manager) = tool_manager(&fixture, Some(workflows_dir.clone()));

    let output = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"start","name":"hang"}),
        )
        .await
        .unwrap();
    let run_id =
        serde_json::from_str::<serde_json::Value>(&output.content).unwrap()["run"]["runId"]
            .as_str()
            .unwrap()
            .to_owned();

    // Simulated hard process exit: the manager is dropped without a graceful
    // shutdown, so the on-disk run record still says `active`.
    drop(manager);

    // "New process": a fresh manager restores from disk; the tool reports the
    // restored truth (active-at-exit → interrupted, never active).
    let (restored_tool, _restored_manager) = tool_manager(&fixture, Some(workflows_dir));
    let status = restored_tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"status","run":run_id}),
        )
        .await
        .unwrap();
    let run = serde_json::from_str::<serde_json::Value>(&status.content).unwrap()["run"].clone();
    assert_eq!(run["runId"], run_id.as_str());
    assert_eq!(run["status"], "interrupted");
    assert_eq!(run["detailStatus"], "interrupted");
    let _ = WorkflowRunStatus::Active;
}

/// §9.1 #8: a persistence failure at launch rolls back fully and surfaces as
/// a stable `workflow.persistence_failed` error (no ghost runs).
#[tokio::test]
async fn persistence_failure_rolls_back_with_stable_code() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    // A regular file where the workflows directory should be → writes fail.
    let blocked = fixture.home.join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let (tool, manager) = tool_manager(&fixture, Some(blocked));

    let error = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"start","name":"hang"}),
        )
        .await
        .expect_err("persistence must fail");
    assert_eq!(error.code, "workflow.persistence_failed");
    assert!(
        manager.list().is_empty(),
        "failed launches must not leave ghost runs"
    );
}
