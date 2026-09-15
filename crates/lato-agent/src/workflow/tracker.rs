// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/tracker.rs
// License: Apache-2.0
// Lato changes: drop token leases and persistence fields; journals stay in memory (Phase 7B4).

use lato_workflow::{PauseKind, ScriptOutcome};
use serde::{Deserialize, Serialize};

pub const WORKFLOW_HISTORY_MAX: usize = 64;
pub const WORKFLOW_MAX_ACTIVE_RUNS_PER_SESSION: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Active,
    UserPaused,
    BackOffPaused,
    NoProgressPaused,
    InfraPaused,
    Blocked,
    BudgetLimited,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
}

impl WorkflowRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// Same-process resumable per spec §5: paused family, blocked, failed,
    /// cancelled. Budget-limited is resumable only with a raised budget
    /// (checked by the manager). Not resumable: complete, interrupted, active.
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            Self::UserPaused
                | Self::BackOffPaused
                | Self::NoProgressPaused
                | Self::InfraPaused
                | Self::Blocked
                | Self::Failed
                | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunState {
    pub run_id: String,
    pub display_name: String,
    pub status: WorkflowRunStatus,
    #[serde(rename = "phase")]
    pub current_phase: Option<String>,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub pause_message: Option<String>,
    pub elapsed_ms_floor: u64,
}

pub struct WorkflowTracker {
    runs: Vec<WorkflowRunState>,
}

impl Default for WorkflowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowTracker {
    pub fn new() -> Self {
        Self { runs: Vec::new() }
    }

    pub fn start_run(
        &mut self,
        run_id: String,
        meta_name: String,
        agent_budget: Option<u64>,
    ) -> WorkflowRunState {
        let display_name = self.allocate_display_name(&meta_name);
        let state = WorkflowRunState {
            run_id,
            display_name,
            status: WorkflowRunStatus::Active,
            current_phase: None,
            agent_budget,
            agents_used: 0,
            pause_message: None,
            elapsed_ms_floor: 0,
        };
        self.runs.push(state.clone());
        state
    }

    /// Grok allocation: `meta.name` if free, else `name-2`, `name-3`, …
    fn allocate_display_name(&self, meta_name: &str) -> String {
        if self.by_display_name(meta_name).is_none() {
            return meta_name.to_owned();
        }
        for seq in 2..u64::MAX {
            let candidate = format!("{meta_name}-{seq}");
            if self.by_display_name(&candidate).is_none() {
                return candidate;
            }
        }
        unreachable!("display name space exhausted");
    }

    pub fn get(&self, run_id: &str) -> Option<WorkflowRunState> {
        self.runs
            .iter()
            .find(|run| run.run_id == run_id)
            .cloned()
    }

    pub fn by_display_name(&self, name: &str) -> Option<WorkflowRunState> {
        self.runs
            .iter()
            .find(|run| run.display_name == name)
            .cloned()
    }

    pub fn list(&self) -> Vec<WorkflowRunState> {
        self.runs.clone()
    }

    pub fn active_count(&self) -> usize {
        self.runs
            .iter()
            .filter(|run| run.status == WorkflowRunStatus::Active)
            .count()
    }

    pub fn apply_outcome(&mut self, run_id: &str, outcome: &ScriptOutcome) {
        let (status, pause_message) = match outcome {
            ScriptOutcome::Completed { .. } => (WorkflowRunStatus::Complete, None),
            ScriptOutcome::Paused { kind, message } => (pause_status(*kind), Some(message.clone())),
            ScriptOutcome::BudgetExceeded { message } => {
                (WorkflowRunStatus::BudgetLimited, Some(message.clone()))
            }
            ScriptOutcome::Cancelled => (WorkflowRunStatus::Cancelled, None),
            ScriptOutcome::Failed { error } => (WorkflowRunStatus::Failed, Some(error.clone())),
        };
        self.set_status(run_id, status, pause_message);
    }

