// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: real task-tree records own hierarchical budgets and workspace leases

use crate::task::{RegistryCounts, TaskSnapshot};
use lato_core::{BudgetAccount, BudgetReservation, TaskId, TaskNode, TaskStatus};
use lato_workspace::WorkspaceLease;
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

pub(crate) struct RuntimeTaskRecord {
    pub(crate) node: TaskNode,
    pub(crate) budget: BudgetAccount,
    pub(crate) workspace_lease: Option<WorkspaceLease>,
    pub(crate) reservation: Option<BudgetReservation>,
    pub(crate) reservation_parent_id: Option<TaskId>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) depth: u32,
    pub(crate) cleanup_error: Option<lato_core::TaskError>,
    pub(crate) last_event_sequence: u64,
}

#[derive(Default)]
pub(crate) struct CoordinatorState {
    pub(crate) tasks: HashMap<TaskId, RuntimeTaskRecord>,
    pub(crate) roots: HashSet<TaskId>,
}

impl CoordinatorState {
    pub(crate) fn contains(&self, task_id: &TaskId) -> bool {
        self.tasks.contains_key(task_id)
    }

    pub(crate) fn child_count(&self, parent_id: &TaskId) -> usize {
        self.tasks
            .values()
            .filter(|record| record.node.parent_id.as_ref() == Some(parent_id))
            .count()
    }

    pub(crate) fn depth(&self, task_id: &TaskId) -> Option<u32> {
        self.tasks.get(task_id).map(|record| record.depth)
    }

    pub(crate) fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|record| {
                record.node.parent_id.is_some()
                    && (matches!(record.node.status, TaskStatus::Preparing)
                        || record.node.status.is_running())
            })
            .count()
    }

    pub(crate) fn running_count_for_root(&self, root_id: &TaskId) -> usize {
        self.tasks
            .values()
            .filter(|record| {
                &record.node.root_id == root_id
                    && record.node.parent_id.is_some()
                    && (matches!(record.node.status, TaskStatus::Preparing)
                        || record.node.status.is_running())
            })
            .count()
    }

    pub(crate) fn inspection(&self, task_id: &TaskId) -> Option<TaskSnapshot> {
        self.tasks.get(task_id).map(|record| TaskSnapshot {
            node: record.node.clone(),
            budget_limits: record.budget.limits().clone(),
            budget_spent: record.budget.spent(),
            budget_reserved: record.budget.reserved(),
            workspace_lease: record.workspace_lease.clone(),
            has_parent_reservation: record.reservation.is_some(),
            event_sequence: record.last_event_sequence,
            cleanup_error: record.cleanup_error.clone(),
        })
    }

    pub(crate) fn is_self_or_descendant(
        &self,
        requester_root: &TaskId,
        requester_task: &TaskId,
        target: &TaskId,
    ) -> bool {
        let Some(requester) = self.tasks.get(requester_task) else {
            return false;
        };
        let Some(target_record) = self.tasks.get(target) else {
            return false;
        };
        if &requester.node.root_id != requester_root
            || &target_record.node.root_id != requester_root
        {
            return false;
        }

        let mut cursor = Some(target);
        for _ in 0..=self.tasks.len() {
            let Some(task_id) = cursor else {
                return false;
            };
            if task_id == requester_task {
                return true;
            }
            cursor = self
                .tasks
                .get(task_id)
                .and_then(|record| record.node.parent_id.as_ref());
        }
        false
    }

    pub(crate) fn counts(
        &self,
        dropped_sink_events: u64,
        dropped_callback_work: u64,
    ) -> RegistryCounts {
        let mut counts = RegistryCounts {
            roots: self.roots.len(),
            total: self.tasks.len(),
            dropped_sink_events,
            dropped_callback_work,
            ..RegistryCounts::default()
        };
        for record in self.tasks.values() {
            match record.node.status {
                TaskStatus::Queued => counts.queued += 1,
                TaskStatus::Preparing => counts.preparing += 1,
                status if status.is_running() => counts.running += 1,
                status if status.is_terminal() => counts.completed += 1,
                _ => {}
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{
        AgentProfile, BudgetLimits, ResultContract, SessionId, TaskOwner, TaskScope, TurnId,
    };

    fn record(id: &str, parent_id: Option<&str>, root_id: &str) -> RuntimeTaskRecord {
        let profile = AgentProfile::worker();
        RuntimeTaskRecord {
            node: TaskNode {
                id: TaskId::from(id),
                parent_id: parent_id.map(TaskId::from),
                root_id: TaskId::from(root_id),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from("session"),
                    turn_id: TurnId::from("turn"),
                },
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
            depth: if parent_id.is_some() { 1 } else { 0 },
            cleanup_error: None,
            last_event_sequence: 0,
        }
    }

    fn seeded_tree() -> CoordinatorState {
        let mut state = CoordinatorState::default();
        for entry in [
            record("root", None, "root"),
            record("left", Some("root"), "root"),
            record("left-child", Some("left"), "root"),
            record("right", Some("root"), "root"),
            record("right-child", Some("right"), "root"),
            record("other-root", None, "other-root"),
        ] {
            state.tasks.insert(entry.node.id.clone(), entry);
        }
        state
    }

    #[test]
    fn ancestry_scope_allows_only_self_and_descendants() {
        let state = seeded_tree();
        let root = TaskId::from("root");
        let left = TaskId::from("left");
        assert!(state.is_self_or_descendant(&root, &left, &left));
        assert!(state.is_self_or_descendant(&root, &left, &TaskId::from("left-child")));
        assert!(!state.is_self_or_descendant(&root, &left, &root));
        assert!(!state.is_self_or_descendant(&root, &left, &TaskId::from("right")));
        assert!(!state.is_self_or_descendant(&root, &left, &TaskId::from("right-child")));
        assert!(!state.is_self_or_descendant(&root, &left, &TaskId::from("other-root")));
    }

    #[test]
    fn root_scope_sees_every_descendant_in_its_tree() {
        let state = seeded_tree();
        let root = TaskId::from("root");
        assert!(state.is_self_or_descendant(&root, &root, &TaskId::from("left-child")));
        assert!(state.is_self_or_descendant(&root, &root, &TaskId::from("right")));
    }
}
