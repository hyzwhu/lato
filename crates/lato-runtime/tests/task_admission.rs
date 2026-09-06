mod task_support;

use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, ResultContract, TaskError, TaskErrorCode, TaskId,
    TaskScope, ToolCapability,
};
use lato_runtime::{
    CoordinatorConfig, LimitBehavior, MemoryTaskEventSink, SinkShutdown, SpawnMode,
    SpawnTaskRequest, StartedTask, TaskEventEnvelope, TaskEventPayload, TaskEventSink,
    TaskReporter, TaskRootRequest, TaskRunOutput, TaskRunRequest, TaskRunner,
    spawn_task_coordinator,
};
use lato_workspace::{
    MemoryWorkspaceAllocator, WorkspaceAllocator, WorkspaceLease, WorkspaceRequest,
};
use std::{
    future::pending,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use task_support::{ControlledTaskControl, GatedTaskRunner, Harness};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn request(id: &str) -> SpawnTaskRequest {
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
            max_output_bytes: 1024,
        },
        mode: SpawnMode::Background,
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn task_starts_immediately_and_is_promoted_after_acknowledgement() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let disposition = root.spawn(request("child")).await.unwrap();
    assert!(disposition.is_started());
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    assert_eq!(
        harness.runner.started_ids().await,
        vec![TaskId::from("child")]
    );
    assert_eq!(harness.allocator.live_count().await, 1);
}

#[tokio::test]
async fn duplicate_depth_child_total_and_queue_bounds_are_enforced() {
    let config = CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        max_queue: 1,
        max_depth: 1,
        max_children_per_parent: 2,
        max_total_tasks: 4,
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("one")).await.unwrap();

    assert_eq!(
        root.spawn(request("one")).await.unwrap_err().code,
        TaskErrorCode::DuplicateTask
    );
    let queued = root.spawn(request("two")).await.unwrap();
    assert!(queued.is_queued());
    assert_eq!(
        root.spawn(request("three")).await.unwrap_err().code,
        TaskErrorCode::ChildLimit
    );
    assert_eq!(
        queued
            .handle
            .spawn(request("grandchild"))
            .await
            .unwrap_err()
            .code,
        TaskErrorCode::DepthLimit
    );
}

#[tokio::test]
async fn queue_capacity_and_reject_policy_report_stable_errors() {
    let queue_harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        max_queue: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = queue_harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("one")).await.unwrap();
    assert!(root.spawn(request("two")).await.unwrap().is_queued());
    assert_eq!(
        root.spawn(request("three")).await.unwrap_err().code,
        TaskErrorCode::QueueFull
    );
    assert_eq!(
        queue_harness
            .handle
            .inspect_admin(TaskId::from("three"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Failed
    );

    let reject_harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        admission_behavior: LimitBehavior::Reject,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = reject_harness
        .register_root_scoped("other", "session", "turn")
        .await;
    root.spawn(request("one")).await.unwrap();
    assert_eq!(
        root.spawn(request("two")).await.unwrap_err().code,
        TaskErrorCode::ConcurrencyLimit
    );
    assert_eq!(
        reject_harness
            .handle
            .inspect_admin(TaskId::from("two"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Failed
    );
}

#[tokio::test]
async fn saturated_root_does_not_block_another_root() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 2,
        max_running_per_root: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let root_a = harness
        .register_root_scoped("root-a", "session-a", "turn-a")
        .await;
    let root_b = harness
        .register_root_scoped("root-b", "session-b", "turn-b")
        .await;
    root_a.spawn(request("a1")).await.unwrap();
    assert!(root_a.spawn(request("a2")).await.unwrap().is_queued());
    assert!(root_b.spawn(request("b1")).await.unwrap().is_started());
    harness
        .wait_for_status("b1", lato_core::TaskStatus::Running)
        .await;
}

#[tokio::test]
async fn cancelling_a_queued_token_prevents_runner_start() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_millis(5),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("blocker")).await.unwrap();
    let queued_token = CancellationToken::new();
    let mut queued_request = request("queued");
    queued_request.cancellation = queued_token.clone();
    assert!(root.spawn(queued_request).await.unwrap().is_queued());
    queued_token.cancel();
    harness
        .wait_for_status("queued", lato_core::TaskStatus::Cancelled)
        .await;
    assert!(
        !harness
            .runner
            .started_ids()
            .await
            .contains(&TaskId::from("queued"))
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved
            .child_tasks,
        1
    );
}

