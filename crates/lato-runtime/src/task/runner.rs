// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: injectable child-runner protocol with acknowledged startup

use crate::task::{ActiveMessageAdmission, ActiveMessageDelivery, ScopedTaskHandle};
use futures_util::future::BoxFuture;
use lato_core::{AgentProfile, TaskError, TaskId, TaskNode, TaskProgress, TaskResult, TaskUsage};
use lato_workspace::WorkspaceLease;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub trait TaskChildControl: Send + Sync + 'static {
    fn progress(&self) -> TaskProgress;
    /// Creates one admission future without blocking the coordinator actor.
    /// Implementations must return promptly; asynchronous admission work
    /// belongs in the returned bounded future.
    fn send_active_message(
        &self,
        delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission>;
    /// Requests runner-specific cancellation.
    ///
    /// The cancellation token is authoritative. The coordinator invokes this
    /// advisory callback on a bounded, panic-contained dispatcher, never on
    /// the actor or async executor thread.
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
    Started {
        task_id: TaskId,
        generation: u64,
        started: StartedTask<C>,
        acknowledgement: oneshot::Sender<bool>,
    },
    Usage {
        task_id: TaskId,
        generation: u64,
        usage: TaskUsage,
        acknowledgement: oneshot::Sender<bool>,
    },
    Progress {
        task_id: TaskId,
        generation: u64,
        progress: TaskProgress,
    },
}

pub struct TaskReporter<C: TaskChildControl> {
    task_id: TaskId,
    generation: u64,
    event_tx: mpsc::Sender<RunnerEvent<C>>,
}

impl<C: TaskChildControl> Clone for TaskReporter<C> {
    fn clone(&self) -> Self {
        Self {
            task_id: self.task_id.clone(),
            generation: self.generation,
            event_tx: self.event_tx.clone(),
        }
    }
}

impl<C: TaskChildControl> TaskReporter<C> {
    #[allow(dead_code)]
    pub(crate) fn new(
        task_id: TaskId,
        generation: u64,
        event_tx: mpsc::Sender<RunnerEvent<C>>,
    ) -> Self {
        Self {
            task_id,
            generation,
            event_tx,
        }
    }

    pub async fn started(&self, started: StartedTask<C>) -> bool {
        let (acknowledgement, response) = oneshot::channel();
        if self
            .event_tx
            .send(RunnerEvent::Started {
                task_id: self.task_id.clone(),
                generation: self.generation,
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
        let (acknowledgement, response) = oneshot::channel();
        if self
            .event_tx
            .send(RunnerEvent::Usage {
                task_id: self.task_id.clone(),
                generation: self.generation,
                usage,
                acknowledgement,
            })
            .await
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }

    pub async fn report_progress(&self, progress: TaskProgress) -> bool {
        self.event_tx
            .send(RunnerEvent::Progress {
                task_id: self.task_id.clone(),
                generation: self.generation,
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

    /// Validates a profile without blocking the executor thread.
    ///
    /// The coordinator runs this in an owned Tokio task and enforces a
    /// deadline by aborting and joining that task. Implementations must keep
    /// the async cancellation contract: CPU-heavy or blocking work belongs in
    /// an implementation-owned bounded worker that it can join.
    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError>;

    /// Loads runner-owned output referenced by a retained result.
    ///
    /// The coordinator invokes this outside its actor loop. Returning `None`
    /// leaves the capped inline output unchanged.
    async fn load_persisted_output(&self, _output_ref: &str) -> Result<Option<String>, TaskError> {
        Ok(None)
    }

    /// Observes a completion after terminal state and resource cleanup commit.
    ///
    /// This callback runs on the coordinator's bounded, panic-contained
    /// callback dispatcher rather than on the actor or async executor thread.
    fn on_completed(&self, completion: TaskCompletion);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::ready;

    struct TestControl;

    impl TaskChildControl for TestControl {
        fn progress(&self) -> TaskProgress {
            TaskProgress::default()
        }

        fn send_active_message(
            &self,
            _delivery: ActiveMessageDelivery,
        ) -> BoxFuture<'static, ActiveMessageAdmission> {
            Box::pin(ready(ActiveMessageAdmission::Rejected))
        }

        fn cancel(&self) {}
    }

    #[tokio::test]
    async fn usage_report_returns_false_when_the_actor_channel_is_closed() {
        let (event_tx, event_rx) = mpsc::channel(1);
        drop(event_rx);
        let reporter = TaskReporter::<TestControl>::new(TaskId::from("closed"), 1, event_tx);
        assert!(!reporter.report_usage(TaskUsage::default()).await);
    }

    #[tokio::test]
    async fn usage_report_returns_false_when_actor_drops_the_acknowledgement() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let reporter = TaskReporter::<TestControl>::new(TaskId::from("dropped"), 1, event_tx);
        let report = tokio::spawn(async move { reporter.report_usage(TaskUsage::default()).await });
        let event = event_rx.recv().await.expect("usage event is enqueued");
        drop(event);
        assert!(!report.await.unwrap());
    }
}
