mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, SessionId, TaskErrorCode, TaskId, TaskOwner,
    TaskScope, TaskStatus, ToolCapability, TurnId,
};
use lato_runtime::{
    CancelOutcome, CancelTarget, CoordinatorConfig, NoopTaskEventSink, SpawnMode, SpawnTaskRequest,
    TaskEventPayload, TaskRootRequest, spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{sync::Arc, time::Duration};
use task_support::{GatedTaskRunner, Harness};
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
            max_output_bytes: 1_024,
        },
        mode: SpawnMode::Background,
        cancellation: CancellationToken::new(),
    }
}

#[test]
fn cancel_targets_round_trip_with_stable_tagged_shapes() {
    let targets = [
        CancelTarget::Task {
            task_id: TaskId::from("task"),
        },
        CancelTarget::Turn {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
        },
        CancelTarget::Root {
            root_id: TaskId::from("root"),
        },
        CancelTarget::Workflow {
            run_id: "run".into(),
            root_id: Some(TaskId::from("root")),
        },
    ];
    for target in targets {
        let value = serde_json::to_value(&target).unwrap();
        assert!(value["type"].is_string());
        assert_eq!(
            serde_json::from_value::<CancelTarget>(value).unwrap(),
            target
        );
    }
    assert_eq!(
        serde_json::to_value(CancelTarget::Task {
            task_id: TaskId::from("task")
        })
        .unwrap(),
        serde_json::json!({"type": "task", "task_id": "task"})
    );
}

async fn wait_terminal(harness: &Harness, id: &str) -> lato_runtime::TaskSnapshot {
    match harness
        .handle
        .wait_admin(TaskId::from(id), Duration::from_secs(2))
        .await
        .unwrap()
    {
        lato_runtime::WaitOutcome::Finished(snapshot) => snapshot,
        other => panic!("task {id} did not finish: {other:?}"),
    }
}

