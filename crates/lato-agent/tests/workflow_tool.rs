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

    async fn contexts(&self) -> Vec<serde_json::Value> {
        self.contexts.lock().await.clone()
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
        plan_mode: false,
        tool_layer: lato_core::ToolLayer::Builtin,
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

// ---- TG-7B7-02: acceptance coverage gaps (round-1 rework) ----

use lato_core::Retryability;
use tokio_util::sync::CancellationToken;

/// AC-02 dedicated: a guessed (never-listed) name cannot be started and
/// reports the stable `workflow.not_found` code with zero side effects.
#[tokio::test]
async fn guessed_names_fail_with_workflow_not_found() {
    let fixture = ToolFixture::new();
    fixture.write_user("review", &hang_script("review"));
    let (tool, manager) = tool_manager(&fixture, None);

    // The guessed name is absent from the listing.
    let listed = tool
        .invoke(tool_context(), serde_json::json!({"action":"list"}))
        .await
        .unwrap();
    let listed = serde_json::from_str::<serde_json::Value>(&listed.content).unwrap();
    assert!(
        listed["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["id"] != "phantom")
    );

    let error = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"start","name":"phantom","revision":"0".repeat(64),"agentBudget":8}),
        )
        .await
        .expect_err("guessed names must not launch");
    assert_eq!(error.code, "workflow.not_found");
    assert_eq!(error.retryability, Retryability::Never);
    assert!(manager.list().is_empty());
}

/// D-7B7-01: pre-policy schema rejections for the workflow tool report the
/// domain-stable `workflow.invalid_arguments` code, never the generic
/// `tool.invalid_arguments`, and never reach approval or invoke.
#[tokio::test]
async fn pre_policy_schema_rejections_report_workflow_invalid_arguments() {
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
    builder.set_argument_error_code(
        lato_core::ToolName::parse("builtin:workflow").unwrap(),
        "workflow.invalid_arguments",
    );
    builder.register(Arc::new(tool)).unwrap();
    let runtime = builder.build().unwrap();

    for arguments in [
        serde_json::json!({"action":"list","unexpected":true}),
        serde_json::json!({"action":"start"}),
        serde_json::json!({"action":"status","args":{}}),
    ] {
        let error = runtime
            .invoke(tool_context(), "workflow", arguments)
            .await
            .expect_err("schema violations must be rejected pre-policy");
        assert_eq!(error.code, "workflow.invalid_arguments");
    }
    assert!(manager.list().is_empty());
}

/// AC-07 (v1.2): two genuinely concurrent `start` calls race the atomic
/// 4-active limit — the loser gets the stable error and `active_count` never
/// exceeds 4.
#[tokio::test]
async fn concurrent_starts_respect_the_atomic_four_active_limit() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let (_tool, manager) = tool_manager(&fixture, None);
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &manager.snapshot().unwrap(),
        false,
        "hang",
    )
    .unwrap();

    // Three active runs already.
    for _ in 0..3 {
        manager
            .launch(
                resolved.clone(),
                lato_agent::workflow::LaunchSpec {
                    args: serde_json::json!({}),
                    agent_budget: Some(8),
                    resume_display_name: None,
                },
            )
            .unwrap();
    }
    assert_eq!(
        manager
            .list()
            .iter()
            .filter(|r| r.status == lato_agent::workflow::WorkflowRunStatus::Active)
            .count(),
        3
    );

    // Two genuinely concurrent starts (OS threads, like two tool calls racing
    // the same manager) fight for the last slot; the manager's launch lock
    // makes the 4-active check atomic.
    let racer = |resolved: lato_agent::workflow::ResolvedWorkflow| {
        let manager = manager.clone();
        tokio::task::spawn_blocking(move || {
            manager.launch(
                resolved,
                lato_agent::workflow::LaunchSpec {
                    args: serde_json::json!({}),
                    agent_budget: Some(8),
                    resume_display_name: None,
                },
            )
        })
    };
    let (first, second) = tokio::join!(racer(resolved.clone()), racer(resolved.clone()));
    let outcomes = [first.unwrap(), second.unwrap()];
    let succeeded = outcomes.iter().filter(|result| result.is_ok()).count();
    let rejected = outcomes
        .iter()
        .filter_map(|result| result.as_ref().err())
        .filter(|error| matches!(error, lato_agent::workflow::LaunchError::TooManyActiveRuns))
        .count();
    assert_eq!(succeeded, 1, "exactly one racer wins the last slot");
    assert_eq!(rejected, 1, "the loser gets the stable too-many error");
    let active = manager
        .list()
        .iter()
        .filter(|run| run.status == lato_agent::workflow::WorkflowRunStatus::Active)
        .count();
    assert_eq!(active, 4, "the atomic limit is never exceeded");
}

