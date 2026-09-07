// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: authoritative real-tree cancellation scopes with root and workflow isolation

use crate::task::InspectCaller;
use crate::task::state::CoordinatorState;
use lato_core::{SessionId, TaskError, TaskErrorCode, TaskId, TaskOwner, TurnId};
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CancelTarget {
    Task {
        task_id: TaskId,
    },
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Root {
        root_id: TaskId,
    },
    Workflow {
        run_id: String,
        root_id: Option<TaskId>,
    },
}

impl CancelTarget {
    pub fn task(task_id: TaskId) -> Self {
        Self::Task { task_id }
    }

    pub fn root(root_id: TaskId) -> Self {
        Self::Root { root_id }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CancelOutcome {
    pub matched: usize,
    pub newly_requested: usize,
    pub already_terminal: usize,
}

pub(crate) struct ResolvedCancellation {
    pub(crate) task_ids: Vec<TaskId>,
    pub(crate) admission_roots: Vec<TaskId>,
    pub(crate) admission_tasks: Vec<TaskId>,
    pub(crate) outcome: CancelOutcome,
}

pub(crate) fn resolve_cancellation(
    state: &CoordinatorState,
    caller: &InspectCaller,
    target: &CancelTarget,
) -> Result<ResolvedCancellation, TaskError> {
    let task_ids = match target {
        CancelTarget::Task { task_id } => {
            if !caller_owns(state, caller, task_id) {
                return Err(not_found());
            }
            state.tasks.get(task_id).ok_or_else(not_found)?;
            state.descendants_including(task_id)
        }
        CancelTarget::Turn {
            session_id,
            turn_id,
        } => state
            .tasks
            .iter()
            .filter(|(_, record)| {
                record.node.parent_id.is_some()
                    && matches!(
                        &record.node.owner,
                        TaskOwner::Interactive {
                            session_id: owner_session,
                            turn_id: owner_turn,
                        } if owner_session == session_id && owner_turn == turn_id
                    )
            })
            .map(|(task_id, _)| task_id.clone())
            .collect(),
        CancelTarget::Root { root_id } => {
            if !state.roots.contains(root_id) {
                return Err(not_found());
            }
            state
                .tasks
                .iter()
                .filter(|(_, record)| {
                    record.node.parent_id.is_some()
                        && &record.node.root_id == root_id
                        && matches!(record.node.owner, TaskOwner::Interactive { .. })
                })
                .map(|(task_id, _)| task_id.clone())
                .collect()
        }
        CancelTarget::Workflow { run_id, root_id } => {
            if root_id
                .as_ref()
                .is_some_and(|root_id| !state.roots.contains(root_id))
            {
                return Err(not_found());
            }
            state
                .tasks
                .iter()
                .filter(|(_, record)| {
                    record.node.parent_id.is_some()
                        && root_id
                            .as_ref()
                            .is_none_or(|root_id| &record.node.root_id == root_id)
                        && matches!(
                            &record.node.owner,
                            TaskOwner::Workflow { run_id: owner_run, .. } if owner_run == run_id
                        )
                })
                .map(|(task_id, _)| task_id.clone())
                .collect()
        }
    };

    let mut task_ids: Vec<_> = task_ids;
    // Descendants are cancelled before ancestors, avoiding transient parent
    // completion from admitting queued work back into the dying subtree.
    task_ids.sort_by_key(|task_id| {
        std::cmp::Reverse(state.tasks.get(task_id).map_or(0, |record| record.depth))
    });
    let already_terminal = task_ids
        .iter()
        .filter(|task_id| state.tasks[*task_id].node.status.is_terminal())
        .count();
    let newly_requested = task_ids
        .iter()
        .filter(|task_id| {
            let record = &state.tasks[*task_id];
            !record.node.status.is_terminal() && !record.cancellation.is_cancelled()
        })
        .count();

    let admission_tasks = match target {
        CancelTarget::Task { task_id } => vec![task_id.clone()],
        _ => Vec::new(),
    };
    let admission_roots = match target {
        CancelTarget::Task { .. } => Vec::new(),
        CancelTarget::Root { root_id } => vec![root_id.clone()],
        CancelTarget::Turn {
            session_id,
            turn_id,
        } => roots_for(state, |owner| {
            matches!(
                owner,
                TaskOwner::Interactive {
                    session_id: owner_session,
                    turn_id: owner_turn,
                } if owner_session == session_id && owner_turn == turn_id
            )
        }),
        CancelTarget::Workflow { run_id, root_id } => roots_for(state, |owner| {
            matches!(owner, TaskOwner::Workflow { run_id: owner_run, .. } if owner_run == run_id)
        })
        .into_iter()
        .filter(|candidate| root_id.as_ref().is_none_or(|root_id| candidate == root_id))
        .collect(),
    };

    Ok(ResolvedCancellation {
        outcome: CancelOutcome {
            matched: task_ids.len(),
            newly_requested,
            already_terminal,
        },
        task_ids,
        admission_roots,
        admission_tasks,
    })
}

pub(crate) fn resolve_root_teardown(
    state: &CoordinatorState,
    caller: &InspectCaller,
    root_id: &TaskId,
) -> Result<ResolvedCancellation, TaskError> {
    let authorized = match caller {
        InspectCaller::Admin => true,
        InspectCaller::Scoped {
            root_id: caller_root,
            task_id: caller_task,
        } => caller_root == root_id && caller_task == root_id,
    };
    if !state.roots.contains(root_id) || !authorized {
        return Err(not_found());
    }
    let mut task_ids: Vec<_> = state
        .tasks
        .iter()
        .filter(|(_, record)| record.node.parent_id.is_some() && &record.node.root_id == root_id)
        .map(|(task_id, _)| task_id.clone())
        .collect();
    task_ids.sort_by_key(|task_id| {
        std::cmp::Reverse(state.tasks.get(task_id).map_or(0, |record| record.depth))
    });
    let already_terminal = task_ids
        .iter()
        .filter(|task_id| state.tasks[*task_id].node.status.is_terminal())
        .count();
    let newly_requested = task_ids
        .iter()
        .filter(|task_id| {
            let record = &state.tasks[*task_id];
            !record.node.status.is_terminal() && !record.cancellation.is_cancelled()
        })
        .count();
    Ok(ResolvedCancellation {
        outcome: CancelOutcome {
            matched: task_ids.len(),
            newly_requested,
            already_terminal,
        },
        task_ids,
        admission_roots: vec![root_id.clone()],
        admission_tasks: Vec::new(),
    })
}

pub(crate) fn target_matches_live(state: &CoordinatorState, target: &CancelTarget) -> bool {
    match target {
        CancelTarget::Root { root_id } => state.tasks.values().any(|record| {
            record.node.parent_id.is_some()
                && &record.node.root_id == root_id
                && !record.node.status.is_terminal()
        }),
        CancelTarget::Workflow { run_id, root_id } => state.tasks.values().any(|record| {
            record.node.parent_id.is_some()
                && !record.node.status.is_terminal()
                && root_id
                    .as_ref()
                    .is_none_or(|root_id| &record.node.root_id == root_id)
                && matches!(
                    &record.node.owner,
                    TaskOwner::Workflow { run_id: owner_run, .. } if owner_run == run_id
                )
        }),
        CancelTarget::Task { .. } | CancelTarget::Turn { .. } => false,
    }
}

pub(crate) fn target_contains_task(
    state: &CoordinatorState,
    target: &CancelTarget,
    task_id: &TaskId,
) -> bool {
    let Some(record) = state.tasks.get(task_id) else {
        return false;
    };
    if record.node.parent_id.is_none() {
        return false;
    }
    match target {
        CancelTarget::Root { root_id } => &record.node.root_id == root_id,
        CancelTarget::Workflow { run_id, root_id } => {
            root_id
                .as_ref()
                .is_none_or(|root_id| &record.node.root_id == root_id)
                && matches!(
                    &record.node.owner,
                    TaskOwner::Workflow { run_id: owner_run, .. } if owner_run == run_id
                )
        }
        CancelTarget::Task { .. } | CancelTarget::Turn { .. } => false,
    }
}

fn roots_for(
    state: &CoordinatorState,
    mut owner_matches: impl FnMut(&TaskOwner) -> bool,
) -> Vec<TaskId> {
    let mut roots = HashSet::new();
    for record in state
        .tasks
        .values()
        .filter(|record| owner_matches(&record.node.owner))
    {
        roots.insert(record.node.root_id.clone());
    }
    roots.into_iter().collect()
}

fn caller_owns(state: &CoordinatorState, caller: &InspectCaller, target: &TaskId) -> bool {
    match caller {
        InspectCaller::Admin => state.contains(target),
        InspectCaller::Scoped { root_id, task_id } => {
            state.is_self_or_descendant(root_id, task_id, target)
        }
    }
}

fn not_found() -> TaskError {
    TaskError::new(
        TaskErrorCode::NotFoundOrNotOwned,
        "task was not found in the requested scope",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SpawnMode;
    use crate::task::state::RuntimeTaskRecord;
    use lato_core::{
        AgentProfile, BudgetAccount, BudgetLimits, ResultContract, TaskNode, TaskProgress,
        TaskScope, TaskStatus,
    };
    use tokio_util::sync::CancellationToken;

    fn record(id: &str, parent_id: Option<&str>, owner: TaskOwner) -> RuntimeTaskRecord {
        let profile = AgentProfile::worker();
        RuntimeTaskRecord {
            node: TaskNode {
                id: TaskId::from(id),
                parent_id: parent_id.map(TaskId::from),
                root_id: TaskId::from("root"),
                owner,
                scope: TaskScope {
                    objective: id.into(),
                    context_refs: Vec::new(),
                },
                permissions: Vec::new(),
                workspace_intent: profile.workspace,
                result_contract: ResultContract {
                    schema: None,
                    max_output_bytes: 1,
                },
                profile,
                status: TaskStatus::Running,
            },
            budget: BudgetAccount::new(BudgetLimits::unlimited()),
            workspace_lease: None,
            reservation: None,
            reservation_parent_id: None,
            cancellation: CancellationToken::new(),
            spawn_admission_closed: false,
            depth: u32::from(parent_id.is_some()),
            cleanup_error: None,
            last_event_sequence: 0,
            progress: TaskProgress::default(),
            usage: Default::default(),
            result: None,
            completion_disposition: None,
            output_metadata: None,
            spawn_mode: Some(SpawnMode::Background),
            enqueued_at: tokio::time::Instant::now(),
            active_messages: Default::default(),
            generation: Some(1),
            verification_output: None,
            verification_wait: None,
            budget_failure: None,
            budget_settled: false,
            charged_wall_time_ms: 0,
        }
    }

    #[test]
    fn root_cancel_spares_workflow_descendants_but_teardown_includes_them() {
        let interactive = TaskOwner::Interactive {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
        };
        let workflow = TaskOwner::Workflow {
            run_id: "run".into(),
            session_id: SessionId::from("session"),
        };
        let mut state = CoordinatorState::default();
        for entry in [
            record("root", None, interactive.clone()),
            record("interactive-child", Some("root"), interactive),
            record("workflow-child", Some("root"), workflow),
        ] {
            state.tasks.insert(entry.node.id.clone(), entry);
        }
        state.roots.insert(TaskId::from("root"));

        let root_id = TaskId::from("root");
        let cancelled = resolve_cancellation(
            &state,
            &InspectCaller::Admin,
            &CancelTarget::root(root_id.clone()),
        )
        .unwrap();
        assert_eq!(cancelled.task_ids, vec![TaskId::from("interactive-child")]);

        let mut teardown = resolve_root_teardown(&state, &InspectCaller::Admin, &root_id)
            .unwrap()
            .task_ids;
        teardown.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        assert_eq!(
            teardown,
            vec![
                TaskId::from("interactive-child"),
                TaskId::from("workflow-child")
            ]
        );
    }
}
