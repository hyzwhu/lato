// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/active_message.rs
// License: Apache-2.0
// Lato changes: bounded Tokio actor with a single committed root transition path

use crate::task::admission::{AdmissionDecision, decide};
use crate::task::cancel::{
    resolve_cancellation, resolve_root_teardown, target_contains_task, target_matches_live,
};
use crate::task::query::{BlockingWaiter, ForegroundWaiter, caller_owns, inspection};
use crate::task::queue::{QueuedTask, SpawnQueue};
use crate::task::spawn::{
    PendingSpawnOccupancy, reserve, validate_profile_authority, validate_structure,
};
use crate::task::state::{CoordinatorState, RuntimeTaskRecord};
use crate::task::{
    ACTIVE_MESSAGE_ADMISSION_TIMEOUT, ACTIVE_MESSAGE_FINALIZATION_TIMEOUT, ActiveMessage,
    ActiveMessageAdmissionLease, ActiveMessageCompletion, ActiveMessageFuture,
    ActiveMessageOutcome, ActiveMessageRequest, CancelOutcome, CancelTarget, CompletionDisposition,
    CoordinatorConfig, InspectCaller, OutputMetadata, RunnerEvent, ScopedTaskHandle, SinkShutdown,
    SpawnDisposition, SpawnMode, SpawnTaskRequest, TaskCallbackKind, TaskChildControl, TaskCommand,
    TaskCommandSender, TaskCompletion, TaskEventEnvelope, TaskEventPayload, TaskEventSink,
    TaskHandle, TaskReporter, TaskRunOutput, TaskRunRequest, TaskRunner, WaitOutcome,
    coordinator_closed, root_node,
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
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
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
    reply: PendingSpawnReply,
    enqueued_at: Instant,
}

enum PendingSpawnReply {
    Spawn(oneshot::Sender<Result<SpawnDisposition, TaskError>>),
    SpawnAndWait(oneshot::Sender<Result<CompletionDisposition, TaskError>>),
}

impl PendingSpawnReply {
    fn is_closed(&self) -> bool {
        match self {
            Self::Spawn(reply) => reply.is_closed(),
            Self::SpawnAndWait(reply) => reply.is_closed(),
        }
    }
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

struct PendingTerminalOutput {
    output: TaskRunOutput,
    deadline: Instant,
}

struct DrainWaiter {
    target: CancelTarget,
    outcome: CancelOutcome,
    reply: oneshot::Sender<Result<CancelOutcome, TaskError>>,
    deadline: Instant,
    reopen_on_backstop: bool,
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
    Progress {
        task_id: TaskId,
        control: Arc<C>,
    },
}

impl<C: TaskChildControl> CallbackWork<C> {
    fn task_id(&self) -> &TaskId {
        match self {
            Self::Cancel { task_id, .. }
            | Self::Completed { task_id, .. }
            | Self::Progress { task_id, .. } => task_id,
        }
    }

    fn kind(&self) -> TaskCallbackKind {
        match self {
            Self::Cancel { .. } => TaskCallbackKind::Cancel,
            Self::Completed { .. } => TaskCallbackKind::Completed,
            Self::Progress { .. } => TaskCallbackKind::Progress,
        }
    }
}

struct CallbackOutcome {
    task_id: TaskId,
    kind: TaskCallbackKind,
    error: Option<TaskError>,
    progress: Option<lato_core::TaskProgress>,
}

struct OutputLoadWork {
    output_ref: String,
    inspection: crate::task::TaskInspection,
    reply: oneshot::Sender<Result<crate::task::TaskInspection, TaskError>>,
    timeout: std::time::Duration,
    max_bytes: usize,
}

enum OutputLoadEvent {
    Reply {
        reply: oneshot::Sender<Result<crate::task::TaskInspection, TaskError>>,
        result: Box<Result<crate::task::TaskInspection, TaskError>>,
    },
    SlotReleased,
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
    progress_poll_inflight: HashSet<TaskId>,
    next_progress_poll: Instant,
    state: CoordinatorState,
    queue: SpawnQueue,
    controls: HashMap<TaskId, crate::task::TaskControl<R::Control>>,
    jobs: FuturesUnordered<BoxFuture<'static, TaskJobExit>>,
    active_messages: FuturesUnordered<ActiveMessageFuture>,
    job_aborts: HashMap<TaskId, OwnedAbortHandle>,
    cancel_deadlines: HashMap<TaskId, Instant>,
    administrative_cancel_pending: HashSet<TaskId>,
    drain_waiters: Vec<DrainWaiter>,
    abandoned_teardown_roots: HashSet<TaskId>,
    cancellation_batch_active: bool,
    validations: FuturesUnordered<BoxFuture<'static, ProfileValidation>>,
    validation_aborts: HashMap<TaskId, tokio::task::AbortHandle>,
    pending_spawns: HashMap<TaskId, PendingSpawn>,
    validation_order: VecDeque<TaskId>,
    validation_results: HashMap<TaskId, Result<(), TaskError>>,
    cleanup_inflight: HashSet<TaskId>,
    pending_completions: HashMap<TaskId, TaskCompletion>,
    pending_terminal_outputs: HashMap<TaskId, PendingTerminalOutput>,
    waiters: HashMap<TaskId, Vec<BlockingWaiter>>,
    foreground_waiters: HashMap<TaskId, ForegroundWaiter>,
    completed_order: VecDeque<TaskId>,
    next_queue_reap: Instant,
    output_load_tx: Option<std::sync::mpsc::SyncSender<OutputLoadWork>>,
    output_load_event_rx: mpsc::UnboundedReceiver<OutputLoadEvent>,
    output_load_drained: Option<oneshot::Receiver<()>>,
    output_load_worker: Option<std::thread::JoinHandle<()>>,
    output_loads_inflight: usize,
    output_load_supervisors: Arc<AtomicUsize>,
    shutdown: Option<ShutdownState>,
    weak_handle: TaskHandle,
    sequence: u64,
    next_message_id: u64,
    next_generation: u64,
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
    let active_message_slots =
        Arc::new(tokio::sync::Semaphore::new(config.active_message_capacity));
    let handle = TaskHandle {
        command_tx: TaskCommandSender::Strong(command_tx),
        event_tx: event_tx.clone(),
        active_message_slots,
        active_message_capacity: config.active_message_capacity,
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
    let (output_load_tx, output_load_rx) = std::sync::mpsc::sync_channel(config.max_output_loads);
    let (output_load_event_tx, output_load_event_rx) = mpsc::unbounded_channel();
    let (output_load_drained_tx, output_load_drained) = oneshot::channel();
    let output_load_supervisors = Arc::new(AtomicUsize::new(0));
    let output_load_worker = spawn_output_load_dispatcher(
        Arc::clone(&runner),
        output_load_rx,
        output_load_event_tx,
        output_load_drained_tx,
        tokio::runtime::Handle::current(),
        Arc::clone(&output_load_supervisors),
    );
    let next_queue_reap = Instant::now() + config.queued_reap_interval;
    let next_progress_poll = Instant::now() + config.progress_poll_interval;
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
        progress_poll_inflight: HashSet::new(),
        next_progress_poll,
        state: CoordinatorState::default(),
        controls: HashMap::new(),
        jobs: FuturesUnordered::new(),
        active_messages: FuturesUnordered::new(),
        job_aborts: HashMap::new(),
        cancel_deadlines: HashMap::new(),
        administrative_cancel_pending: HashSet::new(),
        drain_waiters: Vec::new(),
        abandoned_teardown_roots: HashSet::new(),
        cancellation_batch_active: false,
        validations: FuturesUnordered::new(),
        validation_aborts: HashMap::new(),
        pending_spawns: HashMap::new(),
        validation_order: VecDeque::new(),
        validation_results: HashMap::new(),
        cleanup_inflight: HashSet::new(),
        pending_completions: HashMap::new(),
        pending_terminal_outputs: HashMap::new(),
        waiters: HashMap::new(),
        foreground_waiters: HashMap::new(),
        completed_order: VecDeque::new(),
        next_queue_reap,
        output_load_tx: Some(output_load_tx),
        output_load_event_rx,
        output_load_drained: Some(output_load_drained),
        output_load_worker: Some(output_load_worker),
        output_loads_inflight: 0,
        output_load_supervisors,
        shutdown: None,
        weak_handle,
        sequence: 0,
        next_message_id: 0,
        next_generation: 0,
    };
    let actor = tokio::spawn(coordinator.run());
    (handle, actor)
}

impl<R: TaskRunner, A: WorkspaceAllocator> TaskCoordinator<R, A> {
    fn next_deadline(&self) -> Instant {
        let mut deadline = self.next_queue_reap;
        deadline = deadline.min(self.next_progress_poll);
        for candidate in self.cancel_deadlines.values().copied() {
            deadline = deadline.min(candidate);
        }
        for pending in self.pending_terminal_outputs.values() {
            deadline = deadline.min(pending.deadline);
        }
        for waiter in &self.drain_waiters {
            deadline = deadline.min(waiter.deadline);
        }
        if let Some(shutdown) = &self.shutdown {
            deadline = deadline.min(shutdown.deadline);
        }
        for waiter in self.waiters.values().flatten() {
            deadline = deadline.min(waiter.deadline);
        }
        for candidate in self
            .foreground_waiters
            .values()
            .filter_map(|waiter| waiter.deadline)
        {
            deadline = deadline.min(candidate);
        }
        deadline
    }

