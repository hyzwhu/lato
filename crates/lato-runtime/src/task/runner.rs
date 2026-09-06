// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: injectable child-runner protocol with acknowledged startup

use crate::task::ScopedTaskHandle;
use futures_util::future::BoxFuture;
use lato_core::{AgentProfile, TaskError, TaskId, TaskNode, TaskProgress, TaskResult, TaskUsage};
use lato_workspace::WorkspaceLease;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveMessageKind {
    Queue,
    Steer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMessageDelivery {
    pub message_id: u64,
    pub sender_id: TaskId,
    pub kind: ActiveMessageKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveMessageAdmission {
    Accepted,
    Rejected,
    Uncertain,
}

pub trait TaskChildControl: Send + Sync + 'static {
    fn progress(&self) -> TaskProgress;
    fn send_active_message(
        &self,
        delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission>;
    fn cancel(&self);
}

#[derive(Clone)]
pub struct TaskControl<C: TaskChildControl> {
    child: Arc<C>,
    cancellation: CancellationToken,
}

impl<C: TaskChildControl> TaskControl<C> {
    pub fn new(child: Arc<C>, cancellation: CancellationToken) -> Self {
        Self {
            child,
            cancellation,
        }
    }

    pub fn child(&self) -> &Arc<C> {
        &self.child
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

pub struct StartedTask<C: TaskChildControl> {
    pub control: TaskControl<C>,
}

impl<C: TaskChildControl> StartedTask<C> {
    pub fn new(control: Arc<C>, cancellation: CancellationToken) -> Self {
        Self {
            control: TaskControl::new(control, cancellation),
        }
    }
}

#[derive(Clone)]
pub struct TaskRunRequest {
    pub node: TaskNode,
    pub workspace_lease: WorkspaceLease,
    pub scoped_handle: ScopedTaskHandle,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRunOutput {
    pub result: TaskResult,
    pub external_snapshot_ref: Option<String>,
}

impl From<TaskResult> for TaskRunOutput {
    fn from(result: TaskResult) -> Self {
        Self {
            result,
            external_snapshot_ref: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCompletion {
    pub task_id: TaskId,
    pub result: TaskResult,
}

pub(crate) enum RunnerEvent<C: TaskChildControl> {
    WorkspaceAllocated {
        task_id: TaskId,
        lease: WorkspaceLease,
        acknowledgement: oneshot::Sender<bool>,
    },
    WorkspaceAllocationFailed {
        task_id: TaskId,
        error: TaskError,
    },
    Started {
        task_id: TaskId,
        started: StartedTask<C>,
        acknowledgement: oneshot::Sender<bool>,
    },
    Usage {
        task_id: TaskId,
        usage: TaskUsage,
    },
    Progress {
        task_id: TaskId,
        progress: TaskProgress,
    },
    Completed {
        task_id: TaskId,
        output: TaskRunOutput,
    },
}

#[derive(Clone)]
pub struct TaskReporter<C: TaskChildControl> {
    task_id: TaskId,
    event_tx: mpsc::Sender<RunnerEvent<C>>,
}

impl<C: TaskChildControl> TaskReporter<C> {
    #[allow(dead_code)]
    pub(crate) fn new(task_id: TaskId, event_tx: mpsc::Sender<RunnerEvent<C>>) -> Self {
        Self { task_id, event_tx }
    }

    pub async fn started(&self, started: StartedTask<C>) -> bool {
        let (acknowledgement, response) = oneshot::channel();
        if self
            .event_tx
            .send(RunnerEvent::Started {
                task_id: self.task_id.clone(),
                started,
                acknowledgement,
            })
            .await
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }

    pub async fn report_usage(&self, usage: TaskUsage) -> bool {
        self.event_tx
            .send(RunnerEvent::Usage {
                task_id: self.task_id.clone(),
                usage,
            })
            .await
            .is_ok()
    }

    pub async fn report_progress(&self, progress: TaskProgress) -> bool {
        self.event_tx
            .send(RunnerEvent::Progress {
                task_id: self.task_id.clone(),
                progress,
            })
            .await
            .is_ok()
    }
}

#[async_trait::async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    type Control: TaskChildControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput;

    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError>;

    fn on_completed(&self, completion: TaskCompletion);
}
