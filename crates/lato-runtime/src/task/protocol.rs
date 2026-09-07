// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: stable provider-neutral task protocol and bounded actor handles

use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, ResultContract, SessionId, TaskError, TaskErrorCode,
    TaskId, TaskNode, TaskOwner, TaskResult, TaskScope, TaskStatus, TaskUsage, ToolCapability,
    TurnId,
};
use lato_workspace::WorkspaceLease;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitBehavior {
    Queue,
    Reject,
}

#[derive(Clone, Debug)]
pub struct CoordinatorConfig {
    pub command_capacity: usize,
    pub callback_capacity: usize,
    pub event_capacity: usize,
    pub active_message_capacity: usize,
    pub active_messages_per_task: usize,
    pub max_global_running: usize,
    pub max_running_per_root: usize,
    pub max_queue: usize,
    pub max_depth: u32,
    pub max_children_per_parent: usize,
    pub max_total_tasks: usize,
    pub max_completed: usize,
    pub max_waiters: usize,
    pub max_waiters_per_task: usize,
    pub max_output_loads: usize,
    pub max_loaded_output_bytes: usize,
    pub output_load_timeout: Duration,
    pub progress_poll_capacity: usize,
    pub progress_poll_interval: Duration,
    pub foreground_budget: Duration,
    pub waiter_timeout_cap: Duration,
    pub cancel_grace: Duration,
    pub teardown_drain_timeout: Duration,
    pub queued_reap_interval: Duration,
    pub profile_validation_timeout: Duration,
    pub admission_behavior: LimitBehavior,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            command_capacity: 128,
            callback_capacity: 128,
            event_capacity: 256,
            active_message_capacity: 64,
            active_messages_per_task: 8,
            max_global_running: 8,
            max_running_per_root: 4,
            max_queue: 256,
            max_depth: 8,
            max_children_per_parent: 16,
            max_total_tasks: 1_024,
            max_completed: 256,
            max_waiters: 1_024,
            max_waiters_per_task: 64,
            max_output_loads: 64,
            max_loaded_output_bytes: 1_048_576,
            output_load_timeout: Duration::from_secs(5),
            progress_poll_capacity: 64,
            progress_poll_interval: Duration::from_millis(250),
            foreground_budget: Duration::from_secs(45),
            waiter_timeout_cap: Duration::from_secs(3_600),
            cancel_grace: Duration::from_secs(5),
            teardown_drain_timeout: Duration::from_secs(30),
            queued_reap_interval: Duration::from_millis(250),
            profile_validation_timeout: Duration::from_secs(5),
            admission_behavior: LimitBehavior::Queue,
        }
    }
}

impl CoordinatorConfig {
    pub(crate) fn assert_valid(&self) {
        assert!(
            self.command_capacity > 0,
            "command capacity must be positive"
        );
        assert!(
            self.callback_capacity > 0,
            "callback capacity must be positive"
        );
        assert!(self.event_capacity > 0, "event capacity must be positive");
        assert!(
            self.active_message_capacity > 0,
            "active-message capacity must be positive"
        );
        assert!(
            self.active_messages_per_task > 0,
            "per-task active-message capacity must be positive"
        );
        assert!(
            self.max_global_running > 0,
            "global concurrency must be positive"
        );
        assert!(
            self.max_running_per_root > 0,
            "per-root concurrency must be positive"
        );
        assert!(self.max_total_tasks > 0, "task capacity must be positive");
        assert!(
            self.max_completed > 0,
            "completion capacity must be positive"
        );
        assert!(self.max_waiters > 0, "waiter capacity must be positive");
        assert!(
            self.max_waiters_per_task > 0,
            "per-task waiter capacity must be positive"
        );
        assert!(
            self.max_output_loads > 0,
            "output-load capacity must be positive"
        );
        assert!(
            self.max_loaded_output_bytes > 0,
            "loaded-output byte capacity must be positive"
        );
        assert!(
            !self.output_load_timeout.is_zero(),
            "output-load timeout must be positive"
        );
        assert!(
            self.progress_poll_capacity > 0,
            "progress-poll capacity must be positive"
        );
        assert!(
            !self.progress_poll_interval.is_zero(),
            "progress-poll interval must be positive"
        );
        assert!(
            !self.queued_reap_interval.is_zero(),
            "queued cancellation reap interval must be positive"
        );
        assert!(
            !self.waiter_timeout_cap.is_zero(),
            "waiter timeout cap must be positive"
        );
        assert!(
            !self.profile_validation_timeout.is_zero(),
            "profile validation timeout must be positive"
        );
    }
}

