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
