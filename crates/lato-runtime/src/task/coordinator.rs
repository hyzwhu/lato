// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: bounded Tokio actor with a single committed root transition path

use crate::task::admission::{AdmissionDecision, decide};
use crate::task::queue::{QueuedTask, SpawnQueue};
use crate::task::spawn::{
    PendingSpawnOccupancy, reserve, validate_profile_authority, validate_structure,
};
use crate::task::state::{CoordinatorState, RuntimeTaskRecord};
use crate::task::{
    CoordinatorConfig, InspectCaller, RunnerEvent, ScopedTaskHandle, SinkShutdown,
    SpawnDisposition, SpawnTaskRequest, TaskCallbackKind, TaskChildControl, TaskCommand,
    TaskCommandSender, TaskCompletion, TaskEventEnvelope, TaskEventPayload, TaskEventSink,
    TaskHandle, TaskReporter, TaskRunRequest, TaskRunner, coordinator_closed, root_node,
};
use futures_util::{
    FutureExt, StreamExt,
    future::{AbortHandle as FutureAbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use lato_core::{
    BudgetAccount, TaskError, TaskErrorCode, TaskId, TaskMachine, TaskNode, TaskStatus,
};
use lato_workspace::{WorkspaceAllocator, WorkspaceRequest};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

struct PendingSpawn {
    root_id: TaskId,
    parent_id: TaskId,
    request: SpawnTaskRequest,
    reply: oneshot::Sender<Result<SpawnDisposition, TaskError>>,
}

struct ProfileValidation {
    task_id: TaskId,
    result: Result<(), TaskError>,
}

enum TaskJobExit {
    WorkspaceAllocated(TaskId, lato_workspace::WorkspaceLease),
    Completed(TaskId, crate::task::TaskRunOutput),
    WorkspaceAllocationFailed(TaskId, TaskError),
    LeaseReleased(TaskId, lato_core::LeaseId),
    LeaseReleaseFailed(TaskId, TaskError),
    Aborted(TaskId, JobPhase),
}

#[derive(Clone, Copy)]
enum JobPhase {
    Preparing,
    Running,
    Cleanup,
}

enum OwnedAbortHandle {
    Future(FutureAbortHandle),
    Tokio(tokio::task::AbortHandle),
}

impl OwnedAbortHandle {
    fn abort(&self) {
        match self {
            Self::Future(handle) => handle.abort(),
            Self::Tokio(handle) => handle.abort(),
        }
    }
}

struct ShutdownState {
    replies: Vec<oneshot::Sender<SinkShutdown>>,
    deadline: Instant,
}

enum CallbackWork<C: TaskChildControl> {
    Cancel {
        task_id: TaskId,
        control: Arc<C>,
    },
    Completed {
        task_id: TaskId,
        completion: TaskCompletion,
    },
}

impl<C: TaskChildControl> CallbackWork<C> {
    fn task_id(&self) -> &TaskId {
        match self {
            Self::Cancel { task_id, .. } | Self::Completed { task_id, .. } => task_id,
        }
    }

    fn kind(&self) -> TaskCallbackKind {
        match self {
            Self::Cancel { .. } => TaskCallbackKind::Cancel,
            Self::Completed { .. } => TaskCallbackKind::Completed,
        }
    }
}

struct CallbackOutcome {
    task_id: TaskId,
    kind: TaskCallbackKind,
    error: Option<TaskError>,
}

pub struct TaskCoordinator<R: TaskRunner, A: WorkspaceAllocator> {
    config: CoordinatorConfig,
    runner: Arc<R>,
    workspace_allocator: Arc<A>,
    command_rx: mpsc::Receiver<TaskCommand>,
    command_channel_closed: bool,
    _internal_tx: mpsc::Sender<RunnerEvent<R::Control>>,
    internal_rx: mpsc::Receiver<RunnerEvent<R::Control>>,
    event_tx: broadcast::Sender<TaskEventEnvelope>,
    sink_tx: Option<mpsc::Sender<TaskEventEnvelope>>,
    sink_drained: Option<tokio::sync::oneshot::Receiver<()>>,
    sink_worker: Option<std::thread::JoinHandle<()>>,
    dropped_sink_events: u64,
    callback_tx: Option<std::sync::mpsc::SyncSender<CallbackWork<R::Control>>>,
    callback_rx: mpsc::UnboundedReceiver<CallbackOutcome>,
    callback_drained: Option<oneshot::Receiver<()>>,
    callback_worker: Option<std::thread::JoinHandle<()>>,
    dropped_callback_work: u64,
    callback_execution_failures: u64,
    state: CoordinatorState,
    queue: SpawnQueue,
    controls: HashMap<TaskId, crate::task::TaskControl<R::Control>>,
    jobs: FuturesUnordered<BoxFuture<'static, TaskJobExit>>,
    job_aborts: HashMap<TaskId, OwnedAbortHandle>,
    cancel_deadlines: HashMap<TaskId, Instant>,
    validations: FuturesUnordered<BoxFuture<'static, ProfileValidation>>,
    validation_aborts: HashMap<TaskId, tokio::task::AbortHandle>,
    pending_spawns: HashMap<TaskId, PendingSpawn>,
    validation_order: VecDeque<TaskId>,
    validation_results: HashMap<TaskId, Result<(), TaskError>>,
    cleanup_inflight: HashSet<TaskId>,
    pending_completions: HashMap<TaskId, TaskCompletion>,
    shutdown: Option<ShutdownState>,
    weak_handle: TaskHandle,
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
        command_tx: TaskCommandSender::Strong(command_tx),
        event_tx: event_tx.clone(),
    };
    let weak_handle = handle.downgrade();
    let (callback_tx, callback_work_rx) = std::sync::mpsc::sync_channel(config.callback_capacity);
    let (callback_result_tx, callback_rx) = mpsc::unbounded_channel();
    let (callback_drained_tx, callback_drained) = oneshot::channel();
    let callback_worker = spawn_callback_dispatcher(
        Arc::clone(&runner),
        callback_work_rx,
        callback_result_tx,
        callback_drained_tx,
    );
    let coordinator = TaskCoordinator {
        queue: SpawnQueue::new(config.max_queue),
        config,
        runner,
        workspace_allocator,
        command_rx,
        command_channel_closed: false,
        _internal_tx: internal_tx,
        internal_rx,
        event_tx,
        sink_tx: Some(sink_tx),
        sink_drained: Some(sink_drained),
        sink_worker: Some(sink_worker),
        dropped_sink_events: 0,
        callback_tx: Some(callback_tx),
        callback_rx,
        callback_drained: Some(callback_drained),
        callback_worker: Some(callback_worker),
        dropped_callback_work: 0,
        callback_execution_failures: 0,
        state: CoordinatorState::default(),
        controls: HashMap::new(),
        jobs: FuturesUnordered::new(),
        job_aborts: HashMap::new(),
        cancel_deadlines: HashMap::new(),
        validations: FuturesUnordered::new(),
        validation_aborts: HashMap::new(),
        pending_spawns: HashMap::new(),
        validation_order: VecDeque::new(),
        validation_results: HashMap::new(),
        cleanup_inflight: HashSet::new(),
        pending_completions: HashMap::new(),
        shutdown: None,
        weak_handle,
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
            if self.shutdown.is_some() && self.jobs.is_empty() && self.validations.is_empty() {
                let callbacks_drained = self.shutdown_callbacks().await;
                while let Ok(outcome) = self.callback_rx.try_recv() {
                    self.handle_callback_outcome(outcome);
                }
                let sink_outcome = self.shutdown_sink().await;
                let unreleased_leases = self
                    .state
                    .tasks
                    .values()
                    .filter(|record| record.workspace_lease.is_some())
                    .count();
                let outcome = if unreleased_leases == 0 {
                    if callbacks_drained && sink_outcome == SinkShutdown::Drained {
                        SinkShutdown::Drained
                    } else {
                        SinkShutdown::TimedOutDetached
                    }
                } else {
                    SinkShutdown::CleanupIncomplete {
                        unreleased_leases,
                        sink_drained: sink_outcome == SinkShutdown::Drained,
                        callbacks_drained,
                    }
                };
                if let Some(shutdown) = self.shutdown.take() {
                    for reply in shutdown.replies {
                        let _ = reply.send(outcome);
                    }
                }
                break;
            }
            tokio::select! {
                command = self.command_rx.recv(), if !self.command_channel_closed => {
                    if let Some(command) = command {
                        self.handle_command(command).await;
                    } else {
                        self.command_channel_closed = true;
                        if self.shutdown.is_none() {
                            self.begin_shutdown(None).await;
                        }
                    }
                }
                event = self.internal_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_runner_event(event).await;
                    }
                }
                validation = self.validations.next(), if !self.validations.is_empty() => {
                    if let Some(validation) = validation {
                        self.finish_profile_validation(validation);
                    }
                }
                job = self.jobs.next(), if !self.jobs.is_empty() => {
                    if let Some(job) = job {
                        self.finish_job(job).await;
                    }
                }
                callback = self.callback_rx.recv() => {
                    if let Some(callback) = callback {
                        self.handle_callback_outcome(callback);
                    }
                }
                _ = reap.tick() => self.reap_cancelled().await,
            }
        }
    }

    async fn handle_command(&mut self, command: TaskCommand) {
        match command {
            TaskCommand::RegisterRoot { request, reply } => {
                let result = if self.shutdown.is_some() {
                    Err(coordinator_closed())
                } else {
                    self.register_root(*request)
                };
                let _ = reply.send(result);
            }
            TaskCommand::Spawn {
                root_id,
                parent_id,
                request,
                reply,
            } => {
                self.begin_spawn(root_id, parent_id, *request, reply);
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
                let _ = reply.send(self.state.counts(
                    self.dropped_sink_events,
                    self.dropped_callback_work,
                    self.callback_execution_failures,
                ));
            }
            TaskCommand::ShutdownRoot { root_id, reply } => {
                let result = self.shutdown_root(&root_id);
                let _ = reply.send(result);
            }
            TaskCommand::Shutdown { reply } => {
                self.begin_shutdown(Some(reply)).await;
            }
        }
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
                spawn_admission_closed: false,
                depth: 0,
                cleanup_error: None,
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
        let root = self
            .state
            .tasks
            .get_mut(root_id)
            .expect("registered root must have a runtime record");
        root.spawn_admission_closed = true;
        root.node.status = lato_core::TaskStatus::Cancelled;
        self.commit_transition(root_id.clone(), TaskEventPayload::RootClosed);
        Ok(())
    }

    fn begin_spawn(
        &mut self,
        root_id: TaskId,
        parent_id: TaskId,
        request: SpawnTaskRequest,
        reply: oneshot::Sender<Result<SpawnDisposition, TaskError>>,
    ) {
        if self.shutdown.is_some() {
            let _ = reply.send(Err(coordinator_closed()));
            return;
        }
        let pending_duplicate = self.pending_spawns.contains_key(&request.task_id);
        let pending_child_count = self
            .pending_spawns
            .values()
            .filter(|pending| pending.parent_id == parent_id)
            .count();
        let structure = match validate_structure(
            &self.state,
            &self.config,
            &root_id,
            &parent_id,
            &request,
            PendingSpawnOccupancy {
                duplicate: pending_duplicate,
                tasks: self.pending_spawns.len(),
                children: pending_child_count,
            },
        ) {
            Ok(structure) => structure,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if let AdmissionDecision::Reject(error) = decide(
            &self.config,
            self.state.running_count(),
            self.state.running_count_for_root(&root_id),
            self.queue.len(),
        ) {
            let _ =
                reply.send(self.retain_admission_rejection(parent_id, request, structure, error));
            return;
        }
        let task_id = request.task_id.clone();
        let profile = request.profile.clone();
        let runner = Arc::clone(&self.runner);
        let timeout = self.config.profile_validation_timeout;
        self.pending_spawns.insert(
            task_id.clone(),
            PendingSpawn {
                root_id,
                parent_id,
                request,
                reply,
            },
        );
        self.validation_order.push_back(task_id.clone());
        let validation_task = tokio::spawn(async move {
            let validation =
                std::panic::AssertUnwindSafe(runner.validate_profile(&profile)).catch_unwind();
            match tokio::time::timeout(timeout, validation).await {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(error))) => Err(TaskError::new(
                    TaskErrorCode::InvalidProfile,
                    format!("task profile validation failed: {error}"),
                )),
                Ok(Err(_)) => Err(TaskError::new(
                    TaskErrorCode::RunnerPanic,
                    "task runner panicked during profile validation",
                )),
                Err(_) => Err(TaskError::new(
                    TaskErrorCode::InvalidProfile,
                    "task profile validation timed out",
                )),
            }
        });
        self.validation_aborts
            .insert(task_id.clone(), validation_task.abort_handle());
        self.validations.push(Box::pin(async move {
            let result = match validation_task.await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Err(coordinator_closed()),
                Err(_) => Err(TaskError::new(
                    TaskErrorCode::RunnerPanic,
                    "task profile validation task terminated unexpectedly",
                )),
            };
            ProfileValidation { task_id, result }
        }));
    }

    fn finish_profile_validation(&mut self, validation: ProfileValidation) {
        self.validation_aborts.remove(&validation.task_id);
        self.validation_results
            .insert(validation.task_id, validation.result);
        while let Some(task_id) = self.validation_order.front() {
            let Some(validation) = self.validation_results.remove(task_id) else {
                break;
            };
            let task_id = self
                .validation_order
                .pop_front()
                .expect("validation order front exists");
            let Some(pending) = self.pending_spawns.remove(&task_id) else {
                continue;
            };
            let result = self.finish_spawn(
                pending.root_id,
                pending.parent_id,
                pending.request,
                validation,
            );
            let _ = pending.reply.send(result);
        }
    }

    fn finish_spawn(
        &mut self,
        root_id: TaskId,
        parent_id: TaskId,
        request: SpawnTaskRequest,
        profile_validation: Result<(), TaskError>,
    ) -> Result<SpawnDisposition, TaskError> {
        if self.shutdown.is_some() {
            return Err(coordinator_closed());
        }
        let response_handle = self.weak_handle.upgrade().ok_or_else(coordinator_closed)?;
        let structure = validate_structure(
            &self.state,
            &self.config,
            &root_id,
            &parent_id,
            &request,
            PendingSpawnOccupancy::default(),
        )?;
        let admission = decide(
            &self.config,
            self.state.running_count(),
            self.state.running_count_for_root(&root_id),
            self.queue.len(),
        );
        if let AdmissionDecision::Reject(error) = &admission {
            return self.retain_admission_rejection(parent_id, request, structure, error.clone());
        }
        profile_validation?;
        validate_profile_authority(&self.state.tasks[&parent_id].node.profile, &request.profile)?;
        let (permissions, effective_budget, reservation) =
            reserve(&mut self.state, &parent_id, &request)?;
        let task_id = request.task_id.clone();
        let initial_status = match admission {
            AdmissionDecision::Start => TaskStatus::Preparing,
            AdmissionDecision::Enqueue => TaskStatus::Queued,
            AdmissionDecision::Reject(_) => unreachable!("admission rejection handled above"),
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
                budget: BudgetAccount::new(effective_budget),
                workspace_lease: None,
                reservation: Some(reservation),
                reservation_parent_id: Some(parent_id),
                cancellation: request.cancellation,
                spawn_admission_closed: false,
                depth: structure.depth,
                cleanup_error: None,
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
            AdmissionDecision::Reject(_) => unreachable!("admission rejection handled above"),
        }

        Ok(SpawnDisposition {
            task_id: task_id.clone(),
            status: initial_status,
            handle: ScopedTaskHandle::new(root_id, task_id, response_handle),
        })
    }

    fn retain_admission_rejection(
        &mut self,
        parent_id: TaskId,
        request: SpawnTaskRequest,
        structure: crate::task::spawn::SpawnStructure,
        error: TaskError,
    ) -> Result<SpawnDisposition, TaskError> {
        let permissions = self.state.tasks[&parent_id].node.permissions.clone();
        let task_id = request.task_id.clone();
        self.state.tasks.insert(
            task_id.clone(),
            RuntimeTaskRecord {
                node: TaskNode {
                    id: task_id.clone(),
                    parent_id: Some(parent_id),
                    root_id: structure.root_id,
                    owner: structure.owner,
                    profile: request.profile.clone(),
                    scope: request.scope,
                    status: TaskStatus::Failed,
                    permissions,
                    workspace_intent: request.profile.workspace,
                    result_contract: request.result_contract,
                },
                budget: BudgetAccount::new(lato_core::BudgetLimits::limited(
                    lato_core::BudgetAmount::ZERO,
                )),
                workspace_lease: None,
                reservation: None,
                reservation_parent_id: None,
                cancellation: request.cancellation,
                spawn_admission_closed: true,
                depth: structure.depth,
                cleanup_error: None,
                last_event_sequence: 0,
            },
        );
        self.commit_transition(
            task_id,
            TaskEventPayload::AdmissionRejected {
                error: error.clone(),
            },
        );
        Err(error)
    }

    fn launch_preparation(&mut self, task_id: TaskId) {
        let record = self
            .state
            .tasks
            .get(&task_id)
            .expect("preparing task remains registered");
        let node = record.node.clone();
        let allocator = Arc::clone(&self.workspace_allocator);
        let job_task_id = task_id.clone();
        let future = async move {
            let allocation = allocator.allocate(WorkspaceRequest::new(
                task_id.clone(),
                node.workspace_intent,
            ));
            match std::panic::AssertUnwindSafe(allocation)
                .catch_unwind()
                .await
            {
                Ok(Ok(lease)) => TaskJobExit::WorkspaceAllocated(task_id, lease),
                Ok(Err(error)) => TaskJobExit::WorkspaceAllocationFailed(task_id, error),
                Err(_) => TaskJobExit::WorkspaceAllocationFailed(
                    task_id,
                    TaskError::new(
                        TaskErrorCode::WorkspaceAllocation,
                        "workspace allocator panicked during task preparation",
                    ),
                ),
            }
        };
        self.push_job(job_task_id, JobPhase::Preparing, future);
    }

    fn launch_runner(&mut self, task_id: TaskId) {
        let record = self
            .state
            .tasks
            .get(&task_id)
            .expect("prepared task remains registered");
        let node = record.node.clone();
        let cancellation = record.cancellation.clone();
        let lease = record
            .workspace_lease
            .clone()
            .expect("runner starts only after actor accepts a workspace lease");
        let runner = Arc::clone(&self.runner);
        let event_tx = self._internal_tx.clone();
        let scoped_handle = ScopedTaskHandle::new(
            node.root_id.clone(),
            task_id.clone(),
            self.weak_handle.clone(),
        );
        let job_task_id = task_id.clone();
        let future = async move {
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
            TaskJobExit::Completed(task_id, output)
        };
        self.push_job(job_task_id, JobPhase::Running, future);
    }

    fn push_job(
        &mut self,
        task_id: TaskId,
        phase: JobPhase,
        future: impl std::future::Future<Output = TaskJobExit> + Send + 'static,
    ) {
        let (abort, registration) = FutureAbortHandle::new_pair();
        self.job_aborts
            .insert(task_id.clone(), OwnedAbortHandle::Future(abort));
        self.jobs.push(Box::pin(async move {
            match Abortable::new(future, registration).await {
                Ok(exit) => exit,
                Err(_) => TaskJobExit::Aborted(task_id, phase),
            }
        }));
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
        if status == TaskStatus::Verifying || status.is_terminal() {
            record.spawn_admission_closed = true;
        }
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
        let completion = TaskCompletion {
            task_id: task_id.clone(),
            result: output.result,
        };
        self.pending_completions.insert(task_id.clone(), completion);
        self.cleanup_terminal(&task_id).await;
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
    }

    async fn cleanup_terminal(&mut self, task_id: &TaskId) {
        self.controls.remove(task_id);
        if self.cleanup_inflight.contains(task_id) {
            return;
        }
        let lease = self
            .state
            .tasks
            .get(task_id)
            .and_then(|record| record.workspace_lease.clone());
        if let Some(lease) = lease {
            self.launch_lease_cleanup(task_id.clone(), lease);
            return;
        }
        self.release_reservation(task_id);
        self.finish_terminal_cleanup(task_id);
    }

    fn launch_lease_cleanup(&mut self, task_id: TaskId, lease: lato_workspace::WorkspaceLease) {
        self.cleanup_inflight.insert(task_id.clone());
        let allocator = Arc::clone(&self.workspace_allocator);
        let lease_id = lease.id.clone();
        let timeout = self.config.teardown_drain_timeout;
        let cleanup_task_id = task_id.clone();
        let cleanup_task = tokio::spawn(async move {
            match tokio::time::timeout(timeout, allocator.release(&lease)).await {
                Ok(Ok(())) => TaskJobExit::LeaseReleased(cleanup_task_id, lease_id),
                Ok(Err(error)) => TaskJobExit::LeaseReleaseFailed(cleanup_task_id, error),
                Err(_) => TaskJobExit::LeaseReleaseFailed(
                    cleanup_task_id,
                    TaskError::new(
                        TaskErrorCode::WorkspaceRelease,
                        "workspace release timed out",
                    ),
                ),
            }
        });
        self.job_aborts.insert(
            task_id.clone(),
            OwnedAbortHandle::Tokio(cleanup_task.abort_handle()),
        );
        self.jobs.push(Box::pin(async move {
            match cleanup_task.await {
                Ok(exit) => exit,
                Err(error) if error.is_panic() => TaskJobExit::LeaseReleaseFailed(
                    task_id,
                    TaskError::new(
                        TaskErrorCode::WorkspaceRelease,
                        "workspace allocator panicked during lease release",
                    ),
                ),
                Err(_) => TaskJobExit::Aborted(task_id, JobPhase::Cleanup),
            }
        }));
    }

    fn finish_terminal_cleanup(&mut self, task_id: &TaskId) {
        if let Some(completion) = self.pending_completions.remove(task_id) {
            self.dispatch_callback(CallbackWork::Completed {
                task_id: task_id.clone(),
                completion,
            });
        }
        if self.shutdown.is_none() {
            self.promote_queue();
        }
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
        let cancelled = self.queue.remove_matching(|queued| {
            self.state.tasks.get(&queued.task_id).is_none_or(|record| {
                record.node.status != TaskStatus::Queued || record.cancellation.is_cancelled()
            })
        });
        for queued in cancelled {
            if self.state.tasks.get(&queued.task_id).is_some_and(|record| {
                record.node.status == TaskStatus::Queued && record.cancellation.is_cancelled()
            }) {
                self.set_status(&queued.task_id, TaskStatus::Cancelled);
                self.commit_transition(queued.task_id.clone(), TaskEventPayload::Cancelled);
                self.release_reservation(&queued.task_id);
            }
        }
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
        let newly_cancelled: Vec<_> = self
            .state
            .tasks
            .iter()
            .filter(|(task_id, record)| {
                !record.node.status.is_terminal()
                    && record.cancellation.is_cancelled()
                    && !self.cancel_deadlines.contains_key(*task_id)
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in newly_cancelled {
            self.request_cancel(task_id).await;
        }
        let now = Instant::now();
        let expired: Vec<_> = self
            .cancel_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in expired {
            if let Some(abort) = self.job_aborts.get(&task_id) {
                abort.abort();
            }
        }
        if let Some(shutdown) = &self.shutdown
            && shutdown.deadline <= now
        {
            for abort in self.job_aborts.values() {
                abort.abort();
            }
        }
    }

    async fn request_cancel(&mut self, task_id: TaskId) {
        let status = self.state.tasks[&task_id].node.status;
        let record = self
            .state
            .tasks
            .get_mut(&task_id)
            .expect("cancel target remains registered");
        record.spawn_admission_closed = true;
        record.cancellation.cancel();
        if status == TaskStatus::Queued {
            self.queue
                .remove_matching(|queued| queued.task_id == task_id);
            self.set_status(&task_id, TaskStatus::Cancelled);
            self.commit_transition(task_id.clone(), TaskEventPayload::Cancelled);
            self.cleanup_terminal(&task_id).await;
            return;
        }
        if let Some(control) = self.controls.get(&task_id) {
            self.dispatch_callback(CallbackWork::Cancel {
                task_id: task_id.clone(),
                control: Arc::clone(control.child()),
            });
        }
        self.commit_transition(task_id.clone(), TaskEventPayload::CancellationRequested);
        self.cancel_deadlines
            .insert(task_id, Instant::now() + self.config.cancel_grace);
    }

    async fn finish_job(&mut self, exit: TaskJobExit) {
        let task_id = match &exit {
            TaskJobExit::WorkspaceAllocated(task_id, _)
            | TaskJobExit::Completed(task_id, _)
            | TaskJobExit::WorkspaceAllocationFailed(task_id, _)
            | TaskJobExit::LeaseReleased(task_id, _)
            | TaskJobExit::LeaseReleaseFailed(task_id, _)
            | TaskJobExit::Aborted(task_id, _) => task_id.clone(),
        };
        self.job_aborts.remove(&task_id);
        match &exit {
            TaskJobExit::LeaseReleased(_, lease_id) => {
                self.cleanup_inflight.remove(&task_id);
                let record = self
                    .state
                    .tasks
                    .get_mut(&task_id)
                    .expect("cleanup completion references retained task");
                record.workspace_lease = None;
                record.cleanup_error = None;
                self.commit_transition(
                    task_id.clone(),
                    TaskEventPayload::WorkspaceLeaseReleased {
                        lease_id: lease_id.clone(),
                    },
                );
                self.release_reservation(&task_id);
                self.finish_terminal_cleanup(&task_id);
                return;
            }
            TaskJobExit::LeaseReleaseFailed(_, error) => {
                self.finish_failed_lease_cleanup(&task_id, error.clone());
                return;
            }
            TaskJobExit::Aborted(_, JobPhase::Cleanup) => {
                self.finish_failed_lease_cleanup(
                    &task_id,
                    TaskError::new(
                        TaskErrorCode::WorkspaceRelease,
                        "workspace release was aborted at the shutdown deadline",
                    ),
                );
                return;
            }
            _ => {}
        }
        let cancellation_requested = self.cancel_deadlines.remove(&task_id).is_some()
            || self
                .state
                .tasks
                .get(&task_id)
                .is_some_and(|record| record.cancellation.is_cancelled());
        if self
            .state
            .tasks
            .get(&task_id)
            .is_none_or(|record| record.node.status.is_terminal())
        {
            return;
        }
        if let TaskJobExit::WorkspaceAllocated(_, lease) = &exit {
            let lease_id = lease.id.clone();
            let record = self
                .state
                .tasks
                .get_mut(&task_id)
                .expect("allocated lease remains actor-owned");
            record.workspace_lease = Some(lease.clone());
            self.commit_transition(
                task_id.clone(),
                TaskEventPayload::WorkspaceLeaseAllocated { lease_id },
            );
            if cancellation_requested {
                self.set_status(&task_id, TaskStatus::Cancelled);
                self.commit_transition(task_id.clone(), TaskEventPayload::Cancelled);
                self.cleanup_terminal(&task_id).await;
            } else {
                self.launch_runner(task_id);
            }
            return;
        }
        if cancellation_requested {
            self.set_status(&task_id, TaskStatus::Cancelled);
            self.commit_transition(task_id.clone(), TaskEventPayload::Cancelled);
            self.cleanup_terminal(&task_id).await;
        } else {
            match exit {
                TaskJobExit::WorkspaceAllocated(_, _) => unreachable!("handled above"),
                TaskJobExit::Completed(_, output) => self.complete_task(task_id, output).await,
                TaskJobExit::WorkspaceAllocationFailed(_, error) => {
                    self.fail_task(task_id, error).await;
                }
                TaskJobExit::LeaseReleased(_, _) | TaskJobExit::LeaseReleaseFailed(_, _) => {
                    unreachable!("cleanup exits handled above")
                }
                TaskJobExit::Aborted(_, _) => {
                    self.fail_task(
                        task_id,
                        TaskError::new(
                            TaskErrorCode::RunnerInitialization,
                            "task execution ended without a completion result",
                        ),
                    )
                    .await;
                }
            }
        }
    }

    fn finish_failed_lease_cleanup(&mut self, task_id: &TaskId, error: TaskError) {
        self.cleanup_inflight.remove(task_id);
        let lease_id = self
            .state
            .tasks
            .get(task_id)
            .and_then(|record| record.workspace_lease.as_ref())
            .map(|lease| lease.id.clone())
            .expect("failed cleanup retains its lease authority");
        self.state
            .tasks
            .get_mut(task_id)
            .expect("cleanup failure references retained task")
            .cleanup_error = Some(error.clone());
        self.commit_transition(
            task_id.clone(),
            TaskEventPayload::WorkspaceLeaseReleaseFailed { lease_id, error },
        );
        self.finish_terminal_cleanup(task_id);
    }

    async fn begin_shutdown(&mut self, reply: Option<oneshot::Sender<SinkShutdown>>) {
        if let Some(shutdown) = &mut self.shutdown {
            if let Some(reply) = reply {
                shutdown.replies.push(reply);
            }
            return;
        }
        let mut replies = Vec::new();
        if let Some(reply) = reply {
            replies.push(reply);
        }
        self.shutdown = Some(ShutdownState {
            replies,
            deadline: Instant::now() + self.config.teardown_drain_timeout,
        });
        self.command_rx.close();
        self.command_channel_closed = true;

        for abort in self.validation_aborts.values() {
            abort.abort();
        }
        for (_, pending) in self.pending_spawns.drain() {
            let _ = pending.reply.send(Err(coordinator_closed()));
        }
        self.validation_order.clear();
        self.validation_results.clear();

        let live: Vec<_> = self
            .state
            .tasks
            .iter()
            .filter(|(_, record)| {
                record.node.parent_id.is_some() && !record.node.status.is_terminal()
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in live {
            self.request_cancel(task_id).await;
        }
        self.retry_terminal_cleanup().await;
    }

    async fn retry_terminal_cleanup(&mut self) {
        let retry_cleanup: Vec<_> = self
            .state
            .tasks
            .iter()
            .filter(|(_, record)| {
                record.node.status.is_terminal() && record.workspace_lease.is_some()
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in retry_cleanup {
            self.cleanup_terminal(&task_id).await;
        }
    }

    async fn handle_runner_event(&mut self, event: RunnerEvent<R::Control>) {
        match event {
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
                    self.dispatch_callback(CallbackWork::Cancel {
                        task_id: task_id.clone(),
                        control: Arc::clone(started.control.child()),
                    });
                }
                let _ = acknowledgement.send(accepted);
            }
            RunnerEvent::Usage { task_id, usage } => {
                let _ = (task_id, usage);
            }
            RunnerEvent::Progress { task_id, progress } => {
                let _ = (task_id, progress);
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

    fn dispatch_callback(&mut self, work: CallbackWork<R::Control>) {
        let task_id = work.task_id().clone();
        let kind = work.kind();
        let result = match self.callback_tx.as_ref() {
            Some(callback_tx) => callback_tx.try_send(work),
            None => Err(std::sync::mpsc::TrySendError::Disconnected(work)),
        };
        if let Err(error) = result {
            let reason = match error {
                std::sync::mpsc::TrySendError::Full(_) => {
                    "task callback dispatcher capacity is exhausted"
                }
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    "task callback dispatcher is closed"
                }
            };
            self.record_callback_dispatch_failure(
                task_id,
                kind,
                TaskError::new(TaskErrorCode::RunnerProtocolViolation, reason),
            );
        }
    }

    fn handle_callback_outcome(&mut self, outcome: CallbackOutcome) {
        if let Some(error) = outcome.error {
            self.callback_execution_failures = self.callback_execution_failures.saturating_add(1);
            if self.state.tasks.contains_key(&outcome.task_id) {
                self.commit_transition(
                    outcome.task_id,
                    TaskEventPayload::CallbackExecutionFailed {
                        callback: outcome.kind,
                        error,
                    },
                );
            }
        }
    }

    fn record_callback_dispatch_failure(
        &mut self,
        task_id: TaskId,
        callback: TaskCallbackKind,
        error: TaskError,
    ) {
        self.dropped_callback_work = self.dropped_callback_work.saturating_add(1);
        if self.state.tasks.contains_key(&task_id) {
            self.commit_transition(
                task_id,
                TaskEventPayload::CallbackDispatchFailed { callback, error },
            );
        }
    }

    async fn shutdown_callbacks(&mut self) -> bool {
        self.callback_tx.take();
        let Some(drained) = self.callback_drained.take() else {
            return true;
        };
        if !matches!(
            tokio::time::timeout(self.config.teardown_drain_timeout, drained).await,
            Ok(Ok(()))
        ) {
            self.callback_worker.take();
            return false;
        }
        if let Some(worker) = self.callback_worker.take() {
            let _ = worker.join();
        }
        true
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

fn spawn_callback_dispatcher<R: TaskRunner>(
    runner: Arc<R>,
    callback_rx: std::sync::mpsc::Receiver<CallbackWork<R::Control>>,
    result_tx: mpsc::UnboundedSender<CallbackOutcome>,
    drained: oneshot::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lato-task-callbacks".into())
        .spawn(move || {
            while let Ok(work) = callback_rx.recv() {
                let task_id = work.task_id().clone();
                let kind = work.kind();
                let callback = || match work {
                    CallbackWork::Cancel { control, .. } => control.cancel(),
                    CallbackWork::Completed { completion, .. } => runner.on_completed(completion),
                };
                let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
                    .err()
                    .map(|_| {
                        TaskError::new(TaskErrorCode::RunnerPanic, "task runner callback panicked")
                    });
                let _ = result_tx.send(CallbackOutcome {
                    task_id,
                    kind,
                    error,
                });
            }
            let _ = drained.send(());
        })
        .expect("task callback dispatcher thread must start")
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
