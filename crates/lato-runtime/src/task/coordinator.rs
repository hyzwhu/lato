// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: bounded Tokio actor with a single committed root transition path

use crate::task::admission::{AdmissionDecision, decide};
use crate::task::queue::{QueuedTask, SpawnQueue};
use crate::task::spawn::{reserve, validate_structure};
use crate::task::state::{CoordinatorState, RuntimeTaskRecord};
use crate::task::{
    CoordinatorConfig, InspectCaller, RunnerEvent, ScopedTaskHandle, SinkShutdown,
    SpawnDisposition, SpawnTaskRequest, TaskChildControl, TaskCommand, TaskCompletion,
    TaskEventEnvelope, TaskEventPayload, TaskEventSink, TaskHandle, TaskReporter, TaskRunRequest,
    TaskRunner, root_node,
};
use futures_util::FutureExt;
use lato_core::{
    BudgetAccount, TaskError, TaskErrorCode, TaskId, TaskMachine, TaskNode, TaskStatus,
};
use lato_workspace::{WorkspaceAllocator, WorkspaceRequest};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

pub struct TaskCoordinator<R: TaskRunner, A: WorkspaceAllocator> {
    config: CoordinatorConfig,
    runner: Arc<R>,
    workspace_allocator: Arc<A>,
    command_rx: mpsc::Receiver<TaskCommand>,
    _internal_tx: mpsc::Sender<RunnerEvent<R::Control>>,
    internal_rx: mpsc::Receiver<RunnerEvent<R::Control>>,
    event_tx: broadcast::Sender<TaskEventEnvelope>,
    sink_tx: Option<mpsc::Sender<TaskEventEnvelope>>,
    sink_drained: Option<tokio::sync::oneshot::Receiver<()>>,
    sink_worker: Option<std::thread::JoinHandle<()>>,
    dropped_sink_events: u64,
    state: CoordinatorState,
    queue: SpawnQueue,
    controls: HashMap<TaskId, crate::task::TaskControl<R::Control>>,
    handle: TaskHandle,
    sequence: u64,
}

pub fn spawn_task_coordinator<R, A>(
    config: CoordinatorConfig,
    runner: Arc<R>,
    workspace_allocator: Arc<A>,
    event_sink: Arc<dyn TaskEventSink>,
) -> (TaskHandle, JoinHandle<()>)
where
    R: TaskRunner,
    A: WorkspaceAllocator,
{
    config.assert_valid();
    let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
    let (internal_tx, internal_rx) = mpsc::channel(config.command_capacity);
    let (event_tx, _) = broadcast::channel(config.event_capacity);
    let (sink_tx, sink_rx) = mpsc::channel(config.event_capacity);
    let (sink_drained_tx, sink_drained) = tokio::sync::oneshot::channel();
    let sink_worker = spawn_sink_dispatcher(event_sink, sink_rx, sink_drained_tx);
    let handle = TaskHandle {
        command_tx,
        event_tx: event_tx.clone(),
    };
    let coordinator = TaskCoordinator {
        queue: SpawnQueue::new(config.max_queue),
        config,
        runner,
        workspace_allocator,
        command_rx,
        _internal_tx: internal_tx,
        internal_rx,
        event_tx,
        sink_tx: Some(sink_tx),
        sink_drained: Some(sink_drained),
        sink_worker: Some(sink_worker),
        dropped_sink_events: 0,
        state: CoordinatorState::default(),
        controls: HashMap::new(),
        handle: handle.clone(),
        sequence: 0,
    };
    let actor = tokio::spawn(coordinator.run());
    (handle, actor)
}