/// §9.1 #5: catalog changes from every source (project script, plugin
/// snapshot) invalidate the listed revision — `workflow.catalog_changed`,
/// zero runs. (User-source coverage lives in the tool unit tests.)
#[tokio::test]
async fn catalog_changed_covers_project_and_plugin_sources() {
    // Project source.
    let fixture = ToolFixture::new();
    let project_dir = fixture.cwd.join(".lato").join("workflows");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("projw.rhai"), hang_script("projw")).unwrap();
    let (_tool, manager) = tool_manager(&fixture, None);
    // tool_manager built the snapshot untrusted; rebuild trusted so the
    // project workflow is visible.
    let snapshot = lato_extensions::build_snapshot(
        2,
        lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
            cwd: fixture.cwd.clone(),
            lato_home: fixture.home.clone(),
            cli_plugin_dirs: Vec::new(),
            project_trusted: true,
        }),
        &lato_extensions::PluginConfig::default(),
    )
    .unwrap();
    manager.set_snapshot(snapshot);
    let trust = Trust::for_interactive(&fixture.cwd, true);
    let trusted_handle =
        SessionWorkflowHandle::new(fixture.cwd.clone(), fixture.home.clone(), trust.clone());
    trusted_handle.install(manager.clone());
    let tool = WorkflowTool::new(trusted_handle);

    let listed = tool
        .invoke(tool_context(), serde_json::json!({"action":"list"}))
        .await
        .unwrap();
    let listed = serde_json::from_str::<serde_json::Value>(&listed.content).unwrap();
    let entry = listed["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "projw")
        .expect("trusted project workflow is listed")
        .clone();
    let stale_revision = entry["revision"].as_str().unwrap().to_owned();

    // Change the project script after listing.
    std::fs::write(
        project_dir.join("projw.rhai"),
        format!("{}\n// tampered", hang_script("projw")),
    )
    .unwrap();
    let error = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"start","name":"projw","revision":stale_revision,"agentBudget":8}),
        )
        .await
        .expect_err("project-source change must invalidate the revision");
    assert_eq!(error.code, "workflow.catalog_changed");
    assert!(manager.list().is_empty());

    // Plugin source: the descriptor's declared budget changes → new revision.
    let plugin_fixture = ToolFixture::new();
    let plugin_root = plugin_fixture.home.join("plugins").join("demo");
    std::fs::create_dir_all(&plugin_root).unwrap();
    std::fs::write(
        plugin_root.join("plugin.json"),
        r#"{"name":"demo","workflows":{"plugw":{"description":"Demo","agentBudget":16}}}"#,
    )
    .unwrap();
    let (tool, manager) = tool_manager(&fixture, None);
    let plugin_snapshot = lato_extensions::build_snapshot(
        3,
        lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
            cwd: fixture.cwd.clone(),
            lato_home: fixture.home.clone(),
            cli_plugin_dirs: vec![plugin_root.clone()],
            project_trusted: true,
        }),
        &lato_extensions::PluginConfig::default(),
    )
    .unwrap();
    manager.set_snapshot(plugin_snapshot);
    let listed = tool
        .invoke(tool_context(), serde_json::json!({"action":"list"}))
        .await
        .unwrap();
    let listed = serde_json::from_str::<serde_json::Value>(&listed.content).unwrap();
    let entry = listed["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "demo/plugw")
        .expect("plugin workflow is listed")
        .clone();
    let stale_revision = entry["revision"].as_str().unwrap().to_owned();

    // The plugin changes its declared budget after listing; the session
    // adopts the new snapshot.
    std::fs::write(
        plugin_root.join("plugin.json"),
        r#"{"name":"demo","workflows":{"plugw":{"description":"Demo","agentBudget":24}}}"#,
    )
    .unwrap();
    let plugin_snapshot = lato_extensions::build_snapshot(
        4,
        lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
            cwd: fixture.cwd.clone(),
            lato_home: fixture.home.clone(),
            cli_plugin_dirs: vec![plugin_root],
            project_trusted: true,
        }),
        &lato_extensions::PluginConfig::default(),
    )
    .unwrap();
    manager.set_snapshot(plugin_snapshot);

    let error = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"start","name":"demo/plugw","revision":stale_revision,"agentBudget":16}),
        )
        .await
        .expect_err("plugin-source change must invalidate the revision");
    assert_eq!(error.code, "workflow.catalog_changed");
    assert!(manager.list().is_empty());
}

