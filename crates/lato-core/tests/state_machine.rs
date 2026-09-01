use lato_core::{
    SessionMachine, SessionPhase, StartBehavior, StartDecision, TransitionError, TurnId,
};

#[test]
fn starts_only_one_foreground_turn() {
    let mut machine = SessionMachine::new();
    assert_eq!(
        machine
            .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
            .unwrap(),
        StartDecision::StartNow,
    );
    assert_eq!(
        machine.request_start(TurnId::from("turn-2"), StartBehavior::Reject),
        Err(TransitionError::TurnAlreadyActive),
    );
}

#[test]
fn replacement_cancels_old_turn_before_new_turn_starts() {
    let mut machine = SessionMachine::new();
    machine
        .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
        .unwrap();
    assert_eq!(
        machine
            .request_start(TurnId::from("turn-2"), StartBehavior::Replace)
            .unwrap(),
        StartDecision::CancelThenStart {
            active: TurnId::from("turn-1"),
            pending: TurnId::from("turn-2"),
        },
    );
    let active = machine.active_turn().unwrap();
    assert_eq!(active.id, TurnId::from("turn-1"));
    assert!(active.cancel_requested);
    assert_eq!(
        machine.finish(&TurnId::from("turn-2")),
        Err(TransitionError::NotActiveTurn),
    );
}

#[test]
fn only_the_active_turn_can_finish() {
    let mut machine = SessionMachine::new();
    machine
        .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
        .unwrap();
    assert_eq!(
        machine.finish(&TurnId::from("turn-2")),
        Err(TransitionError::NotActiveTurn),
    );
    machine.finish(&TurnId::from("turn-1")).unwrap();
    assert_eq!(machine.phase(), &SessionPhase::Idle);
}

#[test]
fn stopped_sessions_reject_new_turns() {
    let mut machine = SessionMachine::new();
    machine.stop();
    assert_eq!(
        machine.request_start(TurnId::from("turn-1"), StartBehavior::Reject),
        Err(TransitionError::SessionStopped),
    );
}
