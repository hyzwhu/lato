use futures_util::future::BoxFuture;
use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, SessionId, TaskError, TaskErrorCode, TaskId,
    TaskOwner, TaskProgress, TaskResult, TaskScope, ToolCapability, TurnId,
};
use lato_runtime::{
    ACTIVE_MESSAGE_ADMISSION_TIMEOUT, ActiveMessageAdmission, ActiveMessageAdmissionLease,
    ActiveMessageDelivery, ActiveMessageOperation, ActiveMessageOutcome, ActiveMessageRequest,
    CoordinatorConfig, MAX_ACTIVE_MESSAGE_BYTES, NoopTaskEventSink, SpawnMode, SpawnTaskRequest,
    StartedTask, TaskChildControl, TaskHandle, TaskReporter, TaskRootRequest, TaskRunOutput,
    TaskRunRequest, TaskRunner, spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct AdmissionCall {
    delivery: ActiveMessageDelivery,
    release: oneshot::Sender<ActiveMessageAdmission>,
}

struct MessageControl {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    factory_behavior: FactoryBehavior,
    constructor_gate: Arc<ConstructorGate>,
}

#[derive(Clone, Copy, Default)]
enum FactoryBehavior {
    #[default]
    Normal,
    PanicOpen,
    PanicClaimed,
    Block,
    DropPanic,
}

struct DropPanicsFuture {
    response: oneshot::Receiver<ActiveMessageAdmission>,
}

impl Future for DropPanicsFuture {
    type Output = ActiveMessageAdmission;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.response)
            .poll(context)
            .map(|result| result.unwrap_or(ActiveMessageAdmission::ChannelClosed))
    }
}

impl Drop for DropPanicsFuture {
    fn drop(&mut self) {
        panic!("runner active-message future drop panic");
    }
}

#[derive(Default)]
struct ConstructorGate {
    entered: AtomicBool,
    released: StdMutex<bool>,
    wake: Condvar,
}

impl ConstructorGate {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl TaskChildControl for MessageControl {
    fn progress(&self) -> TaskProgress {
        TaskProgress::default()
    }

    fn send_active_message(
        &self,
        delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission> {
        let (release, response) = oneshot::channel();
        let _ = self.admissions.send(AdmissionCall {
            delivery: delivery.clone(),
            release,
        });
        match self.factory_behavior {
            FactoryBehavior::Normal => {}
            FactoryBehavior::PanicOpen => panic!("constructor panic while open"),
            FactoryBehavior::PanicClaimed => {
                delivery.commit_admission(|| panic!("constructor panic while claimed"));
                unreachable!()
            }
            FactoryBehavior::Block => self.constructor_gate.block(),
            FactoryBehavior::DropPanic => return Box::pin(DropPanicsFuture { response }),
        }
        Box::pin(async move {
            response
                .await
                .unwrap_or(ActiveMessageAdmission::ChannelClosed)
        })
    }

    fn cancel(&self) {}
}

struct MessageRunner {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    finishes: Mutex<HashMap<TaskId, Arc<Notify>>>,
    changed: Notify,
    factory_behavior: FactoryBehavior,
    constructor_gate: Arc<ConstructorGate>,
}

#[async_trait::async_trait]
impl TaskRunner for MessageRunner {
    type Control = MessageControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let finish = Arc::new(Notify::new());
        self.finishes
            .lock()
            .await
            .insert(request.node.id.clone(), Arc::clone(&finish));
        self.changed.notify_waiters();
        if reporter
            .started(StartedTask::new(
                Arc::new(MessageControl {
                    admissions: self.admissions.clone(),
                    factory_behavior: self.factory_behavior,
                    constructor_gate: Arc::clone(&self.constructor_gate),
                }),
                request.cancellation.clone(),
            ))
            .await
        {
            tokio::select! {
                _ = finish.notified() => {},
                _ = request.cancellation.cancelled() => {},
            }
        }
        TaskRunOutput::from(TaskResult {
            success: true,
            output: "done".into(),
            error: None,
            usage: Default::default(),
            duration_ms: 1,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    fn on_completed(&self, _: lato_runtime::TaskCompletion) {}
}

struct Harness {
    handle: TaskHandle,
    runner: Arc<MessageRunner>,
    admissions: Mutex<mpsc::UnboundedReceiver<AdmissionCall>>,
    _actor: tokio::task::JoinHandle<()>,
    _workspace: TempDir,
}

impl Harness {
    async fn new(config: CoordinatorConfig) -> Self {
        Self::with_behavior(config, FactoryBehavior::Normal).await
    }

    async fn with_behavior(config: CoordinatorConfig, factory_behavior: FactoryBehavior) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
        let (admission_tx, admissions) = mpsc::unbounded_channel();
        let runner = Arc::new(MessageRunner {
            admissions: admission_tx,
            finishes: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            factory_behavior,
            constructor_gate: Arc::new(ConstructorGate::default()),
        });
        let (handle, actor) = spawn_task_coordinator(
            config,
            Arc::clone(&runner),
            allocator,
            Arc::new(NoopTaskEventSink),
        );
        Self {
            handle,
            runner,
            admissions: Mutex::new(admissions),
            _actor: actor,
            _workspace: workspace,
        }
    }

    async fn root(&self, id: &str, session: &str) -> lato_runtime::ScopedTaskHandle {
        self.handle
            .register_root(TaskRootRequest {
                task_id: TaskId::from(id),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from(session),
                    turn_id: TurnId::from("turn"),
                },
                profile: AgentProfile::worker(),
                permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
                budget: BudgetLimits::unlimited(),
            })
            .await
            .unwrap()
    }