#[tokio::test]
async fn cancellation_during_preparation_rejects_promotion_and_releases_lease() {
    let harness = Harness::new_paused(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let token = CancellationToken::new();
    let mut child = request("child");
    child.cancellation = token.clone();
    root.spawn(child).await.unwrap();
    harness.runner.wait_until_entered("child").await;
    token.cancel();
    harness.runner.allow_start("child").await;
    harness
        .wait_for_status("child", lato_core::TaskStatus::Cancelled)
        .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness.allocator.live_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn total_task_bound_is_independent_of_child_and_queue_bounds() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 4,
        max_running_per_root: 4,
        max_children_per_parent: 10,
        max_total_tasks: 2,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("one")).await.unwrap();
    assert_eq!(
        root.spawn(request("two")).await.unwrap_err().code,
        TaskErrorCode::RetentionLimit
    );
}

#[tokio::test]
async fn compound_spawn_errors_follow_the_specified_precedence() {
    let terminal_runner = Arc::new(GatedTaskRunner::new(false));
    let terminal_harness = Harness::with_runner(
        CoordinatorConfig {
            max_total_tasks: 2,
            ..CoordinatorConfig::default()
        },
        terminal_runner.clone(),
    )
    .await;
    let terminal_root = terminal_harness
        .register_root_scoped("terminal-root", "s", "t")
        .await;
    let terminal_child = terminal_root
        .spawn(request("terminal-child"))
        .await
        .unwrap();
    terminal_harness
        .wait_for_status("terminal-child", lato_core::TaskStatus::Running)
        .await;
    terminal_runner.finish("terminal-child").await;
    terminal_harness
        .wait_for_status("terminal-child", lato_core::TaskStatus::Completed)
        .await;
    assert_eq!(
        terminal_child
            .handle
            .spawn(request("terminal-root"))
            .await
            .unwrap_err()
            .code,
        TaskErrorCode::TerminalParent,
        "terminal parent must beat duplicate and retention errors"
    );

    assert_eq!(
        terminal_root
            .spawn(request("terminal-child"))
            .await
            .unwrap_err()
            .code,
        TaskErrorCode::DuplicateTask,
        "duplicate must beat retention exhaustion"
    );

    let reject_harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        admission_behavior: LimitBehavior::Reject,
        ..CoordinatorConfig::default()
    })
    .await;
    let reject_root = reject_harness
        .register_root_scoped("reject-root", "s", "t")
        .await;
    reject_root.spawn(request("blocker")).await.unwrap();
    reject_harness
        .wait_for_status("blocker", lato_core::TaskStatus::Running)
        .await;
    let mut expanded = request("expanded");
    expanded.requested_capabilities = Some(vec![ToolCapability::NetworkWrite]);
    assert_eq!(
        reject_root.spawn(expanded).await.unwrap_err().code,
        TaskErrorCode::ConcurrencyLimit,
        "admission rejection must precede capability narrowing"
    );
    assert!(!reject_harness.runner.validation_entered());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_every_external_handle_performs_bounded_shutdown_and_cleanup() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let runner = Arc::new(GatedTaskRunner::new(false));
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(10),
            queued_reap_interval: Duration::from_millis(1),
            teardown_drain_timeout: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator.clone(),
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let child = root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(allocator.live_count().await, 1);
    drop(child);
    drop(root);
    drop(handle);

    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("last external handle drop must close and drain the coordinator")
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn queue_promotes_in_fifo_order_when_capacity_returns() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_millis(5),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let blocker_token = CancellationToken::new();
    let mut blocker = request("blocker");
    blocker.cancellation = blocker_token.clone();
    root.spawn(blocker).await.unwrap();
    root.spawn(request("second")).await.unwrap();
    root.spawn(request("third")).await.unwrap();

    blocker_token.cancel();
    harness
        .wait_for_status("second", lato_core::TaskStatus::Running)
        .await;
    assert_eq!(
        harness.runner.started_ids().await,
        vec![TaskId::from("blocker"), TaskId::from("second")]
    );
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("third"))
            .await
            .unwrap()
            .node
            .status
            .is_queued()
    );
}

#[tokio::test]
async fn workspace_allocation_failure_never_starts_runner_and_releases_reservation() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    harness.allocator.fail_next_allocation();
    root.spawn(request("child")).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Failed)
        .await;
    assert!(harness.runner.started_ids().await.is_empty());
    assert_eq!(harness.allocator.live_count().await, 0);
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved,
        BudgetAmount::ZERO
    );
}

#[derive(Clone)]
struct PanickingWorkspaceAllocator;

#[async_trait::async_trait]
impl WorkspaceAllocator for PanickingWorkspaceAllocator {
    async fn allocate(&self, _request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        panic!("injected workspace allocation panic")
    }

    async fn release(&self, _lease: &WorkspaceLease) -> Result<(), TaskError> {
        unreachable!("a panicking allocation cannot return a lease")
    }
}

