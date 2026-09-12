use async_trait::async_trait;
use lato_agent::workflow::{
    WorkflowHostParams, spawn_workflow_host_service, workflow_max_concurrent_agents,
};
use lato_ai::{FakeModelStream, ModelStream, StreamPiece};
use lato_core::{ModelError, SessionId};
use lato_extensions::PluginSnapshot;
use lato_workflow::{AgentOpts, HostError, WorkflowHostRequest};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn worker_output_text() -> String {
    serde_json::json!({
        "summary": "workflow child finished",
        "changed_files": [],
        "tests": [],
        "artifacts": []
    })
    .to_string()
}

fn explorer_output_text() -> String {
    serde_json::json!({
        "answer": "workspace is read-only",
        "evidence": [{
            "id": "readme",
            "summary": "repository readme",
            "location": "README.md"
        }],
        "citations": ["readme"]
    })
    .to_string()
}

fn init_git_repo() -> tempfile::TempDir {
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
    repo
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

fn start_host(
    agent_budget: u64,
    stream: Arc<dyn ModelStream>,
    cwd: std::path::PathBuf,
) -> (
    mpsc::UnboundedSender<WorkflowHostRequest>,
    tokio::task::JoinHandle<()>,
    CancellationToken,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let params = WorkflowHostParams {
        run_id: "wf-1".into(),
        session_id: SessionId::from("cli-workflow"),
        max_concurrent_agents: workflow_max_concurrent_agents(32),
        agent_budget,
        cwd: cwd.clone(),
        stream,
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(&cwd),
        snapshot: PluginSnapshot::empty(),
        cancel: cancel.clone(),
    };
    let join = spawn_workflow_host_service(params, rx);
    (tx, join, cancel)
}

async fn shutdown_host(
    tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    join: tokio::task::JoinHandle<()>,
) {
    drop(tx);
    join.await.unwrap();
}

struct CapturingStream {
    contexts: Arc<Mutex<Vec<serde_json::Value>>>,
    reply: String,
}

#[async_trait]
impl ModelStream for CapturingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: tokio::sync::mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.contexts.lock().unwrap().push(context);
        let _ = tx.send(StreamPiece::Text(self.reply.clone())).await;
        Ok(())
    }
}

fn tool_names(context: &serde_json::Value) -> Vec<String> {
    context
        .get("tools")
        .and_then(|tools| tools.as_array())
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.pointer("/function/name")
                .and_then(|name| name.as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn system_content(context: &serde_json::Value) -> String {
    context
        .get("messages")
        .and_then(|messages| messages.as_array())
        .into_iter()
        .flatten()
        .find_map(|message| {
            if message.get("role").and_then(|role| role.as_str()) == Some("system") {
                Some(
                    message
                        .get("content")
                        .and_then(|content| content.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                )
            } else {
                None
            }
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn spawn_agent_runs_child_session_with_fake_stream() {
    let repo = init_git_repo();
    let stream = Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
        worker_output_text(),
    )]]));
    let (tx, join, _cancel) = start_host(8, stream, repo.path().to_path_buf());
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts {
            prompt: "do the delegated work".into(),
            ..AgentOpts::default()
        },
        reply: reply_tx,
    })
    .unwrap();
    let result = reply_rx.await.unwrap().expect("spawn agent");
    assert!(result.success, "{result:?}");
    assert!(!result.cancelled);
    assert!(
        result
            .output
            .as_str()
            .is_some_and(|output| output.contains("workflow child finished")),
        "{result:?}"
    );
    shutdown_host(tx, join).await;
}

#[tokio::test]
async fn read_only_mode_uses_explorer_workspace() {
    let repo = init_git_repo();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream = Arc::new(CapturingStream {
        contexts: Arc::clone(&contexts),
        reply: explorer_output_text(),
    });
    let (tx, join, _cancel) = start_host(8, stream, repo.path().to_path_buf());
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts {
            prompt: "inspect the workspace".into(),
            capability_mode: Some("read-only".into()),
            ..AgentOpts::default()
        },
        reply: reply_tx,
    })
    .unwrap();
    let result = reply_rx.await.unwrap().expect("spawn agent");
    assert!(result.success, "{result:?}");
    let captured = contexts.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "expected one child model call");
    let system = system_content(&captured[0]);
    assert!(
        system.contains("Inspect the available evidence without modifying the workspace."),
        "{system}"
    );
    let names = tool_names(&captured[0]);
    assert!(
        names.iter().any(|name| name == "read_file"),
        "explorer tools: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|name| name == "write_file" || name == "run_terminal_command"),
        "read-only must not expose write/execute tools: {names:?}"
    );
    shutdown_host(tx, join).await;
}

#[tokio::test]
async fn fork_context_is_unsupported() {
    let repo = init_git_repo();
    let stream = Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
        worker_output_text(),
    )]]));
    let (tx, join, _cancel) = start_host(8, stream, repo.path().to_path_buf());
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts {
            prompt: "forked child".into(),
            fork_context: true,
            ..AgentOpts::default()
        },
        reply: reply_tx,
    })
    .unwrap();
    match reply_rx.await.unwrap() {
        Err(HostError::Unsupported(message)) => {
            assert!(message.contains("fork_context"), "{message}");
        }
        other => panic!("expected unsupported fork_context, got {other:?}"),
    }
    shutdown_host(tx, join).await;
}

#[tokio::test]
async fn parallel_reserve_rejects_over_budget_without_spawns() {
    let repo = init_git_repo();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream = Arc::new(CapturingStream {
        contexts: Arc::clone(&contexts),
        reply: worker_output_text(),
    });
    let (tx, join, _cancel) = start_host(1, stream, repo.path().to_path_buf());
    let (reserve_tx, reserve_rx) = oneshot::channel();
    tx.send(WorkflowHostRequest::ReserveAgentCalls {
        count: 2,
        reply: reserve_tx,
    })
    .unwrap();
    match reserve_rx.await.unwrap() {
        Err(HostError::AgentCallQuotaExceeded { requested, maximum }) => {
            assert_eq!(requested, 2);
            assert_eq!(maximum, 1);
        }
        other => panic!("expected quota exceeded, got {other:?}"),
    }
    assert!(
        contexts.lock().unwrap().is_empty(),
        "over-budget parallel must not launch children"
    );
    shutdown_host(tx, join).await;
}
