mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, TaskErrorCode, TaskId, TaskScope, TaskStatus,
    TaskUsage,
};
use lato_runtime::{CoordinatorConfig, SpawnMode, SpawnTaskRequest, TaskEventPayload, WaitOutcome};
use std::time::Duration;
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
    assert!(harness.runner.report_usage("child", usage(50)).await);
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

    assert!(harness.runner.report_usage("child", usage(90)).await);
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
    stale.report_usage(usage(19)).await;
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
