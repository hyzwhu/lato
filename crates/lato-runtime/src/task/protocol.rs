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

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitBehavior {
    Queue,
    Reject,
}

#[derive(Clone, Debug)]
pub struct CoordinatorConfig {
    pub command_capacity: usize,
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
    pub foreground_budget: Duration,
    pub waiter_timeout_cap: Duration,
    pub cancel_grace: Duration,
    pub teardown_drain_timeout: Duration,
    pub queued_reap_interval: Duration,
    pub admission_behavior: LimitBehavior,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            command_capacity: 128,
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
            foreground_budget: Duration::from_secs(45),
            waiter_timeout_cap: Duration::from_secs(3_600),
            cancel_grace: Duration::from_secs(5),
            teardown_drain_timeout: Duration::from_secs(30),
            queued_reap_interval: Duration::from_millis(250),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshot {
    pub node: TaskNode,
    pub budget_limits: BudgetLimits,
    pub budget_spent: BudgetAmount,
    pub budget_reserved: BudgetAmount,
    pub workspace_lease: Option<WorkspaceLease>,
    pub has_parent_reservation: bool,
    pub event_sequence: u64,
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
    AdmissionRejected { error: TaskError },
    Preparing,
    Started,
    PhaseChanged { status: TaskStatus },
    UsageUpdated { usage: TaskUsage },
    BudgetExhausted { error: TaskError },
    ActiveMessageAccepted { message_id: u64 },
    ActiveMessageRejected { message_id: u64, error: TaskError },
    ActiveMessageUncertain { message_id: u64 },
    ForegroundReleased,
    Backgrounded,
    CancellationRequested,
    Finalizing,
    VerificationStarted,
    Completed { result: TaskResult },
    Failed { error: TaskError },
    Cancelled,
    TimedOut,
    WorkspaceLeaseAllocated { lease_id: lato_core::LeaseId },
    WorkspaceLeaseReleased { lease_id: lato_core::LeaseId },
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
    Inspect {
        task_id: TaskId,
        requester_root: Option<TaskId>,
        reply: oneshot::Sender<Result<TaskSnapshot, TaskError>>,
    },
    RegistryCounts {
        reply: oneshot::Sender<RegistryCounts>,
    },
    ShutdownRoot {
        root_id: TaskId,
        reply: oneshot::Sender<Result<(), TaskError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct TaskHandle {
    pub(crate) command_tx: mpsc::Sender<TaskCommand>,
    pub(crate) event_tx: broadcast::Sender<TaskEventEnvelope>,
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

    pub async fn inspect(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::Inspect {
            task_id,
            requester_root: None,
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

    pub async fn shutdown(&self) -> Result<(), TaskError> {
        let (reply, response) = oneshot::channel();
        self.send(TaskCommand::Shutdown { reply }).await?;
        response.await.map_err(|_| coordinator_closed())
    }

    async fn send(&self, command: TaskCommand) -> Result<(), TaskError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| coordinator_closed())
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
    parent_id: TaskId,
    inner: TaskHandle,
}

impl ScopedTaskHandle {
    pub(crate) fn new(root_id: TaskId, parent_id: TaskId, inner: TaskHandle) -> Self {
        Self {
            root_id,
            parent_id,
            inner,
        }
    }

    pub fn root_id(&self) -> &TaskId {
        &self.root_id
    }

    pub fn parent_id(&self) -> &TaskId {
        &self.parent_id
    }

    pub async fn inspect(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
        let (reply, response) = oneshot::channel();
        self.inner
            .send(TaskCommand::Inspect {
                task_id,
                requester_root: Some(self.root_id.clone()),
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