    async fn spawn(
        &self,
        parent: &lato_runtime::ScopedTaskHandle,
        id: &str,
    ) -> lato_runtime::ScopedTaskHandle {
        parent.spawn(spawn_request(id)).await.unwrap().handle
    }

    async fn wait_running(&self, id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self
                    .runner
                    .finishes
                    .lock()
                    .await
                    .contains_key(&TaskId::from(id))
                {
                    return;
                }
                self.runner.changed.notified().await;
            }
        })
        .await
        .unwrap();
    }

    async fn finish(&self, id: &str) {
        self.runner.finishes.lock().await[&TaskId::from(id)].notify_one();
    }

    async fn admission(&self) -> AdmissionCall {
        for _ in 0..100_000 {
            if let Ok(call) = self.admissions.lock().await.try_recv() {
                return call;
            }
            std::thread::yield_now();
            tokio::task::yield_now().await;
        }
        panic!("active-message constructor was not dispatched")
    }
}

fn spawn_request(id: &str) -> SpawnTaskRequest {
    SpawnTaskRequest {
        task_id: TaskId::from(id),
        scope: TaskScope {
            objective: id.into(),
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

async fn next_message_event(
    events: &mut tokio::sync::broadcast::Receiver<lato_runtime::TaskEventEnvelope>,
) -> lato_runtime::TaskEventEnvelope {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageAccepted { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageRejected { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
            ) {
                return event;
            }
        }
    })
    .await
    .unwrap()
}

#[test]
fn request_validation_uses_utf8_bytes_and_preserves_operations() {
    assert_eq!(
        ActiveMessageRequest::try_new(TaskId::from("child"), "").unwrap_err(),
        ActiveMessageOutcome::Limit {
            max_bytes: MAX_ACTIVE_MESSAGE_BYTES,
            observed_bytes: 0,
        }
    );
    let exact = "é".repeat(MAX_ACTIVE_MESSAGE_BYTES / 2);
    assert_eq!(
        ActiveMessageRequest::try_new(TaskId::from("child"), exact)
            .unwrap()
            .operation(),
        ActiveMessageOperation::Queue
    );
    let too_large = "é".repeat(MAX_ACTIVE_MESSAGE_BYTES / 2 + 1);
    assert!(matches!(
        ActiveMessageRequest::try_new_with_operation(
            TaskId::from("child"),
            too_large,
            ActiveMessageOperation::Steer
        ),
        Err(ActiveMessageOutcome::Limit { observed_bytes, .. })
            if observed_bytes == MAX_ACTIVE_MESSAGE_BYTES + 2
    ));
}

#[test]
fn admitted_requires_proven_synchronous_commit() {
    let lease = ActiveMessageAdmissionLease::new_for_test();
    assert!(lease.commit_admission(|| ()).is_some());
    assert!(lease.settle(ActiveMessageAdmission::Admitted));
    assert!(lease.commit_admission(|| ()).is_none());
}

#[tokio::test]
async fn queue_and_steer_are_delivered_with_bound_sender_identity() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    for operation in [ActiveMessageOperation::Queue, ActiveMessageOperation::Steer] {
        let send = tokio::spawn({
            let root = root.clone();
            async move {
                root.send_active_message(
                    ActiveMessageRequest::try_new_with_operation(
                        TaskId::from("child"),
                        "hello",
                        operation,
                    )
                    .unwrap(),
                )
                .await
            }
        });
        let call = harness.admission().await;
        assert_eq!(call.delivery.operation(), operation);
        assert_eq!(
            call.delivery.message().sender_session_id,
            SessionId::from("session")
        );
        assert!(call.delivery.commit_admission(|| ()).is_some());
        call.release.send(ActiveMessageAdmission::Admitted).unwrap();
        assert!(matches!(
            send.await.unwrap(),
            ActiveMessageOutcome::Accepted { .. }
        ));
    }
}

