mod task_support;

use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, ResultContract, TaskErrorCode, TaskId, TaskScope,
};
use lato_runtime::{CoordinatorConfig, LimitBehavior, SpawnMode, SpawnTaskRequest};
use std::time::Duration;
use task_support::Harness;
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
        reservation: BudgetAmount {
            child_tasks: 1,
            worktrees: 1,
            ..BudgetAmount::ZERO
        },
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
    assert_eq!(harness.allocator.live_count().await, 0);
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
