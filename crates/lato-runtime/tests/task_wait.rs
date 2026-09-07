mod task_support;

use futures_util::future::BoxFuture;
use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, TaskError, TaskId, TaskProgress, TaskResult,
    TaskScope, TaskStatus, TaskUsage,
};
use lato_runtime::{
    ActiveMessageAdmission, ActiveMessageDelivery, CompletionDisposition, CoordinatorConfig,
    SpawnMode, SpawnTaskRequest, StartedTask, TaskChildControl, TaskReporter, TaskRunOutput,
    TaskRunRequest, TaskRunner, WaitOutcome, spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{
    collections::HashMap,
    future::ready,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use task_support::{GatedTaskRunner, Harness};
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

fn request(id: &str, mode: SpawnMode) -> SpawnTaskRequest {
    SpawnTaskRequest {
        task_id: TaskId::from(id),
        scope: TaskScope {
            objective: format!("run {id}"),
            context_refs: Vec::new(),
        },
        profile: AgentProfile::worker(),
        requested_capabilities: None,
        budget: BudgetLimits::unlimited(),
        result_contract: ResultContract {
            schema: None,
            max_output_bytes: 1_024,
        },
        mode,
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn queued_time_consumes_the_foreground_budget() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        foreground_budget: Duration::from_secs(45),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("blocker", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("blocker", TaskStatus::Running)
        .await;

    let waiting_root = root.clone();
    let pending = tokio::spawn(async move {
        waiting_root
            .spawn_and_wait(request("queued", SpawnMode::Foreground))
            .await
            .unwrap()
    });
    harness.wait_for_status("queued", TaskStatus::Queued).await;
    tokio::time::advance(Duration::from_secs(45)).await;
    let disposition = pending.await.unwrap();
    assert!(disposition.backgrounded);
    assert!(!disposition.foreground_delivered);
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("queued"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Queued
    );
    harness.runner.finish("blocker").await;
}

#[tokio::test(start_paused = true)]
async fn await_completion_has_no_foreground_deadline_and_explicit_background_returns_immediately() {
    let harness = Harness::new(CoordinatorConfig {
        foreground_budget: Duration::from_secs(1),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let await_root = root.clone();
    let awaiting = tokio::spawn(async move {
        await_root
            .spawn_and_wait(request("await", SpawnMode::AwaitCompletion))
            .await
            .unwrap()
    });
    harness.wait_for_status("await", TaskStatus::Running).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(!awaiting.is_finished());
    harness.runner.finish("await").await;
    let disposition = awaiting.await.unwrap();
    assert!(disposition.foreground_delivered);
    assert!(!disposition.backgrounded);

    let background = root
        .spawn_and_wait(request("background", SpawnMode::Background))
        .await
        .unwrap();
    assert_eq!(
        background,
        CompletionDisposition {
            backgrounded: true,
            ..CompletionDisposition::default()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_foreground_caller_and_waiter_does_not_cancel_or_suppress_later_surfacing() {
    let harness = Harness::new(CoordinatorConfig {
        foreground_budget: Duration::from_secs(30),
        queued_reap_interval: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let disposition = root
        .spawn(request("child", SpawnMode::Foreground))
        .await
        .unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    let child = disposition.handle.clone();
    let foreground = tokio::spawn(async move { child.wait_foreground().await });
    let waiter_root = root.clone();
    let waiter = tokio::spawn(async move {
        waiter_root
            .wait(TaskId::from("child"), Duration::from_secs(20))
            .await
    });
    tokio::task::yield_now().await;
    foreground.abort();
    waiter.abort();
    tokio::time::advance(Duration::from_millis(10)).await;
    harness.runner.finish("child").await;
    harness
        .wait_for_status("child", TaskStatus::Completed)
        .await;
    let snapshot = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert!(snapshot.completion_disposition.unwrap().should_surface);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_workflow_await_cancels_owned_work_on_the_bounded_reap_deadline() {
    let harness = Harness::new(CoordinatorConfig {
        queued_reap_interval: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Workflow {
                run_id: "workflow".into(),
                session_id: lato_core::SessionId::from("session"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let waiting_root = root.clone();
    let foreground = tokio::spawn(async move {
        waiting_root
            .spawn_and_wait(request("child", SpawnMode::AwaitCompletion))
            .await
    });
    harness.wait_for_status("child", TaskStatus::Running).await;
    tokio::task::yield_now().await;
    foreground.abort();
    tokio::time::advance(Duration::from_millis(20)).await;
    harness
        .wait_for_status("child", TaskStatus::Cancelled)
        .await;
    let disposition = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap()
        .completion_disposition
        .unwrap();
    assert!(disposition.explicitly_killed);
    assert!(!disposition.should_surface);
}

#[tokio::test]
async fn workflow_drop_during_profile_validation_never_reserves_or_starts_the_child() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, false, false, true));
    let harness = Harness::with_runner(CoordinatorConfig::default(), runner.clone()).await;
    let root = harness
        .handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Workflow {
                run_id: "workflow".into(),
                session_id: lato_core::SessionId::from("session"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let waiting_root = root.clone();
    let foreground = tokio::spawn(async move {
        waiting_root
            .spawn_and_wait(request("child", SpawnMode::AwaitCompletion))
            .await
    });
    runner.wait_until_validation_entered().await;
    foreground.abort();
    runner.allow_validation();
    tokio::time::timeout(Duration::from_secs(1), async {
        while runner.validation_calls() == 0 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
    })
    .await
    .unwrap();
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_err()
    );
    assert!(runner.started_ids().await.is_empty());
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved
            .child_tasks,
        0
    );
}

#[tokio::test(start_paused = true)]
async fn waiters_have_independent_timeouts_and_a_survivor_suppresses_surfacing() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    let short_root = root.clone();
    let short = tokio::spawn(async move {
        short_root
            .wait(TaskId::from("child"), Duration::from_secs(2))
            .await
            .unwrap()
    });
    let long_root = root.clone();
    let long = tokio::spawn(async move {
        long_root
            .wait(TaskId::from("child"), Duration::from_secs(20))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(short.await.unwrap(), WaitOutcome::TimedOut(_)));
    assert!(!long.is_finished());
    harness.runner.finish("child").await;
    assert!(matches!(long.await.unwrap(), WaitOutcome::Finished(_)));
    let snapshot = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    let disposition = snapshot.completion_disposition.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
}

#[tokio::test(start_paused = true)]
async fn foreground_handoff_plus_live_waiter_uses_waiter_delivery_precedence() {
    let harness = Harness::new(CoordinatorConfig {
        foreground_budget: Duration::from_secs(3),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let foreground_root = root.clone();
    let foreground = tokio::spawn(async move {
        foreground_root
            .spawn_and_wait(request("child", SpawnMode::Foreground))
            .await
            .unwrap()
    });
    harness.wait_for_status("child", TaskStatus::Running).await;
    let waiter_root = root.clone();
    let waiter = tokio::spawn(async move {
        waiter_root
            .wait(TaskId::from("child"), Duration::from_secs(20))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(foreground.await.unwrap().backgrounded);
    harness.runner.finish("child").await;
    assert!(matches!(waiter.await.unwrap(), WaitOutcome::Finished(_)));
    let disposition = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap()
        .completion_disposition
        .unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.waiter_delivered);
    assert!(!disposition.foreground_delivered);
    assert!(!disposition.should_surface);
}

#[tokio::test]
async fn definition_background_overrides_foreground_and_waiter_admission_is_bounded() {
    let harness = Harness::new(CoordinatorConfig {
        max_waiters: 1,
        max_waiters_per_task: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let mut root_profile = AgentProfile::worker();
    root_profile.definition_background = true;
    let root = harness
        .handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: root_profile,
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let mut background = request("definition-background", SpawnMode::Foreground);
    background.profile.definition_background = true;
    assert!(root.spawn_and_wait(background).await.unwrap().backgrounded);

    root.spawn(request("wait-target", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("wait-target", TaskStatus::Running)
        .await;
    let first_root = root.clone();
    let first = tokio::spawn(async move {
        first_root
            .wait(TaskId::from("wait-target"), Duration::from_secs(60))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        root.wait(TaskId::from("wait-target"), Duration::from_secs(60))
            .await
            .is_err()
    );
    first.abort();
}

#[tokio::test(start_paused = true)]
async fn waiter_timeout_is_capped_and_reports_current_progress_without_stopping_work() {
    let harness = Harness::new(CoordinatorConfig {
        waiter_timeout_cap: Duration::from_secs(4),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;
    let waiter_root = root.clone();
    let waiter = tokio::spawn(async move {
        waiter_root
            .wait(TaskId::from("child"), Duration::from_secs(400))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    let WaitOutcome::TimedOut(snapshot) = waiter.await.unwrap() else {
        panic!("wait must time out at the configured cap");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Running);
    assert!(snapshot.elapsed_ms >= 4_000);
    assert_eq!(harness.runner.active_runs(), 1);
}

#[tokio::test]
async fn terminal_wait_is_immediate_and_scoped_queries_do_not_leak_foreign_tasks() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root_a = harness
        .register_root_scoped("root-a", "session-a", "turn-a")
        .await;
    let root_b = harness
        .register_root_scoped("root-b", "session-b", "turn-b")
        .await;
    root_a
        .spawn(request("child-a", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("child-a", TaskStatus::Running)
        .await;
    assert!(matches!(
        root_b
            .wait(TaskId::from("child-a"), Duration::from_secs(1))
            .await
            .unwrap(),
        WaitOutcome::NotFoundOrNotOwned
    ));
    assert!(root_b.inspect(TaskId::from("child-a")).await.is_err());
    assert!(
        root_b
            .list_running()
            .await
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.node.root_id == TaskId::from("root-b"))
    );
    harness.runner.finish("child-a").await;
    harness
        .wait_for_status("child-a", TaskStatus::Completed)
        .await;
    assert!(matches!(
        root_a
            .wait(TaskId::from("child-a"), Duration::from_secs(60))
            .await
            .unwrap(),
        WaitOutcome::Finished(_)
    ));
}

#[derive(Default)]
struct DataControl;

impl TaskChildControl for DataControl {
    fn progress(&self) -> TaskProgress {
        TaskProgress::default()
    }

    fn send_active_message(
        &self,
        _delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission> {
        Box::pin(ready(ActiveMessageAdmission::Rejected))
    }

    fn cancel(&self) {}
}

struct DataRunner {
    gates: Mutex<HashMap<TaskId, Arc<Notify>>>,
    block_load: bool,
    load_calls: AtomicUsize,
    load_gate: Notify,
    loaded_output: Option<String>,
    block_first_poll: bool,
}

impl Default for DataRunner {
    fn default() -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
            block_load: false,
            load_calls: AtomicUsize::new(0),
            load_gate: Notify::new(),
            loaded_output: None,
            block_first_poll: false,
        }
    }
}

impl DataRunner {
    async fn finish(&self, task_id: &str) {
        loop {
            if let Some(gate) = self.gates.lock().await.get(&TaskId::from(task_id)).cloned() {
                gate.notify_one();
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait::async_trait]
impl TaskRunner for DataRunner {
    type Control = DataControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let gate = Arc::new(Notify::new());
        self.gates
            .lock()
            .await
            .insert(request.node.id.clone(), gate.clone());
        if reporter
            .started(StartedTask::new(
                Arc::new(DataControl),
                request.cancellation.clone(),
            ))
            .await
        {
            reporter
                .report_progress(TaskProgress {
                    phase: Some("working".into()),
                    message: Some("halfway".into()),
                    completed_units: 5,
                    total_units: Some(10),
                })
                .await;
            reporter
                .report_usage(TaskUsage {
                    total_tokens: 42,
                    tool_calls: 3,
                    ..TaskUsage::default()
                })
                .await;
            gate.notified().await;
        }
        TaskRunOutput::from(TaskResult {
            success: true,
            output: "abcdefgh".into(),
            error: None,
            usage: TaskUsage {
                total_tokens: 42,
                tool_calls: 3,
                ..TaskUsage::default()
            },
            duration_ms: 7,
            output_ref: Some("memory://full".into()),
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    async fn load_persisted_output(&self, output_ref: &str) -> Result<Option<String>, TaskError> {
        assert_eq!(output_ref, "memory://full");
        self.load_calls.fetch_add(1, Ordering::AcqRel);
        if self.block_first_poll {
            std::thread::sleep(Duration::from_secs(2));
        }
        if self.block_load {
            self.load_gate.notified().await;
        }
        Ok(Some(
            self.loaded_output
                .clone()
                .unwrap_or_else(|| "abcdefgh".into()),
        ))
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}

#[tokio::test]
async fn persisted_output_loads_are_bounded_tracked_and_do_not_block_the_actor() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(DataRunner {
        block_load: true,
        ..DataRunner::default()
    });
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            max_output_loads: 1,
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == TaskStatus::Running)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == TaskStatus::Completed)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let first_handle = handle.clone();
    let first = tokio::spawn(async move {
        first_handle
            .inspect_detailed_admin(TaskId::from("child"))
            .await
    });
    while runner.load_calls.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    assert!(handle.registry_counts().await.is_ok());
    assert!(
        handle
            .inspect_detailed_admin(TaskId::from("child"))
            .await
            .is_err()
    );
    runner.load_gate.notify_one();
    assert_eq!(
        first
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .result
            .unwrap()
            .output,
        "abcdefgh"
    );
}

#[tokio::test]
async fn progress_usage_and_persisted_output_are_visible_without_retaining_large_inline_output() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(DataRunner::default());
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        runner.clone(),
        allocator,
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let mut child = request("child", SpawnMode::Background);
    child.result_contract.max_output_bytes = 3;
    root.spawn(child).await.unwrap();
    loop {
        let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
        if snapshot.progress.completed_units == 5 && snapshot.usage.total_tokens == 42 {
            assert_eq!(snapshot.progress.phase.as_deref(), Some("working"));
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    loop {
        let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
        if snapshot.node.status == TaskStatus::Completed {
            assert_eq!(snapshot.result.as_ref().unwrap().output, "abc");
            break;
        }
        tokio::task::yield_now().await;
    }
    let detailed = handle
        .inspect_detailed_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert_eq!(
        detailed.owner.session_id(),
        &lato_core::SessionId::from("session")
    );
    assert_eq!(detailed.snapshot.result.unwrap().output, "abcdefgh");
}

#[tokio::test]
async fn completed_retention_is_bounded_fifo_and_eviction_makes_the_oldest_unknown() {
    let harness = Harness::new(CoordinatorConfig {
        max_completed: 2,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    for id in ["one", "two", "three"] {
        root.spawn(request(id, SpawnMode::Background))
            .await
            .unwrap();
        harness.wait_for_status(id, TaskStatus::Running).await;
        harness.runner.finish(id).await;
        harness.wait_for_status(id, TaskStatus::Completed).await;
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness
            .handle
            .inspect_admin(TaskId::from("one"))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("two"))
            .await
            .is_ok()
    );
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("three"))
            .await
            .is_ok()
    );
    assert_eq!(harness.handle.registry_counts().await.unwrap().completed, 2);
}

#[tokio::test]
async fn retention_never_evicts_a_parent_while_a_descendant_depends_on_its_authority() {
    let harness = Harness::new(CoordinatorConfig {
        max_completed: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let parent = root
        .spawn(request("parent", SpawnMode::Background))
        .await
        .unwrap();
    harness.wait_for_status("parent", TaskStatus::Running).await;
    parent
        .handle
        .spawn(request("grandchild", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("grandchild", TaskStatus::Running)
        .await;
    harness.runner.finish("parent").await;
    harness
        .wait_for_status("parent", TaskStatus::Completed)
        .await;

    root.spawn(request("sibling", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("sibling", TaskStatus::Running)
        .await;
    harness.runner.finish("sibling").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness
            .handle
            .inspect_admin(TaskId::from("sibling"))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        root.inspect(TaskId::from("grandchild")).await.is_ok(),
        "root scope must retain the real ancestry chain"
    );

    harness.runner.finish("grandchild").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness.handle.registry_counts().await.unwrap().completed != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("parent"))
            .await
            .is_ok()
    );
    assert_eq!(harness.handle.registry_counts().await.unwrap().completed, 1);
}

#[tokio::test]
async fn queued_cancellation_during_promotion_uses_terminal_pipeline_and_resolves_waiters() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_secs(60),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("blocker", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("blocker", TaskStatus::Running)
        .await;
    let token = CancellationToken::new();
    let mut queued = request("queued", SpawnMode::Background);
    queued.cancellation = token.clone();
    root.spawn(queued).await.unwrap();
    token.cancel();
    let waiter_root = root.clone();
    let waiter = tokio::spawn(async move {
        waiter_root
            .wait(TaskId::from("queued"), Duration::from_secs(2))
            .await
            .unwrap()
    });
    harness.runner.finish("blocker").await;
    let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("queued terminal observer must resolve")
        .unwrap();
    let WaitOutcome::Finished(snapshot) = outcome else {
        panic!("cancelled queue entry must finish, not background or time out");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Cancelled);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        lato_core::TaskErrorCode::Cancelled
    );
    assert!(snapshot.completion_disposition.unwrap().waiter_delivered);
}

#[tokio::test(start_paused = true)]
async fn terminal_foreground_registration_respects_background_mode_and_elapsed_deadline() {
    let harness = Harness::new(CoordinatorConfig {
        foreground_budget: Duration::from_secs(3),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;

    let background = root
        .spawn(request("background", SpawnMode::Background))
        .await
        .unwrap();
    harness
        .wait_for_status("background", TaskStatus::Running)
        .await;
    harness.runner.finish("background").await;
    harness
        .wait_for_status("background", TaskStatus::Completed)
        .await;
    let disposition = background.handle.wait_foreground().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(!disposition.foreground_delivered);

    let foreground = root
        .spawn(request("foreground", SpawnMode::Foreground))
        .await
        .unwrap();
    harness
        .wait_for_status("foreground", TaskStatus::Running)
        .await;
    tokio::time::advance(Duration::from_secs(3)).await;
    harness.runner.finish("foreground").await;
    harness
        .wait_for_status("foreground", TaskStatus::Completed)
        .await;
    let disposition = foreground.handle.wait_foreground().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    assert!(!disposition.foreground_delivered);
}

#[tokio::test]
async fn terminal_wait_snapshot_truthfully_contains_its_successful_delivery() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;
    harness.runner.finish("child").await;
    harness
        .wait_for_status("child", TaskStatus::Completed)
        .await;
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("child"), Duration::from_secs(1))
        .await
        .unwrap()
    else {
        panic!("terminal task must answer immediately");
    };
    let disposition = snapshot.completion_disposition.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
}

#[tokio::test]
async fn blocking_first_output_poll_times_out_without_blocking_the_actor() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(DataRunner {
        block_first_poll: true,
        ..DataRunner::default()
    });
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            output_load_timeout: Duration::from_millis(20),
            teardown_drain_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == TaskStatus::Running)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == TaskStatus::Completed)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let loading_handle = handle.clone();
    let loading = tokio::spawn(async move {
        loading_handle
            .inspect_detailed_admin(TaskId::from("child"))
            .await
    });
    while runner.load_calls.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(Duration::from_millis(100), handle.registry_counts())
        .await
        .expect("blocking first poll must not stall the actor")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), loading)
            .await
            .expect("supervisor must enforce output deadline")
            .unwrap()
            .is_err()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(200), handle.shutdown())
            .await
            .expect("shutdown must remain bounded by the coordinator deadline")
            .unwrap(),
        lato_runtime::SinkShutdown::TimedOutDetached
    );
}

#[tokio::test]
async fn retained_and_loaded_outputs_are_utf8_safely_capped_with_metadata() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(DataRunner {
        loaded_output: Some("你好吗世界".into()),
        ..DataRunner::default()
    });
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            max_loaded_output_bytes: 7,
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let mut child = request("child", SpawnMode::Background);
    child.result_contract.max_output_bytes = 5;
    root.spawn(child).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == TaskStatus::Running)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    let retained = loop {
        let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
        if snapshot.node.status == TaskStatus::Completed {
            break snapshot;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(retained.result.unwrap().output, "abcde");
    assert_eq!(
        retained.output_metadata.unwrap(),
        lato_runtime::OutputMetadata {
            source_bytes: 8,
            retained_bytes: 5,
            truncated: true,
        }
    );
    let loaded = handle
        .inspect_detailed_admin(TaskId::from("child"))
        .await
        .unwrap()
        .snapshot;
    assert_eq!(loaded.result.unwrap().output, "你好");
    assert_eq!(
        loaded.output_metadata.unwrap(),
        lato_runtime::OutputMetadata {
            source_bytes: 15,
            retained_bytes: 6,
            truncated: true,
        }
    );
}

struct PollControl {
    completed: AtomicU64,
    calls: AtomicUsize,
    block_millis: u64,
}

impl TaskChildControl for PollControl {
    fn progress(&self) -> TaskProgress {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.block_millis > 0 {
            std::thread::sleep(Duration::from_millis(self.block_millis));
        }
        TaskProgress {
            phase: Some("polled".into()),
            message: None,
            completed_units: self.completed.load(Ordering::Acquire),
            total_units: Some(10),
        }
    }

    fn send_active_message(
        &self,
        _delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission> {
        Box::pin(ready(ActiveMessageAdmission::Rejected))
    }

    fn cancel(&self) {}
}

#[derive(Default)]
struct PollRunner {
    control: std::sync::OnceLock<Arc<PollControl>>,
    finish: Notify,
    block_progress_millis: u64,
}

#[async_trait::async_trait]
impl TaskRunner for PollRunner {
    type Control = PollControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let control = Arc::new(PollControl {
            completed: AtomicU64::new(0),
            calls: AtomicUsize::new(0),
            block_millis: self.block_progress_millis,
        });
        let _ = self.control.set(control.clone());
        if reporter
            .started(StartedTask::new(control, request.cancellation))
            .await
        {
            self.finish.notified().await;
        }
        TaskRunOutput::from(TaskResult {
            success: true,
            output: String::new(),
            error: None,
            usage: TaskUsage::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}

#[tokio::test(start_paused = true)]
async fn bounded_control_polling_commits_progress_through_the_event_sequence() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(PollRunner::default());
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let sink = Arc::new(lato_runtime::MemoryTaskEventSink::default());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            progress_poll_capacity: 1,
            progress_poll_interval: Duration::from_millis(10),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        sink.clone(),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    while runner.control.get().is_none() {
        tokio::task::yield_now().await;
    }
    runner
        .control
        .get()
        .unwrap()
        .completed
        .store(4, Ordering::Release);
    tokio::time::advance(Duration::from_millis(10)).await;
    let snapshot = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
            if snapshot.progress.completed_units == 4 {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(snapshot.progress.phase.as_deref(), Some("polled"));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if sink.events().iter().any(|event| {
                event.task_id == TaskId::from("child")
                    && matches!(
                        event.payload,
                        lato_runtime::TaskEventPayload::ProgressUpdated { ref progress }
                            if progress.completed_units == 4
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("progress event must reach the bounded sink dispatcher");
    runner.finish.notify_one();
}

#[tokio::test]
async fn arbitrary_blocking_progress_poll_never_blocks_the_actor() {
    let workspace = TempDir::new().unwrap();
    let runner = Arc::new(PollRunner {
        block_progress_millis: 500,
        ..PollRunner::default()
    });
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            progress_poll_capacity: 1,
            progress_poll_interval: Duration::from_millis(5),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(lato_runtime::TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("session"),
                turn_id: lato_core::TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child", SpawnMode::Background))
        .await
        .unwrap();
    let control = loop {
        if let Some(control) = runner.control.get() {
            break control;
        }
        tokio::task::yield_now().await;
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while control.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_millis(100), handle.registry_counts())
        .await
        .expect("synchronous control polling must run outside the actor")
        .unwrap();
    runner.finish.notify_one();
}
