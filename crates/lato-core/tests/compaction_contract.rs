use lato_core::*;

#[test]
fn compact_command_has_stable_wire_shape() {
    let command = Command::CompactSession(CompactSession {
        user_context: Some("preserve the parser diagnosis".into()),
        trigger: CompactionTrigger::Manual,
    });
    assert_eq!(
        serde_json::to_value(command).unwrap(),
        serde_json::json!({
            "type": "compact_session",
            "user_context": "preserve the parser diagnosis",
            "trigger": "manual"
        })
    );
}

#[test]
fn compaction_ids_reject_empty_values() {
    assert!(CompactionId::parse(" ").is_err());
}

#[test]
fn compaction_error_codes_are_stable() {
    assert_eq!(
        CompactionError::NothingToCompact.code(),
        "compaction.nothing_to_compact"
    );
    assert_eq!(
        CompactionError::AlreadyActive.code(),
        "compaction.already_active"
    );
    assert_eq!(CompactionError::Cancelled.code(), "compaction.cancelled");
}

#[test]
fn turn_and_compaction_are_mutually_exclusive() {
    let mut machine = SessionMachine::new();
    let cid = CompactionId::from("compact-1");
    machine.request_compaction(cid.clone()).unwrap();
    assert_eq!(
        machine.request_start(TurnId::from("turn-1"), StartBehavior::Reject),
        Err(TransitionError::CompactionAlreadyActive)
    );
    machine.request_compaction_cancel(&cid).unwrap();
    machine.finish_compaction(&cid).unwrap();
    assert!(matches!(machine.phase(), SessionPhase::Idle));
}

#[test]
fn compact_while_turn_active_is_rejected() {
    let mut machine = SessionMachine::new();
    machine
        .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
        .unwrap();
    assert_eq!(
        machine.request_compaction(CompactionId::from("compact-1")),
        Err(TransitionError::TurnAlreadyActive)
    );
}

#[test]
fn compaction_event_round_trips_with_warning() {
    let payload = EventPayload::CompactionCompleted {
        compaction_id: CompactionId::from("compact-1"),
        before: CompactionSize {
            message_count: 12,
            serialized_bytes: 30_000,
        },
        after: CompactionSize {
            message_count: 3,
            serialized_bytes: 4_000,
        },
        checkpoint_id: "checkpoint-1".into(),
        warning: None,
    };
    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(value["type"], "compaction_completed");
    assert_eq!(
        serde_json::from_value::<EventPayload>(value).unwrap(),
        payload
    );
}

#[test]
fn grok_default_threshold_is_eighty_five_percent() {
    assert_eq!(CompactionPolicy::default().threshold_percent, 85);
}

#[test]
fn provider_overflow_trigger_has_a_stable_wire_shape() {
    assert_eq!(
        serde_json::to_value(CompactionTrigger::ProviderOverflow).unwrap(),
        serde_json::json!("provider_overflow")
    );
    assert_eq!(
        serde_json::from_value::<CompactionTrigger>(serde_json::json!("threshold")).unwrap(),
        CompactionTrigger::Threshold
    );
}

#[test]
fn provider_usage_becomes_the_confirmed_baseline_without_double_counting() {
    let mut ledger = ContextLedger::default();
    let usage = ModelUsage {
        input_tokens: Some(800),
        output_tokens: Some(100),
        reasoning_tokens: Some(60),
        cached_input_tokens: Some(400),
    };

    assert!(ledger.observe(&usage, 720));
    assert_eq!(ledger.measure(760, Some(1_000)).estimated_input_tokens, 940);
}

#[test]
fn missing_usage_does_not_erase_a_confirmed_baseline() {
    let mut ledger = ContextLedger::default();
    assert!(ledger.observe(
        &ModelUsage {
            input_tokens: Some(700),
            output_tokens: Some(100),
            reasoning_tokens: None,
            cached_input_tokens: None,
        },
        600,
    ));
    assert!(!ledger.observe(
        &ModelUsage {
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: Some(20),
            cached_input_tokens: Some(50),
        },
        650,
    ));
    assert_eq!(ledger.measure(700, None).estimated_input_tokens, 900);
}

#[test]
fn threshold_is_inclusive_and_unknown_windows_do_not_trigger() {
    let at = ContextLedger::default().measure(850, Some(1_000));
    let below = ContextLedger::default().measure(849, Some(1_000));
    let unknown = ContextLedger::default().measure(999_999, None);

    assert!(at.threshold_reached(85));
    assert!(!below.threshold_reached(85));
    assert!(!unknown.threshold_reached(85));
    assert_eq!(unknown.context_window, 0);
}

#[test]
fn replacement_reseed_scales_provider_overhead_and_caps_growth() {
    let mut ledger = ContextLedger::default();
    assert!(ledger.observe(
        &ModelUsage {
            input_tokens: Some(900),
            output_tokens: Some(100),
            reasoning_tokens: None,
            cached_input_tokens: None,
        },
        800,
    ));

    ledger.reseed(200);
    assert_eq!(ledger.measure(200, Some(2_000)).estimated_input_tokens, 250);

    ledger.reseed(2_000);
    assert_eq!(
        ledger.measure(2_000, Some(3_000)).estimated_input_tokens,
        250
    );
}

#[test]
fn context_usage_event_has_a_stable_wire_shape() {
    let payload = EventPayload::ContextUsageUpdated {
        usage: ContextUsage {
            estimated_input_tokens: 850,
            context_window: 1_000,
            utilization_percent: 85,
        },
    };

    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "context_usage_updated",
            "usage": {
                "estimated_input_tokens": 850,
                "context_window": 1_000,
                "utilization_percent": 85
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<EventPayload>(value).unwrap(),
        payload
    );
}