#[tokio::test]
async fn ownership_and_inactive_lifecycles_fail_closed() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 1,
        max_running_per_root: 1,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "running").await;
    harness.wait_running("running").await;
    harness.spawn(&root, "queued").await;
    let foreign = harness.root("foreign", "other").await;
    assert_eq!(
        foreign
            .send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("running"), "x").unwrap()
            )
            .await,
        ActiveMessageOutcome::NotFoundOrNotOwned
    );
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("queued"), "x").unwrap()
        )
        .await,
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("missing"), "x").unwrap()
        )
        .await,
        ActiveMessageOutcome::NotFoundOrNotOwned
    );
}

#[tokio::test]
async fn per_task_and_global_admission_are_independently_bounded() {
    let harness = Harness::new(CoordinatorConfig {
        active_message_capacity: 2,
        active_messages_per_task: 1,
        max_global_running: 2,
        max_running_per_root: 2,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "one").await;
    harness.spawn(&root, "two").await;
    harness.wait_running("one").await;
    harness.wait_running("two").await;
    let first = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "a").unwrap(),
            )
            .await
        }
    });
    let first_call = harness.admission().await;
    assert_eq!(
        root.send_active_message(ActiveMessageRequest::try_new(TaskId::from("one"), "b").unwrap())
            .await,
        ActiveMessageOutcome::Saturated { max_in_flight: 1 }
    );
    let second = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("two"), "a").unwrap(),
            )
            .await
        }
    });
    let second_call = harness.admission().await;
    assert_eq!(
        root.send_active_message(ActiveMessageRequest::try_new(TaskId::from("two"), "b").unwrap())
            .await,
        ActiveMessageOutcome::Saturated { max_in_flight: 2 }
    );
    for call in [first_call, second_call] {
        call.release.send(ActiveMessageAdmission::Rejected).unwrap();
    }
    assert_eq!(
        first.await.unwrap(),
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    assert_eq!(
        second.await.unwrap(),
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
}

#[tokio::test]
async fn global_saturation_flood_never_enters_or_pressures_the_command_channel() {
    let harness = Harness::new(CoordinatorConfig {
        command_capacity: 1,
        active_message_capacity: 1,
        active_messages_per_task: 1,
        cancel_grace: Duration::from_millis(5),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let mut events = harness.handle.subscribe();
    let held_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "held").unwrap(),
            )
            .await
        }
    });
    let held = harness.admission().await;
    for index in 0..256 {
        assert_eq!(
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), format!("saturated-{index}"),)
                    .unwrap(),
            )
            .await,
            ActiveMessageOutcome::Saturated { max_in_flight: 1 }
        );
    }
    assert_eq!(
        harness
            .handle
            .registry_counts()
            .await
            .unwrap()
            .dropped_active_message_rejections,
        255
    );
    tokio::time::timeout(
        Duration::from_millis(100),
        root.cancel_task(TaskId::from("child")),
    )
    .await
    .expect("saturation flood delayed cancellation through the command channel")
    .unwrap();
    assert_eq!(
        held_send.await.unwrap(),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline
    );
    drop(held.release);
    let event = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            let event = next_message_event(&mut events).await;
            if matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageRejected { ref error, .. }
                    if error.code == TaskErrorCode::MessageLimit
            ) {
                break event;
            }
        }
    })
    .await
    .expect("bounded saturation observation was not evented");
    assert_eq!(event.task_id, TaskId::from("child"));
}

#[tokio::test(start_paused = true)]
async fn timeout_revokes_open_lease_but_claimed_admission_is_uncertain() {
    for claimed in [false, true] {
        let harness = Harness::new(CoordinatorConfig::default()).await;
        let root = harness.root("root", "session").await;
        harness.spawn(&root, "child").await;
        harness.wait_running("child").await;
        let mut events = harness.handle.subscribe();
        let send = tokio::spawn({
            let root = root.clone();
            async move {
                root.send_active_message(
                    ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
                )
                .await
            }
        });
        let call = harness.admission().await;
        if claimed {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.delivery
                        .commit_admission(|| panic!("leave lease claimed"));
                }))
                .is_err()
            );
        }
        tokio::time::advance(ACTIVE_MESSAGE_ADMISSION_TIMEOUT).await;
        tokio::task::yield_now().await;
        let expected = if claimed {
            ActiveMessageOutcome::AdmissionUncertain
        } else {
            ActiveMessageOutcome::NotAcceptedBeforeDeadline
        };
        assert_eq!(send.await.unwrap(), expected);
        let event = next_message_event(&mut events).await;
        if claimed {
            assert!(matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
            ));
        } else {
            assert!(matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
                    if error.code == TaskErrorCode::TimedOut
            ));
        }
        assert!(call.delivery.commit_admission(|| ()).is_none());
    }
}