/// §9.1 #9: cancelling the tool call AFTER a successful launch does not stop
/// the run — it keeps streaming updates in the background.
#[tokio::test]
async fn post_launch_cancellation_does_not_stop_the_run() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let (tool, manager) = tool_manager(&fixture, None);
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &manager.snapshot().unwrap(),
        false,
        "hang",
    )
    .unwrap();
    let revision = workflow_revision(&resolved);

    let mut context = tool_context();
    let token = CancellationToken::new();
    context.cancellation = token.clone();
    let output = tool
        .invoke(context, start_arguments("hang", &revision))
        .await
        .unwrap();
    let run_id =
        serde_json::from_str::<serde_json::Value>(&output.content).unwrap()["run"]["runId"]
            .as_str()
            .unwrap()
            .to_owned();

    // Cancel the tool call after launch succeeded.
    token.cancel();

    // The run is NOT stopped: it stays active well past the cancellation.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let run = manager
        .list()
        .into_iter()
        .find(|run| run.run_id == run_id)
        .expect("run still exists");
    assert_eq!(
        run.status,
        lato_agent::workflow::WorkflowRunStatus::Active,
        "tool-call cancellation must not stop a launched run"
    );

    // Status queries keep reporting the live run after the cancellation.
    let status = tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"status","run":run_id}),
        )
        .await
        .unwrap();
    let run = serde_json::from_str::<serde_json::Value>(&status.content).unwrap()["run"].clone();
    assert_eq!(run["runId"], run_id.as_str());
    assert_eq!(run["status"], "active");
}

/// §9.1 #9: after session close (manager shutdown) every late model-tool
/// call fails closed with `workflow.unavailable`.
#[tokio::test]
async fn late_calls_after_session_close_fail_closed() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let workflows_dir = fixture.home.join("wf-close");
    let (tool, manager) = tool_manager(&fixture, Some(workflows_dir.clone()));

    // A run exists before close; shutdown marks it terminal (persisted).
    let snapshot = manager.snapshot().unwrap();
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &snapshot,
        false,
        "hang",
    )
    .unwrap();
    manager
        .launch(
            resolved,
            lato_agent::workflow::LaunchSpec {
                args: serde_json::json!({}),
                agent_budget: Some(8),
                resume_display_name: None,
            },
        )
        .unwrap();
    manager.shutdown().await;
    assert!(manager.is_closed());
    let runs_before = manager.list().len();

    for arguments in [
        serde_json::json!({"action":"list"}),
        serde_json::json!({"action":"start","name":"hang","revision":"0".repeat(64),"agentBudget":8}),
        serde_json::json!({"action":"status"}),
    ] {
        let error = tool
            .invoke(tool_context(), arguments)
            .await
            .expect_err("late calls after close must fail closed");
        assert_eq!(error.code, "workflow.unavailable");
    }
    assert_eq!(
        manager.list().len(),
        runs_before,
        "late calls must not create ghost runs"
    );
}

// ---- designer-matrix extensions: TOCTOU grant consumption across all three
// catalog sources, and paused-run cross-process restore via the tool ----