    async fn process_deadlines(&mut self) {
        let now = Instant::now();
        if self.next_queue_reap <= now {
            self.next_queue_reap = now + self.config.queued_reap_interval;
        }
        if self.next_progress_poll <= now {
            self.next_progress_poll = now + self.config.progress_poll_interval;
            self.dispatch_progress_polls();
        }
        self.reap_cancelled().await;
        self.reap_terminalization_deadlines().await;
        self.reap_closed_drain_waiters();
        self.expire_drain_waiters(now);
        self.expire_waiters(now);
        self.expire_foreground(now);
    }

    fn dispatch_progress_polls(&mut self) {
        let available = self
            .config
            .progress_poll_capacity
            .saturating_sub(self.progress_poll_inflight.len());
        let candidates: Vec<_> = self
            .controls
            .iter()
            .filter(|(task_id, _)| {
                !self.progress_poll_inflight.contains(*task_id)
                    && self.state.tasks.get(*task_id).is_some_and(|record| {
                        record.node.status.is_running() && !record.node.status.is_terminal()
                    })
            })
            .take(available)
            .map(|(task_id, control)| (task_id.clone(), Arc::clone(control.child())))
            .collect();
        for (task_id, control) in candidates {
            self.progress_poll_inflight.insert(task_id.clone());
            if !self.dispatch_callback(CallbackWork::Progress {
                task_id: task_id.clone(),
                control,
            }) {
                self.progress_poll_inflight.remove(&task_id);
            }
        }
    }

    fn register_waiter(
        &mut self,
        task_id: TaskId,
        caller: InspectCaller,
        timeout: std::time::Duration,
        reply: oneshot::Sender<Result<WaitOutcome, TaskError>>,
    ) {
        if !caller_owns(&self.state, &caller, &task_id) {
            let _ = reply.send(Ok(WaitOutcome::NotFoundOrNotOwned));
            return;
        }
        let snapshot = self
            .state
            .inspection(&task_id)
            .expect("authorized wait target remains registered");
        if snapshot.node.status.is_terminal() {
            let mut disposition = self.state.tasks[&task_id]
                .completion_disposition
                .unwrap_or_default();
            disposition.waiter_delivered = true;
            disposition.should_surface = false;
            let mut snapshot = self
                .state
                .inspection(&task_id)
                .expect("terminal waiter target remains registered");
            snapshot.completion_disposition = Some(disposition);
            if reply.send(Ok(WaitOutcome::Finished(snapshot))).is_ok() {
                self.state
                    .tasks
                    .get_mut(&task_id)
                    .expect("terminal waiter target remains registered")
                    .completion_disposition = Some(disposition);
            }
            return;
        }
        let total_waiters: usize = self.waiters.values().map(Vec::len).sum::<usize>()
            + self.foreground_waiters.len()
            + self.drain_waiters.len();
        let per_task = self.waiters.get(&task_id).map_or(0, Vec::len);
        if total_waiters >= self.config.max_waiters || per_task >= self.config.max_waiters_per_task
        {
            let _ = reply.send(Err(TaskError::new(
                TaskErrorCode::RetentionLimit,
                "task waiter capacity is exhausted",
            )));
            return;
        }
        let timeout = timeout.min(self.config.waiter_timeout_cap);
        if timeout.is_zero() {
            let _ = reply.send(Ok(WaitOutcome::TimedOut(snapshot)));
            return;
        }
        self.waiters
            .entry(task_id)
            .or_default()
            .push(BlockingWaiter {
                deadline: Instant::now() + timeout,
                reply,
            });
    }