#[tokio::test]
async fn channel_close_and_unsupported_are_distinct_settled_results() {
    for (admission, expected) in [
        (
            ActiveMessageAdmission::ChannelClosed,
            ActiveMessageOutcome::ChannelClosed,
        ),
        (
            ActiveMessageAdmission::Unsupported,
            ActiveMessageOutcome::Unsupported,
        ),
    ] {
        let harness = Harness::new(CoordinatorConfig::default()).await;
        let root = harness.root("root", "session").await;
        harness.spawn(&root, "child").await;
        harness.wait_running("child").await;
        let send = tokio::spawn({
            let root = root.clone();
            async move {
                root.send_active_message(
                    ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
                )
                .await
            }
        });
        harness.admission().await.release.send(admission).unwrap();
        assert_eq!(send.await.unwrap(), expected);
    }
}

#[tokio::test]
async fn terminalization_waits_for_selected_admission_and_rejects_new_ones() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "first").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    harness.finish("child").await;
    tokio::task::yield_now().await;
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("child"), "late").unwrap()
        )
        .await,
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    assert_eq!(
        harness.handle.registry_counts().await.unwrap().finalizing,
        1
    );
    call.release.send(ActiveMessageAdmission::Rejected).unwrap();
    assert_eq!(
        send.await.unwrap(),
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    let outcome = harness
        .handle
        .wait_admin(TaskId::from("child"), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        matches!(outcome, lato_runtime::WaitOutcome::Finished(snapshot) if snapshot.result.as_ref().unwrap().success)
    );
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("child"), "after terminal").unwrap()
        )
        .await,
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
}

#[tokio::test]
async fn claimed_but_unsettled_admission_prevents_false_clean_completion() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call.delivery
                .commit_admission(|| panic!("leave lease claimed"));
        }))
        .is_err()
    );
    harness.finish("child").await;
    call.release.send(ActiveMessageAdmission::Rejected).unwrap();
    assert_eq!(
        send.await.unwrap(),
        ActiveMessageOutcome::AdmissionUncertain
    );
    let outcome = harness
        .handle
        .wait_admin(TaskId::from("child"), Duration::from_secs(1))
        .await
        .unwrap();
    let lato_runtime::WaitOutcome::Finished(snapshot) = outcome else {
        panic!("not terminal")
    };
    let result = snapshot.result.unwrap();
    assert!(!result.success);
    assert_eq!(
        result.error.unwrap().code,
        TaskErrorCode::AdmissionUncertain
    );
    assert_eq!(
        TaskErrorCode::AdmissionUncertain.as_str(),
        "task.active_message_uncertain"
    );
}

#[tokio::test]
async fn selected_admissions_are_independent_between_children() {
    let harness = Harness::new(CoordinatorConfig {
        max_global_running: 2,
        max_running_per_root: 2,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    for id in ["one", "two"] {
        harness.spawn(&root, id).await;
        harness.wait_running(id).await;
    }
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "x").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    harness.finish("one").await;
    harness.finish("two").await;
    let two = harness
        .handle
        .wait_admin(TaskId::from("two"), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(two, lato_runtime::WaitOutcome::Finished(_)));
    assert_eq!(
        harness.handle.registry_counts().await.unwrap().finalizing,
        1
    );
    call.release.send(ActiveMessageAdmission::Rejected).unwrap();
    let _ = send.await.unwrap();
}

#[tokio::test]
async fn cancellation_wins_during_finalization_for_open_and_claimed_admissions() {
    for claimed in [false, true] {
        let harness = Harness::new(CoordinatorConfig::default()).await;
        let root = harness.root("root", "session").await;
        harness.spawn(&root, "child").await;
        harness.wait_running("child").await;
        let mut events = harness.handle.subscribe();
        let send = tokio::spawn({
            let root = root.clone();
            async move {
                root.send_active_message(
                    ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
                )
                .await
            }
        });
        let call = harness.admission().await;
        if claimed {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.delivery.commit_admission(|| panic!("leave claimed"));
                }))
                .is_err()
            );
        }
        harness.finish("child").await;
        loop {
            if harness
                .handle
                .inspect_admin(TaskId::from("child"))
                .await
                .unwrap()
                .node
                .status
                == lato_core::TaskStatus::Finalizing
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        root.cancel_task(TaskId::from("child")).await.unwrap();
        let send_outcome = send.await.unwrap();
        assert_eq!(
            send_outcome,
            if claimed {
                ActiveMessageOutcome::AdmissionUncertain
            } else {
                ActiveMessageOutcome::NotAcceptedBeforeDeadline
            }
        );
        drop(call.release);
        let lato_runtime::WaitOutcome::Finished(snapshot) = harness
            .handle
            .wait_admin(TaskId::from("child"), Duration::from_secs(1))
            .await
            .unwrap()
        else {
            panic!("cancelled task did not terminalize")
        };
        assert_eq!(snapshot.node.status, lato_core::TaskStatus::Cancelled);
        assert!(snapshot.completion_disposition.unwrap().explicitly_killed);
        assert_eq!(
            snapshot.result.unwrap().error.unwrap().code,
            TaskErrorCode::Cancelled
        );
        let payloads: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| event.payload)
            .collect();
        let finalizing = payloads
            .iter()
            .position(|payload| matches!(payload, lato_runtime::TaskEventPayload::Finalizing))
            .unwrap();
        let cancellation = payloads
            .iter()
            .position(|payload| {
                matches!(
                    payload,
                    lato_runtime::TaskEventPayload::CancellationRequested
                )
            })
            .unwrap();
        let cancelled = payloads
            .iter()
            .position(|payload| matches!(payload, lato_runtime::TaskEventPayload::Cancelled))
            .unwrap();
        assert!(finalizing < cancellation && cancellation < cancelled);
        assert!(!payloads.iter().any(|payload| matches!(
            payload,
            lato_runtime::TaskEventPayload::Completed { .. }
                | lato_runtime::TaskEventPayload::Failed { .. }
        )));
    }
}

