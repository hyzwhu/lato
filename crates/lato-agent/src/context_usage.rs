// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-chat-state/src/actor/queries.rs
// License: Apache-2.0
// Lato changes: provider-neutral history estimation and model-switch compaction decisions

use crate::{AutoCompactionSuppression, AutomaticRecoveryState, HistoryItem, SuppressionReason};
use lato_ai::{ActiveModelPort, ModelCallReport, ModelMetadata};
use lato_core::{ContextLedger, ContextUsage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchCompaction {
    None,
    Immediate,
    BeforeNextSample,
}

#[derive(Debug, Default)]
pub struct ContextTracker {
    ledger: ContextLedger,
    pending_model_switch: bool,
    recovery: AutomaticRecoveryState,
}

impl ContextTracker {
    pub fn measure(&self, history: &[HistoryItem], active: &ActiveModelPort) -> ContextUsage {
        self.ledger.measure(
            estimate_history_tokens(history),
            active
                .metadata
                .context_window
                .or(active.capabilities.context_window),
        )
    }

    pub fn observe(
        &mut self,
        history: &[HistoryItem],
        report: &ModelCallReport,
        active_generation: u64,
    ) -> bool {
        if report.generation != active_generation {
            return false;
        }
        report
            .usage
            .as_ref()
            .is_some_and(|usage| self.ledger.observe(usage, estimate_history_tokens(history)))
    }

    pub fn reseed(&mut self, history: &[HistoryItem]) {
        self.ledger.reseed(estimate_history_tokens(history));
    }

    pub fn mark_model_switch_check(&mut self) {
        self.pending_model_switch = true;
    }

    pub fn take_model_switch_check(&mut self) -> bool {
        std::mem::take(&mut self.pending_model_switch)
    }

    pub fn automatic_compaction_allowed(&self, trigger: lato_core::CompactionTrigger) -> bool {
        self.recovery.allows(trigger)
    }

    pub fn automatic_compaction_suppression(&self) -> AutoCompactionSuppression {
        self.recovery.suppression()
    }

    pub fn suppress_automatic_compaction(&mut self, reason: SuppressionReason) -> bool {
        self.recovery.suppress(reason)
    }

    pub fn on_new_turn(&mut self) {
        self.recovery.on_new_turn();
    }

    pub fn on_context_budget_changed(&mut self) {
        self.recovery.on_context_budget_changed();
    }

    pub fn on_compaction_success(&mut self) {
        self.recovery.on_compaction_success();
    }

    pub fn on_provider_success(&mut self) {
        self.recovery.on_provider_success();
    }

    pub fn on_auth_refreshed(&mut self) {
        self.recovery.on_auth_refreshed();
    }
}

pub fn estimate_history_tokens(history: &[HistoryItem]) -> u64 {
    let bytes = history.iter().fold(0_u64, |total, item| {
        let item_bytes = match item {
            HistoryItem::System(text)
            | HistoryItem::User(text)
            | HistoryItem::AssistantText(text)
            | HistoryItem::CompactionSummary(text) => usize_as_u64(text.len()),
            HistoryItem::ToolCall {
                id,
                name,
                arguments,
            } => usize_as_u64(id.len())
                .saturating_add(usize_as_u64(name.len()))
                .saturating_add(
                    serde_json::to_vec(arguments)
                        .map(|bytes| usize_as_u64(bytes.len()))
                        .unwrap_or(u64::MAX),
                ),
            HistoryItem::ToolResult { id, output } => {
                usize_as_u64(id.len()).saturating_add(usize_as_u64(output.len()))
            }
        };
        total.saturating_add(item_bytes)
    });
    bytes / 4
}

pub fn has_model_authored_history(history: &[HistoryItem]) -> bool {
    history.iter().any(|item| {
        matches!(
            item,
            HistoryItem::AssistantText(_)
                | HistoryItem::ToolCall { .. }
                | HistoryItem::CompactionSummary(_)
        )
    })
}

pub fn decide_switch_compaction(
    previous: &ModelMetadata,
    candidate: &ModelMetadata,
    estimated_tokens: u64,
    has_model_authored_history: bool,
    threshold_percent: u8,
) -> SwitchCompaction {
    if has_model_authored_history
        && matches!(
            (&previous.model_family, &candidate.model_family),
            (Some(old), Some(new)) if old != new
        )
    {
        return SwitchCompaction::Immediate;
    }
    let (Some(old_window), Some(new_window)) = (previous.context_window, candidate.context_window)
    else {
        return SwitchCompaction::None;
    };
    if old_window <= new_window {
        return SwitchCompaction::None;
    }
    let usage = ContextUsage {
        estimated_input_tokens: estimated_tokens,
        context_window: new_window,
        utilization_percent: estimated_tokens
            .saturating_mul(100)
            .checked_div(new_window)
            .unwrap_or_default()
            .min(u64::from(u8::MAX)) as u8,
    };
    if usage.threshold_reached(threshold_percent) {
        SwitchCompaction::BeforeNextSample
    } else {
        SwitchCompaction::None
    }
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
