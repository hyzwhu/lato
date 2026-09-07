mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, SessionId, TaskErrorCode, TaskId, TaskOwner, ToolCapability, TurnId,
};
use lato_runtime::{
    CoordinatorConfig, MemoryTaskEventSink, SinkShutdown, TaskEventEnvelope, TaskEventPayload,
    TaskEventSink, TaskRootRequest, VerificationDecision, VerificationOutcome, VerificationRequest,
    VerificationResume, spawn_task_coordinator, spawn_task_coordinator_with_verifier,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use task_support::ControlledTaskControl;
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
        .inspect_admin(TaskId::from("root-1"))
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
    delivered: AtomicUsize,
    gate: Mutex<bool>,
    released: Condvar,
}

impl BlockingSink {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            delivered: AtomicUsize::new(0),
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
        self.delivered.fetch_add(1, Ordering::Release);
    }
}

#[tokio::test]
async fn blocking_sink_never_blocks_commands_and_saturation_is_counted() {
    let workspace = tempfile::tempdir().unwrap();
    let sink = Arc::new(BlockingSink::new());
    let config = CoordinatorConfig {
        event_capacity: 1,
        teardown_drain_timeout: Duration::from_millis(20),
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

    assert_eq!(
        handle.shutdown().await.unwrap(),
        SinkShutdown::TimedOutDetached
    );
    actor.await.unwrap();
    sink.release();
}

#[tokio::test]
async fn normal_shutdown_waits_for_every_accepted_sink_event_to_drain() {
    let workspace = tempfile::tempdir().unwrap();
    let sink = Arc::new(BlockingSink::new());
    let config = CoordinatorConfig {
        teardown_drain_timeout: Duration::from_secs(1),
        ..CoordinatorConfig::default()
    };
    let (handle, actor) = spawn_task_coordinator(
        config,
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        sink.clone(),
    );
    handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !sink.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let shutdown_handle = handle.clone();
    let shutdown = tokio::spawn(async move { shutdown_handle.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    sink.release();
    assert_eq!(shutdown.await.unwrap().unwrap(), SinkShutdown::Drained);
    assert_eq!(sink.delivered.load(Ordering::Acquire), 1);
    actor.await.unwrap();
}

struct DropGate {
    started: AtomicBool,
    finished: AtomicBool,
    open: Mutex<bool>,
    released: Condvar,
}

impl DropGate {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            open: Mutex::new(false),
            released: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.released.notify_all();
    }
}

struct DropBlockingSink {
    gate: Arc<DropGate>,
}

impl TaskEventSink for DropBlockingSink {
    fn on_event(&self, _event: TaskEventEnvelope) {}
}

impl Drop for DropBlockingSink {
    fn drop(&mut self) {
        self.gate.started.store(true, Ordering::Release);
        let mut open = self.gate.open.lock().unwrap();
        while !*open {
            open = self.gate.released.wait(open).unwrap();
        }
        self.gate.finished.store(true, Ordering::Release);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sink_drop_that_blocks_times_out_without_hanging_the_runtime() {
    let workspace = tempfile::tempdir().unwrap();
    let gate = Arc::new(DropGate::new());
    let config = CoordinatorConfig {
        teardown_drain_timeout: Duration::from_millis(20),
        ..CoordinatorConfig::default()
    };
    let (handle, actor) = spawn_task_coordinator(
        config,
        Arc::new(task_support::ControlledTaskRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(DropBlockingSink { gate: gate.clone() }),
    );
    handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
        .await
        .expect("sink destruction must not block the current-thread runtime")
        .unwrap();
    assert_eq!(outcome, SinkShutdown::TimedOutDetached);
    assert!(gate.started.load(Ordering::Acquire));
    actor.await.unwrap();

    gate.release();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !gate.finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached sink worker should finish after test cleanup release");
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
            .inspect_admin(TaskId::from("root-1"))
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

struct ImmediateRunner;

#[async_trait::async_trait]
impl lato_runtime::TaskRunner for ImmediateRunner {
    type Control = ControlledTaskControl;

    async fn run(
        &self,
        request: lato_runtime::TaskRunRequest,
        reporter: lato_runtime::TaskReporter<Self::Control>,
    ) -> lato_runtime::TaskRunOutput {
        let _ = reporter
            .started(lato_runtime::StartedTask::new(
                Arc::new(ControlledTaskControl::new()),
                request.cancellation,
            ))
            .await;
        lato_runtime::TaskRunOutput::from(lato_core::TaskResult {
            success: true,
            output: "ok".into(),
            error: None,
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), lato_core::TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}

struct StaticVerifier(VerificationOutcome);

#[async_trait::async_trait]
impl lato_runtime::TaskVerifier for StaticVerifier {
    async fn verify(&self, _request: VerificationRequest) -> VerificationOutcome {
        self.0.clone()
    }
}

fn verification_child(id: &str) -> lato_runtime::SpawnTaskRequest {
    lato_runtime::SpawnTaskRequest {
        task_id: TaskId::from(id),
        scope: lato_core::TaskScope {
            objective: "verify".into(),
            context_refs: Vec::new(),
        },
        profile: AgentProfile::worker(),
        requested_capabilities: None,
        budget: BudgetLimits::unlimited(),
        result_contract: lato_core::ResultContract {
            schema: None,
            max_output_bytes: 1024,
        },
        mode: lato_runtime::SpawnMode::Background,
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

async fn verifier_harness(
    outcome: VerificationOutcome,
) -> (
    lato_runtime::TaskHandle,
    lato_runtime::ScopedTaskHandle,
    Arc<MemoryTaskEventSink>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    verifier_harness_with(
        CoordinatorConfig::default(),
        Arc::new(StaticVerifier(outcome)),
    )
    .await
}

async fn verifier_harness_with(
    config: CoordinatorConfig,
    verifier: Arc<dyn lato_runtime::TaskVerifier>,
) -> (
    lato_runtime::TaskHandle,
    lato_runtime::ScopedTaskHandle,
    Arc<MemoryTaskEventSink>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let workspace = tempfile::tempdir().unwrap();
    let sink = Arc::new(MemoryTaskEventSink::default());
    let (handle, actor) = spawn_task_coordinator_with_verifier(
        config,
        Arc::new(ImmediateRunner),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        verifier,
        sink.clone(),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("verification-root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("verification-session"),
                turn_id: TurnId::from("verification-turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    (handle, root, sink, actor, workspace)
}

#[tokio::test]
async fn verification_events_precede_completed_publication() {
    let (handle, root, sink, actor, _workspace) =
        verifier_harness(VerificationOutcome::Passed).await;
    root.spawn(verification_child("verified-child"))
        .await
        .unwrap();
    let _ = root
        .wait(TaskId::from("verified-child"), Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if sink.events().iter().any(|event| {
                event.task_id == TaskId::from("verified-child")
                    && matches!(event.payload, TaskEventPayload::Completed { .. })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let child_events: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| event.task_id == TaskId::from("verified-child"))
        .map(|event| event.payload)
        .collect();
    let started = child_events
        .iter()
        .position(|event| matches!(event, TaskEventPayload::VerificationStarted))
        .unwrap();
    let passed = child_events
        .iter()
        .position(|event| matches!(event, TaskEventPayload::VerificationPassed))
        .unwrap();
    let completed = child_events
        .iter()
        .position(|event| matches!(event, TaskEventPayload::Completed { .. }))
        .unwrap();
    assert!(started < passed && passed < completed);
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn waiting_verification_resume_requires_scope_kind_and_exact_id() {
    let reviewer = TaskId::from("expected-reviewer");
    let (handle, root, _sink, actor, _workspace) =
        verifier_harness(VerificationOutcome::WaitingForChild {
            reviewer_task_id: reviewer.clone(),
        })
        .await;
    root.spawn(verification_child("waiting-child"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handle
                .inspect_admin(TaskId::from("waiting-child"))
                .await
                .is_ok_and(|snapshot| {
                    snapshot.node.status == lato_core::TaskStatus::WaitingForChildren
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let foreign = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("foreign-root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("foreign-session"),
                turn_id: TurnId::from("foreign-turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead],
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    let foreign_error = foreign
        .resume_verification(
            TaskId::from("waiting-child"),
            VerificationResume::Child {
                reviewer_task_id: reviewer.clone(),
                decision: VerificationDecision::Passed,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(foreign_error.code, TaskErrorCode::NotFoundOrNotOwned);

    let wrong = root
        .resume_verification(
            TaskId::from("waiting-child"),
            VerificationResume::Child {
                reviewer_task_id: TaskId::from("wrong-reviewer"),
                decision: VerificationDecision::Passed,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, TaskErrorCode::VerificationPending);
    root.resume_verification(
        TaskId::from("waiting-child"),
        VerificationResume::Child {
            reviewer_task_id: reviewer,
            decision: VerificationDecision::Passed,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        handle
            .inspect_admin(TaskId::from("waiting-child"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Completed
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

struct PanickingVerifier;

#[async_trait::async_trait]
impl lato_runtime::TaskVerifier for PanickingVerifier {
    async fn verify(&self, _request: VerificationRequest) -> VerificationOutcome {
        panic!("injected verifier panic");
    }
}

struct PendingVerifier {
    dropped: Arc<AtomicBool>,
}

struct VerificationDropGuard(Arc<AtomicBool>);

impl Drop for VerificationDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl lato_runtime::TaskVerifier for PendingVerifier {
    async fn verify(&self, _request: VerificationRequest) -> VerificationOutcome {
        let _guard = VerificationDropGuard(self.dropped.clone());
        std::future::pending().await
    }
}

#[tokio::test]
async fn verifier_panic_is_contained_as_a_verification_failure() {
    let (handle, root, _sink, actor, _workspace) =
        verifier_harness_with(CoordinatorConfig::default(), Arc::new(PanickingVerifier)).await;
    root.spawn(verification_child("panic-child")).await.unwrap();
    let outcome = root
        .wait(TaskId::from("panic-child"), Duration::from_secs(2))
        .await
        .unwrap();
    let lato_runtime::WaitOutcome::Finished(snapshot) = outcome else {
        panic!("panicking verifier must terminate");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Failed);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::VerificationFailed
    );
    assert_eq!(handle.registry_counts().await.unwrap().roots, 1);
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn cancellation_aborts_and_drops_a_blocked_verifier_boundedly() {
    let dropped = Arc::new(AtomicBool::new(false));
    let config = CoordinatorConfig {
        cancel_grace: Duration::from_millis(10),
        verification_timeout: Duration::from_secs(5),
        ..CoordinatorConfig::default()
    };
    let (handle, root, _sink, actor, _workspace) = verifier_harness_with(
        config,
        Arc::new(PendingVerifier {
            dropped: dropped.clone(),
        }),
    )
    .await;
    root.spawn(verification_child("blocked-child"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handle
                .inspect_admin(TaskId::from("blocked-child"))
                .await
                .is_ok_and(|snapshot| snapshot.node.status == lato_core::TaskStatus::Verifying)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    root.cancel_task(TaskId::from("blocked-child"))
        .await
        .unwrap();
    let outcome = root
        .wait(TaskId::from("blocked-child"), Duration::from_secs(2))
        .await
        .unwrap();
    let lato_runtime::WaitOutcome::Finished(snapshot) = outcome else {
        panic!("cancelled verifier must terminate");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Cancelled);
    assert!(dropped.load(Ordering::Acquire));
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn blocked_verifier_times_out_without_blocking_the_actor() {
    let dropped = Arc::new(AtomicBool::new(false));
    let config = CoordinatorConfig {
        verification_timeout: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    };
    let (handle, root, _sink, actor, _workspace) = verifier_harness_with(
        config,
        Arc::new(PendingVerifier {
            dropped: dropped.clone(),
        }),
    )
    .await;
    root.spawn(verification_child("timeout-child"))
        .await
        .unwrap();
    let outcome = root
        .wait(TaskId::from("timeout-child"), Duration::from_secs(2))
        .await
        .unwrap();
    let lato_runtime::WaitOutcome::Finished(snapshot) = outcome else {
        panic!("timed out verifier must terminate");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Failed);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::VerificationFailed
    );
    assert!(dropped.load(Ordering::Acquire));
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}
