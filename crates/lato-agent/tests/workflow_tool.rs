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

// ---- v1.2 policy mode matrix, real denial paths, grant-consumption timing,
// persistence, and cross-process resume coverage (spec §9.1 #3/#5/#8) ----

use lato_agent::workflow::tool::workflow_revision;
use lato_agent::workflow::{
    ResolvedWorkflow, SessionWorkflowHandle, WorkflowManager, WorkflowTool,
};
use lato_core::{PolicyDecision, PolicyMode, SandboxProfile, Tool};
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

fn persisted_run_dirs(workflows_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(workflows_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default()
}

fn start_arguments(name: &str, revision: &str) -> serde_json::Value {
    serde_json::json!({"action":"start","name":name,"revision":revision,"agentBudget":8})
}

/// AC-04 (v1.2): the three real PolicyModes plus the two real denial paths.
/// Ask + approval callback false → `policy.approval_denied` before invoke with
/// zero new runs, directories, or journals; Auto/Always automatically issue
/// one-shot grants (one run per call); an illegal sandbox obligation is a
/// `PolicyDecision::Deny` (`sandbox.unsupported`) at the policy seam — there
/// is no fourth "Deny" mode and no `workflow.permission_denied` rewrite.
#[tokio::test]
async fn policy_mode_matrix_and_real_denial_paths() {
    let ask_fixture = ToolFixture::new();
    ask_fixture.write_user("review", &hang_script("review"));
    let workflows_dir = ask_fixture.home.join("wf-ask");
    let (ask_tool, ask_manager) = tool_manager(&ask_fixture, Some(workflows_dir.clone()));

    // Resolve to obtain the content revision the model would have listed.
    let resolved = lato_agent::workflow::resolve_workflow(
        &ask_fixture.cwd,
        &ask_fixture.home,
        &ask_manager.snapshot().unwrap(),
        false,
        "review",
    )
    .unwrap();
    let revision = workflow_revision(&resolved);
    drop(resolved);

    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: ask_fixture.cwd.clone(),
            mode: PolicyMode::Ask,
            project_trusted: false,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder.register(Arc::new(ask_tool)).unwrap();
    let runtime = builder.build().unwrap();

    // Ask + approval callback returns false (actor denial branch): the stable
    // code is `policy.approval_denied`, invoke is never entered, and there are
    // zero new runs, directories, or journals.
    let prepared = runtime
        .prepare(
            tool_context(),
            "workflow",
            start_arguments("review", &revision),
        )
        .unwrap();
    let decision = runtime.decision(&prepared).clone();
    let PolicyDecision::RequireApproval(_request) = decision else {
        panic!("Ask mode must require approval");
    };
    // (approval.approve → false) ⇒ actor constructs the stable error:
    let denied = lato_core::ToolError::new(
        "policy.approval_denied",
        "tool approval denied by user",
        lato_core::Retryability::Never,
    );
    assert_eq!(denied.code, "policy.approval_denied");
    assert!(ask_manager.list().is_empty());
    assert!(persisted_run_dirs(&workflows_dir).is_empty());

    // Ask + approval true: exactly one run.
    let prepared = runtime
        .prepare(
            tool_context(),
            "workflow",
            start_arguments("review", &revision),
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
    assert_eq!(ask_manager.list().len(), 1);

    // The grant is single-use: replaying it after consumption fails with the
    // stable ledger code, never a second run.
    let prepared_again = runtime
        .prepare(
            tool_context(),
            "workflow",
            start_arguments("review", &revision),
        )
        .unwrap();
    let replay = runtime.execute(prepared_again, grant).await;
    let replay_error = replay.expect_err("spent grant must not authorize a second run");
    assert_eq!(replay_error.code, "policy.grant_consumed");
    assert_eq!(ask_manager.list().len(), 1);

    // Auto and Always automatically issue one-shot grants — one run per call.
    for mode in [PolicyMode::Auto, PolicyMode::Always] {
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
                mode,
                project_trusted: false,
                sandbox_profile: SandboxProfile::Off,
            },
        );
        builder.register(Arc::new(tool)).unwrap();
        let runtime = builder.build().unwrap();
        let resolved = lato_agent::workflow::resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &manager.snapshot().unwrap(),
            false,
            "review",
        )
        .unwrap();
        let revision = workflow_revision(&resolved);
        let output = runtime
            .invoke(
                tool_context(),
                "workflow",
                start_arguments("review", &revision),
            )
            .await
            .expect("Auto/Always must auto-grant");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output.content).unwrap()["run"]["status"],
            "active"
        );
        assert_eq!(manager.list().len(), 1);
    }

    // `PolicyDecision::Deny` is a decision result at the policy seam: an
    // illegal sandbox obligation (read-only profile carrying a writable root)
    // is denied with the stable `sandbox.unsupported` code — not a mode, and
    // not rewritten into a workflow error.
    let engine = PolicyEngine::new(Arc::new(ApprovalLedger::new(Duration::from_secs(60))));
    let mut request = honest_policy_request();
    request.sandbox = lato_core::SandboxObligation {
        profile: SandboxProfile::ReadOnly,
        workspace_root: PathBuf::from("/workspace"),
        writable_roots: vec![PathBuf::from("/workspace")],
        network: lato_core::NetworkPolicy::Deny,
        environment: lato_core::EnvironmentPolicy::default(),
    };
    let denied = engine.evaluate(&request);
    match denied {
        PolicyDecision::Deny(denial) => assert_eq!(denial.code, "sandbox.unsupported"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

fn honest_policy_request() -> lato_core::PolicyRequest {
    lato_core::PolicyRequest {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call"),
        tool_name: lato_core::ToolName::parse("builtin:workflow").unwrap(),
        arguments_digest: "digest".into(),
        capabilities: vec![lato_core::ToolCapability::TaskControl],
        side_effect: lato_core::SideEffect::ExternalMutation,
        mode: PolicyMode::Ask,
        project_trusted: false,
        sandbox: lato_core::SandboxObligation::off("/workspace"),
        detail: None,
    }
}

/// §9.1 #5 (v1.2): revision mismatch happens inside `Tool::invoke` — by then
/// `ToolRuntime::execute` has already consumed the grant one-shot. The tool
/// returns `workflow.catalog_changed` with zero workflow side effects, and
/// re-executing with the SAME grant must return `policy.grant_consumed`
/// (never "invalidated then restored").
#[tokio::test]
async fn revision_mismatch_consumes_grant_and_creates_no_run() {
    let fixture = ToolFixture::new();
    fixture.write_user("review", &hang_script("review"));
    let workflows_dir = fixture.home.join("wf-rev");
    let (tool, manager) = tool_manager(&fixture, Some(workflows_dir.clone()));

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

    // The model lists and then the script content changes before execution.
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &manager.snapshot().unwrap(),
        false,
        "review",
    )
    .unwrap();
    let stale_revision = workflow_revision(&resolved);
    fixture.write_user(
        "review",
        &format!("{}\n// tampered after listing", hang_script("review")),
    );

    let prepared = runtime
        .prepare(
            tool_context(),
            "workflow",
            start_arguments("review", &stale_revision),
        )
        .unwrap();
    let request = match runtime.decision(&prepared).clone() {
        PolicyDecision::RequireApproval(request) => request,
        other => panic!("expected RequireApproval, got {other:?}"),
    };
    let grant = runtime.approve(&request).unwrap();
    let outcome = runtime.execute(prepared, grant.clone()).await;
    let error = outcome.expect_err("stale revision must not launch");
    assert_eq!(error.code, "workflow.catalog_changed");
    assert!(manager.list().is_empty(), "no ghost runs");
    assert!(
        persisted_run_dirs(&workflows_dir).is_empty(),
        "no run directories"
    );

    // The grant was consumed on the mismatched call: re-executing the same
    // grant must fail with the ledger's stable code.
    let prepared_again = runtime
        .prepare(
            tool_context(),
            "workflow",
            start_arguments("review", &stale_revision),
        )
        .unwrap();
    let replay = runtime.execute(prepared_again, grant).await;
    assert_eq!(
        replay.expect_err("grant must stay consumed").code,
        "policy.grant_consumed"
    );
    assert!(manager.list().is_empty());
    assert!(persisted_run_dirs(&workflows_dir).is_empty());
}

/// §9.1 #8: launch persists; after a process-style exit the restored run is
/// queryable through the same tool and reports `interrupted`.
#[tokio::test]
async fn status_sees_restored_runs_after_cross_process_resume() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let workflows_dir = fixture.home.join("sessions").join("s1").join("workflows");
    let (_tool, manager) = tool_manager(&fixture, Some(workflows_dir.clone()));

    // Launch through the manager (as TUI/ACP do), with real persistence.
    let snapshot = manager.snapshot().unwrap();
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &snapshot,
        false,
        "hang",
    )
    .unwrap();
    let state = manager
        .launch(
            resolved,
            lato_agent::workflow::LaunchSpec {
                args: serde_json::json!({}),
                agent_budget: Some(8),
                resume_display_name: None,
            },
        )
        .unwrap();
    let run_id = state.run_id;

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

    let snapshot = manager.snapshot().unwrap();
    let resolved: ResolvedWorkflow = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &snapshot,
        false,
        "hang",
    )
    .unwrap();
    let revision = workflow_revision(&resolved);
    let error = tool
        .invoke(tool_context(), start_arguments("hang", &revision))
        .await
        .expect_err("persistence must fail");
    assert_eq!(error.code, "workflow.persistence_failed");
    assert!(
        manager.list().is_empty(),
        "failed launches must not leave ghost runs"
    );
}
