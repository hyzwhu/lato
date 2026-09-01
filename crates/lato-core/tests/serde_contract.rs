use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventEnvelope, EventId, EventPayload,
    Retryability, SessionId, StartBehavior, StartTurn, TurnId, TurnOutput, UserInput,
};

#[test]
fn ids_and_errors_have_stable_json_shapes() {
    let session_id = SessionId::from("session-7");
    let turn_id = TurnId::from("turn-9");
    assert_eq!(serde_json::to_string(&session_id).unwrap(), "\"session-7\"");
    assert_eq!(serde_json::to_string(&turn_id).unwrap(), "\"turn-9\"");

    let error = AgentError::new(
        "runtime.bus_closed",
        ErrorCategory::InternalInvariant,
        "runtime command bus closed",
        Retryability::Never,
    );
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["code"], "runtime.bus_closed");
    assert_eq!(value["category"], "internal_invariant");
    assert_eq!(value["retryability"], "never");
}

#[test]
fn empty_ids_are_rejected_by_checked_constructor() {
    assert!(SessionId::parse(" ").is_err());
    assert!(TurnId::parse("").is_err());
}

#[test]
fn command_uses_tagged_snake_case_wire_shape() {
    let command = Command::StartTurn(StartTurn {
        input: UserInput::text("inspect the repository"),
        behavior: StartBehavior::Replace,
    });
    let value = serde_json::to_value(command).unwrap();
    assert_eq!(value["type"], "start_turn");
    assert_eq!(value["input"]["text"], "inspect the repository");
    assert_eq!(value["behavior"], "replace");
}

#[test]
fn event_envelope_round_trips_without_losing_identity() {
    let envelope = EventEnvelope {
        schema_version: 1,
        event_id: EventId::from("event-1"),
        session_id: SessionId::from("session-1"),
        turn_id: Some(TurnId::from("turn-1")),
        parent_event_id: None,
        sequence: 3,
        timestamp_ms: 1_700_000_000_000,
        payload: EventPayload::TurnCompleted(TurnOutput {
            final_text: "done".into(),
        }),
    };
    let encoded = serde_json::to_string(&envelope).unwrap();
    let decoded: EventEnvelope = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, envelope);
}

#[test]
fn cancellation_reason_is_explicit() {
    let payload = EventPayload::TurnCancelled {
        reason: CancelReason::Replaced,
    };
    assert_eq!(serde_json::to_value(payload).unwrap()["reason"], "replaced");
}
