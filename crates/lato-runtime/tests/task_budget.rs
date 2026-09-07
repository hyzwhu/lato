mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, SessionId, TaskError, TaskErrorCode, TaskId,
    TaskOwner, TaskScope, TaskStatus, TaskUsage, TurnId,
};
use lato_runtime::{
    CoordinatorConfig, SpawnMode, SpawnTaskRequest, TaskEventPayload, TaskRootRequest, WaitOutcome,
    spawn_task_coordinator,
};
use lato_workspace::{
    MemoryWorkspaceAllocator, WorkspaceAllocator, WorkspaceLease, WorkspaceRequest,
};
use std::{future::pending, sync::Arc, time::Duration};
use task_support::Harness;
use tokio_util::sync::CancellationToken;

fn budget(total_tokens: Option<u64>) -> BudgetLimits {
    BudgetLimits {
        total_tokens,
        ..BudgetLimits::unlimited()
    }
}

fn request(id: &str, total_tokens: Option<u64>) -> SpawnTaskRequest {
    SpawnTaskRequest {
        task_id: TaskId::from(id),
        scope: TaskScope {
            objective: format!("run {id}"),
            context_refs: Vec::new(),
        },
        profile: AgentProfile::worker(),
        requested_capabilities: None,
        budget: budget(total_tokens),
        result_contract: ResultContract {
            schema: None,
            max_output_bytes: 1024,
        },
        mode: SpawnMode::Background,
        cancellation: CancellationToken::new(),
    }
}

fn usage(total_tokens: u64) -> TaskUsage {
    TaskUsage {
        total_tokens,
        ..TaskUsage::default()
    }
}

async fn wait_until_settled(harness: &Harness, task_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if harness
                .handle
                .inspect_admin(TaskId::from(task_id))
                .await
                .is_ok_and(|snapshot| !snapshot.has_parent_reservation)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task reservation did not settle");
}

async fn wait_for_completion_callback(harness: &Harness) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while harness.runner.completion_callbacks() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completion callback was not delivered");
}

#[tokio::test]
async fn cumulative_usage_is_monotone_and_budget_exhaustion_settles_once() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(100)))
        .await;
    root.spawn(request("child", Some(80))).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    assert!(harness.runner.report_usage("child", usage(40)).await);
    assert!(harness.runner.report_usage("child", usage(60)).await);
    assert!(!harness.runner.report_usage("child", usage(50)).await);
    assert!(harness.runner.report_usage("child", usage(81)).await);

    let terminal = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap();
    let WaitOutcome::Finished(terminal) = terminal else {
        panic!("exhausted task must terminate");
    };
    assert_eq!(terminal.node.status, TaskStatus::Failed);
    assert_eq!(
        terminal.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );
    assert_eq!(terminal.usage.total_tokens, 80);
    wait_until_settled(&harness, "child").await;
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_spent
            .total_tokens,
        80
    );

    assert!(!harness.runner.report_usage("child", usage(90)).await);
    tokio::task::yield_now().await;
    let after_late = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert_eq!(after_late.usage.total_tokens, 80);
    assert_eq!(
        after_late.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );

    wait_for_completion_callback(&harness).await;
    let completions = harness.runner.completion_results();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].task_id, TaskId::from("child"));
    assert_eq!(completions[0].result.usage.total_tokens, 80);
    assert_eq!(
        completions[0].result.error.as_ref().unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );
}

#[tokio::test]
async fn runner_failure_invokes_one_truthful_completion_callback() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    root.spawn(request("failed-child", None)).await.unwrap();
    harness
        .wait_for_status("failed-child", TaskStatus::Running)
        .await;
    harness
        .runner
        .set_final_error(
            "failed-child",
            lato_core::TaskError::new(
                TaskErrorCode::RunnerProtocolViolation,
                "injected runner failure",
            ),
        )
        .await;
    harness.runner.finish("failed-child").await;
    let outcome = root
        .wait(TaskId::from("failed-child"), Duration::from_secs(2))
        .await
        .unwrap();
    let WaitOutcome::Finished(snapshot) = outcome else {
        panic!("runner failure must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    wait_for_completion_callback(&harness).await;
    let completions = harness.runner.completion_results();
    assert_eq!(completions.len(), 1);
    assert_eq!(
        completions[0].result.error.as_ref().unwrap().code,
        TaskErrorCode::RunnerProtocolViolation
    );
}

#[tokio::test]
async fn running_cancellation_invokes_one_truthful_completion_callback() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    root.spawn(request("cancelled-child", None)).await.unwrap();
    harness
        .wait_for_status("cancelled-child", TaskStatus::Running)
        .await;
    root.cancel_task(TaskId::from("cancelled-child"))
        .await
        .unwrap();
    let outcome = root
        .wait(TaskId::from("cancelled-child"), Duration::from_secs(2))
        .await
        .unwrap();
    let WaitOutcome::Finished(snapshot) = outcome else {
        panic!("cancelled runner must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Cancelled);
    wait_for_completion_callback(&harness).await;
    let completions = harness.runner.completion_results();
    assert_eq!(completions.len(), 1);
    assert_eq!(
        completions[0].result.error.as_ref().unwrap().code,
        TaskErrorCode::Cancelled
    );
}

#[tokio::test]
async fn nested_usage_rolls_up_after_each_exact_settlement() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(100)))
        .await;
    let child = root.spawn(request("child", Some(80))).await.unwrap().handle;
    harness.wait_for_status("child", TaskStatus::Running).await;
    let grandchild = child
        .spawn(request("grandchild", Some(30)))
        .await
        .unwrap()
        .handle;
    harness
        .wait_for_status("grandchild", TaskStatus::Running)
        .await;

    harness.runner.report_usage("grandchild", usage(20)).await;
    harness.runner.finish("grandchild").await;
    let _ = grandchild
        .wait(TaskId::from("grandchild"), Duration::from_secs(2))
        .await
        .unwrap();
    wait_until_settled(&harness, "grandchild").await;
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .budget_spent
            .total_tokens,
        20
    );

    harness.runner.report_usage("child", usage(10)).await;
    harness.runner.finish("child").await;
    let _ = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap();
    wait_until_settled(&harness, "child").await;
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_spent
            .total_tokens,
        30
    );
}