#[tokio::test]
async fn workspace_allocator_panic_is_a_structured_failure_and_actor_survives() {
    let runner = Arc::new(GatedTaskRunner::new(false));
    let sink = Arc::new(MemoryTaskEventSink::default());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        runner.clone(),
        Arc::new(PanickingWorkspaceAllocator),
        sink.clone(),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
            if snapshot.node.status == lato_core::TaskStatus::Failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.registry_counts().await.unwrap().completed, 1);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if sink.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    TaskEventPayload::Failed { error }
                        if error.code == TaskErrorCode::WorkspaceAllocation
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved,
        BudgetAmount::ZERO
    );
    assert!(runner.started_ids().await.is_empty());
}

#[tokio::test]
async fn queued_head_with_saturated_root_does_not_block_startable_tail() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 2,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_millis(2),
        ..CoordinatorConfig::default()
    })
    .await;
    let root_a = harness.register_root_scoped("root-a", "a", "a").await;
    let root_b = harness.register_root_scoped("root-b", "b", "b").await;
    let root_c = harness.register_root_scoped("root-c", "c", "c").await;
    root_a.spawn(request("a1")).await.unwrap();
    let b_token = CancellationToken::new();
    let mut b1 = request("b1");
    b1.cancellation = b_token.clone();
    root_b.spawn(b1).await.unwrap();
    assert!(root_a.spawn(request("a2")).await.unwrap().is_queued());
    assert!(root_c.spawn(request("c1")).await.unwrap().is_queued());

    b_token.cancel();
    harness
        .wait_for_status("c1", lato_core::TaskStatus::Running)
        .await;
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("a2"))
            .await
            .unwrap()
            .node
            .status
            .is_queued()
    );
}

#[tokio::test]
async fn cancelled_queue_head_does_not_consume_capacity_before_live_tail() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        queued_reap_interval: Duration::from_millis(2),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let blocker_token = CancellationToken::new();
    let mut blocker = request("blocker");
    blocker.cancellation = blocker_token.clone();
    root.spawn(blocker).await.unwrap();
    let cancelled_token = CancellationToken::new();
    let mut cancelled = request("cancelled-head");
    cancelled.cancellation = cancelled_token.clone();
    root.spawn(cancelled).await.unwrap();
    root.spawn(request("live-tail")).await.unwrap();

    cancelled_token.cancel();
    blocker_token.cancel();
    harness
        .wait_for_status("live-tail", lato_core::TaskStatus::Running)
        .await;
    assert!(
        !harness
            .runner
            .started_ids()
            .await
            .contains(&TaskId::from("cancelled-head"))
    );
}

#[tokio::test]
async fn child_budget_is_derived_from_parent_remaining_and_fixed_costs() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget(
            "root",
            BudgetLimits {
                total_tokens: Some(100),
                child_tasks: Some(1),
                worktrees: Some(1),
                ..BudgetLimits::unlimited()
            },
        )
        .await;
    root.spawn(request("child")).await.unwrap();
    let child = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert_eq!(child.budget_limits.total_tokens, Some(100));
    assert_eq!(child.budget_limits.child_tasks, Some(0));
    assert_eq!(child.budget_limits.worktrees, Some(0));
    let parent = harness
        .handle
        .inspect_admin(TaskId::from("root"))
        .await
        .unwrap();
    assert_eq!(parent.budget_reserved.total_tokens, 100);
    assert_eq!(parent.budget_reserved.child_tasks, 1);
    assert_eq!(parent.budget_reserved.worktrees, 1);
}

#[tokio::test]
async fn budget_amplification_and_fixed_cost_overflow_are_atomic() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped_with_budget(
            "root",
            BudgetLimits {
                total_tokens: Some(100),
                ..BudgetLimits::unlimited()
            },
        )
        .await;
    let mut amplified = request("amplified");
    amplified.budget.total_tokens = Some(101);
    assert_eq!(
        root.spawn(amplified).await.unwrap_err().code,
        TaskErrorCode::BudgetReservation
    );
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("amplified"))
            .await
            .is_err()
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved,
        BudgetAmount::ZERO
    );

    let mut overflow = request("overflow");
    overflow.budget.child_tasks = Some(u64::MAX);
    assert_eq!(
        root.spawn(overflow).await.unwrap_err().code,
        TaskErrorCode::BudgetReservation
    );
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("overflow"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn capability_expansion_is_rejected_after_profile_validation_but_before_visibility() {
    let runner = Arc::new(GatedTaskRunner::new(false));
    let harness = Harness::with_runner(CoordinatorConfig::default(), runner.clone()).await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let mut child = request("child");
    child.requested_capabilities = Some(vec![ToolCapability::NetworkWrite]);
    assert_eq!(
        root.spawn(child).await.unwrap_err().code,
        TaskErrorCode::CapabilityExpansion
    );
    assert_eq!(runner.validation_calls(), 1);
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("child"))
            .await
            .is_err()
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("root"))
            .await
            .unwrap()
            .budget_reserved,
        BudgetAmount::ZERO
    );
}

