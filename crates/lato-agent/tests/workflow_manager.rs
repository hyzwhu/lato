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
        description: "test".to_owned(),
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
        None,
    )
}

/// Manager with a persistent workflows directory (Phase 7B5 restore tests).
fn persistent_manager(stream: Arc<dyn ModelStream>, workflows_dir: &std::path::Path) -> WorkflowManager {
    let cwd = std::env::temp_dir();
    WorkflowManager::new(
        "s-manager-test",
        cwd.clone(),
        SessionTrust::for_headless_prompt(&cwd),
        Arc::new(FileLocks::new()),
        stream,
        None,
        Some(workflows_dir.to_path_buf()),
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

// --- Phase 7B5: cross-process journal resume --------------------------------

fn persist_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn find_run_dir(workflows: &std::path::Path, display_name: &str) -> std::path::PathBuf {
    for entry in std::fs::read_dir(workflows).unwrap().flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(dir.join("run.json"))
            && let Ok(record) =
                serde_json::from_str::<serde_json::Value>(&body)
            && record["displayName"] == display_name
        {
            return dir;
        }
    }
    panic!("no persisted run named {display_name} under {}", workflows.display());
}

const GATED_SCRIPT: &str = r#"
    let meta = #{ name: "gated", description: "d" };
    await_user("user", "need human");
    complete("ok");
"#;

#[tokio::test]
async fn launch_persists_run_layout_under_workflows_dir() {
    let workflows = persist_dir();
    let manager = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    manager
        .launch(resolved("gated", GATED_SCRIPT), spec(serde_json::json!({})))
        .unwrap();
    wait_for(&manager, "gated", WorkflowRunStatus::UserPaused).await;

    let dir = find_run_dir(workflows.path(), "gated");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(record["version"], 1);
    assert_eq!(record["displayName"], "gated");
    assert_eq!(record["status"], "user_paused");
    assert_eq!(record["workflowId"], "demo/gated");
    assert_eq!(record["pauseMessage"], "need human");
    let script = std::fs::read_to_string(dir.join("script.rhai")).unwrap();
    assert!(script.contains("await_user"));
    assert!(dir.join("journal.jsonl").is_file());
    manager.shutdown().await;
}

#[tokio::test]
async fn paused_run_survives_new_manager_and_resumes() {
    let workflows = persist_dir();
    let first = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    first
        .launch(resolved("gated", GATED_SCRIPT), spec(serde_json::json!({})))
        .unwrap();
    wait_for(&first, "gated", WorkflowRunStatus::UserPaused).await;
    drop(first);

    let second = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    let runs = second.list();
    let restored = runs
        .iter()
        .find(|state| state.display_name == "gated")
        .expect("restored paused run");
    assert_eq!(restored.status, WorkflowRunStatus::UserPaused);

    let resumed = second.resume("gated", None).unwrap();
    assert_eq!(resumed.status, WorkflowRunStatus::Active);
    wait_for(&second, "gated", WorkflowRunStatus::Complete).await;
    second.shutdown().await;
}

#[tokio::test]
async fn active_on_disk_restores_as_interrupted() {
    let workflows = persist_dir();
    let run_dir = workflows.path().join("wf_manual-0");
    lato_agent::workflow::persist::write_script(
        &run_dir,
        r#"let meta = #{ name: "crashed", description: "d" }; complete("ok");"#,
    )
    .unwrap();
    lato_agent::workflow::persist::write_run_record(
        &run_dir,
        &lato_agent::workflow::PersistedRun {
            version: lato_agent::workflow::RUN_RECORD_VERSION,
            run_id: "wf_manual-0".into(),
            display_name: "crashed".into(),
            status: lato_agent::workflow::WorkflowRunStatus::Active,
            phase: Some("Run".into()),
            agent_budget: Some(8),
            agents_used: 1,
            pause_message: None,
            elapsed_ms_floor: 500,
            workflow_id: "demo/crashed".into(),
            source: "user".into(),
            compiled: false,
            description: "test".into(),
            args: serde_json::json!({}),
        },
    )
    .unwrap();

    let manager = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    let restored = manager
        .list()
        .into_iter()
        .find(|state| state.display_name == "crashed")
        .expect("restored run");
    assert_eq!(restored.status, WorkflowRunStatus::Interrupted);
    assert_eq!(
        restored.pause_message.as_deref(),
        Some("process exited while active")
    );

    // The rewritten record is terminal on disk too.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_dir.join("run.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["status"], "interrupted");

    match manager.resume("crashed", None) {
        Err(LaunchError::NotResumable(name)) => assert_eq!(name, "crashed"),
        other => panic!("resume of interrupted run: {other:?}"),
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn budget_limited_restore_rejects_bare_resume() {
    let workflows = persist_dir();
    let first = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    first
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
    wait_for(&first, "budgeted", WorkflowRunStatus::BudgetLimited).await;
    drop(first);

    let second = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    match second.resume("budgeted", None) {
        Err(LaunchError::BudgetNotRaised { used, limit }) => {
            assert_eq!(used, 1);
            assert_eq!(limit, 1);
        }
        other => panic!("bare resume after restore: {other:?}"),
    }
    second.resume("budgeted", Some(2)).unwrap();
    wait_for(&second, "budgeted", WorkflowRunStatus::Complete).await;
    second.shutdown().await;
}

#[tokio::test]
async fn corrupt_journal_skips_that_run_only() {
    let workflows = persist_dir();
    let first = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    first
        .launch(resolved("gated", GATED_SCRIPT), spec(serde_json::json!({})))
        .unwrap();
    first
        .launch(
            resolved("sick", GATED_SCRIPT),
            spec(serde_json::json!({})),
        )
        .unwrap();
    wait_for(&first, "gated", WorkflowRunStatus::UserPaused).await;
    wait_for(&first, "sick", WorkflowRunStatus::UserPaused).await;
    drop(first);

    let sick_dir = find_run_dir(workflows.path(), "sick");
    std::fs::write(sick_dir.join("journal.jsonl"), "not-json\n").unwrap();

    let second = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    let names: Vec<String> = second
        .list()
        .into_iter()
        .map(|state| state.display_name)
        .collect();
    assert!(names.contains(&"gated".to_owned()), "{names:?}");
    assert!(!names.contains(&"sick".to_owned()), "{names:?}");
    second.shutdown().await;
}

#[tokio::test]
async fn restored_display_name_is_not_reallocated() {
    let workflows = persist_dir();
    let first = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    first
        .launch(resolved("gated", GATED_SCRIPT), spec(serde_json::json!({})))
        .unwrap();
    wait_for(&first, "gated", WorkflowRunStatus::UserPaused).await;
    drop(first);

    let second = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    let second_launch = second
        .launch(resolved("gated", GATED_SCRIPT), spec(serde_json::json!({})))
        .unwrap();
    assert_eq!(second_launch.display_name, "gated-2");
    second.shutdown().await;
}

#[tokio::test]
async fn scratch_written_before_pause_is_live_after_restore() {
    let workflows = persist_dir();
    let first = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    first
        .launch(
            resolved(
                "scratchy",
                r#"
                let meta = #{ name: "scratchy", description: "d" };
                write_scratch_file("note.md", "persisted body");
                await_user("user", "need human");
                // Deliberately after await_user: this live read is not in the
                // journal and only succeeds if restore finds the file on disk.
                let body = read_scratch_file("note.md");
                write_scratch_file("echo.md", body);
                complete("ok");
                "#,
            ),
            spec(serde_json::json!({})),
        )
        .unwrap();
    wait_for(&first, "scratchy", WorkflowRunStatus::UserPaused).await;
    // The journaled pre-pause write already landed on the run's scratch dir.
    let run_dir = find_run_dir(workflows.path(), "scratchy");
    assert_eq!(
        std::fs::read_to_string(run_dir.join("scratch").join("note.md")).unwrap(),
        "persisted body"
    );
    drop(first);

    let second = persistent_manager(lato_agent::default_fake_stream(), workflows.path());
    second.resume("scratchy", None).unwrap();
    wait_for(&second, "scratchy", WorkflowRunStatus::Complete).await;
    second.shutdown().await;

    assert_eq!(
        std::fs::read_to_string(run_dir.join("scratch").join("echo.md")).unwrap(),
        "persisted body"
    );
}
