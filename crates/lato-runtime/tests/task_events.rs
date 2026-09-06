mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, SessionId, TaskErrorCode, TaskId, TaskOwner, ToolCapability, TurnId,
};
use lato_runtime::{
    CoordinatorConfig, MemoryTaskEventSink, TaskEventPayload, TaskRootRequest,
    spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::sync::Arc;
use task_support::Harness;

#[tokio::test]
async fn root_registration_is_actor_owned_and_evented() {
    let mut harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;

    let event = harness.next_event().await;
    assert_eq!(event.sequence, 1);
    assert_eq!(event.task_id, TaskId::from("root-1"));
    assert_eq!(event.parent_id, None);
    assert_eq!(event.root_id, TaskId::from("root-1"));
    assert!(matches!(event.payload, TaskEventPayload::RootRegistered));

    let inspection = harness
        .handle
        .inspect(TaskId::from("root-1"))
        .await
        .unwrap();
    assert_eq!(inspection.node.id, TaskId::from("root-1"));
    assert_eq!(inspection.event_sequence, 1);
}

#[tokio::test]
async fn duplicate_root_is_rejected_without_second_event() {
    let mut harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;
    harness.next_event().await;

    let error = harness
        .try_register_root("root-1", "session-1", "turn-1")
        .await
        .unwrap_err();
    assert_eq!(error.code, TaskErrorCode::DuplicateTask);
    assert_eq!(harness.handle.registry_counts().await.unwrap().roots, 1);
    assert!(!harness.has_pending_event());
}

#[tokio::test]
async fn shutdown_closes_the_actor_after_acknowledgement() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;

    harness.handle.shutdown().await.unwrap();
    let error = harness.handle.registry_counts().await.unwrap_err();
    assert_eq!(error.code, TaskErrorCode::CoordinatorClosed);
}

#[tokio::test]
async fn root_events_have_a_dense_global_sequence_and_reach_the_sink() {
    let workspace = tempfile::tempdir().unwrap();
    let sink = Arc::new(MemoryTaskEventSink::default());
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        sink.clone(),
    );
    for suffix in ["a", "b"] {
        handle
            .register_root(TaskRootRequest {
                task_id: TaskId::from(format!("root-{suffix}")),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from(format!("session-{suffix}")),
                    turn_id: TurnId::from(format!("turn-{suffix}")),
                },
                profile: AgentProfile::worker(),
                permissions: vec![ToolCapability::FileRead],
                budget: BudgetLimits::unlimited(),
            })
            .await
            .unwrap();
    }

    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].sequence, 2);
    assert!(
        events
            .iter()
            .all(|event| matches!(event.payload, TaskEventPayload::RootRegistered))
    );

    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn root_shutdown_commits_terminal_state_before_publishing() {
    let mut harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;
    harness.next_event().await;

    harness
        .handle
        .shutdown_root(TaskId::from("root-1"))
        .await
        .unwrap();
    let event = harness.next_event().await;
    assert_eq!(event.sequence, 2);
    assert!(matches!(event.payload, TaskEventPayload::RootClosed));
    assert!(
        harness
            .handle
            .inspect(TaskId::from("root-1"))
            .await
            .unwrap()
            .node
            .status
            .is_terminal()
    );
    let counts = harness.handle.registry_counts().await.unwrap();
    assert_eq!(counts.roots, 0);
    assert_eq!(counts.completed, 1);
}
