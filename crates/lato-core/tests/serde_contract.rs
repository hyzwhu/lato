use lato_core::{AgentError, ErrorCategory, Retryability, SessionId, TurnId};

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
