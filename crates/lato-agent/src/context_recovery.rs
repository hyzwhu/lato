// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs
// License: Apache-2.0
// Lato changes: provider-neutral automatic-compaction suppression and single-use overflow recovery

use lato_core::{CompactionTrigger, ModelError, ModelErrorKind};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCompactionSuppression {
    #[default]
    None,
    Turn,
    Sticky,
    UntilSuccess,
    Auth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuppressionReason {
    Size,
    Schema,
    Credit,
    Auth,
    Other,
}

#[derive(Debug, Default)]
pub struct SamplingRecoveryBudget {
    overflow_used: bool,
}

impl SamplingRecoveryBudget {
    pub fn try_use_overflow_recovery(&mut self) -> bool {
        if self.overflow_used {
            false
        } else {
            self.overflow_used = true;
            true
        }
    }
}

#[derive(Debug, Default)]
pub struct AutomaticRecoveryState {
    suppression: AutoCompactionSuppression,
}

impl AutomaticRecoveryState {
    pub fn suppression(&self) -> AutoCompactionSuppression {
        self.suppression
    }

    pub fn allows(&self, trigger: CompactionTrigger) -> bool {
        trigger == CompactionTrigger::Manual || self.suppression == AutoCompactionSuppression::None
    }

    /// Returns true only for the first transition out of the unsuppressed state.
    pub fn suppress(&mut self, reason: SuppressionReason) -> bool {
        if self.suppression != AutoCompactionSuppression::None {
            return false;
        }
        self.suppression = match reason {
            SuppressionReason::Size | SuppressionReason::Schema => {
                AutoCompactionSuppression::Sticky
            }
            SuppressionReason::Credit => AutoCompactionSuppression::UntilSuccess,
            SuppressionReason::Auth => AutoCompactionSuppression::Auth,
            SuppressionReason::Other => AutoCompactionSuppression::Turn,
        };
        true
    }

    pub fn on_new_turn(&mut self) {
        self.clear_if(AutoCompactionSuppression::Turn);
    }

    pub fn on_context_budget_changed(&mut self) {
        if matches!(
            self.suppression,
            AutoCompactionSuppression::Turn | AutoCompactionSuppression::Sticky
        ) {
            self.suppression = AutoCompactionSuppression::None;
        }
    }

    pub fn on_compaction_success(&mut self) {
        if matches!(
            self.suppression,
            AutoCompactionSuppression::Turn | AutoCompactionSuppression::Sticky
        ) {
            self.suppression = AutoCompactionSuppression::None;
        }
    }

    pub fn on_provider_success(&mut self) {
        self.clear_if(AutoCompactionSuppression::UntilSuccess);
    }

    pub fn on_auth_refreshed(&mut self) {
        self.clear_if(AutoCompactionSuppression::Auth);
    }

    fn clear_if(&mut self, expected: AutoCompactionSuppression) {
        if self.suppression == expected {
            self.suppression = AutoCompactionSuppression::None;
        }
    }
}

pub fn suppression_reason(error: &ModelError) -> SuppressionReason {
    match error.kind {
        ModelErrorKind::ContextOverflow => SuppressionReason::Size,
        ModelErrorKind::Authentication => SuppressionReason::Auth,
        ModelErrorKind::Credit => SuppressionReason::Credit,
        ModelErrorKind::InvalidRequest => SuppressionReason::Schema,
        ModelErrorKind::RateLimited | ModelErrorKind::Transport | ModelErrorKind::Cancelled => {
            SuppressionReason::Other
        }
        ModelErrorKind::Other => legacy_suppression_reason(&error.message),
    }
}

fn legacy_suppression_reason(message: &str) -> SuppressionReason {
    let message = message.to_ascii_lowercase();
    if ["unauthorized", "invalid api key", "expired token"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        SuppressionReason::Auth
    } else if [
        "out of credits",
        "spending limit",
        "usage balance exhausted",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        SuppressionReason::Credit
    } else if [
        "context length exceeded",
        "context window exceeded",
        "prompt too long",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        SuppressionReason::Size
    } else if ["invalid schema", "tool schema", "invalid request"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        SuppressionReason::Schema
    } else {
        SuppressionReason::Other
    }
}
