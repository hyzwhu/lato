use lato_core::{
    CompactionId, SessionMachine, SessionPhase, StartBehavior, StartDecision, TransitionError,
    TurnId,
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

#[test]
fn a_turn_can_enter_and_leave_nested_compaction() {
    let mut machine = SessionMachine::new();
    let turn_id = TurnId::from("turn-1");
    let compaction_id = CompactionId::from("compact-1");
    machine
        .request_start(turn_id.clone(), StartBehavior::Reject)
        .unwrap();

    machine
        .request_turn_compaction(&turn_id, compaction_id.clone())
        .unwrap();

    let active_turn = machine.active_turn().unwrap();
    assert_eq!(active_turn.id, turn_id);
    assert_eq!(
        active_turn
            .active_compaction
            .as_ref()
            .map(|active| &active.id),
        Some(&compaction_id)
    );
    assert_eq!(
        machine.active_compaction().map(|active| &active.id),
        Some(&compaction_id)
    );
    assert_eq!(
        machine.request_start(TurnId::from("turn-2"), StartBehavior::Replace),
        Err(TransitionError::CompactionAlreadyActive)
    );
    assert_eq!(
        machine.request_compaction(CompactionId::from("compact-2")),
        Err(TransitionError::CompactionAlreadyActive)
    );

    machine
        .finish_turn_compaction(&turn_id, &compaction_id)
        .unwrap();
    assert!(machine.active_compaction().is_none());
    assert_eq!(machine.active_turn().unwrap().id, turn_id);
    machine.finish(&turn_id).unwrap();
    assert_eq!(machine.phase(), &SessionPhase::Idle);
}

#[test]
fn nested_compaction_rejects_duplicates_and_mismatched_finish() {
    let mut machine = SessionMachine::new();
    let turn_id = TurnId::from("turn-1");
    let compaction_id = CompactionId::from("compact-1");
    machine
        .request_start(turn_id.clone(), StartBehavior::Reject)
        .unwrap();
    machine
        .request_turn_compaction(&turn_id, compaction_id.clone())
        .unwrap();

    assert_eq!(
        machine.request_turn_compaction(&turn_id, CompactionId::from("compact-2")),
        Err(TransitionError::CompactionAlreadyActive)
    );
    assert_eq!(
        machine.finish_turn_compaction(&turn_id, &CompactionId::from("compact-2")),
        Err(TransitionError::NotActiveCompaction)
    );
    assert_eq!(
        machine.finish(&turn_id),
        Err(TransitionError::CompactionAlreadyActive)
    );
}

#[test]
fn cancelling_a_turn_also_cancels_its_nested_compaction() {
    let mut machine = SessionMachine::new();
    let turn_id = TurnId::from("turn-1");
    let compaction_id = CompactionId::from("compact-1");
    machine
        .request_start(turn_id.clone(), StartBehavior::Reject)
        .unwrap();
    machine
        .request_turn_compaction(&turn_id, compaction_id.clone())
        .unwrap();

    machine.request_cancel(&turn_id).unwrap();

    let active_turn = machine.active_turn().unwrap();
    assert!(active_turn.cancel_requested);
    assert!(
        active_turn
            .active_compaction
            .as_ref()
            .unwrap()
            .cancel_requested
    );
}

#[test]
fn nested_compaction_cancel_validates_turn_and_compaction_ids() {
    let mut machine = SessionMachine::new();
    let turn_id = TurnId::from("turn-1");
    let compaction_id = CompactionId::from("compact-1");
    machine
        .request_start(turn_id.clone(), StartBehavior::Reject)
        .unwrap();
    machine
        .request_turn_compaction(&turn_id, compaction_id.clone())
        .unwrap();

    assert_eq!(
        machine.request_turn_compaction_cancel(&TurnId::from("turn-2"), &compaction_id,),
        Err(TransitionError::NotActiveTurn)
    );
    assert_eq!(
        machine.request_turn_compaction_cancel(&turn_id, &CompactionId::from("compact-2"),),
        Err(TransitionError::NotActiveCompaction)
    );
    machine
        .request_turn_compaction_cancel(&turn_id, &compaction_id)
        .unwrap();
    assert!(machine.active_compaction().unwrap().cancel_requested);
}