    fn register_foreground_waiter(
        &mut self,
        task_id: TaskId,
        caller: InspectCaller,
        reply: oneshot::Sender<Result<CompletionDisposition, TaskError>>,
    ) {
        if !caller_owns(&self.state, &caller, &task_id) {
            let _ = reply.send(Err(not_found()));
            return;
        }
        let record = self
            .state
            .tasks
            .get(&task_id)
            .expect("authorized foreground target remains registered");
        let mode = record.spawn_mode.unwrap_or(SpawnMode::Background);
        if mode == SpawnMode::Background {
            let disposition = CompletionDisposition {
                backgrounded: true,
                ..record.completion_disposition.unwrap_or_default()
            };
            let workflow_owned = matches!(record.node.owner, lato_core::TaskOwner::Workflow { .. });
            let live = !record.node.status.is_terminal();
            if reply.send(Ok(disposition)).is_err()
                && workflow_owned
                && live
                && let Some(record) = self.state.tasks.get(&task_id)
            {
                record.cancellation.cancel();
            }
            return;
        }
        let deadline = (mode == SpawnMode::Foreground)
            .then(|| record.enqueued_at + self.config.foreground_budget);
        if record.node.status.is_terminal() {
            let mut disposition = record.completion_disposition.unwrap_or_default();
            if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                disposition.backgrounded = true;
                disposition.should_surface =
                    !disposition.waiter_delivered && !disposition.explicitly_killed;
            } else {
                disposition.foreground_delivered = true;
                disposition.should_surface = false;
            }
            if reply.send(Ok(disposition)).is_ok() {
                self.state
                    .tasks
                    .get_mut(&task_id)
                    .expect("terminal foreground target remains registered")
                    .completion_disposition = Some(disposition);
            }
            return;
        }
        let waiter_count = self.foreground_waiters.len()
            + self.waiters.values().map(Vec::len).sum::<usize>()
            + self.drain_waiters.len();
        if self.foreground_waiters.contains_key(&task_id) || waiter_count >= self.config.max_waiters
        {
            let _ = reply.send(Err(TaskError::new(
                TaskErrorCode::RetentionLimit,
                "foreground waiter capacity is exhausted",
            )));
            return;
        }
        self.foreground_waiters
            .insert(task_id.clone(), ForegroundWaiter { deadline, reply });
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            self.expire_foreground(Instant::now());
        }
    }

    fn expire_waiters(&mut self, now: Instant) {
        let task_ids: Vec<_> = self.waiters.keys().cloned().collect();
        for task_id in task_ids {
            let snapshot = self.state.inspection(&task_id);
            let Some(waiters) = self.waiters.remove(&task_id) else {
                continue;
            };
            let mut retained = Vec::with_capacity(waiters.len());
            for waiter in waiters {
                if waiter.reply.is_closed() {
                    continue;
                }
                if waiter.deadline <= now {
                    if let Some(snapshot) = snapshot.clone() {
                        let _ = waiter.reply.send(Ok(WaitOutcome::TimedOut(snapshot)));
                    }
                } else {
                    retained.push(waiter);
                }
            }
            if !retained.is_empty() {
                self.waiters.insert(task_id, retained);
            }
        }
    }

    fn expire_foreground(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .foreground_waiters
            .iter()
            .filter(|(_, waiter)| {
                waiter.reply.is_closed() || waiter.deadline.is_some_and(|d| d <= now)
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in expired {
            let waiter = self
                .foreground_waiters
                .remove(&task_id)
                .expect("selected foreground waiter exists");
            let workflow_owned = matches!(
                &self.state.tasks[&task_id].node.owner,
                lato_core::TaskOwner::Workflow { .. }
            );
            let mut disposition = self.state.tasks[&task_id]
                .completion_disposition
                .unwrap_or_default();
            disposition.backgrounded = true;
            let delivered = waiter.reply.send(Ok(disposition)).is_ok();
            let workflow_drop = !delivered && workflow_owned;
            disposition.backgrounded = delivered || !workflow_drop;
            self.state
                .tasks
                .get_mut(&task_id)
                .expect("foreground task remains registered")
                .completion_disposition = Some(disposition);
            if workflow_drop {
                self.state.tasks[&task_id].cancellation.cancel();
            }
            if delivered {
                self.commit_transition(task_id, TaskEventPayload::Backgrounded);
            } else {
                self.commit_transition(task_id, TaskEventPayload::ForegroundReleased);
            }
        }
    }

    fn resolve_terminal_observers(&mut self, task_id: &TaskId) {
        let mut disposition = self.state.tasks[task_id]
            .completion_disposition
            .unwrap_or_default();
        if let Some(waiter) = self.foreground_waiters.remove(task_id) {
            let expired = waiter
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now());
            let candidate = if expired {
                CompletionDisposition {
                    backgrounded: true,
                    should_surface: !disposition.waiter_delivered && !disposition.explicitly_killed,
                    ..disposition
                }
            } else {
                CompletionDisposition {
                    foreground_delivered: true,
                    should_surface: false,
                    ..disposition
                }
            };
            if waiter.reply.send(Ok(candidate)).is_ok() {
                disposition = candidate;
            }
        }

        let mut waiter_delivered = false;
        if let Some(waiters) = self.waiters.remove(task_id) {
            for waiter in waiters {
                let candidate = CompletionDisposition {
                    waiter_delivered: true,
                    should_surface: false,
                    ..disposition
                };
                let mut snapshot = self
                    .state
                    .inspection(task_id)
                    .expect("terminal waiter target remains registered");
                snapshot.completion_disposition = Some(candidate);
                if waiter
                    .reply
                    .send(Ok(WaitOutcome::Finished(snapshot)))
                    .is_ok()
                {
                    waiter_delivered = true;
                    disposition = candidate;
                }
            }
        }
        disposition.waiter_delivered |= waiter_delivered;
        disposition.should_surface = !disposition.foreground_delivered
            && !disposition.waiter_delivered
            && !disposition.explicitly_killed;
        self.state
            .tasks
            .get_mut(task_id)
            .expect("terminal observer target remains registered")
            .completion_disposition = Some(disposition);
    }

    fn retain_completed(&mut self, _task_id: TaskId) {
        loop {
            let mut eligible: Vec<_> =
                self.state
                    .tasks
                    .iter()
                    .filter(|(task_id, record)| {
                        record.node.parent_id.is_some()
                            && record.node.status.is_terminal()
                            && record.workspace_lease.is_none()
                            && record.reservation.is_none()
                            && record.cleanup_error.is_none()
                            && !self.completed_order.contains(task_id)
                            && !self.state.tasks.values().any(|candidate| {
                                candidate.node.parent_id.as_ref() == Some(*task_id)
                            })
                    })
                    .map(|(task_id, record)| (record.last_event_sequence, task_id.clone()))
                    .collect();
            eligible.sort_by_key(|(sequence, _)| *sequence);
            for (_, task_id) in eligible {
                self.completed_order.push_back(task_id);
            }
            let retained_completed = self
                .state
                .tasks
                .values()
                .filter(|record| {
                    record.node.parent_id.is_some()
                        && record.node.status.is_terminal()
                        && record.workspace_lease.is_none()
                        && record.reservation.is_none()
                        && record.cleanup_error.is_none()
                })
                .count();
            if retained_completed <= self.config.max_completed {
                break;
            }
            let Some(evicted) = self.completed_order.pop_front() else {
                // Terminal ancestors remain live authority/budget anchors until
                // their descendants leave the registry. `max_total_tasks`
                // bounds these non-evictable anchors independently.
                break;
            };
            self.commit_transition(evicted.clone(), TaskEventPayload::CompletedRecordEvicted);
            self.waiters.remove(&evicted);
            self.foreground_waiters.remove(&evicted);
            self.state.tasks.remove(&evicted);
        }
    }

    fn reply_with_loaded_inspection(
        &mut self,
        result: Result<crate::task::TaskInspection, TaskError>,
        reply: oneshot::Sender<Result<crate::task::TaskInspection, TaskError>>,
    ) {
        let Ok(result) = result else {
            let _ = reply.send(result);
            return;
        };
        let Some(output_ref) = result
            .snapshot
            .result
            .as_ref()
            .and_then(|task_result| task_result.output_ref.clone())
        else {
            let _ = reply.send(Ok(result));
            return;
        };
        let timeout = self.config.output_load_timeout;
        if self.output_loads_inflight >= self.config.max_output_loads {
            let _ = reply.send(Err(TaskError::new(
                TaskErrorCode::RetentionLimit,
                "persisted output load capacity is exhausted",
            )));
            return;
        }
        let work = OutputLoadWork {
            output_ref,
            inspection: result,
            reply,
            timeout,
            max_bytes: self.config.max_loaded_output_bytes,
        };
        let send_result = match self.output_load_tx.as_ref() {
            Some(sender) => sender.try_send(work),
            None => Err(std::sync::mpsc::TrySendError::Disconnected(work)),
        };
        match send_result {
            Ok(()) => self.output_loads_inflight += 1,
            Err(std::sync::mpsc::TrySendError::Full(work))
            | Err(std::sync::mpsc::TrySendError::Disconnected(work)) => {
                let _ = work.reply.send(Err(TaskError::new(
                    TaskErrorCode::RetentionLimit,
                    "persisted output load dispatcher is unavailable",
                )));
            }
        }
    }

    pub async fn run(mut self) {
        loop {
            self.resolve_administrative_cancellations();
            self.reap_closed_drain_waiters();
            self.resolve_drain_waiters();
            self.reopen_abandoned_teardown_roots();
            if self.shutdown.as_ref().is_some_and(|shutdown| {
                self.jobs.is_empty()
                    && self.validations.is_empty()
                    && (self.output_loads_inflight == 0 || shutdown.deadline <= Instant::now())
            }) {
                let shutdown_deadline = self
                    .shutdown
                    .as_ref()
                    .expect("shutdown finalization requires shutdown state")
                    .deadline;
                let callbacks_drained = self.shutdown_callbacks(shutdown_deadline).await;
                let output_loads_drained = self.shutdown_output_loads(shutdown_deadline).await;
                while let Ok(event) = self.output_load_event_rx.try_recv() {
                    match event {
                        OutputLoadEvent::Reply { reply, result } => {
                            let _ = reply.send(*result);
                        }
                        OutputLoadEvent::SlotReleased => {
                            self.output_loads_inflight =
                                self.output_loads_inflight.saturating_sub(1);
                        }
                    }
                }
                while let Ok(outcome) = self.callback_rx.try_recv() {
                    self.handle_callback_outcome(outcome);
                }
                let sink_outcome = self.shutdown_sink(shutdown_deadline).await;
                let unreleased_leases = self
                    .state
                    .tasks
                    .values()
                    .filter(|record| record.workspace_lease.is_some())
                    .count();
                let outcome = if unreleased_leases == 0 {
                    if callbacks_drained
                        && output_loads_drained
                        && sink_outcome == SinkShutdown::Drained
                    {
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
            let deadline = self.next_deadline();
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
                completion = self.active_messages.next(), if !self.active_messages.is_empty() => {
                    if let Some(completion) = completion {
                        self.finish_active_message(completion).await;
                    }
                }
                callback = self.callback_rx.recv() => {
                    if let Some(callback) = callback {
                        self.handle_callback_outcome(callback);
                    }
                }
                event = self.output_load_event_rx.recv(), if self.output_loads_inflight > 0 => {
                    if let Some(event) = event {
                        match event {
                            OutputLoadEvent::Reply { reply, result } => {
                                let _ = reply.send(*result);
                            }
                            OutputLoadEvent::SlotReleased => {
                                self.output_loads_inflight = self.output_loads_inflight.saturating_sub(1);
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => self.process_deadlines().await,
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
                enqueued_at,
                reply,
            } => {
                self.begin_spawn(
                    root_id,
                    parent_id,
                    *request,
                    enqueued_at,
                    PendingSpawnReply::Spawn(reply),
                );
            }
            TaskCommand::SpawnAndWait {
                root_id,
                parent_id,
                request,
                enqueued_at,
                reply,
            } => {
                self.begin_spawn(
                    root_id,
                    parent_id,
                    *request,
                    enqueued_at,
                    PendingSpawnReply::SpawnAndWait(reply),
                );
            }
            TaskCommand::Inspect {
                task_id: target_task_id,
                caller,
                reply,
            } => {
                let result = caller_owns(&self.state, &caller, &target_task_id)
                    .then(|| self.state.inspection(&target_task_id))
                    .flatten()
                    .ok_or_else(not_found);
                let _ = reply.send(result);
            }
            TaskCommand::InspectDetailed {
                task_id: target_task_id,
                caller,
                reply,
            } => {
                let result = caller_owns(&self.state, &caller, &target_task_id)
                    .then(|| self.state.inspection(&target_task_id))
                    .flatten()
                    .map(inspection)
                    .ok_or_else(not_found);
                self.reply_with_loaded_inspection(result, reply);
            }
            TaskCommand::ListRunning { caller, reply } => {
                let snapshots = self
                    .state
                    .tasks
                    .keys()
                    .filter(|task_id| caller_owns(&self.state, &caller, task_id))
                    .filter_map(|task_id| self.state.inspection(task_id))
                    .filter(|snapshot| {
                        snapshot.node.parent_id.is_some()
                            && (matches!(snapshot.node.status, TaskStatus::Preparing)
                                || snapshot.node.status.is_running())
                    })
                    .collect();
                let _ = reply.send(snapshots);
            }
            TaskCommand::Wait {
                task_id,
                caller,
                timeout,
                reply,
            } => self.register_waiter(task_id, caller, timeout, reply),
            TaskCommand::ForegroundWait {
                task_id,
                caller,
                reply,
            } => self.register_foreground_waiter(task_id, caller, reply),
            TaskCommand::SendActiveMessage {
                request,
                caller,
                permit,
                reply,
            } => self.begin_active_message(request, caller, permit, reply),
            TaskCommand::Cancel {
                target,
                caller,
                wait_for_drain,
                reopen_on_backstop,
                reply,
            } => {
                self.begin_cancellation(target, caller, wait_for_drain, reopen_on_backstop, reply)
                    .await;
            }
            TaskCommand::SetSpawnAdmission {
                root_id,
                caller,
                closed,
                reply,
            } => {
                let result = self.set_root_spawn_admission(&root_id, &caller, closed);
                let _ = reply.send(result);
            }
            TaskCommand::RegistryCounts { reply } => {
                let mut counts = self.state.counts(
                    self.dropped_sink_events,
                    self.dropped_callback_work,
                    self.callback_execution_failures,
                    self.output_load_supervisors.load(Ordering::Acquire),
                );
                counts.finalizing = self.pending_terminal_outputs.len();
                counts.running = counts.running.saturating_sub(counts.finalizing);
                let _ = reply.send(counts);
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
                progress: Default::default(),
                usage: Default::default(),
                result: None,
                completion_disposition: None,
                output_metadata: None,
                spawn_mode: None,
                enqueued_at: Instant::now(),
                active_messages: Default::default(),
                generation: None,
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

    async fn begin_cancellation(
        &mut self,
        target: CancelTarget,
        caller: InspectCaller,
        wait_for_drain: bool,
        reopen_on_backstop: bool,
        reply: oneshot::Sender<Result<CancelOutcome, TaskError>>,
    ) {
        if self.shutdown.is_some() {
            let _ = reply.send(Err(coordinator_closed()));
            return;
        }
        if wait_for_drain
            && !matches!(
                target,
                CancelTarget::Root { .. } | CancelTarget::Workflow { .. }
            )
        {
            let _ = reply.send(Err(TaskError::new(
                TaskErrorCode::RunnerProtocolViolation,
                "only root and workflow cancellation scopes support drain waiting",
            )));
            return;
        }
        let waiter_count = self.drain_waiters.len()
            + self.foreground_waiters.len()
            + self.waiters.values().map(Vec::len).sum::<usize>();
        if wait_for_drain && waiter_count >= self.config.max_waiters {
            let _ = reply.send(Err(TaskError::new(
                TaskErrorCode::RetentionLimit,
                "task drain waiter capacity is exhausted",
            )));
            return;
        }
        if let Err(error) = self.authorize_cancel_target(&caller, &target) {
            let _ = reply.send(Err(error));
            return;
        }
        let resolved_result = if reopen_on_backstop {
            match &target {
                CancelTarget::Root { root_id } => {
                    resolve_root_teardown(&self.state, &caller, root_id)
                }
                _ => resolve_cancellation(&self.state, &caller, &target),
            }
        } else {
            resolve_cancellation(&self.state, &caller, &target)
        };
        let resolved = match resolved_result {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        // Resolve the entire scope from the authoritative tree before closing
        // latches or touching a queue/runtime registry.
        for root_id in &resolved.admission_roots {
            self.close_admission_record(root_id);
        }
        for task_id in &resolved.admission_tasks {
            self.close_admission_record(task_id);
        }

        if wait_for_drain {
            for root_id in &resolved.admission_roots {
                self.reject_pending_spawns_for_root(root_id);
            }
        }

        self.cancellation_batch_active = true;
        for task_id in resolved.task_ids {
            self.request_cancel(task_id).await;
        }
        self.cancellation_batch_active = false;
        if self.shutdown.is_none() {
            self.promote_queue();
        }

        if wait_for_drain {
            self.drain_waiters.push(DrainWaiter {
                target,
                outcome: resolved.outcome,
                reply,
                deadline: Instant::now() + self.config.teardown_drain_timeout,
                reopen_on_backstop,
            });
            self.resolve_drain_waiters();
        } else {
            let _ = reply.send(Ok(resolved.outcome));
        }
    }

    fn authorize_cancel_target(
        &self,
        caller: &InspectCaller,
        target: &CancelTarget,
    ) -> Result<(), TaskError> {
        match (caller, target) {
            (InspectCaller::Admin, _)
            | (InspectCaller::Scoped { .. }, CancelTarget::Task { .. }) => Ok(()),
            (
                InspectCaller::Scoped { root_id, task_id },
                CancelTarget::Root {
                    root_id: target_root,
                },
            ) if root_id == target_root && task_id == target_root => Ok(()),
            _ => Err(not_found()),
        }
    }

    fn set_root_spawn_admission(
        &mut self,
        root_id: &TaskId,
        caller: &InspectCaller,
        closed: bool,
    ) -> Result<(), TaskError> {
        if self.shutdown.is_some() || !self.root_caller_owns(caller, root_id) {
            return Err(if self.shutdown.is_some() {
                coordinator_closed()
            } else {
                not_found()
            });
        }
        if !closed
            && (self.root_has_active_drain(root_id)
                || self.abandoned_teardown_roots.contains(root_id))
        {
            return Err(TaskError::new(
                TaskErrorCode::SpawnAdmissionClosed,
                "root spawn admission cannot reopen while teardown drain is active",
            ));
        }
        self.set_admission_record(root_id, closed);
        Ok(())
    }

    fn root_caller_owns(&self, caller: &InspectCaller, root_id: &TaskId) -> bool {
        if !self.state.roots.contains(root_id) {
            return false;
        }
        match caller {
            InspectCaller::Admin => true,
            InspectCaller::Scoped {
                root_id: caller_root,
                task_id,
            } => caller_root == root_id && task_id == root_id,
        }
    }

    fn close_admission_record(&mut self, task_id: &TaskId) {
        self.set_admission_record(task_id, true);
    }

    fn set_admission_record(&mut self, task_id: &TaskId, closed: bool) {
        let Some(record) = self.state.tasks.get_mut(task_id) else {
            return;
        };
        if record.spawn_admission_closed == closed {
            return;
        }
        record.spawn_admission_closed = closed;
        self.commit_transition(
            task_id.clone(),
            if closed {
                TaskEventPayload::SpawnAdmissionClosed
            } else {
                TaskEventPayload::SpawnAdmissionOpened
            },
        );
    }

    fn root_has_active_drain(&self, root_id: &TaskId) -> bool {
        self.drain_waiters
            .iter()
            .any(|waiter| match &waiter.target {
                CancelTarget::Root { root_id: candidate } => candidate == root_id,
                CancelTarget::Workflow {
                    root_id: Some(candidate),
                    ..
                } => candidate == root_id,
                CancelTarget::Workflow {
                    run_id,
                    root_id: None,
                } => self.state.tasks.values().any(|record| {
                    &record.node.root_id == root_id
                        && matches!(
                            &record.node.owner,
                            lato_core::TaskOwner::Workflow { run_id: owner_run, .. }
                                if owner_run == run_id
                        )
                }),
                CancelTarget::Task { .. } | CancelTarget::Turn { .. } => false,
            })
    }

    fn reject_pending_spawns_for_root(&mut self, root_id: &TaskId) {
        let task_ids: Vec<_> = self
            .pending_spawns
            .iter()
            .filter(|(_, pending)| &pending.root_id == root_id)
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in task_ids {
            if let Some(abort) = self.validation_aborts.remove(&task_id) {
                abort.abort();
            }
            self.validation_results.remove(&task_id);
            self.validation_order
                .retain(|candidate| candidate != &task_id);
            if let Some(pending) = self.pending_spawns.remove(&task_id) {
                Self::reply_spawn_error(
                    pending.reply,
                    TaskError::new(
                        TaskErrorCode::SpawnAdmissionClosed,
                        "root spawn admission is closed",
                    ),
                );
            }
        }
    }

    fn resolve_drain_waiters(&mut self) {
        let waiters = std::mem::take(&mut self.drain_waiters);
        let mut pending = Vec::with_capacity(waiters.len());
        for waiter in waiters {
            if self.drain_is_pending(&waiter.target) {
                pending.push(waiter);
            } else {
                let reply_dropped = waiter.reply.send(Ok(waiter.outcome)).is_err();
                if reply_dropped
                    && waiter.reopen_on_backstop
                    && let CancelTarget::Root { root_id } = waiter.target
                {
                    self.abandoned_teardown_roots.insert(root_id);
                }
            }
        }
        self.drain_waiters = pending;
        self.reopen_abandoned_teardown_roots();
    }

    fn reap_closed_drain_waiters(&mut self) {
        let waiters = std::mem::take(&mut self.drain_waiters);
        let mut pending = Vec::with_capacity(waiters.len());
        for waiter in waiters {
            if waiter.reply.is_closed() {
                if waiter.reopen_on_backstop
                    && let CancelTarget::Root { root_id } = waiter.target
                {
                    self.abandoned_teardown_roots.insert(root_id);
                }
            } else {
                pending.push(waiter);
            }
        }
        self.drain_waiters = pending;
        self.reopen_abandoned_teardown_roots();
    }

    fn reopen_abandoned_teardown_roots(&mut self) {
        let ready: Vec<_> = self
            .abandoned_teardown_roots
            .iter()
            .filter(|root_id| {
                !self.root_has_active_drain(root_id)
                    && !self.drain_is_pending(&CancelTarget::root((*root_id).clone()))
            })
            .cloned()
            .collect();
        for root_id in ready {
            self.abandoned_teardown_roots.remove(&root_id);
            self.set_admission_record(&root_id, false);
        }
    }

    fn resolve_administrative_cancellations(&mut self) {
        let ready: Vec<_> = self
            .administrative_cancel_pending
            .iter()
            .filter(|root_id| {
                self.state
                    .descendants_including(root_id)
                    .into_iter()
                    .filter(|task_id| task_id != *root_id)
                    .all(|task_id| self.state.tasks[&task_id].node.status.is_terminal())
            })
            .cloned()
            .collect();
        for root_id in ready {
            self.administrative_cancel_pending.remove(&root_id);
            self.cancel_deadlines.remove(&root_id);
            self.terminalize_cancelled(&root_id);
        }
    }

    fn expire_drain_waiters(&mut self, now: Instant) {
        let waiters = std::mem::take(&mut self.drain_waiters);
        let mut pending = Vec::with_capacity(waiters.len());
        let mut reopen = HashSet::new();
        for waiter in waiters {
            if waiter.deadline > now {
                pending.push(waiter);
                continue;
            }
            let mut matching: HashSet<_> = self
                .state
                .tasks
                .keys()
                .filter(|task_id| target_contains_task(&self.state, &waiter.target, task_id))
                .filter(|task_id| !self.state.tasks[*task_id].node.status.is_terminal())
                .cloned()
                .collect();
            matching.extend(
                self.job_aborts
                    .keys()
                    .filter(|task_id| target_contains_task(&self.state, &waiter.target, task_id))
                    .cloned(),
            );
            for task_id in &matching {
                if let Some(abort) = self.job_aborts.get(task_id) {
                    abort.abort();
                }
            }
            let _ = waiter.reply.send(Err(TaskError::new(
                TaskErrorCode::TimedOut,
                format!(
                    "task teardown drain timed out with {} unfinished task(s)",
                    matching.len()
                ),
            )));
            if waiter.reopen_on_backstop
                && let CancelTarget::Root { root_id } = waiter.target
            {
                reopen.insert(root_id);
            }
        }
        self.drain_waiters = pending;
        for root_id in reopen {
            if !self.root_has_active_drain(&root_id) {
                self.set_admission_record(&root_id, false);
            }
        }
    }

    fn drain_is_pending(&self, target: &CancelTarget) -> bool {
        target_matches_live(&self.state, target)
            || self
                .job_aborts
                .keys()
                .any(|task_id| target_contains_task(&self.state, target, task_id))
            || self
                .cleanup_inflight
                .iter()
                .any(|task_id| target_contains_task(&self.state, target, task_id))
    }

    fn begin_spawn(
        &mut self,
        root_id: TaskId,
        parent_id: TaskId,
        request: SpawnTaskRequest,
        enqueued_at: Instant,
        reply: PendingSpawnReply,
    ) {
        if self.shutdown.is_some() {
            Self::reply_spawn_error(reply, coordinator_closed());
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
                Self::reply_spawn_error(reply, error);
                return;
            }
        };
        if let AdmissionDecision::Reject(error) = decide(
            &self.config,
            self.state.running_count(),
            self.state.running_count_for_root(&root_id),
            self.queue.len(),
        ) {
            let result = self.retain_admission_rejection(parent_id, request, structure, error);
            Self::reply_pending_spawn(self, reply, result);
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
                enqueued_at,
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
            let workflow_owned = self
                .state
                .tasks
                .get(&pending.parent_id)
                .is_some_and(|parent| {
                    matches!(&parent.node.owner, lato_core::TaskOwner::Workflow { .. })
                });
            if workflow_owned && pending.reply.is_closed() {
                continue;
            }
            let result = self.finish_spawn(
                pending.root_id,
                pending.parent_id,
                pending.request,
                pending.enqueued_at,
                validation,
            );
            Self::reply_pending_spawn(self, pending.reply, result);
        }
    }

    fn reply_spawn_error(reply: PendingSpawnReply, error: TaskError) {
        match reply {
            PendingSpawnReply::Spawn(reply) => {
                let _ = reply.send(Err(error));
            }
            PendingSpawnReply::SpawnAndWait(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn reply_pending_spawn(
        coordinator: &mut Self,
        reply: PendingSpawnReply,
        result: Result<SpawnDisposition, TaskError>,
    ) {
        match (reply, result) {
            (PendingSpawnReply::Spawn(reply), result) => {
                let workflow_owned = result.as_ref().is_ok_and(|disposition| {
                    matches!(
                        coordinator.state.tasks[&disposition.task_id].node.owner,
                        lato_core::TaskOwner::Workflow { .. }
                    )
                });
                if let Err(Ok(disposition)) = reply.send(result)
                    && workflow_owned
                    && let Some(record) = coordinator.state.tasks.get(&disposition.task_id)
                {
                    record.cancellation.cancel();
                }
            }
            (PendingSpawnReply::SpawnAndWait(reply), Ok(disposition)) => {
                // Spawn visibility and foreground ownership are committed by one
                // actor command. There is no interval in which workflow work can
                // exist without a cancel-on-drop observer.
                coordinator.register_foreground_waiter(
                    disposition.task_id.clone(),
                    InspectCaller::Scoped {
                        root_id: disposition.handle.root_id().clone(),
                        task_id: disposition.handle.task_id().clone(),
                    },
                    reply,
                );
            }
            (PendingSpawnReply::SpawnAndWait(reply), Err(error)) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn finish_spawn(
        &mut self,
        root_id: TaskId,
        parent_id: TaskId,
        request: SpawnTaskRequest,
        enqueued_at: Instant,
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
                progress: Default::default(),
                usage: Default::default(),
                result: None,
                completion_disposition: None,
                output_metadata: None,
                spawn_mode: Some(if request.profile.definition_background {
                    SpawnMode::Background
                } else {
                    request.mode
                }),
                enqueued_at,
                active_messages: Default::default(),
                generation: None,
            },
        );
        self.commit_transition(task_id.clone(), TaskEventPayload::SpawnAccepted);
        if request.profile.definition_background || request.mode == SpawnMode::Background {
            self.state
                .tasks
                .get_mut(&task_id)
                .expect("accepted task remains registered")
                .completion_disposition = Some(CompletionDisposition {
                backgrounded: true,
                ..CompletionDisposition::default()
            });
            self.commit_transition(task_id.clone(), TaskEventPayload::Backgrounded);
        }

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
                progress: Default::default(),
                usage: Default::default(),
                result: None,
                completion_disposition: Some(CompletionDisposition {
                    should_surface: true,
                    ..CompletionDisposition::default()
                }),
                output_metadata: None,
                spawn_mode: Some(request.mode),
                enqueued_at: Instant::now(),
                active_messages: Default::default(),
                generation: None,
            },
        );
        let result = lato_core::TaskResult {
            success: false,
            output: String::new(),
            error: Some(error.clone()),
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        };
        self.commit_terminal(
            &task_id,
            TaskStatus::Failed,
            result,
            TaskEventPayload::AdmissionRejected {
                error: error.clone(),
            },
            None,
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

    fn begin_active_message(
        &mut self,
        request: ActiveMessageRequest,
        caller: InspectCaller,
        permit: tokio::sync::OwnedSemaphorePermit,
        reply: oneshot::Sender<ActiveMessageOutcome>,
    ) {
        let target = request.task_id().clone();
        if !caller_owns(&self.state, &caller, &target) {
            let _ = reply.send(ActiveMessageOutcome::NotFoundOrNotOwned);
            return;
        }
        let Some((generation, cancellation, sender_session_id, sender_root_id, sender_task_id)) =
            self.state.tasks.get(&target).and_then(|record| {
                (record.node.status.is_running()
                    && !record.cancellation.is_cancelled()
                    && self.controls.contains_key(&target))
                .then(|| {
                    let (root_id, task_id) = match &caller {
                        InspectCaller::Scoped { root_id, task_id } => {
                            (root_id.clone(), task_id.clone())
                        }
                        InspectCaller::Admin => return None,
                    };
                    let sender = &self.state.tasks[&task_id];
                    Some((
                        record.generation?,
                        record.cancellation.clone(),
                        sender.node.owner.session_id().clone(),
                        root_id,
                        task_id,
                    ))
                })
                .flatten()
            })
        else {
            let _ = reply.send(ActiveMessageOutcome::NotActiveOrFinalizing);
            return;
        };
        if let Err(outcome) = self
            .state
            .tasks
            .get_mut(&target)
            .expect("owned message target remains registered")
            .active_messages
            .begin(self.config.active_messages_per_task)
        {
            let _ = reply.send(outcome);
            return;
        }

        self.next_message_id = self
            .next_message_id
            .checked_add(1)
            .expect("active-message identifier overflow");
        let message_id = self.next_message_id;
        let lease = ActiveMessageAdmissionLease::new();
        let delivery = crate::task::ActiveMessageDelivery::new(
            ActiveMessage {
                message_id,
                sender_session_id,
                sender_root_id,
                sender_task_id,
                text: Arc::clone(request.text()),
            },
            request.operation(),
            generation,
            Arc::clone(&lease),
        );
        let control = Arc::clone(self.controls[&target].child());
        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control.send_active_message(delivery)
        }));
        let Ok(future) = future else {
            let settled = lease.revoke();
            let _ = self
                .state
                .tasks
                .get_mut(&target)
                .and_then(|record| record.active_messages.finish(settled));
            let outcome = ActiveMessageOutcome::ChannelClosed;
            self.commit_active_message_event(&target, message_id, &outcome);
            let _ = reply.send(outcome);
            return;
        };
        self.active_messages.push(ActiveMessageFuture::new(
            target,
            generation,
            message_id,
            future,
            cancellation,
            Instant::now() + ACTIVE_MESSAGE_ADMISSION_TIMEOUT,
            lease,
            permit,
            reply,
        ));
    }

    async fn finish_active_message(&mut self, mut completion: ActiveMessageCompletion) {
        let current = self
            .state
            .tasks
            .get(&completion.task_id)
            .is_some_and(|record| {
                record.generation == Some(completion.generation)
                    && !record.node.status.is_terminal()
            });
        if !current {
            let outcome = if !completion.settled
                || matches!(
                    completion.outcome,
                    crate::task::ActiveMessageCompletionKind::Admission(
                        crate::task::ActiveMessageAdmission::Admitted
                    )
                ) {
                ActiveMessageOutcome::AdmissionUncertain
            } else {
                completion.protocol_outcome()
            };
            if let Some(reply) = completion.reply.take() {
                let _ = reply.send(outcome);
            }
            return;
        }
        let outcome = completion.protocol_outcome();
        self.commit_active_message_event(&completion.task_id, completion.message_id, &outcome);
        if let Some(reply) = completion.reply.take() {
            let _ = reply.send(outcome);
        }
        let ready = self
            .state
            .tasks
            .get_mut(&completion.task_id)
            .and_then(|record| record.active_messages.finish(completion.settled));
        if let Some(clean) = ready
            && let Some(pending) = self.pending_terminal_outputs.remove(&completion.task_id)
        {
            self.finish_terminal_output(completion.task_id.clone(), pending.output, clean)
                .await;
        }
    }

    fn commit_active_message_event(
        &mut self,
        task_id: &TaskId,
        message_id: u64,
        outcome: &ActiveMessageOutcome,
    ) {
        let payload = match outcome {
            ActiveMessageOutcome::Accepted { .. } => {
                TaskEventPayload::ActiveMessageAccepted { message_id }
            }
            ActiveMessageOutcome::AdmissionUncertain => {
                TaskEventPayload::ActiveMessageUncertain { message_id }
            }
            _ => TaskEventPayload::ActiveMessageRejected {
                message_id,
                error: active_message_error(outcome),
            },
        };
        self.commit_transition(task_id.clone(), payload);
    }

    async fn reap_terminalization_deadlines(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending_terminal_outputs
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in expired {
            let Some(pending) = self.pending_terminal_outputs.remove(&task_id) else {
                continue;
            };
            if let Some(record) = self.state.tasks.get_mut(&task_id) {
                record.active_messages.force_uncertain();
            }
            self.finish_terminal_output(task_id, pending.output, false)
                .await;
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
        let clean = self.state.tasks.get_mut(&task_id).and_then(|record| {
            record.spawn_admission_closed = true;
            record.active_messages.start_finalizing()
        });
        self.commit_transition(task_id.clone(), TaskEventPayload::Finalizing);
        if clean.is_none() {
            self.pending_terminal_outputs.insert(
                task_id,
                PendingTerminalOutput {
                    output,
                    deadline: Instant::now() + ACTIVE_MESSAGE_FINALIZATION_TIMEOUT,
                },
            );
            return;
        }
        self.finish_terminal_output(task_id, output, clean.unwrap())
            .await;
    }

    async fn finish_terminal_output(
        &mut self,
        task_id: TaskId,
        mut output: crate::task::TaskRunOutput,
        clean: bool,
    ) {
        if !clean {
            let error = TaskError::new(
                TaskErrorCode::AdmissionUncertain,
                "task active-message admission could not be proven committed or revoked",
            );
            output.result.success = false;
            output.result.error = Some(error);
        }
        self.set_status(&task_id, TaskStatus::Verifying);
        self.commit_transition(task_id.clone(), TaskEventPayload::VerificationStarted);
        if output.result.output_ref.is_none() {
            output.result.output_ref = output.external_snapshot_ref.take();
        }
        let cap = self.state.tasks[&task_id]
            .node
            .result_contract
            .max_output_bytes;
        let metadata = truncate_utf8(&mut output.result.output, cap);
        self.state.tasks.get_mut(&task_id).unwrap().output_metadata = Some(metadata);
        let (terminal_status, terminal_event) = if output.result.success {
            (
                TaskStatus::Completed,
                TaskEventPayload::Completed {
                    result: output.result.clone(),
                },
            )
        } else {
            let error = output.result.error.clone().unwrap_or_else(|| {
                TaskError::new(
                    TaskErrorCode::RunnerProtocolViolation,
                    "runner failed without error",
                )
            });
            output.result.error = Some(error.clone());
            (TaskStatus::Failed, TaskEventPayload::Failed { error })
        };
        let completion = TaskCompletion {
            task_id: task_id.clone(),
            result: output.result.clone(),
        };
        self.commit_terminal(
            &task_id,
            terminal_status,
            output.result,
            terminal_event,
            Some(completion),
        );
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
        let result = lato_core::TaskResult {
            success: false,
            output: String::new(),
            error: Some(error.clone()),
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        };
        self.commit_terminal(
            &task_id,
            TaskStatus::Failed,
            result,
            TaskEventPayload::Failed { error },
            None,
        );
    }

    fn commit_terminal(
        &mut self,
        task_id: &TaskId,
        status: TaskStatus,
        result: lato_core::TaskResult,
        event: TaskEventPayload,
        completion: Option<TaskCompletion>,
    ) -> bool {
        let current = self
            .state
            .tasks
            .get(task_id)
            .expect("terminal task remains registered");
        if current.result.is_some() {
            return false;
        }
        if !current.node.status.is_terminal() {
            self.set_status(task_id, status);
        } else {
            debug_assert_eq!(current.node.status, status);
        }
        let record = self.state.tasks.get_mut(task_id).unwrap();
        record.usage = result.usage.clone();
        record.result = Some(result);
        if let Some(completion) = completion {
            self.pending_completions.insert(task_id.clone(), completion);
        }
        self.resolve_terminal_observers(task_id);
        // The terminal event is the publication barrier: every authoritative
        // terminal field and successful observer-delivery disposition is
        // already visible to an immediate follow-up inspection.
        self.commit_transition(task_id.clone(), event);
        self.cleanup_terminal(task_id);
        true
    }

    fn cleanup_terminal(&mut self, task_id: &TaskId) {
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
        self.retain_completed(task_id.clone());
        if self.shutdown.is_none() && !self.cancellation_batch_active {
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
                self.terminalize_cancelled(&queued.task_id);
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
        if self
            .state
            .tasks
            .get(&task_id)
            .is_none_or(|record| record.node.status.is_terminal())
        {
            return;
        }
        let status = self.state.tasks[&task_id].node.status;
        let cancellation_already_tracked = self.cancel_deadlines.contains_key(&task_id);
        let record = self
            .state
            .tasks
            .get_mut(&task_id)
            .expect("cancel target remains registered");
        let admission_was_open = !record.spawn_admission_closed;
        record.spawn_admission_closed = true;
        record.cancellation.cancel();
        if admission_was_open {
            self.commit_transition(task_id.clone(), TaskEventPayload::SpawnAdmissionClosed);
        }
        if status == TaskStatus::Queued {
            self.queue
                .remove_matching(|queued| queued.task_id == task_id);
            self.terminalize_cancelled(&task_id);
            return;
        }
        if cancellation_already_tracked {
            return;
        }
        if let Some(control) = self.controls.get(&task_id) {
            self.dispatch_callback(CallbackWork::Cancel {
                task_id: task_id.clone(),
                control: Arc::clone(control.child()),
            });
        }
        self.commit_transition(task_id.clone(), TaskEventPayload::CancellationRequested);
        if self.state.tasks[&task_id].node.parent_id.is_none() {
            self.cancel_deadlines
                .insert(task_id.clone(), Instant::now() + self.config.cancel_grace);
            self.administrative_cancel_pending.insert(task_id);
            return;
        }
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
                self.terminalize_cancelled(&task_id);
            } else {
                self.launch_runner(task_id);
            }
            return;
        }
        if cancellation_requested {
            self.terminalize_cancelled(&task_id);
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

    fn cancelled_result(&mut self, task_id: &TaskId) -> lato_core::TaskResult {
        let record = self
            .state
            .tasks
            .get_mut(task_id)
            .expect("cancelled task remains registered");
        let result = lato_core::TaskResult {
            success: false,
            output: String::new(),
            error: Some(TaskError::new(
                TaskErrorCode::Cancelled,
                "task was cancelled",
            )),
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        };
        let mut disposition = record.completion_disposition.unwrap_or_default();
        disposition.explicitly_killed = true;
        disposition.should_surface = false;
        record.completion_disposition = Some(disposition);
        result
    }

    fn terminalize_cancelled(&mut self, task_id: &TaskId) {
        if self
            .state
            .tasks
            .get(task_id)
            .is_none_or(|record| record.node.status.is_terminal())
        {
            return;
        }
        let result = self.cancelled_result(task_id);
        self.commit_terminal(
            task_id,
            TaskStatus::Cancelled,
            result,
            TaskEventPayload::Cancelled,
            None,
        );
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
            Self::reply_spawn_error(pending.reply, coordinator_closed());
        }
        self.validation_order.clear();
        self.validation_results.clear();
        for waiter in self.drain_waiters.drain(..) {
            let _ = waiter.reply.send(Err(coordinator_closed()));
        }

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
            self.cleanup_terminal(&task_id);
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
                    self.next_generation = self
                        .next_generation
                        .checked_add(1)
                        .expect("active task generation overflow");
                    self.state
                        .tasks
                        .get_mut(&task_id)
                        .expect("promoted task remains registered")
                        .generation = Some(self.next_generation);
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
                if let Some(record) = self.state.tasks.get_mut(&task_id)
                    && !record.node.status.is_terminal()
                    && usage.total_tokens >= record.usage.total_tokens
                    && usage.tool_calls >= record.usage.tool_calls
                {
                    record.usage = usage.clone();
                    self.commit_transition(task_id, TaskEventPayload::UsageUpdated { usage });
                }
            }
            RunnerEvent::Progress { task_id, progress } => {
                if let Some(record) = self.state.tasks.get_mut(&task_id)
                    && !record.node.status.is_terminal()
                    && progress.completed_units >= record.progress.completed_units
                    && progress != record.progress
                {
                    record.progress = progress.clone();
                    self.commit_transition(task_id, TaskEventPayload::ProgressUpdated { progress });
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

    fn dispatch_callback(&mut self, work: CallbackWork<R::Control>) -> bool {
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
            false
        } else {
            true
        }
    }

    fn handle_callback_outcome(&mut self, outcome: CallbackOutcome) {
        if outcome.kind == TaskCallbackKind::Progress {
            self.progress_poll_inflight.remove(&outcome.task_id);
        }
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
        } else if let Some(progress) = outcome.progress
            && let Some(record) = self.state.tasks.get_mut(&outcome.task_id)
            && !record.node.status.is_terminal()
            && progress.completed_units >= record.progress.completed_units
            && progress != record.progress
        {
            record.progress = progress.clone();
            self.commit_transition(
                outcome.task_id,
                TaskEventPayload::ProgressUpdated { progress },
            );
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

    async fn shutdown_callbacks(&mut self, deadline: Instant) -> bool {
        self.callback_tx.take();
        let Some(drained) = self.callback_drained.take() else {
            return true;
        };
        if !matches!(
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), drained).await,
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

    async fn shutdown_output_loads(&mut self, deadline: Instant) -> bool {
        self.output_load_tx.take();
        let Some(drained) = self.output_load_drained.take() else {
            return true;
        };
        if !matches!(
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), drained).await,
            Ok(Ok(()))
        ) {
            self.output_load_worker.take();
            return false;
        }
        if let Some(worker) = self.output_load_worker.take() {
            let _ = worker.join();
        }
        true
    }

    async fn shutdown_sink(&mut self, deadline: Instant) -> SinkShutdown {
        self.sink_tx.take();
        let Some(drained) = self.sink_drained.take() else {
            return SinkShutdown::Drained;
        };
        if !matches!(
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), drained).await,
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
                let mut progress = None;
                let callback = || match work {
                    CallbackWork::Cancel { control, .. } => control.cancel(),
                    CallbackWork::Completed { completion, .. } => runner.on_completed(completion),
                    CallbackWork::Progress { control, .. } => {
                        progress = Some(control.progress());
                    }
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
                    progress,
                });
            }
            let _ = drained.send(());
        })
        .expect("task callback dispatcher thread must start")
}

fn spawn_output_load_dispatcher<R: TaskRunner>(
    runner: Arc<R>,
    work_rx: std::sync::mpsc::Receiver<OutputLoadWork>,
    event_tx: mpsc::UnboundedSender<OutputLoadEvent>,
    drained: oneshot::Sender<()>,
    runtime: tokio::runtime::Handle,
    active_supervisors: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lato-task-output-loads".into())
        .spawn(move || {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let mut supervisors = HashMap::<usize, std::thread::JoinHandle<()>>::new();
            let mut next_supervisor_id = 0usize;
            let mut disconnected = false;
            while !disconnected {
                reap_output_supervisors(&mut supervisors, &done_rx, &event_tx, &active_supervisors);
                match work_rx.recv_timeout(std::time::Duration::from_millis(1)) {
                    Ok(work) => {
                        reap_output_supervisors(
                            &mut supervisors,
                            &done_rx,
                            &event_tx,
                            &active_supervisors,
                        );
                        let supervisor_id = next_supervisor_id;
                        next_supervisor_id = next_supervisor_id.wrapping_add(1);
                        let runner = Arc::clone(&runner);
                        let event_tx = event_tx.clone();
                        let runtime = runtime.clone();
                        let done_tx = done_tx.clone();
                        let supervisor = std::thread::spawn(move || {
                            let (value_tx, value_rx) = std::sync::mpsc::sync_channel(1);
                            let output_ref = work.output_ref.clone();
                            let execution = std::thread::spawn(move || {
                                let loaded =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        runtime.block_on(runner.load_persisted_output(&output_ref))
                                    }));
                                let _ = value_tx.send(loaded);
                            });
                            let response = match value_rx.recv_timeout(work.timeout) {
                                Ok(Ok(Ok(value))) => Some(Ok(value)),
                                Ok(Ok(Err(error))) => Some(Err(error)),
                                Ok(Err(_)) => Some(Err(TaskError::new(
                                    TaskErrorCode::RunnerPanic,
                                    "task runner panicked while loading persisted output",
                                ))),
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                    Some(Err(TaskError::new(
                                        TaskErrorCode::RunnerPanic,
                                        "persisted output worker disconnected",
                                    )))
                                }
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                            };
                            if response.is_none() {
                                let _ = event_tx.send(OutputLoadEvent::Reply {
                                    reply: work.reply,
                                    result: Box::new(Err(TaskError::new(
                                        TaskErrorCode::RunnerProtocolViolation,
                                        "persisted task output load timed out",
                                    ))),
                                });
                                // Preserve the bounded worker slot until the blocking
                                // execution really exits. Otherwise repeated timeouts
                                // could accumulate an unbounded number of stuck OS
                                // threads behind a nominal integer capacity.
                                let _ = value_rx.recv();
                                let _ = execution.join();
                                let _ = done_tx.send(supervisor_id);
                                return;
                            }
                            let response = response.expect("non-timeout response is present");
                            let mut inspection = work.inspection;
                            let reply = match response {
                                Ok(Some(mut output)) => {
                                    let metadata = truncate_utf8(&mut output, work.max_bytes);
                                    if let Some(task_result) = inspection.snapshot.result.as_mut() {
                                        task_result.output = output;
                                    }
                                    inspection.snapshot.output_metadata = Some(metadata);
                                    Ok(inspection)
                                }
                                Ok(None) => Ok(inspection),
                                Err(error) => Err(error),
                            };
                            let _ = event_tx.send(OutputLoadEvent::Reply {
                                reply: work.reply,
                                result: Box::new(reply),
                            });
                            let _ = execution.join();
                            let _ = done_tx.send(supervisor_id);
                        });
                        supervisors.insert(supervisor_id, supervisor);
                        active_supervisors.store(supervisors.len(), Ordering::Release);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        disconnected = true;
                    }
                }
            }
            while !supervisors.is_empty() {
                if let Ok(supervisor_id) = done_rx.recv()
                    && let Some(supervisor) = supervisors.remove(&supervisor_id)
                {
                    let _ = supervisor.join();
                    active_supervisors.store(supervisors.len(), Ordering::Release);
                    let _ = event_tx.send(OutputLoadEvent::SlotReleased);
                }
            }
            let _ = drained.send(());
        })
        .expect("task output-load dispatcher thread must start")
}

fn reap_output_supervisors(
    supervisors: &mut HashMap<usize, std::thread::JoinHandle<()>>,
    done_rx: &std::sync::mpsc::Receiver<usize>,
    event_tx: &mpsc::UnboundedSender<OutputLoadEvent>,
    active_supervisors: &AtomicUsize,
) {
    while let Ok(supervisor_id) = done_rx.try_recv() {
        if let Some(supervisor) = supervisors.remove(&supervisor_id) {
            let _ = supervisor.join();
            active_supervisors.store(supervisors.len(), Ordering::Release);
            let _ = event_tx.send(OutputLoadEvent::SlotReleased);
        }
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

fn active_message_error(outcome: &ActiveMessageOutcome) -> TaskError {
    match outcome {
        ActiveMessageOutcome::Saturated { .. } | ActiveMessageOutcome::Limit { .. } => {
            TaskError::new(
                TaskErrorCode::MessageLimit,
                "active-message admission limit reached",
            )
        }
        ActiveMessageOutcome::AdmissionUncertain => TaskError::new(
            TaskErrorCode::AdmissionUncertain,
            "active-message admission is uncertain",
        ),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline => TaskError::new(
            TaskErrorCode::TimedOut,
            "active-message admission timed out",
        ),
        ActiveMessageOutcome::ChannelClosed => TaskError::new(
            TaskErrorCode::CoordinatorClosed,
            "active-message channel closed",
        ),
        ActiveMessageOutcome::NotFoundOrNotOwned => TaskError::new(
            TaskErrorCode::NotFoundOrNotOwned,
            "task not found or not owned",
        ),
        ActiveMessageOutcome::NotActiveOrFinalizing | ActiveMessageOutcome::Unsupported => {
            TaskError::new(
                TaskErrorCode::RunnerProtocolViolation,
                "task cannot accept active messages",
            )
        }
        ActiveMessageOutcome::Accepted { .. } => unreachable!("accepted messages are not errors"),
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> OutputMetadata {
    let source_bytes = value.len();
    if source_bytes <= max_bytes {
        return OutputMetadata {
            source_bytes,
            retained_bytes: source_bytes,
            truncated: false,
        };
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    OutputMetadata {
        source_bytes,
        retained_bytes: value.len(),
        truncated: true,
    }
}