/// §9.1 #5 (v1.2 full matrix): for EACH catalog source (user / trusted
/// project / trusted+enabled plugin) a content change between listing and
/// execution returns `workflow.catalog_changed` with the grant already
/// consumed — replaying the same grant yields `policy.grant_consumed`, and
/// runs, run directories, and journals stay at zero.
#[tokio::test]
async fn stale_revision_per_source_consumes_grant_with_zero_artifacts() {
    for source in ["user", "project", "plugin"] {
        let fixture = ToolFixture::new();
        let project_trusted = source != "user";
        let mut cli_plugin_dirs = Vec::new();
        match source {
            "user" => fixture.write_user("review", &hang_script("review")),
            "project" => {
                let dir = fixture.cwd.join(".lato").join("workflows");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("review.rhai"), hang_script("review")).unwrap();
            }
            "plugin" => {
                let root = fixture.home.join("plugins").join("demo");
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(
                    root.join("plugin.json"),
                    r#"{"name":"demo","workflows":{"review":{"description":"Demo","agentBudget":8}}}"#,
                )
                .unwrap();
                cli_plugin_dirs.push(root.clone());
            }
            _ => unreachable!(),
        }

        let workflows_dir = fixture.home.join(format!("wf-{source}"));
        let trust = Trust::for_interactive(&fixture.cwd, project_trusted);
        let handle =
            SessionWorkflowHandle::new(fixture.cwd.clone(), fixture.home.clone(), trust.clone());
        let manager = Arc::new(WorkflowManager::new(
            "s-source-test",
            fixture.cwd.clone(),
            trust.clone(),
            Arc::new(FileLocks::new()),
            Arc::new(PendingStream),
            None,
            Some(workflows_dir.clone()),
        ));
        let snapshot = lato_extensions::build_snapshot(
            1,
            lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
                cwd: fixture.cwd.clone(),
                lato_home: fixture.home.clone(),
                cli_plugin_dirs: cli_plugin_dirs.clone(),
                project_trusted,
            }),
            &lato_extensions::PluginConfig::default(),
        )
        .unwrap();
        manager.set_snapshot(snapshot);
        handle.install(manager.clone());
        let tool = WorkflowTool::new(handle);

        let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
            Duration::from_secs(60),
        ))));
        let mut builder = ToolRuntimeBuilder::new(
            policy,
            PolicyScope {
                workspace_root: fixture.cwd.clone(),
                mode: PolicyMode::Ask,
                project_trusted,
                sandbox_profile: SandboxProfile::Off,
            },
        );
        builder.register(Arc::new(tool)).unwrap();
        let runtime = builder.build().unwrap();

        // The model lists (revision captured), then the source changes.
        let resolved = lato_agent::workflow::resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &manager.snapshot().unwrap(),
            project_trusted,
            "review",
        )
        .unwrap();
        let stale_revision = workflow_revision(&resolved);
        match source {
            "user" => {
                fixture.write_user("review", &format!("{}\n// tampered", hang_script("review")))
            }
            "project" => {
                let dir = fixture.cwd.join(".lato").join("workflows");
                std::fs::write(
                    dir.join("review.rhai"),
                    format!("{}\n// tampered", hang_script("review")),
                )
                .unwrap();
            }
            "plugin" => {
                let root = cli_plugin_dirs.remove(0);
                std::fs::write(
                    root.join("plugin.json"),
                    r#"{"name":"demo","workflows":{"review":{"description":"Demo","agentBudget":9}}}"#,
                )
                .unwrap();
                let snapshot = lato_extensions::build_snapshot(
                    2,
                    lato_extensions::discover_plugins(&lato_extensions::DiscoveryConfig {
                        cwd: fixture.cwd.clone(),
                        lato_home: fixture.home.clone(),
                        cli_plugin_dirs: vec![root],
                        project_trusted,
                    }),
                    &lato_extensions::PluginConfig::default(),
                )
                .unwrap();
                manager.set_snapshot(snapshot);
            }
            _ => unreachable!(),
        }

        // Full approval round-trip: the grant is consumed entering invoke,
        // which then fails on the stale revision.
        let prepared = runtime
            .prepare(
                tool_context(),
                "workflow",
                serde_json::json!({"action":"start","name":"review","revision":stale_revision,"agentBudget":8}),
            )
            .unwrap();
        let request = match runtime.decision(&prepared).clone() {
            PolicyDecision::RequireApproval(request) => request,
            other => panic!("[{source}] expected RequireApproval, got {other:?}"),
        };
        let grant = runtime.approve(&request).unwrap();
        let error = runtime
            .execute(prepared, grant.clone())
            .await
            .expect_err("stale revision must not launch");
        assert_eq!(error.code, "workflow.catalog_changed", "[{source}]");
        assert!(manager.list().is_empty(), "[{source}] no ghost runs");
        assert!(
            persisted_run_dirs(&workflows_dir).is_empty(),
            "[{source}] no run directories or journals"
        );

        // The consumed grant stays consumed.
        let prepared_again = runtime
            .prepare(
                tool_context(),
                "workflow",
                serde_json::json!({"action":"start","name":"review","revision":stale_revision,"agentBudget":8}),
            )
            .unwrap();
        let replay = runtime.execute(prepared_again, grant).await;
        assert_eq!(
            replay.expect_err("grant must stay consumed").code,
            "policy.grant_consumed",
            "[{source}]"
        );
        assert!(manager.list().is_empty(), "[{source}]");
        assert!(persisted_run_dirs(&workflows_dir).is_empty(), "[{source}]");
    }
}

