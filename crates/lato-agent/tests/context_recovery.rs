use lato_agent::{
    AutoCompactionSuppression, AutomaticRecoveryState, SamplingRecoveryBudget, SuppressionReason,
    suppression_reason,
};
use lato_core::{CompactionTrigger, ModelError, ModelErrorKind, Retryability};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleEvent {
    NewTurn,
    ContextBudgetChanged,
    CompactionSuccess,
    ProviderSuccess,
    AuthRefreshed,
}

fn apply(state: &mut AutomaticRecoveryState, event: LifecycleEvent) {
    match event {
        LifecycleEvent::NewTurn => state.on_new_turn(),
        LifecycleEvent::ContextBudgetChanged => state.on_context_budget_changed(),
        LifecycleEvent::CompactionSuccess => state.on_compaction_success(),
        LifecycleEvent::ProviderSuccess => state.on_provider_success(),
        LifecycleEvent::AuthRefreshed => state.on_auth_refreshed(),
    }
}

#[test]
fn suppression_lifetimes_clear_only_on_their_documented_events() {
    let cases = [
        (
            SuppressionReason::Other,
            AutoCompactionSuppression::Turn,
            &[LifecycleEvent::NewTurn][..],
        ),
        (
            SuppressionReason::Size,
            AutoCompactionSuppression::Sticky,
            &[
                LifecycleEvent::ContextBudgetChanged,
                LifecycleEvent::CompactionSuccess,
            ][..],
        ),
        (
            SuppressionReason::Credit,
            AutoCompactionSuppression::UntilSuccess,
            &[LifecycleEvent::ProviderSuccess][..],
        ),
        (
            SuppressionReason::Auth,
            AutoCompactionSuppression::Auth,
            &[LifecycleEvent::AuthRefreshed][..],
        ),
    ];
    let all_events = [
        LifecycleEvent::NewTurn,
        LifecycleEvent::ContextBudgetChanged,
        LifecycleEvent::CompactionSuccess,
        LifecycleEvent::ProviderSuccess,
        LifecycleEvent::AuthRefreshed,
    ];

    for (reason, scope, clearing_events) in cases {
        for event in all_events {
            let mut state = AutomaticRecoveryState::default();
            assert!(state.suppress(reason));
            apply(&mut state, event);
            let expected = if clearing_events.contains(&event) {
                AutoCompactionSuppression::None
            } else {
                scope
            };
            assert_eq!(state.suppression(), expected, "{scope:?} after {event:?}");
        }
    }
}

#[test]
fn every_suppression_scope_blocks_automatic_triggers_but_manual_bypasses_policy() {
    for reason in [
        SuppressionReason::Other,
        SuppressionReason::Size,
        SuppressionReason::Credit,
        SuppressionReason::Auth,
    ] {
        let mut state = AutomaticRecoveryState::default();
        assert!(state.suppress(reason));
        assert!(state.allows(CompactionTrigger::Manual), "{reason:?}");
        for trigger in [
            CompactionTrigger::Threshold,
            CompactionTrigger::ModelSwitch,
            CompactionTrigger::PreflightOverflow,
            CompactionTrigger::ProviderOverflow,
        ] {
            assert!(!state.allows(trigger), "{reason:?} allowed {trigger:?}");
        }
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