#[tokio::test(start_paused = true)]
async fn finalization_timeout_is_uncertain_and_cleans_both_capacity_counters() {
    let harness = Harness::new(CoordinatorConfig {
        active_message_capacity: 1,
        active_messages_per_task: 1,
        active_message_admission_timeout: Duration::from_secs(10),
        active_message_finalization_timeout: Duration::from_secs(1),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call.delivery.commit_admission(|| panic!("leave claimed"));
        }))
        .is_err()
    );
    harness.finish("child").await;
    tokio::task::yield_now().await;
    assert_eq!(
        harness
            .handle
            .inspect_admin(TaskId::from("child"))
            .await
            .unwrap()
            .node
            .status,
        lato_core::TaskStatus::Finalizing
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let lato_runtime::WaitOutcome::Finished(snapshot) = harness
        .handle
        .wait_admin(TaskId::from("child"), Duration::from_secs(1))
        .await
        .unwrap()
    else {
        panic!("finalization deadline did not terminalize")
    };
    assert_eq!(
        snapshot.result.unwrap().error.unwrap().code,
        TaskErrorCode::AdmissionUncertain
    );
    // The stale completion cannot affect terminal authority or per-task accounting.
    drop(call.release);
    assert_eq!(
        send.await.unwrap(),
        ActiveMessageOutcome::AdmissionUncertain
    );
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("child"), "after").unwrap()
        )
        .await,
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
}

#[tokio::test(start_paused = true)]
async fn finalization_deadline_retires_open_message_and_reuses_global_capacity_immediately() {
    let harness = Harness::new(CoordinatorConfig {
        active_message_capacity: 1,
        active_messages_per_task: 1,
        active_message_admission_timeout: Duration::from_secs(10),
        active_message_finalization_timeout: Duration::from_secs(1),
        max_global_running: 2,
        max_running_per_root: 2,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "first").await;
    harness.spawn(&root, "second").await;
    harness.wait_running("first").await;
    harness.wait_running("second").await;
    let mut events = harness.handle.subscribe();
    let first_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("first"), "held").unwrap(),
            )
            .await
        }
    });
    let held = harness.admission().await;
    harness.finish("first").await;
    loop {
        if harness
            .handle
            .inspect_admin(TaskId::from("first"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Finalizing
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        first_send.await.unwrap(),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline
    );
    let lato_runtime::WaitOutcome::Finished(first_snapshot) = harness
        .handle
        .wait_admin(TaskId::from("first"), Duration::from_secs(1))
        .await
        .unwrap()
    else {
        panic!("proof-revoked finalization did not finish")
    };
    assert!(first_snapshot.result.unwrap().success);

    let second_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("second"), "next").unwrap(),
            )
            .await
        }
    });
    let second = tokio::time::timeout(Duration::from_millis(50), harness.admission())
        .await
        .expect("retired message kept the only global worker or permit occupied");
    second
        .release
        .send(ActiveMessageAdmission::Rejected)
        .unwrap();
    assert_eq!(
        second_send.await.unwrap(),
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    drop(held.release);
    tokio::task::yield_now().await;
    let first_outcomes = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.task_id == TaskId::from("first"))
        .filter(|event| {
            matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageAccepted { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageRejected { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
            )
        })
        .count();
    assert_eq!(first_outcomes, 1);
}