#[tokio::test]
async fn exhaustion_emits_one_budget_event_and_closes_descendant_admission() {
    let mut harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(50)))
        .await;
    let child = root.spawn(request("child", Some(20))).await.unwrap().handle;
    harness.wait_for_status("child", TaskStatus::Running).await;
    while harness.has_pending_event() {}

    harness.runner.report_usage("child", usage(21)).await;
    let _ = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap();
    let error = child.spawn(request("late", Some(1))).await.unwrap_err();
    assert!(matches!(
        error.code,
        TaskErrorCode::SpawnAdmissionClosed | TaskErrorCode::TerminalParent
    ));

    let mut budget_events = 0;
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(10), harness.next_event()).await
    {
        if matches!(event.payload, TaskEventPayload::BudgetExhausted { .. }) {
            budget_events += 1;
        }
    }
    assert_eq!(budget_events, 1);
}

#[tokio::test]
async fn reused_task_id_ignores_late_usage_from_an_older_generation() {
    let config = CoordinatorConfig {
        max_completed: 1,
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;

    root.spawn(request("reused", Some(20))).await.unwrap();
    harness.wait_for_status("reused", TaskStatus::Running).await;
    let stale = harness.runner.reporter("reused").await;
    stale.report_usage(usage(10)).await;
    harness.runner.finish("reused").await;
    let _ = root
        .wait(TaskId::from("reused"), Duration::from_secs(2))
        .await
        .unwrap();
    wait_until_settled(&harness, "reused").await;

    root.spawn(request("evictor", Some(1))).await.unwrap();
    harness
        .wait_for_status("evictor", TaskStatus::Running)
        .await;
    harness.runner.finish("evictor").await;
    let _ = root
        .wait(TaskId::from("evictor"), Duration::from_secs(2))
        .await
        .unwrap();
    wait_until_settled(&harness, "evictor").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while harness
            .handle
            .inspect_admin(TaskId::from("reused"))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    root.spawn(request("reused", Some(20))).await.unwrap();
    harness.wait_for_status("reused", TaskStatus::Running).await;
    assert!(!stale.report_usage(usage(19)).await);
    tokio::task::yield_now().await;
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("reused"))
            .await
            .unwrap()
            .usage
            .total_tokens,
        0
    );
}

#[tokio::test]
async fn final_result_usage_is_the_authoritative_budget_fence() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(100)))
        .await;
    root.spawn(request("child", Some(80))).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;
    harness.runner.set_final_usage("child", usage(81)).await;
    harness.runner.finish("child").await;

    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("final budget fence must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.usage.total_tokens, 80);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );
}

struct AwaitedFinalUsageRunner;

#[async_trait::async_trait]
impl lato_runtime::TaskRunner for AwaitedFinalUsageRunner {
    type Control = task_support::ControlledTaskControl;

