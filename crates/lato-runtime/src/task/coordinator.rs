// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: bounded Tokio actor with a single committed root transition path

use crate::task::state::{CoordinatorState, RuntimeTaskRecord};
use crate::task::{
    CoordinatorConfig, RunnerEvent, TaskCommand, TaskEventEnvelope, TaskEventPayload,
    TaskEventSink, TaskHandle, TaskRunner, root_node,
};
use lato_core::{BudgetAccount, TaskError, TaskErrorCode, TaskId};
use lato_workspace::WorkspaceAllocator;
use std::sync::Arc;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

pub struct TaskCoordinator<R: TaskRunner, A: WorkspaceAllocator> {
    _config: CoordinatorConfig,
    _runner: Arc<R>,
    _workspace_allocator: Arc<A>,
    command_rx: mpsc::Receiver<TaskCommand>,
    _internal_tx: mpsc::Sender<RunnerEvent<R::Control>>,
    internal_rx: mpsc::Receiver<RunnerEvent<R::Control>>,
    event_tx: broadcast::Sender<TaskEventEnvelope>,
    event_sink: Arc<dyn TaskEventSink>,
    state: CoordinatorState,
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
    let handle = TaskHandle {
        command_tx,
        event_tx: event_tx.clone(),
    };
    let coordinator = TaskCoordinator {
        _config: config,
        _runner: runner,
        _workspace_allocator: workspace_allocator,
        command_rx,
        _internal_tx: internal_tx,
        internal_rx,
        event_tx,
        event_sink,
        state: CoordinatorState::default(),
        sequence: 0,
    };
    let actor = tokio::spawn(coordinator.run());
    (handle, actor)
}

impl<R: TaskRunner, A: WorkspaceAllocator> TaskCoordinator<R, A> {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    if self.handle_command(command) { break; }
                }
                event = self.internal_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_runner_event(event);
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: TaskCommand) -> bool {
        match command {
            TaskCommand::RegisterRoot { request, reply } => {
                let result = self.register_root(*request);
                let _ = reply.send(result);
            }
            TaskCommand::Inspect { task_id, reply } => {
                let result = self.state.inspection(&task_id).ok_or_else(not_found);
                let _ = reply.send(result);
            }
            TaskCommand::RegistryCounts { reply } => {
                let _ = reply.send(self.state.counts());
            }
            TaskCommand::ShutdownRoot { root_id, reply } => {
                let result = self.shutdown_root(&root_id);
                let _ = reply.send(result);
            }
            TaskCommand::Shutdown { reply } => {
                let _ = reply.send(());
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
        if self.state.tasks.len() >= self._config.max_total_tasks {
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

    fn handle_runner_event(&mut self, event: RunnerEvent<R::Control>) {
        match event {
            RunnerEvent::Started {
                task_id,
                started,
                acknowledgement,
            } => {
                let _ = (task_id, started);
                let _ = acknowledgement.send(false);
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
        self.event_sink.on_event(envelope.clone());
        envelope
    }
}

fn not_found() -> TaskError {
    TaskError::new(
        TaskErrorCode::NotFoundOrNotOwned,
        "task was not found in the requested scope",
    )
}
