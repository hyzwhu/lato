#![allow(dead_code)]

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
use std::{
    collections::{HashMap, HashSet},
    future::ready,
    sync::Arc,
};
use tempfile::TempDir;
use tokio::{
    sync::{Mutex, Notify, broadcast},
    task::JoinHandle,
};

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

pub struct GatedTaskRunner {
    pause_before_start: bool,
    entered: Mutex<HashSet<TaskId>>,
    started: Mutex<Vec<TaskId>>,
    gates: Mutex<HashMap<TaskId, Arc<Notify>>>,
    changed: Notify,
}

impl GatedTaskRunner {
    fn new(pause_before_start: bool) -> Self {
        Self {
            pause_before_start,
            entered: Mutex::new(HashSet::new()),
            started: Mutex::new(Vec::new()),
            gates: Mutex::new(HashMap::new()),
            changed: Notify::new(),
        }
    }

    pub async fn started_ids(&self) -> Vec<TaskId> {
        self.started.lock().await.clone()
    }

    pub async fn wait_until_entered(&self, task_id: &str) {
        loop {
            if self.entered.lock().await.contains(&TaskId::from(task_id)) {
                return;
            }
            self.changed.notified().await;
        }
    }

    pub async fn allow_start(&self, task_id: &str) {
        if let Some(gate) = self.gates.lock().await.get(&TaskId::from(task_id)) {
            gate.notify_one();
        }
    }
}

#[async_trait::async_trait]
impl TaskRunner for GatedTaskRunner {
    type Control = ControlledTaskControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let task_id = request.node.id.clone();
        let gate = Arc::new(Notify::new());
        self.gates
            .lock()
            .await
            .insert(task_id.clone(), gate.clone());
        self.entered.lock().await.insert(task_id.clone());
        self.changed.notify_waiters();
        if self.pause_before_start {
            gate.notified().await;
        }
        if reporter
            .started(lato_runtime::StartedTask::new(
                Arc::new(ControlledTaskControl),
                request.cancellation.clone(),
            ))
            .await
        {
            self.started.lock().await.push(task_id);
            request.cancellation.cancelled().await;
        }
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
    pub runner: Arc<GatedTaskRunner>,
    pub allocator: Arc<MemoryWorkspaceAllocator>,
    events: broadcast::Receiver<TaskEventEnvelope>,
    _actor: JoinHandle<()>,
    _workspace: TempDir,
}

impl Harness {
    pub async fn new(config: CoordinatorConfig) -> Self {
        Self::new_with_runner(config, false).await
    }

    pub async fn new_paused(config: CoordinatorConfig) -> Self {
        Self::new_with_runner(config, true).await
    }

    async fn new_with_runner(config: CoordinatorConfig, pause_before_start: bool) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
        let runner = Arc::new(GatedTaskRunner::new(pause_before_start));
        let (handle, actor) = spawn_task_coordinator(
            config,
            runner.clone(),
            allocator.clone(),
            Arc::new(NoopTaskEventSink),
        );
        let events = handle.subscribe();
        Self {
            handle,
            runner,
            allocator,
            events,
            _actor: actor,
            _workspace: workspace,
        }
    }

    pub async fn register_root_scoped(
        &self,
        root: &str,
        session: &str,
        turn: &str,
    ) -> lato_runtime::ScopedTaskHandle {
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
            .unwrap()
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
            .map(|_| ())
    }

    pub async fn next_event(&mut self) -> TaskEventEnvelope {
        self.events.recv().await.unwrap()
    }

    pub fn has_pending_event(&mut self) -> bool {
        self.events.try_recv().is_ok()
    }

    pub async fn wait_for_status(&self, task_id: &str, expected: lato_core::TaskStatus) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if self
                    .handle
                    .inspect_admin(TaskId::from(task_id))
                    .await
                    .is_ok_and(|snapshot| snapshot.node.status == expected)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("task {task_id} did not reach {expected:?}"));
    }
}
