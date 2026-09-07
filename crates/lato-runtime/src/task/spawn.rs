// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: validation and budget reservation preserve Lato's real parent lineage

use crate::task::state::CoordinatorState;
use crate::task::{CoordinatorConfig, SpawnTaskRequest};
use lato_core::{
    BudgetAmount, BudgetDimension, BudgetLimits, BudgetReservation, TaskError, TaskErrorCode,
    TaskId, ToolCapability, VerificationPolicy, WorkspaceIntent,
};

#[derive(Debug)]
pub(crate) struct SpawnStructure {
    pub owner: lato_core::TaskOwner,
    pub root_id: TaskId,
    pub depth: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PendingSpawnOccupancy {
    pub duplicate: bool,
    pub tasks: usize,
    pub children: usize,
}

pub(crate) fn validate_structure(
    state: &CoordinatorState,
    config: &CoordinatorConfig,
    caller_root: &TaskId,
    caller_parent: &TaskId,
    request: &SpawnTaskRequest,
    pending: PendingSpawnOccupancy,
) -> Result<SpawnStructure, TaskError> {
    if !state.is_self_or_descendant(caller_root, caller_parent, caller_parent) {
        return Err(not_owned());
    }
    let parent = state.tasks.get(caller_parent).ok_or_else(not_owned)?;
    if &parent.node.root_id != caller_root {
        return Err(not_owned());
    }
    if parent.node.status.is_terminal() {
        return Err(TaskError::new(
            TaskErrorCode::TerminalParent,
            "terminal tasks cannot spawn children",
        ));
    }
    if parent.spawn_admission_closed || parent.cancellation.is_cancelled() {
        return Err(spawn_admission_closed());
    }
    let mut ancestor_id = parent.node.parent_id.as_ref();
    while let Some(task_id) = ancestor_id {
        let ancestor = state.tasks.get(task_id).ok_or_else(not_owned)?;
        if &ancestor.node.root_id != caller_root {
            return Err(not_owned());
        }
        if ancestor.node.status.is_terminal()
            || ancestor.spawn_admission_closed
            || ancestor.cancellation.is_cancelled()
        {
            return Err(spawn_admission_closed());
        }
        ancestor_id = ancestor.node.parent_id.as_ref();
    }
    if state.contains(&request.task_id) || pending.duplicate {
        return Err(TaskError::new(
            TaskErrorCode::DuplicateTask,
            "task identifier is already registered",
        ));
    }
    let depth = state
        .depth(caller_parent)
        .ok_or_else(not_owned)?
        .saturating_add(1);
    if depth > config.max_depth {
        return Err(TaskError::new(
            TaskErrorCode::DepthLimit,
            "task depth limit is exhausted",
        ));
    }
    if state
        .child_count(caller_parent)
        .saturating_add(pending.children)
        >= config.max_children_per_parent
    {
        return Err(TaskError::new(
            TaskErrorCode::ChildLimit,
            "task child limit is exhausted",
        ));
    }
    if state.tasks.len().saturating_add(pending.tasks) >= config.max_total_tasks {
        return Err(TaskError::new(
            TaskErrorCode::RetentionLimit,
            "task registry capacity is exhausted",
        ));
    }
    Ok(SpawnStructure {
        owner: parent.node.owner.clone(),
        root_id: parent.node.root_id.clone(),
        depth,
    })
}

pub(crate) fn reserve(
    state: &mut CoordinatorState,
    parent_id: &TaskId,
    request: &SpawnTaskRequest,
) -> Result<(Vec<ToolCapability>, BudgetLimits, BudgetReservation), TaskError> {
    let parent = state
        .tasks
        .get_mut(parent_id)
        .expect("structurally validated parent remains actor-owned");
    let permissions = request.profile.effective_capabilities(
        &parent.node.permissions,
        request.requested_capabilities.as_deref(),
    )?;
    let fixed = BudgetAmount {
        child_tasks: 1,
        worktrees: u64::from(request.profile.workspace == WorkspaceIntent::IsolatedWorktree),
        ..BudgetAmount::ZERO
    };
    let (effective, amount) = normalize_budget(&request.budget, &parent.budget.remaining(), fixed)?;
    let reservation = state
        .tasks
        .get_mut(parent_id)
        .expect("validated parent remains actor-owned")
        .budget
        .reserve(request.task_id.clone(), amount)
        .map_err(|error| {
            TaskError::new(
                TaskErrorCode::BudgetReservation,
                format!("task budget reservation failed: {error}"),
            )
        })?;
    Ok((permissions, effective, reservation))
}

pub(crate) fn validate_profile_authority(
    parent: &lato_core::AgentProfile,
    child: &lato_core::AgentProfile,
) -> Result<(), TaskError> {
    if !workspace_narrows(parent.workspace, child.workspace) {
        return Err(TaskError::new(
            TaskErrorCode::InvalidProfile,
            "child workspace policy widens inherited authority",
        ));
    }
    if verification_strength(child.verification) < verification_strength(parent.verification) {
        return Err(TaskError::new(
            TaskErrorCode::InvalidProfile,
            "child verification policy weakens inherited verification",
        ));
    }
    if child.definition_background && !parent.definition_background {
        return Err(TaskError::new(
            TaskErrorCode::InvalidProfile,
            "child background policy widens inherited lifetime authority",
        ));
    }
    Ok(())
}

fn workspace_narrows(parent: WorkspaceIntent, child: WorkspaceIntent) -> bool {
    parent == child
        || matches!(
            (parent, child),
            (
                WorkspaceIntent::SharedSerializedWrite
                    | WorkspaceIntent::IsolatedWorktree
                    | WorkspaceIntent::ExternalLease,
                WorkspaceIntent::SharedReadOnly
            )
        )
}

const fn verification_strength(policy: VerificationPolicy) -> u8 {
    match policy {
        VerificationPolicy::Accept => 0,
        VerificationPolicy::Schema => 1,
        VerificationPolicy::Programmatic => 2,
        VerificationPolicy::IndependentReview => 3,
        VerificationPolicy::HumanGate => 4,
    }
}

fn normalize_budget(
    requested: &BudgetLimits,
    parent_remaining: &BudgetLimits,
    fixed: BudgetAmount,
) -> Result<(BudgetLimits, BudgetAmount), TaskError> {
    let input_tokens = normalize_dimension(
        requested.input_tokens,
        parent_remaining.input_tokens,
        fixed.input_tokens,
        BudgetDimension::InputTokens,
    )?;
    let output_tokens = normalize_dimension(
        requested.output_tokens,
        parent_remaining.output_tokens,
        fixed.output_tokens,
        BudgetDimension::OutputTokens,
    )?;
    let total_tokens = normalize_dimension(
        requested.total_tokens,
        parent_remaining.total_tokens,
        fixed.total_tokens,
        BudgetDimension::TotalTokens,
    )?;
    let tool_calls = normalize_dimension(
        requested.tool_calls,
        parent_remaining.tool_calls,
        fixed.tool_calls,
        BudgetDimension::ToolCalls,
    )?;
    let cost_micros = normalize_dimension(
        requested.cost_micros,
        parent_remaining.cost_micros,
        fixed.cost_micros,
        BudgetDimension::CostMicros,
    )?;
    let wall_time_ms = normalize_dimension(
        requested.wall_time_ms,
        parent_remaining.wall_time_ms,
        fixed.wall_time_ms,
        BudgetDimension::WallTimeMs,
    )?;
    let retries = normalize_dimension(
        requested.retries,
        parent_remaining.retries,
        fixed.retries,
        BudgetDimension::Retries,
    )?;
    let child_tasks = normalize_dimension(
        requested.child_tasks,
        parent_remaining.child_tasks,
        fixed.child_tasks,
        BudgetDimension::ChildTasks,
    )?;
    let worktrees = normalize_dimension(
        requested.worktrees,
        parent_remaining.worktrees,
        fixed.worktrees,
        BudgetDimension::Worktrees,
    )?;
    Ok((
        BudgetLimits {
            input_tokens: input_tokens.0,
            output_tokens: output_tokens.0,
            total_tokens: total_tokens.0,
            tool_calls: tool_calls.0,
            cost_micros: cost_micros.0,
            wall_time_ms: wall_time_ms.0,
            retries: retries.0,
            child_tasks: child_tasks.0,
            worktrees: worktrees.0,
        },
        BudgetAmount {
            input_tokens: input_tokens.1,
            output_tokens: output_tokens.1,
            total_tokens: total_tokens.1,
            tool_calls: tool_calls.1,
            cost_micros: cost_micros.1,
            wall_time_ms: wall_time_ms.1,
            retries: retries.1,
            child_tasks: child_tasks.1,
            worktrees: worktrees.1,
        },
    ))
}

fn normalize_dimension(
    requested: Option<u64>,
    parent_remaining: Option<u64>,
    fixed: u64,
    dimension: BudgetDimension,
) -> Result<(Option<u64>, u64), TaskError> {
    let ceiling = parent_remaining
        .map(|remaining| {
            remaining
                .checked_sub(fixed)
                .ok_or_else(|| budget_exceeded(dimension, fixed, remaining))
        })
        .transpose()?;
    let effective = match (requested, ceiling) {
        (Some(value), Some(ceiling)) if value > ceiling => {
            return Err(budget_exceeded(
                dimension,
                value.saturating_add(fixed),
                ceiling.saturating_add(fixed),
            ));
        }
        (Some(value), _) => Some(value),
        (None, Some(ceiling)) => Some(ceiling),
        (None, None) => None,
    };
    let reservation = effective.unwrap_or(0).checked_add(fixed).ok_or_else(|| {
        TaskError::new(
            TaskErrorCode::BudgetReservation,
            format!("task budget arithmetic overflow in {dimension}"),
        )
    })?;
    Ok((effective, reservation))
}

fn budget_exceeded(dimension: BudgetDimension, requested: u64, available: u64) -> TaskError {
    TaskError::new(
        TaskErrorCode::BudgetReservation,
        format!(
            "task budget exceeds parent remaining {dimension}: requested {requested}, available {available}"
        ),
    )
}

fn not_owned() -> TaskError {
    TaskError::new(
        TaskErrorCode::NotFoundOrNotOwned,
        "task was not found in the requested scope",
    )
}

fn spawn_admission_closed() -> TaskError {
    TaskError::new(
        TaskErrorCode::SpawnAdmissionClosed,
        "task spawn admission is closed for a cancelling or terminal ancestor",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::state::RuntimeTaskRecord;
    use crate::task::{SpawnMode, TaskRootRequest, root_node};
    use lato_core::{
        AgentProfile, BudgetAccount, ResultContract, SessionId, TaskOwner, TaskScope, TurnId,
    };
    use tokio_util::sync::CancellationToken;

    fn root_record(id: &str) -> RuntimeTaskRecord {
        let node = root_node(&TaskRootRequest {
            task_id: TaskId::from(id),
            owner: TaskOwner::Interactive {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            profile: AgentProfile::worker(),
            permissions: vec![ToolCapability::FileRead],
            budget: BudgetLimits::unlimited(),
        });
        RuntimeTaskRecord {
            node,
            budget: BudgetAccount::new(BudgetLimits::unlimited()),
            workspace_lease: None,
            reservation: None,
            reservation_parent_id: None,
            cancellation: CancellationToken::new(),
            spawn_admission_closed: false,
            depth: 0,
            cleanup_error: None,
            last_event_sequence: 0,
            progress: Default::default(),
            usage: Default::default(),
            result: None,
            completion_disposition: None,
            output_metadata: None,
            spawn_mode: None,
            enqueued_at: tokio::time::Instant::now(),
        }
    }

    fn request(id: &str) -> SpawnTaskRequest {
        SpawnTaskRequest {
            task_id: TaskId::from(id),
            scope: TaskScope {
                objective: "test precedence".into(),
                context_refs: Vec::new(),
            },
            profile: AgentProfile::worker(),
            requested_capabilities: None,
            budget: BudgetLimits::unlimited(),
            result_contract: ResultContract {
                schema: None,
                max_output_bytes: 1,
            },
            mode: SpawnMode::Background,
            cancellation: CancellationToken::new(),
        }
    }

    #[test]
    fn foreign_parent_precedes_duplicate_and_retention_errors() {
        let mut state = CoordinatorState::default();
        state
            .tasks
            .insert(TaskId::from("root-a"), root_record("root-a"));
        state
            .tasks
            .insert(TaskId::from("root-b"), root_record("root-b"));
        let config = CoordinatorConfig {
            max_total_tasks: 2,
            ..CoordinatorConfig::default()
        };

        let error = validate_structure(
            &state,
            &config,
            &TaskId::from("root-a"),
            &TaskId::from("root-b"),
            &request("root-b"),
            PendingSpawnOccupancy::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, TaskErrorCode::NotFoundOrNotOwned);
    }
}
