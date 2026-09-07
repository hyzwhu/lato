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
use std::{collections::HashMap, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct AdmissionCall {
    delivery: ActiveMessageDelivery,
    release: oneshot::Sender<ActiveMessageAdmission>,
}

struct MessageControl {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
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
        let workspace = tempfile::tempdir().unwrap();
        let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
        let (admission_tx, admissions) = mpsc::unbounded_channel();
        let runner = Arc::new(MessageRunner {
            admissions: admission_tx,
            finishes: Mutex::new(HashMap::new()),
            changed: Notify::new(),
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
        tokio::time::timeout(Duration::from_secs(2), self.admissions.lock().await.recv())
            .await
            .unwrap()
            .unwrap()
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

#[tokio::test(start_paused = true)]
async fn timeout_revokes_open_lease_but_claimed_admission_is_uncertain() {
    for claimed in [false, true] {
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