    pub fn set_status(
        &mut self,
        run_id: &str,
        status: WorkflowRunStatus,
        pause_message: Option<String>,
    ) -> Option<WorkflowRunState> {
        let run = self.runs.iter_mut().find(|run| run.run_id == run_id)?;
        run.status = status;
        run.pause_message = if status == WorkflowRunStatus::Active {
            None
        } else {
            pause_message
        };
        if status == WorkflowRunStatus::Active {
            run.current_phase = None;
        }
        let state = run.clone();
        self.prune_history();
        Some(state)
    }

    pub fn set_phase(&mut self, run_id: &str, phase: String) {
        if let Some(run) = self.runs.iter_mut().find(|run| run.run_id == run_id) {
            run.current_phase = Some(phase);
        }
    }

    pub fn agent_spawned(&mut self, run_id: &str) {
        if let Some(run) = self.runs.iter_mut().find(|run| run.run_id == run_id) {
            run.agents_used = run.agents_used.saturating_add(1);
        }
    }

    /// Process exit or session close while still active → `interrupted`.
    pub fn interrupt(&mut self, run_id: &str, message: &str) -> Option<WorkflowRunState> {
        self.set_status(
            run_id,
            WorkflowRunStatus::Interrupted,
            Some(message.to_owned()),
        )
    }

    pub fn resume_run(&mut self, run_id: &str, new_budget: Option<u64>) -> Option<WorkflowRunState> {
        let run = self.get(run_id)?;
        if run.status == WorkflowRunStatus::BudgetLimited {
            // Budget-limited runs resume only with a strictly higher budget.
            let raised = new_budget.is_some_and(|budget| budget > run.agents_used);
            if !raised {
                return None;
            }
        } else if !run.status.is_resumable() {
            return None;
        }
        let budget = new_budget.or(run.agent_budget);
        let updated = self.set_status(run_id, WorkflowRunStatus::Active, None)?;
        let run = self.runs.iter_mut().find(|run| run.run_id == run_id)?;
        run.agent_budget = budget;
        Some(updated)
    }

    fn prune_history(&mut self) {
        while self.runs.len() > WORKFLOW_HISTORY_MAX {
            let Some(index) = self.runs.iter().position(|run| run.status.is_terminal()) else {
                break;
            };
            self.runs.remove(index);
        }
    }
}

