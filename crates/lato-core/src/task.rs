use crate::{AgentError, ErrorCategory, Retryability, SessionId, TaskId, ToolCapability, TurnId};
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskOwner {
    Interactive {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Workflow {
        run_id: String,
        session_id: SessionId,
    },
}

impl TaskOwner {
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Interactive { session_id, .. } | Self::Workflow { session_id, .. } => session_id,
        }
    }

    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::Interactive { turn_id, .. } => Some(turn_id),
            Self::Workflow { .. } => None,
        }
    }

    pub fn workflow_run_id(&self) -> Option<&str> {
        match self {
            Self::Interactive { .. } => None,
            Self::Workflow { run_id, .. } => Some(run_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Preparing,
    Running,
    WaitingForChildren,
    WaitingForApproval,
    Finalizing,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    pub fn is_queued(self) -> bool {
        self == Self::Queued
    }

    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Running | Self::WaitingForChildren | Self::WaitingForApproval | Self::Verifying
        )
    }

    pub fn is_cancelled(self) -> bool {
        self == Self::Cancelled
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIntent {
    SharedReadOnly,
    SharedSerializedWrite,
    IsolatedWorktree,
    ExternalLease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    Accept,
    Schema,
    Programmatic,
    IndependentReview,
    HumanGate,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AgentProfile {
    pub name: String,
    pub instructions: String,
    pub capabilities: Vec<ToolCapability>,
    pub workspace: WorkspaceIntent,
    pub verification: VerificationPolicy,
    pub definition_background: bool,
}

impl AgentProfile {
    pub fn explorer() -> Self {
        Self {
            name: "explorer".into(),
            instructions: "Inspect the available evidence without modifying the workspace.".into(),
            capabilities: vec![ToolCapability::FileRead, ToolCapability::NetworkRead],
            workspace: WorkspaceIntent::SharedReadOnly,
            verification: VerificationPolicy::Schema,
            definition_background: false,
        }
    }

    pub fn worker() -> Self {
        Self {
            name: "worker".into(),
            instructions: "Implement the delegated objective within the granted scope.".into(),
            capabilities: vec![
                ToolCapability::FileRead,
                ToolCapability::FileWrite,
                ToolCapability::ProcessSpawn,
                ToolCapability::NetworkRead,
                ToolCapability::NetworkWrite,
                ToolCapability::TaskControl,
                ToolCapability::ExtensionInvoke,
            ],
            workspace: WorkspaceIntent::IsolatedWorktree,
            verification: VerificationPolicy::Programmatic,
            definition_background: false,
        }
    }

    pub fn reviewer() -> Self {
        Self {
            name: "reviewer".into(),
            instructions: "Independently review the delegated result and report evidence.".into(),
            capabilities: vec![ToolCapability::FileRead, ToolCapability::NetworkRead],
            workspace: WorkspaceIntent::SharedReadOnly,
            verification: VerificationPolicy::IndependentReview,
            definition_background: false,
        }
    }

    pub fn effective_capabilities(
        &self,
        parent: &[ToolCapability],
        requested: Option<&[ToolCapability]>,
    ) -> Result<Vec<ToolCapability>, TaskError> {
        let parent_set: HashSet<_> = parent.iter().cloned().collect();
        let profile_set: HashSet<_> = self.capabilities.iter().cloned().collect();

        if let Some(requested) = requested
            && requested
                .iter()
                .any(|value| !parent_set.contains(value) || !profile_set.contains(value))
        {
            return Err(TaskError::new(
                TaskErrorCode::CapabilityExpansion,
                "requested task capabilities exceed the inherited capability ceiling",
            ));
        }

        let requested_set = requested.map(|values| values.iter().cloned().collect::<HashSet<_>>());
        let mut seen = HashSet::new();
        Ok(parent
            .iter()
            .filter(|value| profile_set.contains(*value))
            .filter(|value| {
                requested_set
                    .as_ref()
                    .is_none_or(|requested| requested.contains(*value))
            })
            .filter(|value| seen.insert((*value).clone()))
            .cloned()
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskScope {
    pub objective: String,
    pub context_refs: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResultContract {
    pub schema: Option<serde_json::Value>,
    pub max_output_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskSpec {
    pub profile: AgentProfile,
    pub scope: TaskScope,
    pub requested_capabilities: Option<Vec<ToolCapability>>,
    pub result_contract: ResultContract,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub root_id: TaskId,
    pub owner: TaskOwner,
    pub profile: AgentProfile,
    pub scope: TaskScope,
    pub status: TaskStatus,
    pub permissions: Vec<ToolCapability>,
    pub workspace_intent: WorkspaceIntent,
    pub result_contract: ResultContract,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub tool_calls: u64,
    pub cost_micros: u64,
    pub retries: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskProgress {
    pub phase: Option<String>,
    pub message: Option<String>,
    pub completed_units: u64,
    pub total_units: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskResult {
    pub success: bool,
    pub output: String,
    pub error: Option<TaskError>,
    pub usage: TaskUsage,
    pub duration_ms: u64,
    pub output_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum TaskErrorCode {
    #[serde(rename = "task.invalid_identity")]
    InvalidIdentity,
    #[serde(rename = "task.not_found_or_not_owned")]
    NotFoundOrNotOwned,
    #[serde(rename = "task.duplicate")]
    DuplicateTask,
    #[serde(rename = "task.invalid_parent")]
    InvalidParent,
    #[serde(rename = "task.terminal_parent")]
    TerminalParent,
    #[serde(rename = "task.limit.depth")]
    DepthLimit,
    #[serde(rename = "task.limit.children")]
    ChildLimit,
    #[serde(rename = "task.limit.queue")]
    QueueFull,
    #[serde(rename = "task.limit.concurrency")]
    ConcurrencyLimit,
    #[serde(rename = "task.limit.message")]
    MessageLimit,
    #[serde(rename = "task.limit.retention")]
    RetentionLimit,
    #[serde(rename = "task.budget_reservation")]
    BudgetReservation,
    #[serde(rename = "task.budget_exceeded")]
    BudgetExceeded,
    #[serde(rename = "task.budget_exceeded.input_tokens")]
    BudgetExceededInputTokens,
    #[serde(rename = "task.budget_exceeded.output_tokens")]
    BudgetExceededOutputTokens,
    #[serde(rename = "task.budget_exceeded.total_tokens")]
    BudgetExceededTotalTokens,
    #[serde(rename = "task.budget_exceeded.tool_calls")]
    BudgetExceededToolCalls,
    #[serde(rename = "task.budget_exceeded.cost_micros")]
    BudgetExceededCostMicros,
    #[serde(rename = "task.budget_exceeded.retries")]
    BudgetExceededRetries,
    #[serde(rename = "task.capability_expansion")]
    CapabilityExpansion,
    #[serde(rename = "task.invalid_profile")]
    InvalidProfile,
    #[serde(rename = "task.spawn_admission_closed")]
    SpawnAdmissionClosed,
    #[serde(rename = "task.workspace_allocation")]
    WorkspaceAllocation,
    #[serde(rename = "task.workspace_release")]
    WorkspaceRelease,
    #[serde(rename = "task.runner_initialization")]
    RunnerInitialization,
    #[serde(rename = "task.runner_panic")]
    RunnerPanic,
    #[serde(rename = "task.runner_protocol_violation")]
    RunnerProtocolViolation,
    #[serde(rename = "task.active_message_uncertain")]
    AdmissionUncertain,
    #[serde(rename = "task.active_message_inactive")]
    ActiveMessageInactive,
    #[serde(rename = "task.active_message_unsupported")]
    ActiveMessageUnsupported,
    #[serde(rename = "task.active_message_channel_closed")]
    ActiveMessageChannelClosed,
    #[serde(rename = "task.verification_failed")]
    VerificationFailed,
    #[serde(rename = "task.verification.schema_unsupported")]
    VerificationSchemaUnsupported,
    #[serde(rename = "task.verification_pending")]
    VerificationPending,
    #[serde(rename = "task.cancelled")]
    Cancelled,
    #[serde(rename = "task.timed_out")]
    TimedOut,
    #[serde(rename = "task.coordinator_closed")]
    CoordinatorClosed,
    #[serde(rename = "task.event_lagged")]
    EventLagged,
}

impl TaskErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIdentity => "task.invalid_identity",
            Self::NotFoundOrNotOwned => "task.not_found_or_not_owned",
            Self::DuplicateTask => "task.duplicate",
            Self::InvalidParent => "task.invalid_parent",
            Self::TerminalParent => "task.terminal_parent",
            Self::DepthLimit => "task.limit.depth",
            Self::ChildLimit => "task.limit.children",
            Self::QueueFull => "task.limit.queue",
            Self::ConcurrencyLimit => "task.limit.concurrency",
            Self::MessageLimit => "task.limit.message",
            Self::RetentionLimit => "task.limit.retention",
            Self::BudgetReservation => "task.budget_reservation",
            Self::BudgetExceeded => "task.budget_exceeded",
            Self::BudgetExceededInputTokens => "task.budget_exceeded.input_tokens",
            Self::BudgetExceededOutputTokens => "task.budget_exceeded.output_tokens",
            Self::BudgetExceededTotalTokens => "task.budget_exceeded.total_tokens",
            Self::BudgetExceededToolCalls => "task.budget_exceeded.tool_calls",
            Self::BudgetExceededCostMicros => "task.budget_exceeded.cost_micros",
            Self::BudgetExceededRetries => "task.budget_exceeded.retries",
            Self::CapabilityExpansion => "task.capability_expansion",
            Self::InvalidProfile => "task.invalid_profile",
            Self::SpawnAdmissionClosed => "task.spawn_admission_closed",
            Self::WorkspaceAllocation => "task.workspace_allocation",
            Self::WorkspaceRelease => "task.workspace_release",
            Self::RunnerInitialization => "task.runner_initialization",
            Self::RunnerPanic => "task.runner_panic",
            Self::RunnerProtocolViolation => "task.runner_protocol_violation",
            Self::AdmissionUncertain => "task.active_message_uncertain",
            Self::ActiveMessageInactive => "task.active_message_inactive",
            Self::ActiveMessageUnsupported => "task.active_message_unsupported",
            Self::ActiveMessageChannelClosed => "task.active_message_channel_closed",
            Self::VerificationFailed => "task.verification_failed",
            Self::VerificationSchemaUnsupported => "task.verification.schema_unsupported",
            Self::VerificationPending => "task.verification_pending",
            Self::Cancelled => "task.cancelled",
            Self::TimedOut => "task.timed_out",
            Self::CoordinatorClosed => "task.coordinator_closed",
            Self::EventLagged => "task.event_lagged",
        }
    }

    pub const fn retryability(self) -> Retryability {
        match self {
            Self::QueueFull | Self::ConcurrencyLimit | Self::EventLagged => {
                Retryability::AfterBackoff
            }
            Self::WorkspaceAllocation
            | Self::WorkspaceRelease
            | Self::RunnerInitialization
            | Self::ActiveMessageChannelClosed => Retryability::Safe,
            Self::VerificationPending | Self::AdmissionUncertain => Retryability::RequiresDecision,
            Self::InvalidIdentity
            | Self::NotFoundOrNotOwned
            | Self::DuplicateTask
            | Self::InvalidParent
            | Self::TerminalParent
            | Self::DepthLimit
            | Self::ChildLimit
            | Self::MessageLimit
            | Self::RetentionLimit
            | Self::BudgetReservation
            | Self::BudgetExceeded
            | Self::BudgetExceededInputTokens
            | Self::BudgetExceededOutputTokens
            | Self::BudgetExceededTotalTokens
            | Self::BudgetExceededToolCalls
            | Self::BudgetExceededCostMicros
            | Self::BudgetExceededRetries
            | Self::CapabilityExpansion
            | Self::InvalidProfile
            | Self::SpawnAdmissionClosed
            | Self::RunnerPanic
            | Self::RunnerProtocolViolation
            | Self::ActiveMessageInactive
            | Self::ActiveMessageUnsupported
            | Self::VerificationFailed
            | Self::VerificationSchemaUnsupported
            | Self::Cancelled
            | Self::TimedOut
            | Self::CoordinatorClosed => Retryability::Never,
        }
    }
}

impl std::fmt::Display for TaskErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for TaskErrorCode {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct TaskError {
    pub code: TaskErrorCode,
    pub message: String,
}

impl TaskError {
    pub fn new(code: TaskErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn agent_error(&self) -> AgentError {
        AgentError::new(
            self.code.as_str(),
            ErrorCategory::Task,
            self.message.clone(),
            self.code.retryability(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskMachine {
    status: TaskStatus,
}

impl TaskMachine {
    pub fn new(status: TaskStatus) -> Self {
        Self { status }
    }

    pub fn status(&self) -> TaskStatus {
        self.status
    }

    pub fn transition(&mut self, next: TaskStatus) -> Result<(), TaskTransitionError> {
        if allowed_transition(self.status, next) {
            self.status = next;
            Ok(())
        } else {
            Err(TaskTransitionError {
                from: self.status,
                to: next,
            })
        }
    }
}

fn allowed_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    match from {
        Queued => matches!(to, Preparing | Failed | Cancelled | TimedOut),
        Preparing => matches!(to, Running | Failed | Cancelled | TimedOut),
        Running => matches!(
            to,
            WaitingForChildren
                | WaitingForApproval
                | Finalizing
                | Verifying
                | Failed
                | Cancelled
                | TimedOut
        ),
        WaitingForChildren | WaitingForApproval => {
            matches!(
                to,
                Running | Finalizing | Verifying | Failed | Cancelled | TimedOut
            )
        }
        Finalizing => matches!(to, Verifying | Failed | Cancelled | TimedOut),
        Verifying => matches!(
            to,
            WaitingForChildren | WaitingForApproval | Completed | Failed | Cancelled | TimedOut
        ),
        Completed | Failed | Cancelled | TimedOut => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid task transition from {from:?} to {to:?}")]
pub struct TaskTransitionError {
    pub from: TaskStatus,
    pub to: TaskStatus,
}