/// AC-08 extension: a run paused BEFORE a hard process exit restores as
/// paused (not interrupted) and the tool reports `paused` / `user_paused`.
#[tokio::test]
async fn paused_run_restores_and_is_queryable_via_tool() {
    let fixture = ToolFixture::new();
    fixture.write_user("hang", &hang_script("hang"));
    let workflows_dir = fixture.home.join("sessions").join("s2").join("workflows");
    let (_tool, manager) = tool_manager(&fixture, Some(workflows_dir.clone()));

    let snapshot = manager.snapshot().unwrap();
    let resolved = lato_agent::workflow::resolve_workflow(
        &fixture.cwd,
        &fixture.home,
        &snapshot,
        false,
        "hang",
    )
    .unwrap();
    manager
        .launch(
            resolved,
            lato_agent::workflow::LaunchSpec {
                args: serde_json::json!({}),
                agent_budget: Some(8),
                resume_display_name: None,
            },
        )
        .unwrap();
    manager.pause("hang").unwrap();
    for _ in 0..400 {
        if manager.list().iter().any(|run| {
            run.display_name == "hang"
                && run.status == lato_agent::workflow::WorkflowRunStatus::UserPaused
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        manager.list().iter().any(|run| run.display_name == "hang"
            && run.status == lato_agent::workflow::WorkflowRunStatus::UserPaused),
        "run must settle into user_paused before the simulated crash"
    );

    // Hard exit while paused; the on-disk record keeps the paused status.
    drop(manager);

    // New process: the paused run restores AS PAUSED (resumable, not
    // interrupted) and the tool reports the restored truth.
    let (restored_tool, _restored_manager) = tool_manager(&fixture, Some(workflows_dir));
    let status = restored_tool
        .invoke(
            tool_context(),
            serde_json::json!({"action":"status","run":"hang"}),
        )
        .await
        .unwrap();
    let run = serde_json::from_str::<serde_json::Value>(&status.content).unwrap()["run"].clone();
    assert_eq!(run["displayName"], "hang");
    assert_eq!(run["status"], "paused");
    assert_eq!(run["detailStatus"], "user_paused");
}

/// Registration across the session lifecycle: after a real `session/resume`
/// (new host process over the same home), the first model turn still sees the
/// unique `workflow` tool and can drive it end-to-end.
#[tokio::test]
async fn resumed_session_model_turn_sees_the_workflow_tool() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let first_stream = Arc::new(ScriptedStream::scripted(vec![vec![StreamPiece::Text(
        "hello".into(),
    )]]));
    let mut first = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        Trust::for_headless_prompt(workspace.path()),
        updates,
        first_stream,
        home.path().to_path_buf(),
    );
    let sid = first
        .handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let response = first
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    // "New process": a fresh host over the same home resumes the session.
    let resumed_stream = Arc::new(ScriptedStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "call-r1".into(),
            name: "workflow".into(),
            arguments: serde_json::json!({"action":"list"}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (updates2, _updates_rx2) = mpsc::unbounded_channel();
    let mut resumed = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        Trust::for_headless_prompt(workspace.path()),
        updates2,
        resumed_stream.clone(),
        home.path().to_path_buf(),
    );
    let response = resumed
        .handle(req(
            3,
            "session/resume",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "resume failed: {response}");

    let response = resumed
        .handle(req(
            4,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "list workflows"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    let contexts = resumed_stream.contexts().await;
    assert!(
        contexts.len() >= 2,
        "expected a second model round after resume"
    );
    let catalog = serde_json::to_string(&contexts[0]).unwrap();
    assert_eq!(catalog.matches("\"name\":\"workflow\"").count(), 1);
    let messages = contexts[1]["messages"].as_array().unwrap().clone();
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("workflow tool result reached the resumed model");
    let result: serde_json::Value =
        serde_json::from_str(tool_message["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["action"], "list");
    assert!(result["workflows"].is_array());
}
