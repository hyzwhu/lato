use crate::{AgentError, CompactionId, ErrorCategory, ModelMessage, Retryability};

pub const DEFAULT_COMPACTION_MAX_ATTEMPTS: u8 = 3;
pub const MIN_COMPACTION_SOURCE_CHARS: usize = 2_000;
pub const MIN_COMPACTION_SUMMARY_CHARS: usize = 500;
pub const MAX_COMPACTION_SUMMARY_BYTES: usize = 32 * 1024;
pub const MIN_COMPACTION_REDUCTION_PERCENT: u8 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Threshold,
    PreflightOverflow,
    ModelSwitch,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactSession {
    pub user_context: Option<String>,
    pub trigger: CompactionTrigger,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactionPolicy {
    pub threshold_percent: u8,
    pub max_attempts: u8,
    pub summary_reserve_tokens: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            threshold_percent: 80,
            max_attempts: DEFAULT_COMPACTION_MAX_ATTEMPTS,
            summary_reserve_tokens: 8_192,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ContextUsage {
    pub estimated_input_tokens: u64,
    pub context_window: u64,
    pub utilization_percent: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactionSize {
    pub message_count: u64,
    pub serialized_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionCandidate {
    pub compaction_id: CompactionId,
    pub messages: Vec<ModelMessage>,
    pub before: CompactionSize,
    pub after: CompactionSize,
    pub summary_chars: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CompactionError {
    #[error("nothing to compact")]
    NothingToCompact,
    #[error("a foreground turn is active")]
    ActiveTurn,
    #[error("compaction is already active")]
    AlreadyActive,
    #[error("compaction input exceeds the safe request budget")]
    InputTooLarge,
    #[error("compaction summary is degenerate")]
    DegenerateSummary,
    #[error("invalid compaction summary: {message}")]
    InvalidSummary { message: String },
    #[error("compaction model failed: {source}")]
    ModelFailed { source: AgentError },
    #[error("compaction cancelled")]
    Cancelled,
    #[error("compaction persistence failed: {message}")]
    PersistenceFailed { message: String },
    #[error("compaction reconciliation failed: {message}")]
    ReconciliationFailed { message: String },
}

impl CompactionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NothingToCompact => "compaction.nothing_to_compact",
            Self::ActiveTurn => "compaction.active_turn",
            Self::AlreadyActive => "compaction.already_active",
            Self::InputTooLarge => "compaction.input_too_large",
            Self::DegenerateSummary => "compaction.degenerate_summary",
            Self::InvalidSummary { .. } => "compaction.invalid_summary",
            Self::ModelFailed { .. } => "compaction.model_failed",
            Self::Cancelled => "compaction.cancelled",
            Self::PersistenceFailed { .. } => "compaction.persistence_failed",
            Self::ReconciliationFailed { .. } => "compaction.reconciliation_failed",
        }
    }

    pub fn retryability(&self) -> Retryability {
        match self {
            Self::ModelFailed { source } => source.retryability.clone(),
            Self::PersistenceFailed { .. } => Retryability::AfterBackoff,
            _ => Retryability::Never,
        }
    }

    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::ModelFailed { .. } => ErrorCategory::Model,
            Self::PersistenceFailed { .. } | Self::ReconciliationFailed { .. } => {
                ErrorCategory::Storage
            }
            Self::AlreadyActive | Self::ActiveTurn | Self::NothingToCompact => {
                ErrorCategory::InvalidInput
            }
            _ => ErrorCategory::Task,
        }
    }
}

impl From<CompactionError> for AgentError {
    fn from(error: CompactionError) -> Self {
        AgentError::new(
            error.code(),
            error.category(),
            error.to_string(),
            error.retryability(),
        )
    }
}
