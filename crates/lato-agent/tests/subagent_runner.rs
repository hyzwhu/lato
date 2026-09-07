use lato_agent::{
    BuiltinProfileName, ChildSessionConfig, ContextPackageBuilder, ContextPackageLimits,
    ContextReference, HistoryItem, RuntimeSession, default_fake_stream,
};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, LeaseId, ResultContract, SessionId, TaskId,
    TaskOwner, TaskScope, ToolCallId, ToolCapability, ToolContext, TurnId, VerificationPolicy,
    WorkspaceIntent,
};
use lato_runtime::{
    ChannelBackend, CoordinatorConfig, NoopTaskEventSink, TaskRootRequest,
    spawn_subagent_coordinator_with_verifier,
};
use lato_tools::{
    BuiltinToolEnvironment, builtin_tool_runtime, builtin_tool_runtime_with_subagents,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[test]
fn built_in_profiles_are_closed_and_resolve_expected_authority() {
    let explorer = BuiltinProfileName::try_from("explorer").unwrap().resolve();
    assert_eq!(explorer.workspace, WorkspaceIntent::SharedReadOnly);
    assert_eq!(
        explorer.capabilities,
        vec![ToolCapability::FileRead, ToolCapability::NetworkRead]
    );
    assert!(BuiltinProfileName::try_from("custom").is_err());
}

#[test]
fn worker_cannot_recover_capability_absent_from_parent() {
    let worker = BuiltinProfileName::Worker.resolve();
    let effective = worker
        .effective_capabilities(&[ToolCapability::FileRead, ToolCapability::FileWrite], None)
        .unwrap();
    assert_eq!(
        effective,
        vec![ToolCapability::FileRead, ToolCapability::FileWrite]
    );
    assert!(!effective.contains(&ToolCapability::ProcessSpawn));
}

#[test]
fn context_package_is_bounded_and_contains_only_selected_context() {
    let limits = ContextPackageLimits {
        max_bytes: 1_024,
        max_constraints: 1,
        max_references: 1,
        max_summary_bytes: 32,
    };
    let package = ContextPackageBuilder::new(limits)
        .task("inspect parser")
        .profile_instructions("read only")
        .constraints(vec!["do not write".into(), "discarded".into()])
        .parent_summary("relevant state; unrelated parent turn canary is not supplied")
        .references(vec![
            ContextReference {
                id: "parser".into(),
                summary: "parser source".into(),
                location: Some("src/parser.rs".into()),
            },
            ContextReference {
                id: "discarded".into(),
                summary: "not selected after cap".into(),
                location: None,
            },
        ])
        .workspace_root("/tmp/workspace")
        .remaining_budget(BudgetAmount {
            total_tokens: 100,
            ..BudgetAmount::ZERO
        })
        .build()
        .unwrap();

    assert!(package.encoded_len() <= limits.max_bytes);
    assert_eq!(package.constraints, vec!["do not write"]);
    assert_eq!(package.references.len(), 1);
    assert!(package.parent_summary.unwrap().len() <= limits.max_summary_bytes);
}

#[tokio::test]
async fn child_session_uses_injected_history_and_shuts_down_boundedly() {
    let cwd = std::env::current_dir().unwrap();
    let locks = Arc::new(FileLocks::new());
    let trust = SessionTrust::for_headless_prompt(&cwd);
    let tool_runtime = builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: cwd.clone(),
        locks: locks.clone(),
        trust: trust.clone(),
    })
    .unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_child(ChildSessionConfig {
        session_id: "child-session".into(),
        stream: default_fake_stream(),
        locks,
        trust,
        cwd,
        updates,
        approval: None,
        tool_runtime,
        initial_history: vec![HistoryItem::System("bounded child context".into())],
    })
    .await
    .unwrap();

    assert_eq!(session.session_id().as_str(), "child-session");
    assert_eq!(
        session.history_snapshot().await,
        vec![HistoryItem::System("bounded child context".into())]
    );
    session
        .cancel_and_join(Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn real_runner_executes_a_child_runtime_and_returns_to_coordinator() {
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q"]);
    run_git(
        repo.path(),
        &["config", "user.email", "lato@example.invalid"],
    );
    run_git(repo.path(), &["config", "user.name", "Lato Test"]);
    std::fs::write(repo.path().join("README.md"), "root\n").unwrap();
    run_git(repo.path(), &["add", "README.md"]);
    run_git(repo.path(), &["commit", "-qm", "initial"]);

    let locks = Arc::new(FileLocks::new());
    let trust = SessionTrust::for_headless_prompt(repo.path());
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let runner = Arc::new(lato_agent::ChildSessionRunner::new(
        Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
            serde_json::json!({
                "answer": "README exists",
                "evidence": [{
                    "id": "readme",
                    "summary": "repository readme",
                    "location": "README.md"
                }],
                "citations": ["readme"]
            })
            .to_string(),
        )]])),
        locks,
        trust,
        updates,
        None,
    ));
    let allocator = Arc::new(
        lato_workspace::GitWorkspaceAllocator::new(
            repo.path(),
            repo.path().join(".lato/worktrees"),
        )
        .unwrap(),
    );
    let (handle, actor) = spawn_subagent_coordinator_with_verifier(
        CoordinatorConfig::default(),
        runner,
        allocator,
        Arc::new(lato_agent::ProfileResultVerifier),
        Arc::new(NoopTaskEventSink),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("parent-session"),
                turn_id: TurnId::from("parent-turn"),
            },
            profile: AgentProfile {
                name: "root".into(),
                instructions: "coordinate".into(),
                capabilities: vec![ToolCapability::FileRead, ToolCapability::NetworkRead],
                workspace: WorkspaceIntent::IsolatedWorktree,
                verification: VerificationPolicy::Accept,
                definition_background: false,
            },
            permissions: vec![ToolCapability::FileRead, ToolCapability::NetworkRead],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let runtime = builtin_tool_runtime_with_subagents(
        BuiltinToolEnvironment {
            cwd: repo.path().to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(repo.path()),
        },
        ChannelBackend::new(root).into_resource(),
    )
    .unwrap();
    let definitions = runtime.model_definitions();
    let names = definitions
        .iter()
        .filter_map(|definition| {
            definition
                .pointer("/function/name")
                .and_then(|name| name.as_str())
        })
        .collect::<Vec<_>>();
    for name in ["spawn", "send", "wait", "cancel", "inspect"] {
        assert!(names.contains(&name));
    }
    assert!(!names.contains(&"spawn_subagent"));
    let context = || ToolContext {
        session_id: SessionId::from("parent-session"),
        turn_id: TurnId::from("parent-turn"),
        call_id: ToolCallId::from("task-tool-call"),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    };
    let spawned = runtime
        .invoke(
            context(),
            "spawn",
            serde_json::json!({
                "task_id": "child",
                "profile": "explorer",
                "task": "inspect",
                "context_refs": ["README.md"],
                "background": true
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&spawned.content).unwrap()["task_id"],
        "child"
    );

    let waited = runtime
        .invoke(
            context(),
            "wait",
            serde_json::json!({"task_id":"child", "timeout_ms":3_000}),
        )
        .await
        .unwrap();
    let waited: serde_json::Value = serde_json::from_str(&waited.content).unwrap();
    assert_eq!(waited["wait"], "finished");
    assert_eq!(waited["snapshot"]["task"]["status"], "completed");
    assert!(
        waited["snapshot"]["usage"]["output_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        waited["snapshot"]["result"]["output"]
            .as_str()
            .unwrap()
            .contains("README exists")
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn profile_verifier_accepts_structured_outputs_and_rejects_escaped_paths() {
    use lato_runtime::{TaskVerifier, VerificationOutcome, VerificationRequest};

    let worker = lato_agent::WorkerOutput {
        summary: "implemented".into(),
        changed_files: vec!["src/lib.rs".into()],
        tests: vec![lato_agent::WorkerTestResult {
            command: "cargo test".into(),
            passed: true,
            summary: "passed".into(),
        }],
        artifacts: Vec::new(),
    };
    let request = verification_request(
        AgentProfile::worker(),
        serde_json::to_string(&worker).unwrap(),
    );
    assert_eq!(
        lato_agent::ProfileResultVerifier.verify(request).await,
        VerificationOutcome::Passed
    );

    let escaped = lato_agent::WorkerOutput {
        changed_files: vec!["../outside".into()],
        ..worker.clone()
    };
    let outcome = lato_agent::ProfileResultVerifier
        .verify(verification_request(
            AgentProfile::worker(),
            serde_json::to_string(&escaped).unwrap(),
        ))
        .await;
    assert!(matches!(outcome, VerificationOutcome::Failed(_)));

    let workspace = tempfile::tempdir().unwrap();
    let missing = lato_agent::WorkerOutput {
        changed_files: vec!["missing.rs".into()],
        ..worker
    };
    let mut request = verification_request(
        AgentProfile::worker(),
        serde_json::to_string(&missing).unwrap(),
    );
    request.workspace_lease = Some(lato_workspace::WorkspaceLease::new(
        LeaseId::from("verification-lease"),
        TaskId::from("verify"),
        lato_workspace::WorkspaceMode::IsolatedWorktree,
        workspace.path().to_path_buf(),
        None,
    ));
    assert!(matches!(
        lato_agent::ProfileResultVerifier.verify(request).await,
        VerificationOutcome::Failed(_)
    ));

    fn verification_request(profile: AgentProfile, output: String) -> VerificationRequest {
        VerificationRequest {
            node: lato_core::TaskNode {
                id: TaskId::from("verify"),
                parent_id: Some(TaskId::from("root")),
                root_id: TaskId::from("root"),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from("session"),
                    turn_id: TurnId::from("turn"),
                },
                profile: profile.clone(),
                scope: TaskScope {
                    objective: "verify".into(),
                    context_refs: Vec::new(),
                },
                status: lato_core::TaskStatus::Verifying,
                permissions: profile.capabilities.clone(),
                workspace_intent: profile.workspace,
                result_contract: ResultContract {
                    schema: None,
                    max_output_bytes: 4_096,
                },
            },
            result: lato_core::TaskResult {
                success: true,
                output,
                error: None,
                usage: Default::default(),
                duration_ms: 0,
                output_ref: None,
            },
            workspace_lease: None,
        }
    }
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}
