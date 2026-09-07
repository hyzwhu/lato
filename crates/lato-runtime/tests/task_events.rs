mod task_support;

use lato_core::{
    AgentProfile, BudgetLimits, SessionId, TaskErrorCode, TaskId, TaskOwner, TaskUsage,
    ToolCapability, TurnId,
};
use lato_runtime::{
    ActiveMessageOperation, ActiveMessageRequest, ApprovalVerificationResume, CoordinatorConfig,
    MemoryTaskEventSink, ReviewerVerificationResume, SinkShutdown, TaskEventEnvelope,
    TaskEventPayload, TaskEventSink, TaskRootRequest, VerificationDecision, VerificationOutcome,
    VerificationRequest, WaitOutcome, spawn_task_coordinator, spawn_task_coordinator_with_verifier,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use task_support::{ControlledTaskControl, GatedTaskRunner, Harness};

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
}

async fn wait_for_clean_terminal(harness: &Harness, task_id: &str) {
    harness
        .wait_for_status(task_id, lato_core::TaskStatus::Completed)
        .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let clean = harness
                .handle
                .inspect_admin(TaskId::from(task_id))
                .await
                .is_ok_and(|snapshot| {
                    snapshot.workspace_lease.is_none() && !snapshot.has_parent_reservation
                });
            if clean {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {task_id} did not finish resource cleanup"));
}

#[tokio::test]
async fn fixed_seed_command_sequence_preserves_all_coordinator_invariants() {
    const SEED: u64 = 0x5a17_5eed_c001_d00d;
    let config = CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        max_queue: 4,
        max_completed: 2,
        ..CoordinatorConfig::default()
    };
    let harness = Harness::new(config).await;
    let root = harness
        .register_root_scoped("audit-root", "audit-session", "audit-turn")
        .await;
    let mut random = XorShift64(SEED);
    let first = format!("audit-{:016x}", random.next());
    let queued = format!("audit-{:016x}", random.next());
    let mut covered = std::collections::HashSet::new();

    root.spawn(verification_child(&first)).await.unwrap();
    covered.insert("spawn");
    harness.runner.wait_until_entered(&first).await;
    harness
        .wait_for_status(&first, lato_core::TaskStatus::Running)
        .await;
    harness.audit().await;

    let queued_spawn = root.spawn(verification_child(&queued)).await.unwrap();
    assert!(queued_spawn.is_queued());
    covered.insert("queue");
    harness.audit().await;

    let operation = if random.next() & 1 == 0 {
        ActiveMessageOperation::Queue
    } else {
        ActiveMessageOperation::Steer
    };
    let message = ActiveMessageRequest::try_new_with_operation(
        TaskId::from(first.as_str()),
        format!("audit-message-{}", random.next()),
        operation,
    )
    .unwrap();
    let _ = root.send_active_message(message).await;
    covered.insert("message");
    harness.audit().await;

    let total_tokens = random.next() % 64 + 1;
    assert!(
        harness
            .runner
            .report_usage(
                &first,
                TaskUsage {
                    input_tokens: total_tokens / 2,
                    output_tokens: total_tokens - total_tokens / 2,
                    total_tokens,
                    ..TaskUsage::default()
                }
            )
            .await
    );
    covered.insert("usage");
    harness.audit().await;

    assert!(matches!(
        root.wait(TaskId::from(queued.as_str()), Duration::ZERO)
            .await
            .unwrap(),
        WaitOutcome::TimedOut(_)
    ));
    covered.insert("wait");
    harness.audit().await;

    root.cancel_task(TaskId::from(queued.as_str()))
        .await
        .unwrap();
    covered.insert("cancel");
    harness
        .wait_for_status(&queued, lato_core::TaskStatus::Cancelled)
        .await;
    harness.audit().await;

    harness.runner.finish(&first).await;
    covered.insert("finish");
    wait_for_clean_terminal(&harness, &first).await;
    harness.audit().await;

    for _ in 0..6 {
        let task_id = format!("audit-{:016x}", random.next());
        root.spawn(verification_child(&task_id)).await.unwrap();
        harness.runner.wait_until_entered(&task_id).await;
        harness
            .wait_for_status(&task_id, lato_core::TaskStatus::Running)
            .await;

        match random.next() % 3 {
            0 => {
                let message = ActiveMessageRequest::try_new(
                    TaskId::from(task_id.as_str()),
                    format!("follow-up-{}", random.next()),
                )
                .unwrap();
                let _ = root.send_active_message(message).await;
            }
            1 => {
                let value = random.next() % 32 + 1;
                assert!(
                    harness
                        .runner
                        .report_usage(
                            &task_id,
                            TaskUsage {
                                total_tokens: value,
                                ..TaskUsage::default()
                            }
                        )
                        .await
                );
            }
            _ => {
                assert!(matches!(
                    root.wait(TaskId::from(task_id.as_str()), Duration::ZERO)
                        .await
                        .unwrap(),
                    WaitOutcome::TimedOut(_)
                ));
            }
        }
        harness.audit().await;
        harness.runner.finish(&task_id).await;
        wait_for_clean_terminal(&harness, &task_id).await;
        harness.audit().await;
    }

    assert!(
        harness
            .handle
            .inspect_admin(TaskId::from(first.as_str()))
            .await
            .is_err(),
        "the fixed sequence must exercise completed-record eviction"
    );
    covered.insert("eviction");
    assert_eq!(
        covered,
        std::collections::HashSet::from([
            "spawn", "queue", "message", "usage", "wait", "cancel", "finish", "eviction"
        ])
    );
    harness.audit().await;

    harness.handle.shutdown().await.unwrap();
}

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
async fn waiting_reviewer_verification_rejects_self_foreign_and_wrong_ids() {
    let reviewer = TaskId::from("expected-reviewer");
    let (handle, root, _sink, actor, _workspace) =
        verifier_harness(VerificationOutcome::WaitingForChild {
            reviewer_task_id: reviewer.clone(),
        })
        .await;
    let subject = root
        .spawn(verification_child("waiting-child"))
        .await
        .unwrap()
        .handle;
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
        .resume_reviewer_verification(
            TaskId::from("waiting-child"),
            ReviewerVerificationResume {
                reviewer_task_id: reviewer.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(foreign_error.code, TaskErrorCode::NotFoundOrNotOwned);
    let foreign_unknown = foreign
        .resume_reviewer_verification(
            TaskId::from("unknown-child"),
            ReviewerVerificationResume {
                reviewer_task_id: reviewer.clone(),
            },
        )
        .await
        .unwrap_err();
    let foreign_wrong_id = foreign
        .resume_reviewer_verification(
            TaskId::from("waiting-child"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("wrong-reviewer"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(foreign_unknown, foreign_error);
    assert_eq!(foreign_wrong_id, foreign_error);

    let wrong = root
        .resume_reviewer_verification(
            TaskId::from("waiting-child"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("wrong-reviewer"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, TaskErrorCode::NotFoundOrNotOwned);
    let self_error = subject
        .resume_reviewer_verification(
            TaskId::from("waiting-child"),
            ReviewerVerificationResume {
                reviewer_task_id: reviewer,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(self_error.code, TaskErrorCode::NotFoundOrNotOwned);
    root.cancel_task(TaskId::from("waiting-child"))
        .await
        .unwrap();
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
async fn verification_failure_invokes_one_truthful_completion_callback() {
    let workspace = tempfile::tempdir().unwrap();
    let runner = Arc::new(GatedTaskRunner::new(false));
    let (handle, actor) = spawn_task_coordinator_with_verifier(
        CoordinatorConfig::default(),
        runner.clone(),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(StaticVerifier(VerificationOutcome::Failed(
            lato_core::TaskError::new(TaskErrorCode::VerificationFailed, "injected failure"),
        ))),
        Arc::new(lato_runtime::NoopTaskEventSink),
    );
    let root = handle
        .register_root(TaskRootRequest {
            task_id: TaskId::from("callback-root"),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("callback-session"),
                turn_id: TurnId::from("callback-turn"),
            },
            profile: AgentProfile::worker(),
            permissions: AgentProfile::worker().capabilities,
            budget: BudgetLimits::unlimited(),
        })
        .await
        .unwrap();
    root.spawn(verification_child("callback-child"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("callback-child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::Running)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runner.finish("callback-child").await;
    let outcome = root
        .wait(TaskId::from("callback-child"), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(matches!(outcome, lato_runtime::WaitOutcome::Finished(_)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while runner.completion_callbacks() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let callbacks = runner.completion_results();
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].task_id, TaskId::from("callback-child"));
    assert_eq!(
        callbacks[0].result.error.as_ref().unwrap().code,
        TaskErrorCode::VerificationFailed
    );
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

#[tokio::test]
async fn approval_resume_is_admin_only_and_requires_the_exact_root_bound_id() {
    let (handle, root, _sink, actor, _workspace) =
        verifier_harness(VerificationOutcome::WaitingForApproval {
            approval_id: "approval-1".into(),
        })
        .await;
    root.spawn(verification_child("approval-child"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("approval-child"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::WaitingForApproval)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let wrong = handle
        .resume_approval_verification(
            TaskId::from("approval-child"),
            ApprovalVerificationResume {
                approval_id: "forged".into(),
                decision: VerificationDecision::Passed,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, TaskErrorCode::VerificationPending);
    handle
        .resume_approval_verification(
            TaskId::from("approval-child"),
            ApprovalVerificationResume {
                approval_id: "approval-1".into(),
                decision: VerificationDecision::Passed,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        handle
            .inspect_admin(TaskId::from("approval-child"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Completed
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn external_verification_wait_timeout_releases_capacity_to_the_queue() {
    let config = CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        external_verification_wait_timeout: Duration::from_millis(50),
        ..CoordinatorConfig::default()
    };
    let (handle, root, _sink, actor, _workspace) = verifier_harness_with(
        config,
        Arc::new(StaticVerifier(VerificationOutcome::WaitingForApproval {
            approval_id: "approval".into(),
        })),
    )
    .await;
    root.spawn(verification_child("waiting-slot"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("waiting-slot"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::WaitingForApproval)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        root.spawn(verification_child("queued-after-wait"))
            .await
            .unwrap()
            .is_queued()
    );
    tokio::time::advance(Duration::from_millis(50)).await;
    let outcome = root
        .wait(TaskId::from("waiting-slot"), Duration::from_secs(2))
        .await
        .unwrap();
    let lato_runtime::WaitOutcome::Finished(snapshot) = outcome else {
        panic!("external wait timeout must terminate");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Failed);
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("queued-after-wait"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status == lato_core::TaskStatus::Queued)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn wall_time_budget_continues_through_external_verification_wait() {
    let config = CoordinatorConfig {
        external_verification_wait_timeout: Duration::from_secs(5),
        ..CoordinatorConfig::default()
    };
    let (handle, root, _sink, actor, _workspace) = verifier_harness_with(
        config,
        Arc::new(StaticVerifier(VerificationOutcome::WaitingForApproval {
            approval_id: "approval".into(),
        })),
    )
    .await;
    let mut child = verification_child("waiting-wall");
    child.budget.wall_time_ms = Some(50);
    root.spawn(child).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("waiting-wall"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::WaitingForApproval)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::advance(Duration::from_millis(51)).await;
    let lato_runtime::WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("waiting-wall"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("waiting task must exhaust its wall-time budget");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 50);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn wall_time_budget_continues_during_programmatic_verification() {
    let dropped = Arc::new(AtomicBool::new(false));
    let config = CoordinatorConfig {
        verification_timeout: Duration::from_secs(5),
        cancel_grace: Duration::from_millis(10),
        ..CoordinatorConfig::default()
    };
    let (handle, root, _sink, actor, _workspace) = verifier_harness_with(
        config,
        Arc::new(PendingVerifier {
            dropped: dropped.clone(),
        }),
    )
    .await;
    let mut child = verification_child("verifying-wall");
    child.budget.wall_time_ms = Some(50);
    root.spawn(child).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("verifying-wall"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::Verifying)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::advance(Duration::from_millis(51)).await;
    let lato_runtime::WaitOutcome::Finished(snapshot) = root
        .wait(TaskId::from("verifying-wall"), Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("verifying task must exhaust its wall-time budget");
    };
    assert_eq!(snapshot.node.status, lato_core::TaskStatus::Failed);
    assert_eq!(snapshot.budget_spent.wall_time_ms, 50);
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::BudgetExceededWallTimeMs
    );
    assert!(dropped.load(Ordering::Acquire));
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}

struct ReviewerVerifier;

#[async_trait::async_trait]
impl lato_runtime::TaskVerifier for ReviewerVerifier {
    async fn verify(&self, request: VerificationRequest) -> VerificationOutcome {
        if request.node.id == TaskId::from("subject") {
            VerificationOutcome::WaitingForChild {
                reviewer_task_id: TaskId::from("reviewer"),
            }
        } else {
            VerificationOutcome::Passed
        }
    }
}

struct ReviewSpawningRunner {
    reviewer_handle: tokio::sync::Mutex<Option<lato_runtime::ScopedTaskHandle>>,
    descendant_handle: tokio::sync::Mutex<Option<lato_runtime::ScopedTaskHandle>>,
    reviewer_gate: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lato_runtime::TaskRunner for ReviewSpawningRunner {
    type Control = ControlledTaskControl;

    async fn run(
        &self,
        request: lato_runtime::TaskRunRequest,
        reporter: lato_runtime::TaskReporter<Self::Control>,
    ) -> lato_runtime::TaskRunOutput {
        let task_id = request.node.id.clone();
        if !reporter
            .started(lato_runtime::StartedTask::new(
                Arc::new(ControlledTaskControl::new()),
                request.cancellation,
            ))
            .await
        {
            return lato_runtime::TaskRunOutput::from(lato_core::TaskResult {
                success: false,
                output: String::new(),
                error: Some(lato_core::TaskError::new(
                    TaskErrorCode::Cancelled,
                    "startup cancelled",
                )),
                usage: Default::default(),
                duration_ms: 0,
                output_ref: None,
            });
        }
        if task_id == TaskId::from("subject") {
            let reviewer = request
                .scoped_handle
                .spawn(verification_child("reviewer"))
                .await
                .unwrap()
                .handle;
            *self.reviewer_handle.lock().await = Some(reviewer);
            while self.descendant_handle.lock().await.is_none() {
                tokio::task::yield_now().await;
            }
        } else if task_id == TaskId::from("reviewer") {
            let descendant = request
                .scoped_handle
                .spawn(verification_child("reviewer-descendant"))
                .await
                .unwrap()
                .handle;
            *self.descendant_handle.lock().await = Some(descendant);
            self.reviewer_gate.notified().await;
        }
        lato_runtime::TaskRunOutput::from(lato_core::TaskResult {
            success: true,
            output: "reviewed".into(),
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

#[tokio::test]
async fn reviewer_resume_requires_the_exact_completed_direct_child() {
    let workspace = tempfile::tempdir().unwrap();
    let runner = Arc::new(ReviewSpawningRunner {
        reviewer_handle: tokio::sync::Mutex::new(None),
        descendant_handle: tokio::sync::Mutex::new(None),
        reviewer_gate: tokio::sync::Notify::new(),
    });
    let (handle, actor) = spawn_task_coordinator_with_verifier(
        CoordinatorConfig::default(),
        runner.clone(),
        Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap()),
        Arc::new(ReviewerVerifier),
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
    root.spawn(verification_child("subject")).await.unwrap();
    let reviewer = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(handle) = runner.reviewer_handle.lock().await.clone() {
                break handle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .inspect_admin(TaskId::from("subject"))
            .await
            .is_ok_and(|snapshot| snapshot.node.status != lato_core::TaskStatus::WaitingForChildren)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let unfinished = reviewer
        .resume_reviewer_verification(
            TaskId::from("subject"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("reviewer"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(unfinished.code, TaskErrorCode::VerificationPending);

    let sibling = root
        .spawn(verification_child("sibling"))
        .await
        .unwrap()
        .handle;
    let sibling_error = sibling
        .resume_reviewer_verification(
            TaskId::from("subject"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("reviewer"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(sibling_error.code, TaskErrorCode::NotFoundOrNotOwned);

    let descendant = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(handle) = runner.descendant_handle.lock().await.clone() {
                break handle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let descendant_error = descendant
        .resume_reviewer_verification(
            TaskId::from("subject"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("reviewer"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(descendant_error.code, TaskErrorCode::NotFoundOrNotOwned);

    runner.reviewer_gate.notify_one();
    let _ = reviewer
        .wait(TaskId::from("reviewer"), Duration::from_secs(2))
        .await
        .unwrap();
    reviewer
        .resume_reviewer_verification(
            TaskId::from("subject"),
            ReviewerVerificationResume {
                reviewer_task_id: TaskId::from("reviewer"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        handle
            .inspect_admin(TaskId::from("subject"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Completed
    );
    handle.shutdown().await.unwrap();
    actor.await.unwrap();
}
