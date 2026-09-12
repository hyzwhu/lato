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
    SessionId, TaskError, TaskErrorCode, TaskId, TaskOwner, TaskScope, VerificationPolicy,
    WorkspaceIntent,
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
    steps: Vec<crate::WorkflowStep>,
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
        let child_tasks = u64::from(u32::try_from(descriptor.steps.len()).unwrap_or(1).max(1));
        let reservation = lock(&self.budget)
            .reserve(
                root_id.clone(),
                BudgetAmount {
                    child_tasks,
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
                profile: root_profile(&descriptor.steps),
                permissions: root_profile(&descriptor.steps).capabilities,
                budget: {
                    let mut limits = BudgetLimits::unlimited();
                    limits.child_tasks = Some(child_tasks);
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
                steps: descriptor.steps,
            },
        );
        Ok(run_id)
    }

    pub async fn wait(&self, run_id: &str) -> Result<WorkflowOutcome, WorkflowError> {
        let (scoped, descriptor_id, input, steps) = {
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
                run.steps.clone(),
            )
        };
        let mut last_step_id = None;
        let mut cancelled = false;
        let mut failed = None;
        for (index, step) in steps.iter().enumerate() {
            let step_id = TaskId::from(format!("wf-step-{run_id}-{index}"));
            last_step_id = Some(step_id.clone());
            let profile = step.profile.agent_profile();
            let spawn = scoped
                .spawn_and_wait(SpawnTaskRequest {
                    task_id: step_id.clone(),
                    scope: TaskScope {
                        objective: format!("{}\n\ninput: {input}", step.prompt),
                        context_refs: Vec::new(),
                    },
                    profile: profile.clone(),
                    requested_capabilities: Some(profile.capabilities.clone()),
                    budget: BudgetLimits::unlimited(),
                    result_contract: ResultContract {
                        schema: None,
                        max_output_bytes: 4_096,
                    },
                    mode: SpawnMode::AwaitCompletion,
                    cancellation: CancellationToken::new(),
                })
                .await;
            let step_cancelled = spawn
                .as_ref()
                .is_ok_and(|disposition| disposition.explicitly_killed)
                || self
                    .coordinator
                    .inspect_admin(step_id)
                    .await
                    .is_ok_and(|snapshot| snapshot.node.status.is_cancelled());
            if step_cancelled {
                cancelled = true;
                break;
            }
            if let Err(error) = spawn {
                failed = Some(map_task(error));
                break;
            }
        }
        let _ = last_step_id;
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
        if let Some(error) = failed {
            self.finish_run(run_id, false);
            return Err(error);
        }
        let child_tasks = u64::from(u32::try_from(steps.len()).unwrap_or(1).max(1));
        self.finish_run_with(run_id, Some(child_tasks));
        Ok(WorkflowOutcome {
            run_id: run_id.to_owned(),
            status: WorkflowStatus::Completed,
            output: serde_json::json!({
                "workflow": descriptor_id,
                "input": input,
                "steps": steps.iter().map(|step| step.prompt.clone()).collect::<Vec<_>>(),
            }),
        })
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
        self.finish_run_with(run_id, success.then_some(1));
    }

    fn finish_run_with(&self, run_id: &str, settled_child_tasks: Option<u64>) {
        let reservation = lock(&self.runs)
            .remove(run_id)
            .and_then(|run| run.reservation);
        let Some(reservation) = reservation else {
            return;
        };
        let mut budget = lock(&self.budget);
        if let Some(child_tasks) = settled_child_tasks {
            let _ = budget.settle(
                reservation,
                BudgetAmount {
                    child_tasks,
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

fn root_profile(steps: &[crate::WorkflowStep]) -> AgentProfile {
    let mut capabilities = Vec::new();
    let mut verification = VerificationPolicy::HumanGate;
    let mut workspace = WorkspaceIntent::SharedReadOnly;
    for step in steps {
        let profile = step.profile.agent_profile();
        for capability in profile.capabilities {
            if !capabilities.contains(&capability) {
                capabilities.push(capability);
            }
        }
        verification = min_verification(verification, profile.verification);
        if matches!(profile.workspace, WorkspaceIntent::IsolatedWorktree) {
            workspace = WorkspaceIntent::IsolatedWorktree;
        }
    }
    if capabilities.is_empty() {
        return AgentProfile::worker();
    }
    AgentProfile {
        name: "workflow-root".into(),
        instructions: "Own a declarative workflow run without widening child authority.".into(),
        capabilities,
        workspace,
        verification,
        definition_background: false,
    }
}

fn min_verification(left: VerificationPolicy, right: VerificationPolicy) -> VerificationPolicy {
    if verification_rank(left) <= verification_rank(right) {
        left
    } else {
        right
    }
}

fn verification_rank(policy: VerificationPolicy) -> u8 {
    match policy {
        VerificationPolicy::Accept => 0,
        VerificationPolicy::Schema => 1,
        VerificationPolicy::Programmatic => 2,
        VerificationPolicy::IndependentReview => 3,
        VerificationPolicy::HumanGate => 4,
    }
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