#[tokio::test]
async fn profile_validation_does_not_block_commands_and_shutdown_aborts_it() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, false, false, true));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            profile_validation_timeout: Duration::from_secs(30),
            teardown_drain_timeout: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let spawn = tokio::spawn(async move { root.spawn(request("child")).await });
    runner.wait_until_validation_entered().await;
    tokio::time::timeout(Duration::from_millis(50), harness.handle.registry_counts())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), harness.handle.shutdown())
        .await
        .unwrap()
        .unwrap();
    runner.allow_validation();
    assert_eq!(
        spawn.await.unwrap().unwrap_err().code,
        TaskErrorCode::CoordinatorClosed
    );
}

#[tokio::test]
async fn cooperative_profile_validator_is_bounded_by_configured_timeout() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, false, false, true));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            profile_validation_timeout: Duration::from_millis(25),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let spawn = tokio::spawn(async move { root.spawn(request("child")).await });
    runner.wait_until_validation_entered().await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), spawn)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        TaskErrorCode::InvalidProfile
    );
}

#[tokio::test]
async fn profile_validation_error_and_panic_are_structured_and_atomic() {
    for (runner, expected) in [
        (
            Arc::new(GatedTaskRunner::failing_validator()),
            TaskErrorCode::InvalidProfile,
        ),
        (
            Arc::new(GatedTaskRunner::panicking_validator()),
            TaskErrorCode::RunnerPanic,
        ),
    ] {
        let harness = Harness::with_runner(CoordinatorConfig::default(), runner).await;
        let root = harness.register_root_scoped("root", "s", "t").await;
        assert_eq!(
            root.spawn(request("child")).await.unwrap_err().code,
            expected
        );
        assert!(
            harness
                .handle
                .inspect_admin(TaskId::from("child"))
                .await
                .is_err()
        );
        assert_eq!(
            harness
                .handle
                .inspect_admin(TaskId::from("root"))
                .await
                .unwrap()
                .budget_reserved,
            BudgetAmount::ZERO
        );
    }
}

#[tokio::test]
async fn profile_validation_failure_precedes_later_capability_narrowing_failure() {
    let runner = Arc::new(GatedTaskRunner::failing_validator());
    let harness = Harness::with_runner(CoordinatorConfig::default(), runner.clone()).await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let mut child = request("child");
    child.requested_capabilities = Some(vec![ToolCapability::NetworkWrite]);
    assert_eq!(
        root.spawn(child).await.unwrap_err().code,
        TaskErrorCode::InvalidProfile
    );
    assert_eq!(runner.validation_calls(), 1);
}

#[tokio::test]
async fn terminal_parent_revalidation_precedes_async_profile_failure() {
    let runner = Arc::new(GatedTaskRunner::failing_paused_validator());
    let harness = Harness::with_runner(CoordinatorConfig::default(), runner.clone()).await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let spawning_root = root.clone();
    let spawn = tokio::spawn(async move { spawning_root.spawn(request("child")).await });
    runner.wait_until_validation_entered().await;
    harness
        .handle
        .shutdown_root(TaskId::from("root"))
        .await
        .unwrap();
    runner.allow_validation();
    assert_eq!(
        spawn.await.unwrap().unwrap_err().code,
        TaskErrorCode::TerminalParent
    );
}

struct AdmissionRaceRunner {
    first_entered: AtomicBool,
    second_entered: AtomicBool,
    first_gate: Notify,
    second_gate: Notify,
}

impl AdmissionRaceRunner {
    fn new() -> Self {
        Self {
            first_entered: AtomicBool::new(false),
            second_entered: AtomicBool::new(false),
            first_gate: Notify::new(),
            second_gate: Notify::new(),
        }
    }