#[tokio::test]
async fn blocking_constructor_never_blocks_actor_cancel_or_bounded_shutdown() {
    let harness = Harness::with_behavior(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(5),
            teardown_drain_timeout: Duration::from_millis(40),
            ..CoordinatorConfig::default()
        },
        FactoryBehavior::Block,
    )
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let mut events = harness.handle.subscribe();
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap(),
            )
            .await
        }
    });
    let _call = harness.admission().await;
    assert!(
        harness
            .runner
            .constructor_gate
            .entered
            .load(Ordering::Acquire)
    );
    tokio::time::timeout(Duration::from_millis(100), harness.handle.registry_counts())
        .await
        .expect("registry command blocked by constructor")
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(100),
        root.cancel_task(TaskId::from("child")),
    )
    .await
    .expect("cancel blocked by constructor")
    .unwrap();
    let shutdown = tokio::time::timeout(Duration::from_millis(200), harness.handle.shutdown())
        .await
        .expect("shutdown exceeded its global bound")
        .unwrap();
    assert!(!matches!(shutdown, lato_runtime::SinkShutdown::Drained));
    assert_eq!(send.await.unwrap(), ActiveMessageOutcome::ChannelClosed);
    let message_events = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.task_id == TaskId::from("child"))
        .filter(|event| {
            matches!(
                event.payload,
                lato_runtime::TaskEventPayload::ActiveMessageAccepted { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageRejected { .. }
                    | lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
            )
        })
        .count();
    assert_eq!(message_events, 1);
    harness.runner.constructor_gate.release();
}

#[tokio::test]
async fn constructor_panic_uses_lease_proof_and_keeps_terminalization_truthful() {
    for (behavior, expected, clean) in [
        (
            FactoryBehavior::PanicOpen,
            ActiveMessageOutcome::ChannelClosed,
            true,
        ),
        (
            FactoryBehavior::PanicClaimed,
            ActiveMessageOutcome::AdmissionUncertain,
            false,
        ),
    ] {
        let harness = Harness::with_behavior(CoordinatorConfig::default(), behavior).await;
        let root = harness.root("root", "session").await;
        harness.spawn(&root, "child").await;
        harness.wait_running("child").await;
        let outcome = root
            .send_active_message(ActiveMessageRequest::try_new(TaskId::from("child"), "x").unwrap())
            .await;
        assert_eq!(outcome, expected);
        harness.finish("child").await;
        let lato_runtime::WaitOutcome::Finished(snapshot) = harness
            .handle
            .wait_admin(TaskId::from("child"), Duration::from_secs(1))
            .await
            .unwrap()
        else {
            panic!("task did not terminalize")
        };
        assert_eq!(snapshot.result.unwrap().success, clean);
    }
}

#[tokio::test(start_paused = true)]
async fn future_drop_panic_is_contained_for_ready_timeout_and_cancel_paths() {
    let harness = Harness::with_behavior(
        CoordinatorConfig {
            active_message_admission_timeout: Duration::from_secs(1),
            max_global_running: 3,
            max_running_per_root: 3,
            ..CoordinatorConfig::default()
        },
        FactoryBehavior::DropPanic,
    )
    .await;
    let root = harness.root("root", "session").await;
    for id in ["ready", "timeout", "cancel"] {
        harness.spawn(&root, id).await;
        harness.wait_running(id).await;
    }

    let ready_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("ready"), "ready").unwrap(),
            )
            .await
        }
    });
    let ready = harness.admission().await;
    assert!(ready.delivery.commit_admission(|| ()).is_some());
    ready
        .release
        .send(ActiveMessageAdmission::Admitted)
        .unwrap();
    assert!(matches!(
        ready_send.await.unwrap(),
        ActiveMessageOutcome::Accepted { .. }
    ));

    let timeout_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("timeout"), "timeout").unwrap(),
            )
            .await
        }
    });
    let timeout_call = harness.admission().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        timeout_send.await.unwrap(),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline
    );
    drop(timeout_call.release);

    let cancel_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("cancel"), "cancel").unwrap(),
            )
            .await
        }
    });
    let cancel_call = harness.admission().await;
    root.cancel_task(TaskId::from("cancel")).await.unwrap();
    assert_eq!(
        cancel_send.await.unwrap(),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline
    );
    drop(cancel_call.release);
    tokio::time::timeout(Duration::from_millis(100), harness.handle.registry_counts())
        .await
        .expect("drop panic killed or wedged the actor")
        .unwrap();
}