    async fn run(
        &self,
        request: lato_runtime::TaskRunRequest,
        reporter: lato_runtime::TaskReporter<Self::Control>,
    ) -> lato_runtime::TaskRunOutput {
        assert!(
            reporter
                .started(lato_runtime::StartedTask::new(
                    Arc::new(task_support::ControlledTaskControl::new()),
                    request.cancellation,
                ))
                .await
        );
        assert!(reporter.report_usage(usage(81)).await);
        lato_runtime::TaskRunOutput::from(lato_core::TaskResult {
            success: true,
            output: "stale final usage".into(),
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

#[tokio::test]
async fn awaited_final_report_is_committed_before_the_runner_can_return() {
    let workspace = tempfile::tempdir().unwrap();
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(AwaitedFinalUsageRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: budget(Some(100)),
        })
        .await
        .unwrap();
    root.spawn(request("reported-final", Some(80)))
        .await
        .unwrap();
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("reported-final"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("reported final usage must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.usage.total_tokens, 80);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );
    wait_until_settled_handle(&handle, "reported-final").await;
    let root_snapshot = handle.inspect_admin(TaskId::from("root")).await.unwrap();
    assert_eq!(root_snapshot.budget_spent.total_tokens, 80);
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

async fn wait_until_settled_handle(handle: &lato_runtime::TaskHandle, task_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from(task_id))
            .await
            .is_ok_and(|snapshot| snapshot.has_parent_reservation)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task reservation did not settle");
}

#[tokio::test]
async fn queued_final_report_cannot_lose_to_runner_return_at_the_hard_limit() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(100)))
        .await;
    root.spawn(request("race", Some(80))).await.unwrap();
    harness.wait_for_status("race", TaskStatus::Running).await;
    let reporter = harness.runner.reporter("race").await;
    reporter.report_usage(usage(81)).await;
    harness.runner.set_final_usage("race", usage(50)).await;
    harness.runner.finish("race").await;

    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("race"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("usage/return race must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.usage.total_tokens, 80);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededTotalTokens
    );
}

#[tokio::test]
async fn regressed_final_usage_keeps_the_highest_processed_cumulative_value() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", budget(Some(100)))
        .await;
    root.spawn(request("child", Some(80))).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;
    harness.runner.report_usage("child", usage(60)).await;
    harness.runner.set_final_usage("child", usage(50)).await;
    harness.runner.finish("child").await;

    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("completed task must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Completed);
    assert_eq!(snapshot.usage.total_tokens, 60);
    assert_eq!(snapshot.result.unwrap().usage.total_tokens, 60);
}

#[tokio::test(start_paused = true)]
async fn wall_time_budget_starts_at_enqueue_and_caps_before_failure() {
    let config = CoordinatorConfig {
        queued_reap_interval: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    let mut child = request("child", None);
    child.budget.wall_time_ms = Some(100);
    root.spawn(child).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    tokio::time::advance(Duration::from_millis(101)).await;
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("wall-time exhaustion must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 100);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
}

#[tokio::test(start_paused = true)]
async fn wall_time_exhaustion_wins_a_simultaneous_successful_return() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    let mut child = request("race", None);
    child.budget.wall_time_ms = Some(50);
    root.spawn(child).await.unwrap();
    harness.wait_for_status("race", TaskStatus::Running).await;

    tokio::time::advance(Duration::from_millis(51)).await;
    harness.runner.finish("race").await;
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("race"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("completion race must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 50);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
}

#[tokio::test(start_paused = true)]
async fn queued_time_is_charged_to_the_wall_time_budget() {
    let config = CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    root.spawn(request("blocker", None)).await.unwrap();
    harness
        .wait_for_status("blocker", TaskStatus::Running)
        .await;
    let mut queued = request("queued", None);
    queued.budget.wall_time_ms = Some(50);
    assert!(root.spawn(queued).await.unwrap().is_queued());

    tokio::time::advance(Duration::from_millis(51)).await;
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("queued"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("queued wall-time exhaustion must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 50);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
}

struct PendingWorkspaceAllocator;

#[async_trait::async_trait]
impl WorkspaceAllocator for PendingWorkspaceAllocator {
    async fn allocate(&self, _request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        pending().await
    }

    async fn release(&self, _lease: &WorkspaceLease) -> Result<(), TaskError> {
        unreachable!("pending allocation never creates a lease")
    }
}

#[tokio::test(start_paused = true)]
async fn preparing_time_is_charged_to_the_wall_time_budget() {
    let runner = Arc::new(task_support::GatedTaskRunner::new(false));
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(10),
            ..CoordinatorConfig::default()
        },
        runner,
        Arc::new(PendingWorkspaceAllocator),
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let mut child = request("preparing", None);
    child.budget.wall_time_ms = Some(50);
    root.spawn(child).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("preparing"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != TaskStatus::Preparing)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::advance(Duration::from_millis(51)).await;
    tokio::time::advance(Duration::from_millis(11)).await;
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("preparing"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("preparing task must exhaust its wall-time budget");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 50);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn runner_generation_overflow_fails_closed_without_panicking_the_actor() {
    let config = CoordinatorConfig {
        initial_runner_generation: u64::MAX,
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped_with_budget("root", BudgetLimits::unlimited())
        .await;
    root.spawn(request("child", None)).await.unwrap();
    let WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("child"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("generation exhaustion must terminate");
    };
    assert_eq!(snapshot.node.status, TaskStatus::Failed);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::RunnerInitialization
    );
    assert_eq!(harness.handle.registry_counts().await.unwrap().roots, 1);
}