#[derive(Clone, Debug)]
pub struct TaskRootRequest {
    pub task_id: TaskId,
    pub owner: TaskOwner,
    pub profile: AgentProfile,
    pub permissions: Vec<ToolCapability>,
    pub budget: BudgetLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnMode {
    Foreground,
    AwaitCompletion,
    Background,
}

#[derive(Clone, Debug)]
pub struct SpawnTaskRequest {
    pub task_id: TaskId,
    pub scope: TaskScope,
    /// Child policy envelope. The coordinator rejects authority widening:
    /// workspace access may remain equal or narrow from a write-capable mode
    /// to `SharedReadOnly`; verification may remain equal or strengthen along
    /// `Accept < Schema < Programmatic < IndependentReview < HumanGate`; and a
    /// foreground-only parent cannot create a definition-background child.
    pub profile: AgentProfile,
    pub requested_capabilities: Option<Vec<ToolCapability>>,
    pub budget: BudgetLimits,
    pub result_contract: ResultContract,
    pub mode: SpawnMode,
    pub cancellation: CancellationToken,
}

#[derive(Clone)]
pub struct SpawnDisposition {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub handle: ScopedTaskHandle,
}

impl std::fmt::Debug for SpawnDisposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnDisposition")
            .field("task_id", &self.task_id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl SpawnDisposition {
    pub fn is_queued(&self) -> bool {
        self.status == TaskStatus::Queued
    }

    pub fn is_started(&self) -> bool {
        self.status == TaskStatus::Preparing || self.status.is_running()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshot {
    pub node: TaskNode,
    pub budget_limits: BudgetLimits,
    pub budget_spent: BudgetAmount,
    pub budget_reserved: BudgetAmount,
    pub workspace_lease: Option<WorkspaceLease>,
    pub has_parent_reservation: bool,
    pub event_sequence: u64,
    pub cleanup_error: Option<TaskError>,
    pub elapsed_ms: u64,
    pub progress: lato_core::TaskProgress,
    pub usage: TaskUsage,
    pub result: Option<TaskResult>,
    pub completion_disposition: Option<CompletionDisposition>,
    pub output_metadata: Option<OutputMetadata>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OutputMetadata {
    /// Bytes observed before the coordinator's UTF-8-safe cap was applied.
    pub source_bytes: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInspection {
    pub snapshot: TaskSnapshot,
    pub owner: TaskOwner,
    pub parent_id: Option<TaskId>,
    pub root_id: TaskId,
}

impl std::ops::Deref for TaskInspection {
    type Target = TaskSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Finished(TaskSnapshot),
    TimedOut(TaskSnapshot),
    NotFoundOrNotOwned,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompletionDisposition {
    pub foreground_delivered: bool,
    pub waiter_delivered: bool,
    pub backgrounded: bool,
    pub explicitly_killed: bool,
    pub should_surface: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RegistryCounts {
    pub roots: usize,
    pub queued: usize,
    pub preparing: usize,
    pub running: usize,
    pub finalizing: usize,
    pub completed: usize,
    pub total: usize,
    pub dropped_sink_events: u64,
    pub dropped_callback_work: u64,
    pub callback_execution_failures: u64,
}

/// Result of draining coordinator-owned work, callbacks, and the event sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkShutdown {
    /// Both dispatchers delivered every item they had accepted and exited.
    Drained,
    /// The bounded wait expired. The worker was detached because safe Rust
    /// cannot force-stop arbitrary callback code that is currently blocked.
    TimedOutDetached,
    /// Runner work is joined, but one or more allocator leases could not be
    /// released. Dispatcher outcomes are preserved independently.
    CleanupIncomplete {
        unreleased_leases: usize,
        sink_drained: bool,
        callbacks_drained: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCallbackKind {
    Cancel,
    Completed,
    Progress,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskEventEnvelope {
    pub schema_version: u32,
    pub sequence: u64,
    pub task_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub root_id: TaskId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub workflow_run_id: Option<String>,
    pub timestamp_ms: u64,
    pub payload: TaskEventPayload,
}

impl TaskEventEnvelope {
    pub(crate) fn new(sequence: u64, node: &TaskNode, payload: TaskEventPayload) -> Self {
        let (turn_id, workflow_run_id) = match &node.owner {
            TaskOwner::Interactive { turn_id, .. } => (Some(turn_id.clone()), None),
            TaskOwner::Workflow { run_id, .. } => (None, Some(run_id.clone())),
        };
        Self {
            schema_version: 1,
            sequence,
            task_id: node.id.clone(),
            parent_id: node.parent_id.clone(),
            root_id: node.root_id.clone(),
            session_id: node.owner.session_id().clone(),
            turn_id,
            workflow_run_id,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            payload,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEventPayload {
    RootRegistered,
    RootClosed,
    SpawnAccepted,
    Queued,
    AdmissionRejected {
        error: TaskError,
    },
    Preparing,
    Started,
    PhaseChanged {
        status: TaskStatus,
    },
    UsageUpdated {
        usage: TaskUsage,
    },
    ProgressUpdated {
        progress: lato_core::TaskProgress,
    },
    BudgetExhausted {
        error: TaskError,
    },
    ActiveMessageAccepted {
        message_id: u64,
    },
    ActiveMessageRejected {
        message_id: u64,
        error: TaskError,
    },
    ActiveMessageUncertain {
        message_id: u64,
    },
    ForegroundReleased,
    Backgrounded,
    CancellationRequested,
    Finalizing,
    VerificationStarted,
    Completed {
        result: TaskResult,
    },
    Failed {
        error: TaskError,
    },
    Cancelled,
    TimedOut,
    WorkspaceLeaseAllocated {
        lease_id: lato_core::LeaseId,
    },
    WorkspaceLeaseReleased {
        lease_id: lato_core::LeaseId,
    },
    WorkspaceLeaseReleaseFailed {
        lease_id: lato_core::LeaseId,
        error: TaskError,
    },
    CallbackDispatchFailed {
        callback: TaskCallbackKind,
        error: TaskError,
    },
    CallbackExecutionFailed {
        callback: TaskCallbackKind,
        error: TaskError,
    },
    SpawnAdmissionClosed,
    SpawnAdmissionOpened,
    CompletedRecordEvicted,
}

pub trait TaskEventSink: Send + Sync + 'static {
    fn on_event(&self, event: TaskEventEnvelope);
}

#[derive(Default)]
pub struct NoopTaskEventSink;

impl TaskEventSink for NoopTaskEventSink {
    fn on_event(&self, _event: TaskEventEnvelope) {}
}

#[derive(Clone, Default)]
pub struct MemoryTaskEventSink {
    events: Arc<std::sync::Mutex<Vec<TaskEventEnvelope>>>,
}

impl MemoryTaskEventSink {
    pub fn events(&self) -> Vec<TaskEventEnvelope> {
        self.events
            .lock()
            .expect("task event sink poisoned")
            .clone()
    }
}

impl TaskEventSink for MemoryTaskEventSink {
    fn on_event(&self, event: TaskEventEnvelope) {
        self.events
            .lock()
            .expect("task event sink poisoned")
            .push(event);
    }
}

pub(crate) enum TaskCommand {
    RegisterRoot {
        request: Box<TaskRootRequest>,
        reply: oneshot::Sender<Result<(), TaskError>>,
    },
    Spawn {
        root_id: TaskId,
        parent_id: TaskId,
        request: Box<SpawnTaskRequest>,
        enqueued_at: tokio::time::Instant,
        reply: oneshot::Sender<Result<SpawnDisposition, TaskError>>,
    },
    SpawnAndWait {
        root_id: TaskId,
        parent_id: TaskId,
        request: Box<SpawnTaskRequest>,
        enqueued_at: tokio::time::Instant,
        reply: oneshot::Sender<Result<CompletionDisposition, TaskError>>,
    },
    Inspect {
        task_id: TaskId,
        caller: InspectCaller,
        reply: oneshot::Sender<Result<TaskSnapshot, TaskError>>,
    },
    InspectDetailed {
        task_id: TaskId,
        caller: InspectCaller,
        reply: oneshot::Sender<Result<TaskInspection, TaskError>>,
    },
    ListRunning {
        caller: InspectCaller,
        reply: oneshot::Sender<Vec<TaskSnapshot>>,
    },
    Wait {
        task_id: TaskId,
        caller: InspectCaller,
        timeout: Duration,
        reply: oneshot::Sender<Result<WaitOutcome, TaskError>>,
    },
    ForegroundWait {
        task_id: TaskId,
        caller: InspectCaller,
        reply: oneshot::Sender<Result<CompletionDisposition, TaskError>>,
    },
    RegistryCounts {
        reply: oneshot::Sender<RegistryCounts>,
    },
    ShutdownRoot {
        root_id: TaskId,
        reply: oneshot::Sender<Result<(), TaskError>>,
    },
    Shutdown {
        reply: oneshot::Sender<SinkShutdown>,
    },
}

pub(crate) enum InspectCaller {
    Admin,
    Scoped { root_id: TaskId, task_id: TaskId },
}

#[derive(Clone)]
pub struct TaskHandle {
    pub(crate) command_tx: TaskCommandSender,
    pub(crate) event_tx: broadcast::Sender<TaskEventEnvelope>,
}

#[derive(Clone)]
pub(crate) enum TaskCommandSender {
    Strong(mpsc::Sender<TaskCommand>),
    Weak(mpsc::WeakSender<TaskCommand>),
}

impl TaskHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEventEnvelope> {
        self.event_tx.subscribe()
    }

    pub async fn register_root(
        &self,
        request: TaskRootRequest,
    ) -> Result<ScopedTaskHandle, TaskError> {
        let root_id = request.task_id.clone();
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::RegisterRoot {
            request: Box::new(request),
            reply,
        })
        .await?;
        response.await.map_err(|_| coordinator_closed())??;
        Ok(ScopedTaskHandle::new(
            root_id.clone(),
            root_id,
            self.clone(),
        ))
    }

    /// Performs an unrestricted coordinator-administration lookup.
    pub async fn inspect_admin(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::Inspect {
            task_id,
            caller: InspectCaller::Admin,
            reply,
        })
        .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn inspect_detailed_admin(
        &self,
        task_id: TaskId,
    ) -> Result<TaskInspection, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::InspectDetailed {
            task_id,
            caller: InspectCaller::Admin,
            reply,
        })
        .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn list_running_admin(&self) -> Result<Vec<TaskSnapshot>, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::ListRunning {
            caller: InspectCaller::Admin,
            reply,
        })
        .await?;
        response.await.map_err(|_| coordinator_closed())
    }

    pub async fn wait_admin(
        &self,
        task_id: TaskId,
        timeout: Duration,
    ) -> Result<WaitOutcome, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::Wait {
            task_id,
            caller: InspectCaller::Admin,
            timeout,
            reply,
        })
        .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn registry_counts(&self) -> Result<RegistryCounts, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::RegistryCounts { reply }).await?;
        response.await.map_err(|_| coordinator_closed())
    }

    pub async fn shutdown_root(&self, root_id: TaskId) -> Result<(), TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::ShutdownRoot { root_id, reply })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn shutdown(&self) -> Result<SinkShutdown, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::Shutdown { reply }).await?;
        response.await.map_err(|_| coordinator_closed())
    }

    async fn send(&self, command: TaskCommand) -> Result<(), TaskError> {
        let command_tx = match &self.command_tx {
            TaskCommandSender::Strong(command_tx) => command_tx.clone(),
            TaskCommandSender::Weak(command_tx) => {
                command_tx.upgrade().ok_or_else(coordinator_closed)?
            }
        };
        command_tx
            .send(command)
            .await
            .map_err(|_| coordinator_closed())
    }

    pub(crate) fn downgrade(&self) -> Self {
        let command_tx = match &self.command_tx {
            TaskCommandSender::Strong(command_tx) => command_tx.downgrade(),
            TaskCommandSender::Weak(command_tx) => command_tx.clone(),
        };
        Self {
            command_tx: TaskCommandSender::Weak(command_tx),
            event_tx: self.event_tx.clone(),
        }
    }

    pub(crate) fn upgrade(&self) -> Option<Self> {
        let command_tx = match &self.command_tx {
            TaskCommandSender::Strong(command_tx) => command_tx.clone(),
            TaskCommandSender::Weak(command_tx) => command_tx.upgrade()?,
        };
        Some(Self {
            command_tx: TaskCommandSender::Strong(command_tx),
            event_tx: self.event_tx.clone(),
        })
    }
}

#[derive(Clone)]
/// A root- and parent-bound coordinator client.
///
/// Scoped handles can only be minted by a successful coordinator operation;
/// callers cannot construct one or recover the unrestricted handle.
///
/// ```compile_fail
/// use lato_core::TaskId;
/// use lato_runtime::ScopedTaskHandle;
/// let _ = ScopedTaskHandle::new(
///     TaskId::from("root"),
///     TaskId::from("parent"),
///     unimplemented!(),
/// );
/// ```
///
/// ```compile_fail
/// use lato_runtime::ScopedTaskHandle;
/// let scoped: ScopedTaskHandle = unimplemented!();
/// let _unrestricted = scoped.handle();
/// ```
pub struct ScopedTaskHandle {
    root_id: TaskId,
    task_id: TaskId,
    inner: TaskHandle,
}

impl ScopedTaskHandle {
    pub(crate) fn new(root_id: TaskId, task_id: TaskId, inner: TaskHandle) -> Self {
        Self {
            root_id,
            task_id,
            inner,
        }
    }

