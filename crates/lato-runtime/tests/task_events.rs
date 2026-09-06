mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, SessionId, TaskErrorCode, TaskId, TaskOwner, ToolCapability, TurnId,
};
use lato_runtime::{
    CoordinatorConfig, MemoryTaskEventSink, TaskEventEnvelope, TaskEventPayload, TaskEventSink,
    TaskRootRequest, spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
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

    let events = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let events = sink.events();
            if events.len() == 2 {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
async fn scoped_inspection_does_not_cross_root_ownership() {
    let workspace = tempfile::tempdir().unwrap();
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let register = |root: &str, session: &str, turn: &str| TaskRootRequest {
        task_id: TaskId::from(root),
        owner: TaskOwner::Interactive {
            session_id: SessionId::from(session),
            turn_id: TurnId::from(turn),
        },
        profile: AgentProfile::worker(),
        permissions: vec![ToolCapability::FileRead],
        budget: BudgetLimits::unlimited(),
    };
    let root_a = handle
        .register_root(register("root-a", "session-a", "turn-a"))
        .await
        .unwrap();
    handle
        .register_root(register("root-b", "session-b", "turn-b"))
        .await
        .unwrap();

    assert_eq!(
        root_a
            .inspect(TaskId::from("root-a"))
            .await
            .unwrap()
            .node
            .id,
        TaskId::from("root-a")
    );
    let error = root_a.inspect(TaskId::from("root-b")).await.unwrap_err();
    assert_eq!(error.code, TaskErrorCode::NotFoundOrNotOwned);

    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

struct PanickingSink;

impl TaskEventSink for PanickingSink {
    fn on_event(&self, _event: TaskEventEnvelope) {
        panic!("injected sink panic");
    }
}

#[tokio::test]
async fn panicking_sink_does_not_kill_the_actor_or_change_acknowledgements() {
    let workspace = tempfile::tempdir().unwrap();
    let (handle, actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(PanickingSink),
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
    assert_eq!(handle.registry_counts().await.unwrap().roots, 2);
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

struct BlockingSink {
    entered: AtomicBool,
    gate: Mutex<bool>,
    released: Condvar,
}

impl BlockingSink {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            gate: Mutex::new(false),
            released: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.released.notify_all();
    }
}

impl TaskEventSink for BlockingSink {
    fn on_event(&self, _event: TaskEventEnvelope) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.gate.lock().unwrap();
        while !*released {
            released = self.released.wait(released).unwrap();
        }
    }
}

#[tokio::test]
async fn blocking_sink_never_blocks_commands_and_saturation_is_counted() {
    let workspace = tempfile::tempdir().unwrap();
    let sink = Arc::new(BlockingSink::new());
    let config = CoordinatorConfig {
        event_capacity: 1,
        ..CoordinatorConfig::default()
    };
    let (handle, actor) = spawn_task_coordinator(
        config,
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        sink.clone(),
    );
    let register = |suffix: &str| TaskRootRequest {
        task_id: TaskId::from(format!("root-{suffix}")),
        owner: TaskOwner::Interactive {
            session_id: SessionId::from(format!("session-{suffix}")),
            turn_id: TurnId::from(format!("turn-{suffix}")),
        },
        profile: AgentProfile::worker(),
        permissions: vec![ToolCapability::FileRead],
        budget: BudgetLimits::unlimited(),
    };
    handle.register_root(register("a")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !sink.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    handle.register_root(register("b")).await.unwrap();
    handle.register_root(register("c")).await.unwrap();
    let counts = tokio::time::timeout(Duration::from_millis(100), handle.registry_counts())
        .await
        .expect("blocking observational sink must not delay coordinator commands")
        .unwrap();
    assert_eq!(counts.roots, 3);
    assert!(counts.dropped_sink_events >= 1);

    handle.shutdown().await.unwrap();
    actor.await.unwrap();
    sink.release();
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
