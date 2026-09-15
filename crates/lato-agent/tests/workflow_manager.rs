// Phase 7B4 WorkflowManager integration tests.

use std::{sync::Arc, time::Duration};

use lato_agent::workflow::{
    LaunchError, LaunchSpec, ResolvedWorkflow, WorkflowManager, WorkflowRunStatus,
};
use lato_ai::{ModelStream, StreamPiece};
use lato_core::ModelError;
use lato_workspace::{FileLocks, SessionTrust};
use tokio::sync::mpsc;

fn resolved(name: &str, script: &str) -> ResolvedWorkflow {
    ResolvedWorkflow {
        id: format!("demo/{name}"),
        display_name: name.to_owned(),
        script: script.to_owned(),
        agent_budget: 8,
        source: "plugin",
        compiled: true,
    }
}

fn spec(args: serde_json::Value) -> LaunchSpec {
    LaunchSpec {
        args,
        agent_budget: None,
        resume_display_name: None,
    }
}

struct HangingStream;

#[async_trait::async_trait]
impl ModelStream for HangingStream {
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

/// Blocks while the gate is closed, answers once it opens — deterministic
/// hold-then-complete without depending on stream call ordering.
#[derive(Clone)]
struct GatedStream {
    open: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl ModelStream for GatedStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        while !self.open.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = tx.send(StreamPiece::Text("ok".into())).await;
        Ok(())
    }
}

fn manager(stream: Arc<dyn ModelStream>) -> WorkflowManager {
    let cwd = std::env::temp_dir();
    WorkflowManager::new(
        "s-manager-test",
        cwd.clone(),
        SessionTrust::for_headless_prompt(&cwd),
        Arc::new(FileLocks::new()),
        stream,
        None,
    )
}

async fn wait_for(
    manager: &WorkflowManager,
    name: &str,
    status: WorkflowRunStatus,
) -> Vec<lato_agent::workflow::WorkflowRunState> {
    for _ in 0..400 {
        let runs = manager.list();
        if let Some(state) = runs.iter().find(|state| state.display_name == name)
            && state.status == status
        {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {name} never reached {status:?}: {:?}", manager.list());
}

#[tokio::test]
async fn second_launch_gets_numbered_display_name() {
    let manager = manager(lato_agent::default_fake_stream());
    let first = manager
        .launch(
            resolved("hold", r#"let meta = #{ name: "hold", description: "d" }; complete("ok");"#),
            spec(serde_json::json!({})),
        )
        .unwrap();
    let second = manager
        .launch(
            resolved("hold", r#"let meta = #{ name: "hold", description: "d" }; complete("ok");"#),
            spec(serde_json::json!({})),
        )
        .unwrap();
    assert_eq!(first.display_name, "hold");
    assert_eq!(second.display_name, "hold-2");
    wait_for(&manager, "hold", WorkflowRunStatus::Complete).await;
    wait_for(&manager, "hold-2", WorkflowRunStatus::Complete).await;
    manager.shutdown().await;
}

#[tokio::test]
async fn fifth_active_run_is_rejected() {
    let manager = manager(Arc::new(HangingStream));
    let script = r#"
        let meta = #{ name: "hang", description: "d" };
        let r = agent("work");
        complete(r.output);
    "#;
    for _ in 0..4 {
        let state = manager.launch(resolved("hang", script), spec(serde_json::json!({}))).unwrap();
        assert_eq!(state.status, WorkflowRunStatus::Active);
    }
    let fifth = manager.launch(resolved("hang", script), spec(serde_json::json!({})));
    assert!(
        matches!(fifth, Err(LaunchError::TooManyActiveRuns)),
        "fifth launch: {fifth:?}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn await_user_then_resume_completes() {
    let manager = manager(lato_agent::default_fake_stream());
    manager
        .launch(
            resolved(
                "gated",
                r#"
                let meta = #{ name: "gated", description: "d" };
                await_user("user", "need human");
                complete("ok");
                "#,
            ),
            spec(serde_json::json!({})),
        )
        .unwrap();
    wait_for(&manager, "gated", WorkflowRunStatus::UserPaused).await;

    let resumed = manager.resume("gated", None).unwrap();
    assert_eq!(resumed.status, WorkflowRunStatus::Active);
    wait_for(&manager, "gated", WorkflowRunStatus::Complete).await;
    manager.shutdown().await;
}

#[tokio::test]
async fn budget_limited_bare_resume_rejected() {
    let manager = manager(lato_agent::default_fake_stream());
    manager
        .launch(
            resolved(
                "budgeted",
                r#"
                let meta = #{ name: "budgeted", description: "d" };
                let a = agent("first");
                let b = agent("second");
                complete("ok");
                "#,
            ),
            LaunchSpec {
                args: serde_json::json!({}),
                agent_budget: Some(1),
                resume_display_name: None,
            },
        )
        .unwrap();
    wait_for(&manager, "budgeted", WorkflowRunStatus::BudgetLimited).await;

    match manager.resume("budgeted", None) {
        Err(LaunchError::BudgetNotRaised { used, limit }) => {
            assert_eq!(used, 1);
            assert_eq!(limit, 1);
        }
        other => panic!("bare resume: {other:?}"),
    }

    manager.resume("budgeted", Some(2)).unwrap();
    wait_for(&manager, "budgeted", WorkflowRunStatus::Complete).await;
    manager.shutdown().await;
}

#[tokio::test]
async fn stop_cancels_active_run() {
    let manager = manager(Arc::new(HangingStream));
    manager
        .launch(
            resolved(
                "stoppable",
                r#"
                let meta = #{ name: "stoppable", description: "d" };
                let r = agent("work");
                complete(r.output);
                "#,
            ),
            spec(serde_json::json!({})),
        )
        .unwrap();
    let stopped = manager.stop("stoppable").await.unwrap();
    assert_eq!(stopped.status, WorkflowRunStatus::Cancelled);
    manager.shutdown().await;
}

#[tokio::test]
async fn user_pause_then_resume_completes() {
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let manager = manager(Arc::new(GatedStream {
        open: Arc::clone(&gate),
    }));
    manager
        .launch(
            resolved(
                "pausable",
                r#"
                let meta = #{ name: "pausable", description: "d" };
                let r = agent("work");
                let s = agent("more");
                complete(r.output + s.output);
                "#,
            ),
            spec(serde_json::json!({})),
        )
        .unwrap();
    manager.pause("pausable").unwrap();
    wait_for(&manager, "pausable", WorkflowRunStatus::UserPaused).await;

    // Resume replays the journal; the unjournaled agent call re-executes live.
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    let resumed = manager.resume("pausable", None).unwrap();
    assert_eq!(resumed.status, WorkflowRunStatus::Active);
    wait_for(&manager, "pausable", WorkflowRunStatus::Complete).await;
    manager.shutdown().await;
}
