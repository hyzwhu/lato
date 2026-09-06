// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: validation and budget reservation preserve Lato's real parent lineage

use crate::task::state::CoordinatorState;
use crate::task::{CoordinatorConfig, SpawnTaskRequest};
use lato_core::{BudgetReservation, TaskError, TaskErrorCode, TaskId, ToolCapability};

pub(crate) struct SpawnStructure {
    pub owner: lato_core::TaskOwner,
    pub root_id: TaskId,
    pub depth: u32,
}

pub(crate) fn validate_structure(
    state: &CoordinatorState,
    config: &CoordinatorConfig,
    caller_root: &TaskId,
    caller_parent: &TaskId,
    request: &SpawnTaskRequest,
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
    if state.contains(&request.task_id) {
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
    if state.child_count(caller_parent) >= config.max_children_per_parent {
        return Err(TaskError::new(
            TaskErrorCode::ChildLimit,
            "task child limit is exhausted",
        ));
    }
    if state.tasks.len() >= config.max_total_tasks {
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
) -> Result<(Vec<ToolCapability>, BudgetReservation), TaskError> {
    let parent = state
        .tasks
        .get_mut(parent_id)
        .expect("structurally validated parent remains actor-owned");
    let permissions = request.profile.effective_capabilities(
        &parent.node.permissions,
        request.requested_capabilities.as_deref(),
    )?;
    let reservation = state
        .tasks
        .get_mut(parent_id)
        .expect("validated parent remains actor-owned")
        .budget
        .reserve(request.task_id.clone(), request.reservation)
        .map_err(|error| {
            TaskError::new(
                TaskErrorCode::BudgetReservation,
                format!("task budget reservation failed: {error}"),
            )
        })?;
    Ok((permissions, reservation))
}

fn not_owned() -> TaskError {
    TaskError::new(
        TaskErrorCode::NotFoundOrNotOwned,
        "task was not found in the requested scope",
    )
}