    async fn wait_entered(&self, first: bool) {
        let entered = if first {
            &self.first_entered
        } else {
            &self.second_entered
        };
        while !entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait::async_trait]
impl TaskRunner for AdmissionRaceRunner {
    type Control = ControlledTaskControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        if reporter
            .started(StartedTask::new(
                Arc::new(ControlledTaskControl::new()),
                request.cancellation.clone(),
            ))
            .await
        {
            request.cancellation.cancelled().await;
        }
        TaskRunOutput::from(lato_core::TaskResult {
            success: true,
            output: String::new(),
            error: None,
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError> {
        if profile.name == "first" {
            self.first_entered.store(true, Ordering::Release);
            self.first_gate.notified().await;
            Ok(())
        } else {
            self.second_entered.store(true, Ordering::Release);
            self.second_gate.notified().await;
            Err(TaskError::new(
                TaskErrorCode::InvalidProfile,
                "injected second validation failure",
            ))
        }
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}

#[tokio::test]
async fn newly_saturated_reject_admission_precedes_async_profile_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let runner = Arc::new(AdmissionRaceRunner::new());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            max_global_running: 1,
            max_running_per_root: 1,
            admission_behavior: LimitBehavior::Reject,
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let mut first = request("first");
    first.profile.name = "first".into();
    let first_root = root.clone();
    let first_spawn = tokio::spawn(async move { first_root.spawn(first).await });
    runner.wait_entered(true).await;

    let mut second = request("second");
    second.profile.name = "second".into();
    let second_root = root.clone();
    let second_spawn = tokio::spawn(async move { second_root.spawn(second).await });
    runner.wait_entered(false).await;

    runner.first_gate.notify_one();
    first_spawn.await.unwrap().unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("first"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.second_gate.notify_one();
    assert_eq!(
        second_spawn.await.unwrap().unwrap_err().code,
        TaskErrorCode::ConcurrencyLimit
    );
}

#[tokio::test]
async fn shutdown_aborts_noncooperative_runner_only_after_bounded_grace_and_joins_it() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(10),
            teardown_drain_timeout: Duration::from_millis(100),
            queued_reap_interval: Duration::from_millis(2),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("child")).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    assert_eq!(runner.active_runs(), 1);
    tokio::time::timeout(Duration::from_secs(1), harness.handle.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
    assert_eq!(harness.allocator.live_count().await, 0);
}

#[tokio::test]
async fn shutdown_cancels_preparing_and_queued_tasks_without_starting_them() {
    let runner = Arc::new(GatedTaskRunner::with_options(true, false, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 1,
            max_running_per_root: 1,
            cancel_grace: Duration::from_millis(5),
            queued_reap_interval: Duration::from_millis(2),
            teardown_drain_timeout: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("preparing")).await.unwrap();
    root.spawn(request("queued")).await.unwrap();
    runner.wait_until_entered("preparing").await;
    harness.handle.shutdown().await.unwrap();
    assert!(runner.started_ids().await.is_empty());
    assert_eq!(runner.active_runs(), 0);
    assert_eq!(harness.allocator.live_count().await, 0);
}

#[tokio::test]
async fn running_cancellation_retains_slot_lease_and_reservation_until_confirmed_exit() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 1,
            max_running_per_root: 1,
            cancel_grace: Duration::from_millis(80),
            queued_reap_interval: Duration::from_millis(2),
            ..CoordinatorConfig::default()
        },
        runner,
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let token = CancellationToken::new();
    let mut child = request("child");
    child.cancellation = token.clone();
    root.spawn(child).await.unwrap();
    root.spawn(request("tail")).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    token.cancel();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let child = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert_eq!(child.node.status, lato_core::TaskStatus::Running);
    assert!(child.workspace_lease.is_some());
    assert!(child.has_parent_reservation);
    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from("tail"))
            .await
            .unwrap()
            .node
            .status
            .is_queued()
    );
    harness
        .wait_for_status("tail", lato_core::TaskStatus::Running)
        .await;
}

#[tokio::test]
async fn panicking_completion_callback_cannot_block_cleanup_or_queue_promotion() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, false, true, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 1,
            max_running_per_root: 1,
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("first")).await.unwrap();
    root.spawn(request("second")).await.unwrap();
    harness
        .wait_for_status("first", lato_core::TaskStatus::Running)
        .await;
    runner.finish("first").await;
    harness
        .wait_for_status("second", lato_core::TaskStatus::Running)
        .await;
    assert_eq!(harness.allocator.live_count().await, 1);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if harness
                .handle
                .registry_counts()
                .await
                .unwrap()
                .dropped_callback_work
                >= 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_cancel_callback_never_stalls_actor_or_cancel_grace() {
    let runner = Arc::new(GatedTaskRunner::blocking_cancel());
    let harness = Harness::with_runner(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(10),
            queued_reap_interval: Duration::from_millis(1),
            teardown_drain_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    let cancellation = CancellationToken::new();
    let mut child = request("child");
    child.cancellation = cancellation.clone();
    root.spawn(child).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    cancellation.cancel();
    runner.wait_until_cancel_callback_entered().await;
    tokio::time::timeout(Duration::from_millis(20), harness.handle.registry_counts())
        .await
        .expect("blocking cancel callback must not freeze actor")
        .unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Cancelled)
        .await;
    assert_eq!(
        harness.handle.shutdown().await.unwrap(),
        SinkShutdown::TimedOutDetached
    );
    runner.release_cancel_callback();
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_completion_callback_never_stalls_cleanup_or_promotion() {
    let runner = Arc::new(GatedTaskRunner::blocking_completion());
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 1,
            max_running_per_root: 1,
            teardown_drain_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("first")).await.unwrap();
    root.spawn(request("second")).await.unwrap();
    harness
        .wait_for_status("first", lato_core::TaskStatus::Running)
        .await;
    runner.finish("first").await;
    runner.wait_until_completion_callback_entered().await;
    harness
        .wait_for_status("second", lato_core::TaskStatus::Running)
        .await;
    assert_eq!(
        harness.handle.shutdown().await.unwrap(),
        SinkShutdown::TimedOutDetached
    );
    runner.release_completion_callback();
}

