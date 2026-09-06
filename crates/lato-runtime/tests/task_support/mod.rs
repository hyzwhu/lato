use lato_core::{
    AgentProfile, BudgetLimits, SessionId, TaskError, TaskId, TaskOwner, TaskProgress, TaskResult,
    ToolCapability, TurnId,
};
use lato_runtime::{
    CoordinatorConfig, NoopTaskEventSink, TaskChildControl, TaskEventEnvelope, TaskHandle,
    TaskReporter, TaskRootRequest, TaskRunOutput, TaskRunRequest, TaskRunner,
    spawn_task_coordinator,
};
use lato_workspace::MemoryWorkspaceAllocator;
use std::{future::ready, sync::Arc};
use tempfile::TempDir;
use tokio::{sync::broadcast, task::JoinHandle};

#[derive(Default)]
pub struct ControlledTaskRunner;

pub struct ControlledTaskControl;

impl TaskChildControl for ControlledTaskControl {
    fn progress(&self) -> TaskProgress {
        TaskProgress::default()
    }

    fn send_active_message(
        &self,
        _delivery: lato_runtime::ActiveMessageDelivery,
    ) -> futures_util::future::BoxFuture<'static, lato_runtime::ActiveMessageAdmission> {
        Box::pin(ready(lato_runtime::ActiveMessageAdmission::Rejected))
    }

    fn cancel(&self) {}
}

#[async_trait::async_trait]
impl TaskRunner for ControlledTaskRunner {
    type Control = ControlledTaskControl;

    async fn run(
        &self,
        _request: TaskRunRequest,
        _reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        TaskRunOutput::from(TaskResult {
            success: true,
            output: String::new(),
            error: None,
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}

pub struct Harness {
    pub handle: TaskHandle,
    events: broadcast::Receiver<TaskEventEnvelope>,
    _actor: JoinHandle<()>,
    _workspace: TempDir,
}

impl Harness {
    pub async fn new(config: CoordinatorConfig) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
        let (handle, actor) = spawn_task_coordinator(
            config,
            Arc::new(ControlledTaskRunner),
            allocator,
            Arc::new(NoopTaskEventSink),
        );
        let events = handle.subscribe();
        Self {
            handle,
            events,
            _actor: actor,
            _workspace: workspace,
        }
    }

    pub async fn register_root(&self, root: &str, session: &str, turn: &str) {
        self.try_register_root(root, session, turn).await.unwrap();
    }

    pub async fn try_register_root(
        &self,
        root: &str,
        session: &str,
        turn: &str,
    ) -> Result<(), TaskError> {
        self.handle
            .register_root(TaskRootRequest {
                task_id: TaskId::from(root),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from(session),
                    turn_id: TurnId::from(turn),
                },
                profile: AgentProfile::worker(),
                permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
                budget: BudgetLimits::unlimited(),
            })
            .await
    }

    pub async fn next_event(&mut self) -> TaskEventEnvelope {
        self.events.recv().await.unwrap()
    }

    pub fn has_pending_event(&mut self) -> bool {
        self.events.try_recv().is_ok()
    }
}
