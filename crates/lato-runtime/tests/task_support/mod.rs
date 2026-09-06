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
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
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
    noncooperative: bool,
    panic_on_completed: bool,
    pause_validation: bool,
    validation_error: bool,
    validation_panic: bool,
    validation_entered: AtomicBool,
    validation_gate: Notify,
    entered: Mutex<HashSet<TaskId>>,
    started: Mutex<Vec<TaskId>>,
    gates: Mutex<HashMap<TaskId, Arc<Notify>>>,
    finish_gates: Mutex<HashMap<TaskId, Arc<Notify>>>,
    changed: Notify,
    active_runs: Arc<AtomicUsize>,
    completion_callbacks: AtomicUsize,
}

impl GatedTaskRunner {
    pub fn new(pause_before_start: bool) -> Self {
        Self::with_options(pause_before_start, false, false, false)
    }

    pub fn with_options(
        pause_before_start: bool,
        noncooperative: bool,
        panic_on_completed: bool,
        pause_validation: bool,
    ) -> Self {
        Self {
            pause_before_start,
            noncooperative,
            panic_on_completed,
            pause_validation,
            validation_error: false,
            validation_panic: false,
            validation_entered: AtomicBool::new(false),
            validation_gate: Notify::new(),
            entered: Mutex::new(HashSet::new()),
            started: Mutex::new(Vec::new()),
            gates: Mutex::new(HashMap::new()),
            finish_gates: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            active_runs: Arc::new(AtomicUsize::new(0)),
            completion_callbacks: AtomicUsize::new(0),
        }
    }

    pub fn failing_validator() -> Self {
        let mut runner = Self::new(false);
        runner.validation_error = true;
        runner
    }

    pub fn panicking_validator() -> Self {
        let mut runner = Self::new(false);
        runner.validation_panic = true;
        runner
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

    pub async fn finish(&self, task_id: &str) {
        if let Some(gate) = self.finish_gates.lock().await.get(&TaskId::from(task_id)) {
            gate.notify_one();
        }
    }

    pub fn active_runs(&self) -> usize {
        self.active_runs.load(Ordering::Acquire)
    }

    pub fn completion_callbacks(&self) -> usize {
        self.completion_callbacks.load(Ordering::Acquire)
    }

    pub async fn wait_until_validation_entered(&self) {
        while !self.validation_entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    pub fn validation_entered(&self) -> bool {
        self.validation_entered.load(Ordering::Acquire)
    }

    pub fn allow_validation(&self) {
        self.validation_gate.notify_one();
    }
}

struct ActiveRunGuard(Arc<AtomicUsize>);

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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
        let finish_gate = Arc::new(Notify::new());
        self.active_runs.fetch_add(1, Ordering::AcqRel);
        let _active = ActiveRunGuard(self.active_runs.clone());
        self.gates
            .lock()
            .await
            .insert(task_id.clone(), gate.clone());
        self.finish_gates
            .lock()
            .await
            .insert(task_id.clone(), finish_gate.clone());
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
            if self.noncooperative {
                std::future::pending::<()>().await;
            } else {
                tokio::select! {
                    _ = request.cancellation.cancelled() => {}
                    _ = finish_gate.notified() => {}
                }
            }
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
        if self.pause_validation {
            self.validation_entered.store(true, Ordering::Release);
            self.validation_gate.notified().await;
        }
        assert!(!self.validation_panic, "injected profile validation panic");
        if self.validation_error {
            return Err(TaskError::new(
                lato_core::TaskErrorCode::InvalidProfile,
                "injected profile validation error",
            ));
        }
        Ok(())
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {
        assert!(
            !self.panic_on_completed,
            "injected completion callback panic"
        );
        self.completion_callbacks.fetch_add(1, Ordering::AcqRel);
    }
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
        Self::with_runner(config, Arc::new(GatedTaskRunner::new(pause_before_start))).await
    }

    pub async fn with_runner(config: CoordinatorConfig, runner: Arc<GatedTaskRunner>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
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

    pub async fn register_root_scoped_with_budget(
        &self,
        root: &str,
        budget: BudgetLimits,
    ) -> lato_runtime::ScopedTaskHandle {
        self.handle
            .register_root(TaskRootRequest {
                task_id: TaskId::from(root),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from("session"),
                    turn_id: TurnId::from("turn"),
                },
                profile: AgentProfile::worker(),
                permissions: vec![ToolCapability::FileRead, ToolCapability::FileWrite],
                budget,
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
