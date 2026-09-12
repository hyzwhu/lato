//! Coordinator-backed single-step workflow engine (Phase 7B1).

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use lato_core::{
    AgentProfile, BudgetAccount, BudgetAmount, BudgetLimits, BudgetReservation, ResultContract,
    SessionId, TaskError, TaskErrorCode, TaskId, TaskOwner, TaskScope,
};
use lato_runtime::{SpawnMode, SpawnTaskRequest, TaskHandle, TaskRootRequest};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    WorkflowDescriptor, WorkflowDescriptorSet, WorkflowError, WorkflowOutcome, WorkflowStatus,
};

struct LiveRun {
    root_id: TaskId,
    reservation: Option<BudgetReservation>,
    scoped: Option<lato_runtime::ScopedTaskHandle>,
    descriptor_id: String,
    input: Value,
}

pub struct WorkflowEngine {
    descriptors: Arc<WorkflowDescriptorSet>,
    coordinator: TaskHandle,
    budget: Mutex<BudgetAccount>,
    runs: Mutex<HashMap<String, LiveRun>>,
    next_seq: AtomicU64,
}

impl WorkflowEngine {
    pub fn new(
        descriptors: Arc<WorkflowDescriptorSet>,
        coordinator: TaskHandle,
        budget: BudgetAccount,
    ) -> Self {
        Self {
            descriptors,
            coordinator,
            budget: Mutex::new(budget),
            runs: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(1),
        }
    }

    pub fn budget_spent(&self) -> BudgetAmount {
        lock(&self.budget).spent()
    }

    pub fn budget_reserved(&self) -> BudgetAmount {
        lock(&self.budget).reserved()
    }

    pub async fn start(
        &self,
        qualified_id: &str,
        session_id: SessionId,
        input: Value,
    ) -> Result<String, WorkflowError> {
        let descriptor = self
            .descriptors
            .workflows
            .iter()
            .find(|workflow| workflow.id == qualified_id)
            .cloned()
            .ok_or_else(|| WorkflowError::NotFound(qualified_id.to_owned()))?;
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let run_id = format!("wf-{seq}");
        let root_id = TaskId::from(format!("wf-root-{seq}"));
        let reservation = lock(&self.budget)
            .reserve(
                root_id.clone(),
                BudgetAmount {
                    child_tasks: 1,
                    ..BudgetAmount::ZERO
                },
            )
            .map_err(|error| WorkflowError::BudgetExceeded(error.to_string()))?;

        let started = self
            .coordinator
            .register_root(TaskRootRequest {
                task_id: root_id.clone(),
                owner: TaskOwner::Workflow {
                    run_id: run_id.clone(),
                    session_id,
                },
                profile: AgentProfile::worker(),
                permissions: AgentProfile::worker().capabilities,
                budget: {
                    let mut limits = BudgetLimits::unlimited();
                    limits.child_tasks = Some(1);
                    limits
                },
            })
            .await;
        let scoped = match started {
            Ok(scoped) => scoped,
            Err(error) => {
                let _ = lock(&self.budget).release(reservation);
                return Err(map_task(error));
            }
        };
        lock(&self.runs).insert(
            run_id.clone(),
            LiveRun {
                root_id,
                reservation: Some(reservation),
                scoped: Some(scoped),
                descriptor_id: descriptor.id,
                input,
            },
        );
        Ok(run_id)
    }

    pub async fn wait(&self, run_id: &str) -> Result<WorkflowOutcome, WorkflowError> {
        let (scoped, descriptor_id, input, seq_hint) = {
            let runs = lock(&self.runs);
            let run = runs
                .get(run_id)
                .ok_or_else(|| WorkflowError::NotFound(run_id.to_owned()))?;
            let scoped = run
                .scoped
                .clone()
                .ok_or_else(|| WorkflowError::Failed("run already waited".into()))?;
            (
                scoped,
                run.descriptor_id.clone(),
                run.input.clone(),
                run_id.to_owned(),
            )
        };
        let step_id = TaskId::from(format!("wf-step-{seq_hint}"));
        let spawn = scoped
            .spawn_and_wait(SpawnTaskRequest {
                task_id: step_id.clone(),
                scope: TaskScope {
                    objective: format!("workflow {descriptor_id}"),
                    context_refs: Vec::new(),
                },
                profile: AgentProfile::worker(),
                requested_capabilities: None,
                budget: BudgetLimits::unlimited(),
                result_contract: ResultContract {
                    schema: None,
                    max_output_bytes: 4_096,
                },
                mode: SpawnMode::AwaitCompletion,
                cancellation: CancellationToken::new(),
            })
            .await;
        let cancelled = spawn
            .as_ref()
            .is_ok_and(|disposition| disposition.explicitly_killed)
            || self
                .coordinator
                .inspect_admin(step_id)
                .await
                .is_ok_and(|snapshot| snapshot.node.status.is_cancelled());
        let root_id = lock(&self.runs).get(run_id).map(|run| run.root_id.clone());
        if let Some(root_id) = root_id {
            let _ = self
                .coordinator
                .cancel_workflow(run_id.to_owned(), Some(root_id))
                .await;
        }
        if cancelled {
            self.finish_run(run_id, false);
            return Err(WorkflowError::Cancelled);
        }
        match spawn {
            Ok(_disposition) => {
                self.finish_run(run_id, true);
                Ok(WorkflowOutcome {
                    run_id: run_id.to_owned(),
                    status: WorkflowStatus::Completed,
                    output: serde_json::json!({
                        "workflow": descriptor_id,
                        "input": input,
                    }),
                })
            }
            Err(error) => {
                self.finish_run(run_id, false);
                Err(map_task(error))
            }
        }
    }

    pub async fn run(
        &self,
        qualified_id: &str,
        session_id: SessionId,
        input: Value,
    ) -> Result<WorkflowOutcome, WorkflowError> {
        let run_id = self.start(qualified_id, session_id, input).await?;
        self.wait(&run_id).await
    }

    pub async fn cancel(&self, run_id: &str) -> Result<(), WorkflowError> {
        let root_id = {
            let runs = lock(&self.runs);
            runs.get(run_id)
                .map(|run| run.root_id.clone())
                .ok_or_else(|| WorkflowError::NotFound(run_id.to_owned()))?
        };
        self.coordinator
            .cancel_workflow(run_id.to_owned(), Some(root_id))
            .await
            .map(|_| ())
            .map_err(map_task)
    }

    fn finish_run(&self, run_id: &str, success: bool) {
        let reservation = lock(&self.runs)
            .remove(run_id)
            .and_then(|run| run.reservation);
        let Some(reservation) = reservation else {
            return;
        };
        let mut budget = lock(&self.budget);
        if success {
            let _ = budget.settle(
                reservation,
                BudgetAmount {
                    child_tasks: 1,
                    ..BudgetAmount::ZERO
                },
            );
        } else {
            let _ = budget.release(reservation);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn map_task(error: TaskError) -> WorkflowError {
    match error.code {
        TaskErrorCode::Cancelled => WorkflowError::Cancelled,
        _ => WorkflowError::Failed(error.message),
    }
}

impl WorkflowEngine {
    pub fn descriptor(&self, qualified_id: &str) -> Option<&WorkflowDescriptor> {
        self.descriptors
            .workflows
            .iter()
            .find(|workflow| workflow.id == qualified_id)
    }
}
