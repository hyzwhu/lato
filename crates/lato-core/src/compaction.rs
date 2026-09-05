// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-chat-state/src/actor/state.rs
// License: Apache-2.0
// Lato changes: provider-neutral context ledger with saturating estimates and checkpoint reseeding

use crate::{AgentError, CompactionId, ErrorCategory, ModelMessage, ModelUsage, Retryability};

pub const DEFAULT_COMPACTION_MAX_ATTEMPTS: u8 = 3;
pub const DEFAULT_COMPACTION_THRESHOLD_PERCENT: u8 = 85;
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
    ProviderOverflow,
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
            threshold_percent: DEFAULT_COMPACTION_THRESHOLD_PERCENT,
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

impl ContextUsage {
    pub fn threshold_reached(&self, threshold_percent: u8) -> bool {
        self.context_window > 0
            && self.estimated_input_tokens.saturating_mul(100)
                >= self
                    .context_window
                    .saturating_mul(u64::from(threshold_percent))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextLedger {
    confirmed_total_tokens: Option<u64>,
    estimate_at_confirmation: u64,
}

impl ContextLedger {
    pub fn observe(&mut self, usage: &ModelUsage, history_estimate: u64) -> bool {
        if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
            return false;
        }

        self.confirmed_total_tokens = Some(
            usage
                .input_tokens
                .unwrap_or_default()
                .saturating_add(usage.output_tokens.unwrap_or_default()),
        );
        self.estimate_at_confirmation = history_estimate;
        true
    }

    pub fn measure(&self, history_estimate: u64, context_window: Option<u64>) -> ContextUsage {
        let estimated_input_tokens =
            self.confirmed_total_tokens
                .map_or(history_estimate, |total| {
                    total.saturating_add(
                        history_estimate.saturating_sub(self.estimate_at_confirmation),
                    )
                });
        let context_window = context_window.unwrap_or_default();
        let utilization_percent = if context_window == 0 {
            0
        } else {
            ((u128::from(estimated_input_tokens) * 100) / u128::from(context_window))
                .min(u128::from(u8::MAX)) as u8
        };

        ContextUsage {
            estimated_input_tokens,
            context_window,
            utilization_percent,
        }
    }

    pub fn reseed(&mut self, replacement_estimate: u64) {
        let confirmed_total_tokens =
            match (self.confirmed_total_tokens, self.estimate_at_confirmation) {
                (Some(previous), old_estimate) if old_estimate > 0 => {
                    let numerator = u128::from(replacement_estimate) * u128::from(previous);
                    let scaled =
                        (numerator + u128::from(old_estimate / 2)) / u128::from(old_estimate);
                    u64::try_from(scaled).unwrap_or(u64::MAX).min(previous)
                }
                _ => replacement_estimate,
            };

        self.confirmed_total_tokens = Some(confirmed_total_tokens);
        self.estimate_at_confirmation = replacement_estimate;
    }
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