#[tokio::test]
async fn future_drop_panic_is_contained_when_reply_drops_and_during_shutdown() {
    let harness = Harness::with_behavior(
        CoordinatorConfig {
            cancel_grace: Duration::from_millis(5),
            teardown_drain_timeout: Duration::from_millis(100),
            ..CoordinatorConfig::default()
        },
        FactoryBehavior::DropPanic,
    )
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "dropped-reply").await;
    harness.spawn(&root, "shutdown").await;
    harness.wait_running("dropped-reply").await;
    harness.wait_running("shutdown").await;
    let mut events = harness.handle.subscribe();

    let dropped_reply = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("dropped-reply"), "drop").unwrap(),
            )
            .await
        }
    });
    let dropped_call = harness.admission().await;
    dropped_reply.abort();
    dropped_call
        .release
        .send(ActiveMessageAdmission::Rejected)
        .unwrap();
    let dropped_event =
        tokio::time::timeout(Duration::from_millis(100), next_message_event(&mut events))
            .await
            .expect("drop-panicking future did not produce its plain completion event");
    assert_eq!(dropped_event.task_id, TaskId::from("dropped-reply"));
    tokio::time::timeout(Duration::from_millis(100), harness.handle.registry_counts())
        .await
        .expect("failed response delivery dropped a runner future on the actor")
        .unwrap();

    let shutdown_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("shutdown"), "shutdown").unwrap(),
            )
            .await
        }
    });
    let shutdown_call = harness.admission().await;
    let outcome = tokio::time::timeout(Duration::from_millis(250), harness.handle.shutdown())
        .await
        .expect("drop panic wedged shutdown")
        .unwrap();
    assert!(!matches!(outcome, lato_runtime::SinkShutdown::Drained) || shutdown_send.is_finished());
    assert_eq!(
        shutdown_send.await.unwrap(),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline
    );
    drop(shutdown_call.release);
}

#[tokio::test(start_paused = true)]
async fn reused_id_ignores_stale_generation_completion_and_has_fresh_counters() {
    let harness = Harness::new(CoordinatorConfig {
        max_completed: 1,
        max_global_running: 2,
        max_running_per_root: 2,
        active_message_capacity: 2,
        active_messages_per_task: 1,
        active_message_admission_timeout: Duration::from_secs(20),
        active_message_finalization_timeout: Duration::from_secs(1),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "reused").await;
    harness.wait_running("reused").await;
    let old_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("reused"), "old").unwrap(),
            )
            .await
        }
    });
    let old_call = harness.admission().await;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            old_call
                .delivery
                .commit_admission(|| panic!("leave old generation claimed"));
        }))
        .is_err()
    );
    let old_generation = old_call.delivery.generation();
    harness.finish("reused").await;
    loop {
        if harness
            .handle
            .inspect_admin(TaskId::from("reused"))
            .await
            .unwrap()
            .node
            .status
            == lato_core::TaskStatus::Finalizing
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        harness
            .handle
            .wait_admin(TaskId::from("reused"), Duration::from_millis(1))
            .await
            .unwrap(),
        lato_runtime::WaitOutcome::Finished(_)
    ));

    harness.spawn(&root, "evictor").await;
    harness.wait_running("evictor").await;
    harness.finish("evictor").await;
    assert!(matches!(
        harness
            .handle
            .wait_admin(TaskId::from("evictor"), Duration::from_secs(1))
            .await
            .unwrap(),
        lato_runtime::WaitOutcome::Finished(_)
    ));
    harness.spawn(&root, "reused").await;
    harness.wait_running("reused").await;

    assert!(
        old_call
            .release
            .send(ActiveMessageAdmission::Rejected)
            .is_err()
    );
    assert_eq!(
        old_send.await.unwrap(),
        ActiveMessageOutcome::AdmissionUncertain
    );

    let new_send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("reused"), "new").unwrap(),
            )
            .await
        }
    });
    let new_call = harness.admission().await;
    assert_ne!(new_call.delivery.generation(), old_generation);
    new_call
        .release
        .send(ActiveMessageAdmission::Rejected)
        .unwrap();
    assert_eq!(
        new_send.await.unwrap(),
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
}

