// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: real task-tree records own hierarchical budgets and workspace leases

use crate::task::{RegistryCounts, TaskSnapshot};
use lato_core::{BudgetAccount, BudgetReservation, TaskId, TaskNode, TaskStatus};
use lato_workspace::WorkspaceLease;
use std::collections::{HashMap, HashSet};

pub(crate) struct RuntimeTaskRecord {
    pub(crate) node: TaskNode,
    pub(crate) budget: BudgetAccount,
    pub(crate) workspace_lease: Option<WorkspaceLease>,
    pub(crate) reservation: Option<BudgetReservation>,
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

    pub(crate) fn inspection(&self, task_id: &TaskId) -> Option<TaskSnapshot> {
        self.tasks.get(task_id).map(|record| TaskSnapshot {
            node: record.node.clone(),
            budget_limits: record.budget.limits().clone(),
            budget_spent: record.budget.spent(),
            budget_reserved: record.budget.reserved(),
            workspace_lease: record.workspace_lease.clone(),
            has_parent_reservation: record.reservation.is_some(),
            event_sequence: record.last_event_sequence,
        })
    }

    pub(crate) fn counts(&self, dropped_sink_events: u64) -> RegistryCounts {
        let mut counts = RegistryCounts {
            roots: self.roots.len(),
            total: self.tasks.len(),
            dropped_sink_events,
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