#[tokio::test]
async fn cancelling_parent_cancels_all_descendants_only() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 8,
        max_running_per_root: 8,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let parent = root.spawn(request("parent")).await.unwrap().handle;
    let child = parent.spawn(request("child")).await.unwrap().handle;
    child.spawn(request("grandchild")).await.unwrap();
    root.spawn(request("sibling")).await.unwrap();
    let foreign = harness
        .register_root_scoped("foreign", "other-session", "other-turn")
        .await;
    foreign.spawn(request("foreign-child")).await.unwrap();

    for id in ["parent", "child", "grandchild", "sibling", "foreign-child"] {
        harness.wait_for_status(id, TaskStatus::Running).await;
    }
    let outcome = root.cancel_task(TaskId::from("parent")).await.unwrap();
    assert_eq!(
        outcome,
        CancelOutcome {
            matched: 3,
            newly_requested: 3,
            already_terminal: 0,
        }
    );
    for id in ["parent", "child", "grandchild"] {
        assert!(wait_terminal(&harness, id).await.node.status.is_cancelled());
    }
    assert_eq!(
        root.inspect(TaskId::from("sibling"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );
    assert_eq!(
        foreign
            .inspect(TaskId::from("foreign-child"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );
    assert_eq!(
        root.cancel_task(TaskId::from("foreign-child"))
            .await
            .unwrap_err()
            .code,
        TaskErrorCode::NotFoundOrNotOwned
    );
}

#[tokio::test]
async fn cancelling_root_as_a_task_terminalizes_root_after_its_descendants() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let child = root.spawn(request("child")).await.unwrap().handle;
    child.spawn(request("grandchild")).await.unwrap();
    for id in ["child", "grandchild"] {
        harness.wait_for_status(id, TaskStatus::Running).await;
    }
    let mut terminal_order = Vec::new();
    let mut events = harness.handle.subscribe();
    assert_eq!(
        harness
            .handle
            .cancel_task(TaskId::from("root"))
            .await
            .unwrap(),
        CancelOutcome {
            matched: 3,
            newly_requested: 3,
            already_terminal: 0,
        }
    );
    for id in ["root", "child", "grandchild"] {
        assert_eq!(
            wait_terminal(&harness, id).await.node.status,
            TaskStatus::Cancelled
        );
    }
    while let Ok(event) = events.try_recv() {
        if matches!(event.payload, TaskEventPayload::Cancelled) {
            terminal_order.push(event.task_id);
        }
    }
    let root_position = terminal_order
        .iter()
        .position(|task_id| task_id == &TaskId::from("root"))
        .unwrap();
    assert!(terminal_order[..root_position].contains(&TaskId::from("child")));
    assert!(terminal_order[..root_position].contains(&TaskId::from("grandchild")));
    assert_eq!(terminal_order.len(), 3);
    for id in ["root", "child", "grandchild"] {
        assert_eq!(
            terminal_order
                .iter()
                .filter(|task_id| task_id.as_str() == id)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn queued_cancellation_is_immediate_and_idempotent() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("running")).await.unwrap();
    let queued = root.spawn(request("queued")).await.unwrap();
    assert!(queued.is_queued());

    assert_eq!(
        root.cancel_task(TaskId::from("queued")).await.unwrap(),
        CancelOutcome {
            matched: 1,
            newly_requested: 1,
            already_terminal: 0,
        }
    );
    assert_eq!(
        wait_terminal(&harness, "queued").await.node.status,
        TaskStatus::Cancelled
    );
    assert!(
        !harness
            .runner
            .started_ids()
            .await
            .contains(&TaskId::from("queued"))
    );
    assert_eq!(
        root.cancel_task(TaskId::from("queued")).await.unwrap(),
        CancelOutcome {
            matched: 1,
            newly_requested: 0,
            already_terminal: 1,
        }
    );
}

#[tokio::test]
async fn root_cancellation_counts_terminal_and_live_tasks_without_double_terminalization() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("finished")).await.unwrap();
    root.spawn(request("live")).await.unwrap();
    for id in ["finished", "live"] {
        harness.wait_for_status(id, TaskStatus::Running).await;
    }
    harness.runner.finish("finished").await;
    assert_eq!(
        wait_terminal(&harness, "finished").await.node.status,
        TaskStatus::Completed
    );

    assert_eq!(
        harness
            .handle
            .cancel_root(TaskId::from("root"))
            .await
            .unwrap(),
        CancelOutcome {
            matched: 2,
            newly_requested: 1,
            already_terminal: 1,
        }
    );
    assert_eq!(
        wait_terminal(&harness, "live").await.node.status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        wait_terminal(&harness, "finished").await.node.status,
        TaskStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn preparing_task_is_aborted_without_ever_becoming_running() {
    let harness = Harness::new_paused(CoordinatorConfig {
        cancel_grace: Duration::from_secs(1),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("preparing")).await.unwrap();
    harness.runner.wait_until_entered("preparing").await;
    assert_eq!(
        root.inspect(TaskId::from("preparing"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Preparing
    );
    root.cancel_task(TaskId::from("preparing")).await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        wait_terminal(&harness, "preparing").await.node.status,
        TaskStatus::Cancelled
    );
    assert!(
        !harness
            .runner
            .started_ids()
            .await
            .contains(&TaskId::from("preparing"))
    );
}

#[tokio::test]
async fn turn_and_workflow_cancellation_match_only_their_owner_scope() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let turn_a = harness.register_root_scoped("turn-a", "session", "a").await;
    let turn_b = harness.register_root_scoped("turn-b", "session", "b").await;
    turn_a.spawn(request("a-child")).await.unwrap();
    turn_b.spawn(request("b-child")).await.unwrap();
    let workflow = harness
        .handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("workflow-root"),
            owner: TaskOwner::Workflow {
                run_id: "run-1".into(),
                session_id: SessionId::from("session"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    workflow.spawn(request("workflow-child")).await.unwrap();
    for id in ["a-child", "b-child", "workflow-child"] {
        harness.wait_for_status(id, TaskStatus::Running).await;
    }

    assert_eq!(
        harness
            .handle
            .cancel_turn(SessionId::from("session"), TurnId::from("a"))
            .await
            .unwrap()
            .matched,
        1
    );
    assert_eq!(
        wait_terminal(&harness, "a-child").await.node.status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("b-child"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("workflow-child"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );

    assert_eq!(
        harness
            .handle
            .cancel_workflow("run-1".into(), Some(TaskId::from("workflow-root")))
            .await
            .unwrap()
            .matched,
        1
    );
    assert_eq!(
        wait_terminal(&harness, "workflow-child").await.node.status,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn root_cancellation_includes_workflow_owned_descendants_but_not_the_root_node() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let workflow = harness
        .handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("workflow-root"),
            owner: TaskOwner::Workflow {
                run_id: "run".into(),
                session_id: SessionId::from("session"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    workflow.spawn(request("workflow-child")).await.unwrap();
    harness
        .wait_for_status("workflow-child", TaskStatus::Running)
        .await;
    assert_eq!(
        harness
            .handle
            .cancel_root(TaskId::from("workflow-root"))
            .await
            .unwrap()
            .matched,
        1
    );
    assert_eq!(
        wait_terminal(&harness, "workflow-child").await.node.status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("workflow-root"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );
}

#[tokio::test(start_paused = true)]
async fn dropped_teardown_waiter_frees_capacity_and_reopens_after_safe_drain() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_waiters: 1,
            cancel_grace: Duration::from_secs(1),
            teardown_drain_timeout: Duration::from_secs(5),
            queued_reap_interval: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        runner,
    )
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("child")).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    let abandoned_handle = harness.handle.clone();
    let abandoned = tokio::spawn(async move {
        abandoned_handle
            .teardown_root_and_drain(TaskId::from("root"))
            .await
    });
    tokio::task::yield_now().await;
    abandoned.abort();
    let _ = abandoned.await;
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;

    let next_handle = harness.handle.clone();
    let next = tokio::spawn(async move {
        next_handle
            .teardown_root_and_drain(TaskId::from("root"))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !next.is_finished(),
        "replacement drain waiter must be admitted"
    );
    tokio::time::advance(Duration::from_millis(900)).await;
    tokio::task::yield_now().await;
    assert!(next.await.unwrap().is_ok());
    assert!(root.spawn(request("after-abandoned-drain")).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn teardown_closes_admission_resolves_all_waiters_and_reopens_only_its_root() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            cancel_grace: Duration::from_secs(1),
            teardown_drain_timeout: Duration::from_secs(5),
            ..CoordinatorConfig::default()
        },
        runner,
    )
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let foreign = harness
        .register_root_scoped("foreign", "foreign", "turn")
        .await;
    root.spawn(request("child")).await.unwrap();
    foreign.spawn(request("foreign-child")).await.unwrap();
    harness.wait_for_status("child", TaskStatus::Running).await;

    let handle = harness.handle.clone();
    let first =
        tokio::spawn(async move { handle.teardown_root_and_drain(TaskId::from("root")).await });
    tokio::task::yield_now().await;
    let root_for_waiter = root.clone();
    let second = tokio::spawn(async move { root_for_waiter.teardown_root_and_drain().await });
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    assert_eq!(
        root.spawn(request("late")).await.unwrap_err().code,
        TaskErrorCode::SpawnAdmissionClosed
    );
    assert!(foreign.spawn(request("foreign-late")).await.is_ok());
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        first.await.unwrap().unwrap(),
        CancelOutcome {
            matched: 1,
            newly_requested: 1,
            already_terminal: 0
        }
    );
    assert_eq!(
        second.await.unwrap().unwrap(),
        CancelOutcome {
            matched: 1,
            newly_requested: 0,
            already_terminal: 0
        }
    );

    root.open_spawn_admission().await.unwrap();
    assert!(root.spawn(request("next-turn")).await.is_ok());
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("foreign-child"))
            .await
            .unwrap()
            .node
            .status,
        TaskStatus::Running
    );
}

#[tokio::test(start_paused = true)]
async fn noncooperative_runner_is_aborted_after_grace_exactly_once() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            cancel_grace: Duration::from_secs(2),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
    )
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    root.spawn(request("stuck")).await.unwrap();
    harness.wait_for_status("stuck", TaskStatus::Running).await;
    let mut events = harness.handle.subscribe();
    root.cancel_task(TaskId::from("stuck")).await.unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        wait_terminal(&harness, "stuck").await.node.status,
        TaskStatus::Cancelled
    );
    assert_eq!(runner.active_runs(), 0);

    let mut terminal_events = 0;
    while let Ok(event) = events.try_recv() {
        if event.task_id == TaskId::from("stuck")
            && matches!(event.payload, TaskEventPayload::Cancelled)
        {
            terminal_events += 1;
        }
    }
    assert_eq!(terminal_events, 1);
}

#[tokio::test(start_paused = true)]
async fn teardown_backstop_is_truthful_and_reopens_only_the_selected_root() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            max_global_running: 2,
            max_running_per_root: 1,
            cancel_grace: Duration::from_secs(10),
            teardown_drain_timeout: Duration::from_secs(2),
            ..CoordinatorConfig::default()
        },
        runner,
    )
    .await;
    let root = harness
        .register_root_scoped("root", "session", "turn")
        .await;
    let foreign = harness
        .register_root_scoped("foreign", "foreign", "turn")
        .await;
    root.spawn(request("stuck")).await.unwrap();
    harness.wait_for_status("stuck", TaskStatus::Running).await;
    foreign.close_spawn_admission().await.unwrap();

    let handle = harness.handle.clone();
    let teardown =
        tokio::spawn(async move { handle.teardown_root_and_drain(TaskId::from("root")).await });
    tokio::task::yield_now().await;
    assert_eq!(
        root.open_spawn_admission().await.unwrap_err().code,
        TaskErrorCode::SpawnAdmissionClosed
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    let error = teardown.await.unwrap().unwrap_err();
    assert_eq!(error.code, TaskErrorCode::TimedOut);
    assert!(error.message.contains("1 unfinished task(s)"));
    assert!(root.spawn(request("after-backstop")).await.is_ok());
    assert_eq!(
        foreign
            .spawn(request("foreign-still-closed"))
            .await
            .unwrap_err()
            .code,
        TaskErrorCode::SpawnAdmissionClosed
    );
}

#[tokio::test(start_paused = true)]
async fn workflow_drain_timeout_counts_only_unfinished_descendants() {
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let harness = Harness::with_runner(
        CoordinatorConfig {
            cancel_grace: Duration::from_secs(10),
            teardown_drain_timeout: Duration::from_secs(2),
            ..CoordinatorConfig::default()
        },
        runner,
    )
    .await;
    let workflow = harness
        .handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("workflow-root"),
            owner: TaskOwner::Workflow {
                run_id: "run".into(),
                session_id: SessionId::from("session"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    workflow
        .spawn(request("stuck-workflow-child"))
        .await
        .unwrap();
    harness
        .wait_for_status("stuck-workflow-child", TaskStatus::Running)
        .await;

    let handle = harness.handle.clone();
    let drain = tokio::spawn(async move {
        handle
            .cancel_workflow("run".into(), Some(TaskId::from("workflow-root")))
            .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    let error = drain.await.unwrap().unwrap_err();
    assert_eq!(error.code, TaskErrorCode::TimedOut);
    assert!(error.message.contains("1 unfinished task(s)"));
}

#[tokio::test]
async fn dropping_all_handles_cancels_and_joins_owned_work() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let runner = Arc::new(GatedTaskRunner::with_options(false, true, false, false));
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(5),
            teardown_drain_timeout: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        runner.clone(),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(request("stuck")).await.unwrap();
    runner.wait_until_entered("stuck").await;
    drop(root);
    drop(handle);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor shutdown must be bounded")
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
}