#[tokio::test(flavor = "current_thread")]
async fn callback_dispatch_saturation_is_bounded_and_observable() {
    let runner = Arc::new(GatedTaskRunner::blocking_completion());
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 3,
            max_running_per_root: 3,
            callback_capacity: 1,
            teardown_drain_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    for id in ["one", "two", "three"] {
        root.spawn(request(id)).await.unwrap();
        harness
            .wait_for_status(id, lato_core::TaskStatus::Running)
            .await;
    }
    runner.finish("one").await;
    runner.wait_until_completion_callback_entered().await;
    runner.finish("two").await;
    runner.finish("three").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if harness
                .handle
                .registry_counts()
                .await
                .unwrap()
                .dropped_callback_work
                >= 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness.handle.shutdown().await.unwrap(),
        SinkShutdown::TimedOutDetached
    );
    runner.release_completion_callback();
}

#[tokio::test]
async fn completion_callback_observes_workspace_and_reservation_cleanup_first() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("child")).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    harness.runner.finish("child").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness.runner.completion_callbacks() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let child = harness
        .handle
        .inspect_admin(TaskId::from("child"))
        .await
        .unwrap();
    assert!(child.workspace_lease.is_none());
    assert!(!child.has_parent_reservation);
    assert_eq!(harness.allocator.live_count().await, 0);
}

#[tokio::test]
async fn normal_shutdown_drains_accepted_completion_callbacks() {
    let runner = Arc::new(GatedTaskRunner::new(false));
    let harness = Harness::with_runner(CoordinatorConfig::default(), runner.clone()).await;
    let root = harness.register_root_scoped("root", "s", "t").await;
    root.spawn(request("child")).await.unwrap();
    harness
        .wait_for_status("child", lato_core::TaskStatus::Running)
        .await;
    runner.finish("child").await;
    harness
        .wait_for_status("child", lato_core::TaskStatus::Completed)
        .await;

    assert_eq!(
        harness.handle.shutdown().await.unwrap(),
        SinkShutdown::Drained
    );
    assert_eq!(runner.completion_callbacks(), 1);
}

#[derive(Clone, Default)]
struct CancellationSafeGatedAllocator {
    entered: Arc<AtomicBool>,
    entered_notify: Arc<Notify>,
    live_external_allocations: Arc<AtomicUsize>,
}

impl CancellationSafeGatedAllocator {
    async fn wait_until_entered(&self) {
        while !self.entered.load(Ordering::Acquire) {
            self.entered_notify.notified().await;
        }
    }

    fn live_external_allocations(&self) -> usize {
        self.live_external_allocations.load(Ordering::Acquire)
    }
}

struct PendingAllocationGuard(Arc<AtomicUsize>);

impl Drop for PendingAllocationGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl WorkspaceAllocator for CancellationSafeGatedAllocator {
    async fn allocate(&self, _request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        self.live_external_allocations
            .fetch_add(1, Ordering::AcqRel);
        let guard = PendingAllocationGuard(self.live_external_allocations.clone());
        self.entered.store(true, Ordering::Release);
        self.entered_notify.notify_waiters();
        let result = pending::<Result<WorkspaceLease, TaskError>>().await;
        drop(guard);
        result
    }

    async fn release(&self, _lease: &WorkspaceLease) -> Result<(), TaskError> {
        unreachable!("the gated allocation never returns a lease")
    }
}

#[tokio::test]
async fn cancelling_mid_allocation_drops_transaction_and_leaks_no_external_resource() {
    let allocator = Arc::new(CancellationSafeGatedAllocator::default());
    let runner = Arc::new(GatedTaskRunner::new(false));
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(10),
            queued_reap_interval: Duration::from_millis(1),
            ..CoordinatorConfig::default()
        },
        runner,
        allocator.clone(),
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let mut spawn = request("child");
    spawn.cancellation = cancellation.clone();
    root.spawn(spawn).await.unwrap();
    allocator.wait_until_entered().await;
    assert_eq!(allocator.live_external_allocations(), 1);

    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
            if snapshot.node.status == lato_core::TaskStatus::Cancelled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(allocator.live_external_allocations(), 0);
}