impl<R: TaskRunner, A: WorkspaceAllocator> TaskCoordinator<R, A> {
    pub async fn run(mut self) {
        let mut reap = tokio::time::interval(self.config.queued_reap_interval);
        reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    if self.handle_command(command).await { break; }
                }
                event = self.internal_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_runner_event(event).await;
                    }
                }
                _ = reap.tick() => self.reap_cancelled().await,
            }
        }
    }

    async fn handle_command(&mut self, command: TaskCommand) -> bool {
        match command {
            TaskCommand::RegisterRoot { request, reply } => {
                let result = self.register_root(*request);
                let _ = reply.send(result);
            }
            TaskCommand::Spawn {
                root_id,
                parent_id,
                request,
                reply,
            } => {
                let result = self.spawn(root_id, parent_id, *request).await;
                let _ = reply.send(result);
            }
            TaskCommand::Inspect {
                task_id: target_task_id,
                caller,
                reply,
            } => {
                let authorized = match caller {
                    InspectCaller::Admin => true,
                    InspectCaller::Scoped {
                        root_id,
                        task_id: requester_task_id,
                    } => self.state.is_self_or_descendant(
                        &root_id,
                        &requester_task_id,
                        &target_task_id,
                    ),
                };
                let result = authorized
                    .then(|| self.state.inspection(&target_task_id))
                    .flatten()
                    .ok_or_else(not_found);
                let _ = reply.send(result);
            }
            TaskCommand::RegistryCounts { reply } => {
                let _ = reply.send(self.state.counts(self.dropped_sink_events));
            }
            TaskCommand::ShutdownRoot { root_id, reply } => {
                let result = self.shutdown_root(&root_id);
                let _ = reply.send(result);
            }
            TaskCommand::Shutdown { reply } => {
                let outcome = self.shutdown_sink().await;
                let _ = reply.send(outcome);
                return true;
            }
        }
        false
    }

    fn register_root(&mut self, request: crate::task::TaskRootRequest) -> Result<(), TaskError> {
        if self.state.contains(&request.task_id) {
            return Err(TaskError::new(
                TaskErrorCode::DuplicateTask,
                "task identifier is already registered",
            ));
        }
        if self.state.tasks.len() >= self.config.max_total_tasks {
            return Err(TaskError::new(
                TaskErrorCode::RetentionLimit,
                "task registry capacity is exhausted",
            ));
        }
        let task_id = request.task_id.clone();
        self.state.roots.insert(task_id.clone());
        self.state.tasks.insert(
            task_id.clone(),
            RuntimeTaskRecord {
                node: root_node(&request),
                budget: BudgetAccount::new(request.budget),
                workspace_lease: None,
                reservation: None,
                reservation_parent_id: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                depth: 0,
                last_event_sequence: 0,
            },
        );
        self.commit_transition(task_id, TaskEventPayload::RootRegistered);
        Ok(())
    }

    fn shutdown_root(&mut self, root_id: &TaskId) -> Result<(), TaskError> {
        if !self.state.roots.contains(root_id) {
            return Err(not_found());
        }
        let has_descendants = self
            .state
            .tasks
            .values()
            .any(|record| &record.node.root_id == root_id && &record.node.id != root_id);
        if has_descendants {
            return Err(TaskError::new(
                TaskErrorCode::RunnerProtocolViolation,
                "root cannot close while descendants remain registered",
            ));
        }
        self.state.roots.remove(root_id);
        self.state
            .tasks
            .get_mut(root_id)
            .expect("registered root must have a runtime record")
            .node
            .status = lato_core::TaskStatus::Cancelled;
        self.commit_transition(root_id.clone(), TaskEventPayload::RootClosed);
        Ok(())
    }

    async fn spawn(
        &mut self,
        root_id: TaskId,
        parent_id: TaskId,
        request: SpawnTaskRequest,
    ) -> Result<SpawnDisposition, TaskError> {
        let structure =
            validate_structure(&self.state, &self.config, &root_id, &parent_id, &request)?;
        let admission = decide(
            &self.config,
            self.state.running_count(),
            self.state.running_count_for_root(&root_id),
            self.queue.len(),
        );
        match std::panic::AssertUnwindSafe(self.runner.validate_profile(&request.profile))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(TaskError::new(
                    TaskErrorCode::InvalidProfile,
                    format!("task profile validation failed: {error}"),
                ));
            }
            Err(_) => {
                return Err(TaskError::new(
                    TaskErrorCode::RunnerPanic,
                    "task runner panicked during profile validation",
                ));
            }
        }
        let (permissions, reservation) = reserve(&mut self.state, &parent_id, &request)?;
        let task_id = request.task_id.clone();
        let initial_status = match admission {
            AdmissionDecision::Start => TaskStatus::Preparing,
            AdmissionDecision::Enqueue => TaskStatus::Queued,
            AdmissionDecision::Reject(_) => TaskStatus::Failed,
        };
        let node = TaskNode {
            id: task_id.clone(),
            parent_id: Some(parent_id.clone()),
            root_id: structure.root_id,
            owner: structure.owner,
            profile: request.profile.clone(),
            scope: request.scope.clone(),
            status: initial_status,
            permissions,
            workspace_intent: request.profile.workspace,
            result_contract: request.result_contract.clone(),
        };
        self.state.tasks.insert(
            task_id.clone(),
            RuntimeTaskRecord {
                node,
                budget: BudgetAccount::new(request.budget),
                workspace_lease: None,
                reservation: Some(reservation),
                reservation_parent_id: Some(parent_id),
                cancellation: request.cancellation,
                depth: structure.depth,
                last_event_sequence: 0,
            },
        );
        self.commit_transition(task_id.clone(), TaskEventPayload::SpawnAccepted);

        match admission {
            AdmissionDecision::Start => {
                self.commit_transition(task_id.clone(), TaskEventPayload::Preparing);
                self.launch_preparation(task_id.clone());
            }
            AdmissionDecision::Enqueue => {
                self.queue.push_back(QueuedTask {
                    task_id: task_id.clone(),
                    root_id: root_id.clone(),
                })?;
                self.commit_transition(task_id.clone(), TaskEventPayload::Queued);
            }
            AdmissionDecision::Reject(error) => {
                self.release_reservation(&task_id);
                self.commit_transition(
                    task_id,
                    TaskEventPayload::AdmissionRejected {
                        error: error.clone(),
                    },
                );
                return Err(error);
            }
        }

        Ok(SpawnDisposition {
            task_id: task_id.clone(),
            status: initial_status,
            handle: ScopedTaskHandle::new(root_id, task_id, self.handle.clone()),
        })
    }

    fn launch_preparation(&self, task_id: TaskId) {
        let record = self
            .state
            .tasks
            .get(&task_id)
            .expect("preparing task remains registered");
        let node = record.node.clone();
        let cancellation = record.cancellation.clone();
        let allocator = Arc::clone(&self.workspace_allocator);
        let runner = Arc::clone(&self.runner);
        let event_tx = self._internal_tx.clone();
        let scoped_handle =
            ScopedTaskHandle::new(node.root_id.clone(), task_id.clone(), self.handle.clone());
        tokio::spawn(async move {
            let lease = match allocator
                .allocate(WorkspaceRequest::new(
                    task_id.clone(),
                    node.workspace_intent,
                ))
                .await
            {
                Ok(lease) => lease,
                Err(error) => {
                    let _ = event_tx
                        .send(RunnerEvent::WorkspaceAllocationFailed { task_id, error })
                        .await;
                    return;
                }
            };
            let (acknowledgement, response) = tokio::sync::oneshot::channel();
            if event_tx
                .send(RunnerEvent::WorkspaceAllocated {
                    task_id: task_id.clone(),
                    lease: lease.clone(),
                    acknowledgement,
                })
                .await
                .is_err()
                || !response.await.unwrap_or(false)
            {
                let _ = allocator.release(&lease).await;
                return;
            }
            let reporter = TaskReporter::new(task_id.clone(), event_tx.clone());
            let run = runner.run(
                TaskRunRequest {
                    node,
                    workspace_lease: lease,
                    scoped_handle,
                    cancellation,
                },
                reporter,
            );
            let output = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
                Ok(output) => output,
                Err(_) => crate::task::TaskRunOutput::from(lato_core::TaskResult {
                    success: false,
                    output: String::new(),
                    error: Some(TaskError::new(
                        TaskErrorCode::RunnerPanic,
                        "task runner panicked while executing child task",
                    )),
                    usage: Default::default(),
                    duration_ms: 0,
                    output_ref: None,
                }),
            };
            let _ = event_tx
                .send(RunnerEvent::Completed { task_id, output })
                .await;
        });
    }

    fn is_live_preparing(&self, task_id: &TaskId) -> bool {
        self.state.tasks.get(task_id).is_some_and(|record| {
            record.node.status == TaskStatus::Preparing && !record.cancellation.is_cancelled()
        })
    }

    fn set_status(&mut self, task_id: &TaskId, status: TaskStatus) {
        let record = self
            .state
            .tasks
            .get_mut(task_id)
            .expect("status transition must target a registered task");
        let mut machine = TaskMachine::new(record.node.status);
        machine
            .transition(status)
            .expect("coordinator only commits valid task transitions");
        record.node.status = status;
    }

    async fn complete_task(&mut self, task_id: TaskId, output: crate::task::TaskRunOutput) {
        let Some(status) = self
            .state
            .tasks
            .get(&task_id)
            .map(|record| record.node.status)
        else {
            return;
        };
        if status.is_terminal() {
            return;
        }
        if status != TaskStatus::Running {
            self.fail_task(
                task_id,
                TaskError::new(
                    TaskErrorCode::RunnerProtocolViolation,
                    "runner completed before startup acknowledgement",
                ),
            )
            .await;
            return;
        }
        self.set_status(&task_id, TaskStatus::Verifying);
        self.commit_transition(task_id.clone(), TaskEventPayload::VerificationStarted);
        if output.result.success {
            self.set_status(&task_id, TaskStatus::Completed);
            self.commit_transition(
                task_id.clone(),
                TaskEventPayload::Completed {
                    result: output.result.clone(),
                },
            );
        } else {
            self.set_status(&task_id, TaskStatus::Failed);
            let error = output.result.error.clone().unwrap_or_else(|| {
                TaskError::new(
                    TaskErrorCode::RunnerProtocolViolation,
                    "runner failed without error",
                )
            });
            self.commit_transition(task_id.clone(), TaskEventPayload::Failed { error });
        }
        self.runner.on_completed(TaskCompletion {
            task_id: task_id.clone(),
            result: output.result,
        });
        self.cleanup_terminal(&task_id).await;
        self.promote_queue();
    }

    async fn fail_task(&mut self, task_id: TaskId, error: TaskError) {
        let Some(status) = self
            .state
            .tasks
            .get(&task_id)
            .map(|record| record.node.status)
        else {
            return;
        };
        if status.is_terminal() {
            return;
        }
        self.set_status(&task_id, TaskStatus::Failed);
        self.commit_transition(task_id.clone(), TaskEventPayload::Failed { error });
        self.cleanup_terminal(&task_id).await;
        self.promote_queue();
    }

    async fn cleanup_terminal(&mut self, task_id: &TaskId) {
        self.controls.remove(task_id);
        let lease = self
            .state
            .tasks
            .get_mut(task_id)
            .and_then(|record| record.workspace_lease.take());
        if let Some(lease) = lease {
            let lease_id = lease.id.clone();
            let _ = self.workspace_allocator.release(&lease).await;
            self.commit_transition(
                task_id.clone(),
                TaskEventPayload::WorkspaceLeaseReleased { lease_id },
            );
        }
        self.release_reservation(task_id);
    }

    fn release_reservation(&mut self, task_id: &TaskId) {
        let reservation = self
            .state
            .tasks
            .get_mut(task_id)
            .and_then(|record| record.reservation.take());
        let parent_id = self
            .state
            .tasks
            .get(task_id)
            .and_then(|record| record.reservation_parent_id.clone());
        if let (Some(reservation), Some(parent_id)) = (reservation, parent_id) {
            let _ = self
                .state
                .tasks
                .get_mut(&parent_id)
                .expect("reservation parent remains retained")
                .budget
                .release(reservation);
        }
    }

    fn promote_queue(&mut self) {
        let mut global = self.state.running_count();
        let mut roots = HashMap::<TaskId, usize>::new();
        for root_id in &self.state.roots {
            roots.insert(root_id.clone(), self.state.running_count_for_root(root_id));
        }
        let startable = self.queue.drain_startable(|queued| {
            let root_count = roots.entry(queued.root_id.clone()).or_default();
            if global >= self.config.max_global_running
                || *root_count >= self.config.max_running_per_root
            {
                return false;
            }
            global += 1;
            *root_count += 1;
            true
        });
        for queued in startable {
            if self.state.tasks.get(&queued.task_id).is_some_and(|record| {
                record.node.status == TaskStatus::Queued && !record.cancellation.is_cancelled()
            }) {
                self.set_status(&queued.task_id, TaskStatus::Preparing);
                self.commit_transition(queued.task_id.clone(), TaskEventPayload::Preparing);
                self.launch_preparation(queued.task_id);
            }
        }
    }

    async fn reap_cancelled(&mut self) {
        let cancelled: Vec<_> = self
            .state
            .tasks
            .iter()
            .filter(|(_, record)| {
                !record.node.status.is_terminal() && record.cancellation.is_cancelled()
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        if cancelled.is_empty() {
            return;
        }
        self.queue
            .remove_matching(|queued| cancelled.contains(&queued.task_id));
        for task_id in cancelled {
            self.cancel_one(task_id).await;
        }
        self.promote_queue();
    }

    async fn cancel_one(&mut self, task_id: TaskId) {
        let status = self.state.tasks[&task_id].node.status;
        if status == TaskStatus::Running
            && let Some(control) = self.controls.remove(&task_id)
        {
            control.child().cancel();
        }
        self.set_status(&task_id, TaskStatus::Cancelled);
        self.commit_transition(task_id.clone(), TaskEventPayload::Cancelled);
        self.cleanup_terminal(&task_id).await;
    }

    async fn handle_runner_event(&mut self, event: RunnerEvent<R::Control>) {
        match event {
            RunnerEvent::WorkspaceAllocated {
                task_id,
                lease,
                acknowledgement,
            } => {
                let accepted = self.state.tasks.get(&task_id).is_some_and(|record| {
                    record.node.status == TaskStatus::Preparing
                        && !record.cancellation.is_cancelled()
                });
                if accepted {
                    let lease_id = lease.id.clone();
                    self.state
                        .tasks
                        .get_mut(&task_id)
                        .expect("checked record")
                        .workspace_lease = Some(lease);
                    self.commit_transition(
                        task_id,
                        TaskEventPayload::WorkspaceLeaseAllocated { lease_id },
                    );
                }
                let _ = acknowledgement.send(accepted);
            }
            RunnerEvent::WorkspaceAllocationFailed { task_id, error } => {
                if self.is_live_preparing(&task_id) {
                    self.fail_task(task_id, error).await;
                }
            }
            RunnerEvent::Started {
                task_id,
                started,
                acknowledgement,
            } => {
                let accepted = self.is_live_preparing(&task_id);
                if accepted {
                    self.controls.insert(task_id.clone(), started.control);
                    self.set_status(&task_id, TaskStatus::Running);
                    self.commit_transition(task_id, TaskEventPayload::Started);
                } else {
                    started.control.child().cancel();
                }
                let _ = acknowledgement.send(accepted);
            }
            RunnerEvent::Usage { task_id, usage } => {
                let _ = (task_id, usage);
            }
            RunnerEvent::Progress { task_id, progress } => {
                let _ = (task_id, progress);
            }
            RunnerEvent::Completed { task_id, output } => {
                if self.state.tasks.get(&task_id).is_some_and(|record| {
                    !record.node.status.is_terminal() && record.cancellation.is_cancelled()
                }) {
                    self.cancel_one(task_id).await;
                    self.promote_queue();
                } else {
                    self.complete_task(task_id, output).await;
                }
            }
        }
    }

    fn commit_transition(
        &mut self,
        task_id: TaskId,
        payload: TaskEventPayload,
    ) -> TaskEventEnvelope {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("task event sequence overflow");
        let record = self
            .state
            .tasks
            .get_mut(&task_id)
            .expect("committed task transition must reference a registered task");
        record.last_event_sequence = self.sequence;
        let envelope = TaskEventEnvelope::new(self.sequence, &record.node, payload);
        let _ = self.event_tx.send(envelope.clone());
        if self
            .sink_tx
            .as_ref()
            .is_none_or(|sink_tx| sink_tx.try_send(envelope.clone()).is_err())
        {
            self.dropped_sink_events = self.dropped_sink_events.saturating_add(1);
        }
        envelope
    }

    async fn shutdown_sink(&mut self) -> SinkShutdown {
        self.sink_tx.take();
        let Some(drained) = self.sink_drained.take() else {
            return SinkShutdown::Drained;
        };
        if !matches!(
            tokio::time::timeout(self.config.teardown_drain_timeout, drained).await,
            Ok(Ok(()))
        ) {
            self.sink_worker.take();
            return SinkShutdown::TimedOutDetached;
        }
        if let Some(worker) = self.sink_worker.take() {
            let _ = worker.join();
        }
        SinkShutdown::Drained
    }
}

fn spawn_sink_dispatcher(
    event_sink: Arc<dyn TaskEventSink>,
    mut sink_rx: mpsc::Receiver<TaskEventEnvelope>,
    drained: tokio::sync::oneshot::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lato-task-event-sink".into())
        .spawn(move || {
            while let Some(event) = sink_rx.blocking_recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    event_sink.on_event(event);
                }));
            }
            drop(sink_rx);
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(event_sink))).is_err()
            {
                return;
            }
            let _ = drained.send(());
        })
        .expect("task event sink dispatcher thread must start")
}

fn not_found() -> TaskError {
    TaskError::new(
        TaskErrorCode::NotFoundOrNotOwned,
        "task was not found in the requested scope",
    )
}