#[tokio::test]
async fn every_actor_rejection_and_runner_outcome_emits_a_truthful_event() {
    let harness = Harness::new(CoordinatorConfig {
        active_message_capacity: 2,
        active_messages_per_task: 1,
        max_global_running: 3,
        max_running_per_root: 3,
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    for id in ["one", "two", "three"] {
        harness.spawn(&root, id).await;
        harness.wait_running(id).await;
    }
    let foreign = harness.root("foreign", "other").await;
    let mut events = harness.handle.subscribe();

    assert_eq!(
        foreign
            .send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "foreign").unwrap(),
            )
            .await,
        ActiveMessageOutcome::NotFoundOrNotOwned
    );
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
            if error.code == TaskErrorCode::NotFoundOrNotOwned
    ));

    let unsupported = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "unsupported").unwrap(),
            )
            .await
        }
    });
    harness
        .admission()
        .await
        .release
        .send(ActiveMessageAdmission::Unsupported)
        .unwrap();
    assert_eq!(
        unsupported.await.unwrap(),
        ActiveMessageOutcome::Unsupported
    );
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
            if error.code == TaskErrorCode::ActiveMessageUnsupported
    ));

    let accepted = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "accepted").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    assert!(call.delivery.commit_admission(|| ()).is_some());
    call.release.send(ActiveMessageAdmission::Admitted).unwrap();
    assert!(matches!(
        accepted.await.unwrap(),
        ActiveMessageOutcome::Accepted { .. }
    ));
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageAccepted { .. }
    ));

    let held_one = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("one"), "held-one").unwrap(),
            )
            .await
        }
    });
    let call_one = harness.admission().await;
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("one"), "per-task-full").unwrap(),
        )
        .await,
        ActiveMessageOutcome::Saturated { max_in_flight: 1 }
    );
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
            if error.code == TaskErrorCode::MessageLimit
    ));

    let held_two = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("two"), "held-two").unwrap(),
            )
            .await
        }
    });
    let call_two = harness.admission().await;
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("three"), "global-full").unwrap(),
        )
        .await,
        ActiveMessageOutcome::Saturated { max_in_flight: 2 }
    );
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
            if error.code == TaskErrorCode::MessageLimit
    ));

    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call_one
                .delivery
                .commit_admission(|| panic!("leave claimed"));
        }))
        .is_err()
    );
    call_one
        .release
        .send(ActiveMessageAdmission::Rejected)
        .unwrap();
    call_two
        .release
        .send(ActiveMessageAdmission::ChannelClosed)
        .unwrap();
    assert_eq!(
        held_one.await.unwrap(),
        ActiveMessageOutcome::AdmissionUncertain
    );
    assert_eq!(held_two.await.unwrap(), ActiveMessageOutcome::ChannelClosed);
    let first = next_message_event(&mut events).await.payload;
    let second = next_message_event(&mut events).await.payload;
    assert!(
        matches!(
            first,
            lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
        ) || matches!(
            second,
            lato_runtime::TaskEventPayload::ActiveMessageUncertain { .. }
        )
    );
    assert!(
        matches!(first, lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. } if error.code == TaskErrorCode::ActiveMessageChannelClosed)
            || matches!(second, lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. } if error.code == TaskErrorCode::ActiveMessageChannelClosed)
    );

    harness.finish("three").await;
    assert!(matches!(
        harness
            .handle
            .wait_admin(TaskId::from("three"), Duration::from_secs(1))
            .await
            .unwrap(),
        lato_runtime::WaitOutcome::Finished(_)
    ));
    assert_eq!(
        root.send_active_message(
            ActiveMessageRequest::try_new(TaskId::from("three"), "terminal").unwrap(),
        )
        .await,
        ActiveMessageOutcome::NotActiveOrFinalizing
    );
    assert!(matches!(
        next_message_event(&mut events).await.payload,
        lato_runtime::TaskEventPayload::ActiveMessageRejected { error, .. }
            if error.code == TaskErrorCode::ActiveMessageInactive
    ));
}

#[tokio::test]
async fn dropping_last_handles_with_an_active_admission_shuts_down_boundedly() {
    let harness = Harness::new(CoordinatorConfig {
        cancel_grace: Duration::from_millis(5),
        teardown_drain_timeout: Duration::from_millis(100),
        ..CoordinatorConfig::default()
    })
    .await;
    let root = harness.root("root", "session").await;
    harness.spawn(&root, "child").await;
    harness.wait_running("child").await;
    let send = tokio::spawn({
        let root = root.clone();
        async move {
            root.send_active_message(
                ActiveMessageRequest::try_new(TaskId::from("child"), "held").unwrap(),
            )
            .await
        }
    });
    let call = harness.admission().await;
    send.abort();
    let _ = send.await;
    let Harness {
        handle,
        runner: _,
        admissions: _,
        _actor: actor,
        _workspace: workspace,
    } = harness;
    drop(root);
    drop(handle);
    tokio::time::timeout(Duration::from_millis(300), actor)
        .await
        .expect("last-handle shutdown exceeded its bound")
        .unwrap();
    drop(call);
    drop(workspace);
}