#[derive(Clone)]
struct CancellationSafeGatedReleaseAllocator {
    inner: MemoryWorkspaceAllocator,
    entered: Arc<AtomicBool>,
    entered_notify: Arc<Notify>,
    partial_release: Arc<AtomicUsize>,
}

impl CancellationSafeGatedReleaseAllocator {
    async fn wait_until_release_entered(&self) {
        while !self.entered.load(Ordering::Acquire) {
            self.entered_notify.notified().await;
        }
    }
}

struct PartialReleaseGuard(Arc<AtomicUsize>);

impl Drop for PartialReleaseGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl WorkspaceAllocator for CancellationSafeGatedReleaseAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        self.inner.allocate(request).await
    }

    async fn release(&self, _lease: &WorkspaceLease) -> Result<(), TaskError> {
        self.partial_release.fetch_add(1, Ordering::AcqRel);
        let guard = PartialReleaseGuard(self.partial_release.clone());
        self.entered.store(true, Ordering::Release);
        self.entered_notify.notify_waiters();
        pending::<()>().await;
        drop(guard);
        Ok(())
    }
}

#[tokio::test]
async fn cancelling_partial_release_rolls_back_and_reports_incomplete_cleanup() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let partial_release = Arc::new(AtomicUsize::new(0));
    let allocator = Arc::new(CancellationSafeGatedReleaseAllocator {
        inner,
        entered: Arc::new(AtomicBool::new(false)),
        entered_notify: Arc::new(Notify::new()),
        partial_release: partial_release.clone(),
    });
    let runner = Arc::new(GatedTaskRunner::new(false));
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig {
            queued_reap_interval: Duration::from_millis(1),
            teardown_drain_timeout: Duration::from_millis(10),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator.clone(),
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    allocator.wait_until_release_entered().await;
    assert_eq!(partial_release.load(Ordering::Acquire), 1);

    let outcome = handle.shutdown().await;
    assert!(matches!(
        outcome,
        Ok(SinkShutdown::CleanupIncomplete {
            unreleased_leases: 1,
            sink_drained: true,
            callbacks_drained: true,
        })
    ));
    actor.await.unwrap();
    assert_eq!(partial_release.load(Ordering::Acquire), 0);
}

#[derive(Clone)]
struct FailOnceReleaseAllocator {
    inner: MemoryWorkspaceAllocator,
    fail: Arc<AtomicBool>,
    fail_always: bool,
    release_delay: Option<Duration>,
}

#[derive(Clone)]
struct PanickingReleaseAllocator {
    inner: MemoryWorkspaceAllocator,
}

#[async_trait::async_trait]
impl WorkspaceAllocator for PanickingReleaseAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        self.inner.allocate(request).await
    }

    async fn release(&self, _lease: &WorkspaceLease) -> Result<(), TaskError> {
        panic!("injected workspace release panic")
    }
}

#[tokio::test]
async fn workspace_release_panic_is_truthful_observable_and_actor_survives() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let allocator = Arc::new(PanickingReleaseAllocator {
        inner: inner.clone(),
    });
    let runner = Arc::new(GatedTaskRunner::new(false));
    let sink = Arc::new(MemoryTaskEventSink::default());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        runner.clone(),
        allocator,
        sink.clone(),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
            if let Some(error) = snapshot.cleanup_error {
                assert_eq!(error.code, TaskErrorCode::WorkspaceRelease);
                assert!(error.message.contains("panicked"));
                assert!(snapshot.workspace_lease.is_some());
                assert!(snapshot.has_parent_reservation);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(sink.events().iter().any(|event| {
        matches!(
            &event.payload,
            TaskEventPayload::WorkspaceLeaseReleaseFailed { error, .. }
                if error.code == TaskErrorCode::WorkspaceRelease
                    && error.message.contains("panicked")
        )
    }));
    assert_eq!(handle.registry_counts().await.unwrap().completed, 1);
    assert_eq!(inner.live_count().await, 1);
}

#[async_trait::async_trait]
impl WorkspaceAllocator for FailOnceReleaseAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        self.inner.allocate(request).await
    }

    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError> {
        if let Some(delay) = self.release_delay {
            tokio::time::sleep(delay).await;
        }
        if self.fail_always || self.fail.swap(false, Ordering::AcqRel) {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceRelease,
                "injected release failure",
            ));
        }
        self.inner.release(lease).await
    }
}

