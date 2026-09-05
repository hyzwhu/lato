use lato_agent::{
    AutoCompactionSuppression, AutomaticRecoveryState, SamplingRecoveryBudget, SuppressionReason,
    suppression_reason,
};
use lato_core::{CompactionTrigger, ModelError, ModelErrorKind, Retryability};

#[test]
fn suppression_clear_conditions_match_grok_build() {
    let mut state = AutomaticRecoveryState::default();
    state.suppress(SuppressionReason::Size);
    state.on_new_turn();
    assert_eq!(state.suppression(), AutoCompactionSuppression::Sticky);
    state.on_context_budget_changed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);

    state.suppress(SuppressionReason::Credit);
    state.on_context_budget_changed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::UntilSuccess);
    state.on_provider_success();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);

    state.suppress(SuppressionReason::Auth);
    state.on_provider_success();
    assert_eq!(state.suppression(), AutoCompactionSuppression::Auth);
    state.on_auth_refreshed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);
}

#[test]
fn manual_compaction_bypasses_suppression() {
    let mut state = AutomaticRecoveryState::default();
    state.suppress(SuppressionReason::Other);
    assert!(state.allows(CompactionTrigger::Manual));
    for trigger in [
        CompactionTrigger::Threshold,
        CompactionTrigger::ModelSwitch,
        CompactionTrigger::PreflightOverflow,
        CompactionTrigger::ProviderOverflow,
    ] {
        assert!(!state.allows(trigger));
    }
}

#[test]
fn overflow_recovery_credit_is_single_use() {
    let mut budget = SamplingRecoveryBudget::default();
    assert!(budget.try_use_overflow_recovery());
    assert!(!budget.try_use_overflow_recovery());
}

#[test]
fn typed_failure_kind_drives_suppression_reason() {
    let error = |kind| {
        ModelError::new(
            "model.failed",
            "misleading legacy text",
            Retryability::Never,
        )
        .with_kind(kind)
    };
    assert_eq!(
        suppression_reason(&error(ModelErrorKind::Authentication)),
        SuppressionReason::Auth
    );
    assert_eq!(
        suppression_reason(&error(ModelErrorKind::Credit)),
        SuppressionReason::Credit
    );
    assert_eq!(
        suppression_reason(&error(ModelErrorKind::ContextOverflow)),
        SuppressionReason::Size
    );
    assert_eq!(
        suppression_reason(&error(ModelErrorKind::InvalidRequest)),
        SuppressionReason::Schema
    );
    assert_eq!(
        suppression_reason(&error(ModelErrorKind::Transport)),
        SuppressionReason::Other
    );
}

#[test]
fn repeated_suppression_is_quiet_and_keeps_original_scope() {
    let mut state = AutomaticRecoveryState::default();
    assert!(state.suppress(SuppressionReason::Auth));
    assert!(!state.suppress(SuppressionReason::Size));
    assert_eq!(state.suppression(), AutoCompactionSuppression::Auth);
}