fn pause_status(kind: PauseKind) -> WorkflowRunStatus {
    match kind {
        PauseKind::User => WorkflowRunStatus::UserPaused,
        PauseKind::BackOff => WorkflowRunStatus::BackOffPaused,
        PauseKind::NoProgress => WorkflowRunStatus::NoProgressPaused,
        PauseKind::Verification => WorkflowRunStatus::Blocked,
        PauseKind::Infra => WorkflowRunStatus::InfraPaused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn outcome_paused(kind: PauseKind) -> ScriptOutcome {
        ScriptOutcome::Paused {
            kind,
            message: "hold".into(),
        }
    }

    #[test]
    fn two_launches_get_numbered_display_names() {
        let mut tracker = WorkflowTracker::new();
        let first = tracker.start_run("wf_1".into(), "review".into(), Some(128));
        let second = tracker.start_run("wf_2".into(), "review".into(), Some(128));
        assert_eq!(first.display_name, "review");
        assert_eq!(second.display_name, "review-2");
        assert_eq!(tracker.active_count(), 2);
    }

    #[test]
    fn display_names_number_across_finished_runs_too() {
        let mut tracker = WorkflowTracker::new();
        tracker.start_run("wf_1".into(), "review".into(), None);
        tracker.apply_outcome(
            "wf_1",
            &ScriptOutcome::Completed {
                result: json!("ok"),
            },
        );
        let second = tracker.start_run("wf_2".into(), "review".into(), None);
        assert_eq!(second.display_name, "review-2");
    }

    #[test]
    fn pause_kinds_map_to_statuses() {
        let cases = [
            (PauseKind::User, WorkflowRunStatus::UserPaused),
            (PauseKind::BackOff, WorkflowRunStatus::BackOffPaused),
            (PauseKind::NoProgress, WorkflowRunStatus::NoProgressPaused),
            (PauseKind::Verification, WorkflowRunStatus::Blocked),
            (PauseKind::Infra, WorkflowRunStatus::InfraPaused),
        ];
        for (index, (kind, expected)) in cases.iter().enumerate() {
            let mut tracker = WorkflowTracker::new();
            let run_id = format!("wf_{index}");
            tracker.start_run(run_id.clone(), format!("w{index}"), None);
            tracker.apply_outcome(&run_id, &outcome_paused(*kind));
            assert_eq!(tracker.get(&run_id).unwrap().status, *expected);
            assert_eq!(
                tracker.get(&run_id).unwrap().pause_message.as_deref(),
                Some("hold")
            );
        }
    }

    #[test]
    fn budget_exceeded_maps_to_budget_limited() {
        let mut tracker = WorkflowTracker::new();
        tracker.start_run("wf_1".into(), "w".into(), Some(1));
        tracker.apply_outcome(
            "wf_1",
            &ScriptOutcome::BudgetExceeded {
                message: "over".into(),
            },
        );
        let state = tracker.get("wf_1").unwrap();
        assert_eq!(state.status, WorkflowRunStatus::BudgetLimited);
        // One agent actually ran before the budget terminal.
        tracker.agent_spawned("wf_1");
        // Bare resume is rejected; a raised budget passes.
        assert!(tracker.resume_run("wf_1", None).is_none());
        assert!(tracker.resume_run("wf_1", Some(1)).is_none());
        assert!(tracker.resume_run("wf_1", Some(2)).is_some());
        assert_eq!(tracker.get("wf_1").unwrap().status, WorkflowRunStatus::Active);
        assert_eq!(tracker.get("wf_1").unwrap().agent_budget, Some(2));
    }

    #[test]
    fn complete_and_interrupted_are_not_resumable() {
        let mut tracker = WorkflowTracker::new();
        tracker.start_run("wf_1".into(), "w".into(), None);
        tracker.apply_outcome(
            "wf_1",
            &ScriptOutcome::Completed {
                result: json!("ok"),
            },
        );
        assert!(tracker.resume_run("wf_1", None).is_none());
        tracker.interrupt("wf_1", "process exit");
        assert_eq!(
            tracker.get("wf_1").unwrap().status,
            WorkflowRunStatus::Interrupted
        );
        assert!(tracker.resume_run("wf_1", None).is_none());
    }

    #[test]
    fn paused_runs_resume_without_budget() {
        let mut tracker = WorkflowTracker::new();
        tracker.start_run("wf_1".into(), "w".into(), Some(4));
        tracker.apply_outcome("wf_1", &wf_outcome_paused_user());
        let resumed = tracker.resume_run("wf_1", None).unwrap();
        assert_eq!(resumed.status, WorkflowRunStatus::Active);
        assert_eq!(resumed.agent_budget, Some(4));
    }

    fn wf_outcome_paused_user() -> ScriptOutcome {
        ScriptOutcome::Paused {
            kind: PauseKind::User,
            message: "hold".into(),
        }
    }

    #[test]
    fn history_is_pruned_to_cap() {
        let mut tracker = WorkflowTracker::new();
        for seq in 0..(WORKFLOW_HISTORY_MAX + 8) {
            let run_id = format!("wf_{seq}");
            tracker.start_run(run_id.clone(), format!("w{seq}"), None);
            tracker.apply_outcome(
                &run_id,
                &ScriptOutcome::Completed {
                    result: json!("ok"),
                },
            );
        }
        assert_eq!(tracker.list().len(), WORKFLOW_HISTORY_MAX);
    }
}