#[tokio::test]
async fn release_failure_is_observable_and_never_claimed_as_released() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let allocator = Arc::new(FailOnceReleaseAllocator {
        inner: inner.clone(),
        fail: Arc::new(AtomicBool::new(true)),
        fail_always: false,
        release_delay: None,
    });
    let runner = Arc::new(GatedTaskRunner::new(false));
    let sink = Arc::new(MemoryTaskEventSink::default());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        runner.clone(),
        allocator,
        sink.clone(),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    runner.finish("child").await;
    loop {
        let snapshot = handle.inspect_admin(TaskId::from("child")).await.unwrap();
        if snapshot.node.status == lato_core::TaskStatus::Completed
            && snapshot.cleanup_error.is_some()
        {
            assert!(snapshot.workspace_lease.is_some());
            assert!(snapshot.has_parent_reservation);
            break;
        }
        tokio::task::yield_now().await;
    }
    let events = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let events = sink.events();
            if events.iter().any(|event| {
                matches!(
                    event.payload,
                    TaskEventPayload::WorkspaceLeaseReleaseFailed { .. }
                )
            }) {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(events.iter().any(|event| matches!(
        event.payload,
        TaskEventPayload::WorkspaceLeaseReleaseFailed { .. }
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        TaskEventPayload::WorkspaceLeaseReleased { .. }
    )));
    assert_eq!(inner.live_count().await, 1);
    handle.shutdown().await.unwrap();
    assert_eq!(inner.live_count().await, 0);
}

#[tokio::test]
async fn permanent_release_failure_makes_shutdown_explicitly_incomplete() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let allocator = Arc::new(FailOnceReleaseAllocator {
        inner: inner.clone(),
        fail: Arc::new(AtomicBool::new(false)),
        fail_always: true,
        release_delay: None,
    });
    let runner = Arc::new(GatedTaskRunner::new(false));
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        runner.clone(),
        allocator,
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
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
            .unwrap()
            .cleanup_error
            .is_some()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        handle.shutdown().await.unwrap(),
        SinkShutdown::CleanupIncomplete {
            unreleased_leases: 1,
            sink_drained: true,
            callbacks_drained: true,
        }
    );
    assert_eq!(inner.live_count().await, 1);
}

#[tokio::test]
async fn slow_release_cannot_exceed_shutdown_bound_or_claim_clean_shutdown() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let allocator = Arc::new(FailOnceReleaseAllocator {
        inner,
        fail: Arc::new(AtomicBool::new(false)),
        fail_always: false,
        release_delay: Some(Duration::from_millis(150)),
    });
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(5),
            teardown_drain_timeout: Duration::from_millis(20),
            queued_reap_interval: Duration::from_millis(2),
            ..CoordinatorConfig::default()
        },
        runner,
        allocator,
        Arc::new(MemoryTaskEventSink::default()),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let outcome = tokio::time::timeout(Duration::from_millis(100), handle.shutdown())
        .await
        .expect("shutdown must not await a blocking allocator")
        .unwrap();
    assert_eq!(
        outcome,
        SinkShutdown::CleanupIncomplete {
            unreleased_leases: 1,
            sink_drained: true,
            callbacks_drained: true,
        }
    );
}

struct BlockingEventSink {
    entered: AtomicBool,
    gate: Mutex<bool>,
    wake: Condvar,
}

impl BlockingEventSink {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            gate: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl TaskEventSink for BlockingEventSink {
    fn on_event(&self, _event: TaskEventEnvelope) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.gate.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }
}

#[tokio::test]
async fn cleanup_and_sink_failures_are_both_preserved_in_shutdown_outcome() {
    let workspace = tempfile::tempdir().unwrap();
    let inner = MemoryWorkspaceAllocator::new(workspace.path()).unwrap();
    let allocator = Arc::new(FailOnceReleaseAllocator {
        inner,
        fail: Arc::new(AtomicBool::new(false)),
        fail_always: true,
        release_delay: None,
    });
    let runner = Arc::new(GatedTaskRunner::new(false));
    let sink = Arc::new(BlockingEventSink::new());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig {
            teardown_drain_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        sink.clone(),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: lato_core::TaskOwner::Interactive {
                session_id: lato_core::SessionId::from("s"),
                turn_id: lato_core::TurnId::from("t"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("child")).await.unwrap();
    loop {
        if handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Running
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
            .unwrap()
            .cleanup_error
            .is_some()
            && sink.entered.load(Ordering::Acquire)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        handle.shutdown().await.unwrap(),
        SinkShutdown::CleanupIncomplete {
            unreleased_leases: 1,
            sink_drained: false,
            callbacks_drained: true,
        }
    );
    sink.release();
}