    pub fn root_id(&self) -> &TaskId {
        &self.root_id
    }

    pub fn parent_id(&self) -> &TaskId {
        &self.task_id
    }

    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    pub async fn spawn(&self, request: SpawnTaskRequest) -> Result<SpawnDisposition, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::Spawn {
                root_id: self.root_id.clone(),
                parent_id: self.task_id.clone(),
                request: Box::new(request),
                enqueued_at: tokio::time::Instant::now(),
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn spawn_and_wait(
        &self,
        request: SpawnTaskRequest,
    ) -> Result<CompletionDisposition, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::SpawnAndWait {
                root_id: self.root_id.clone(),
                parent_id: self.task_id.clone(),
                request: Box::new(request),
                enqueued_at: tokio::time::Instant::now(),
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn inspect(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::Inspect {
                task_id,
                caller: InspectCaller::Scoped {
                    root_id: self.root_id.clone(),
                    task_id: self.task_id.clone(),
                },
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn inspect_detailed(&self, task_id: TaskId) -> Result<TaskInspection, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::InspectDetailed {
                task_id,
                caller: InspectCaller::Scoped {
                    root_id: self.root_id.clone(),
                    task_id: self.task_id.clone(),
                },
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn list_running(&self) -> Result<Vec<TaskSnapshot>, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::ListRunning {
                caller: InspectCaller::Scoped {
                    root_id: self.root_id.clone(),
                    task_id: self.task_id.clone(),
                },
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())
    }

    pub async fn wait(&self, task_id: TaskId, timeout: Duration) -> Result<WaitOutcome, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::Wait {
                task_id,
                caller: InspectCaller::Scoped {
                    root_id: self.root_id.clone(),
                    task_id: self.task_id.clone(),
                },
                timeout,
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }

    pub async fn wait_foreground(&self) -> Result<CompletionDisposition, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::ForegroundWait {
                task_id: self.task_id.clone(),
                caller: InspectCaller::Scoped {
                    root_id: self.root_id.clone(),
                    task_id: self.task_id.clone(),
                },
                reply,
            })
            .await?;
        response.await.map_err(|_| coordinator_closed())?
    }
}

pub(crate) fn root_node(request: &TaskRootRequest) -> TaskNode {
    TaskNode {
        id: request.task_id.clone(),
        parent_id: None,
        root_id: request.task_id.clone(),
        owner: request.owner.clone(),
        profile: request.profile.clone(),
        scope: TaskScope {
            objective: "coordinate root task".into(),
            context_refs: Vec::new(),
        },
        status: TaskStatus::Running,
        permissions: request.permissions.clone(),
        workspace_intent: request.profile.workspace,
        result_contract: ResultContract {
            schema: None,
            max_output_bytes: 0,
        },
    }
}

pub(crate) fn coordinator_closed() -> TaskError {
    TaskError::new(
        TaskErrorCode::CoordinatorClosed,
        "task coordinator is closed",
    )
}
