// Phase 7C3: AgentField journal event contract — round-trip, stable wire
// shape, reader-version gate, and replay state machine (fail-closed).

use lato_core::{
    AGENTFIELD_JOURNAL_SCHEMA_VERSION, AGENTFIELD_MAX_SUMMARY_BYTES, AgentFieldJournalEvent,
    AgentFieldRunStatus, JournalEnvelope, JournalError, JournalRecord, JournalRecordId, SessionId,
    decode_journal_envelope, project_journal, validate_agentfield_event,
};

const SID: &str = "s-agentfield";

fn intent(run_id: &str) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: run_id.into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: None,
        status: AgentFieldRunStatus::Queued,
        timestamp_ms: 1_000,
        summary: None,
        summary_truncated: false,
        last_error: None,
    }
}

fn bound(run_id: &str, execution_id: &str) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldExecutionBound {
        run_id: run_id.into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: Some(execution_id.into()),
        status: AgentFieldRunStatus::Queued,
        timestamp_ms: 1_100,
        summary: None,
        summary_truncated: false,
        last_error: None,
    }
}

fn observed(run_id: &str, status: AgentFieldRunStatus) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldStatusObserved {
        run_id: run_id.into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: Some("exec-1".into()),
        status,
        timestamp_ms: 1_200,
        summary: None,
        summary_truncated: false,
        last_error: None,
    }
}

fn terminal(run_id: &str, status: AgentFieldRunStatus) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldRunTerminal {
        run_id: run_id.into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: Some("exec-1".into()),
        status,
        timestamp_ms: 1_300,
        summary: Some("done".into()),
        summary_truncated: false,
        last_error: None,
    }
}

fn agentfield_envelope(
    sid: &SessionId,
    sequence: u64,
    event: AgentFieldJournalEvent,
) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: AGENTFIELD_JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-record-{sequence}", sid.as_str())),
        session_id: sid.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record: JournalRecord::AgentField { event },
    }
}

#[test]
fn five_agentfield_events_round_trip_with_stable_tags() {
    let events = [
        intent("afrun_1"),
        bound("afrun_1", "exec-1"),
        observed("afrun_1", AgentFieldRunStatus::Running),
        AgentFieldJournalEvent::AgentFieldCancelRequested {
            run_id: "afrun_1".into(),
            session_id: SID.into(),
            alias: "contract-review".into(),
            execute_target: "legal.review_contract".into(),
            catalog_revision: "sha256:rev-1".into(),
            input_digest: "a".repeat(64),
            execution_id: Some("exec-1".into()),
            status: AgentFieldRunStatus::Running,
            timestamp_ms: 1_250,
            summary: None,
            summary_truncated: false,
            last_error: None,
        },
        terminal("afrun_1", AgentFieldRunStatus::Completed),
    ];
    for event in events {
        let encoded = serde_json::to_vec(&event).unwrap();
        assert_eq!(
            serde_json::from_slice::<AgentFieldJournalEvent>(&encoded).unwrap(),
            event
        );
    }
    // Stable wire shape: the outer record tag and the event tags.
    let record = serde_json::to_value(JournalRecord::AgentField {
        event: intent("afrun_1"),
    })
    .unwrap();
    assert_eq!(record["type"], "agentfield");
    assert_eq!(record["event"]["type"], "agentfield_run_intent_recorded");
    let bound_json = serde_json::to_value(bound("afrun_1", "exec-1")).unwrap();
    assert_eq!(bound_json["type"], "agentfield_execution_bound");
    let cancel_json = serde_json::to_value(AgentFieldJournalEvent::AgentFieldCancelRequested {
        run_id: "r".into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: None,
        status: AgentFieldRunStatus::Queued,
        timestamp_ms: 1,
        summary: None,
        summary_truncated: false,
        last_error: None,
    })
    .unwrap();
    assert_eq!(cancel_json["type"], "agentfield_cancel_requested");
    let status_json = serde_json::to_value(observed("r", AgentFieldRunStatus::Running)).unwrap();
    assert_eq!(status_json["type"], "agentfield_status_observed");
    assert_eq!(status_json["status"], "running");
    let terminal_json =
        serde_json::to_value(terminal("r", AgentFieldRunStatus::OutcomeUnknown)).unwrap();
    assert_eq!(terminal_json["type"], "agentfield_run_terminal");
    assert_eq!(terminal_json["status"], "outcome_unknown");
}

#[test]
fn validation_enforces_field_caps() {
    // Oversized summary in a terminal event fails closed.
    let oversized = AgentFieldJournalEvent::AgentFieldRunTerminal {
        run_id: "afrun_2".into(),
        session_id: SID.into(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        execution_id: None,
        status: AgentFieldRunStatus::Failed,
        timestamp_ms: 1,
        summary: Some("x".repeat(AGENTFIELD_MAX_SUMMARY_BYTES + 1)),
        summary_truncated: true,
        last_error: None,
    };
    assert!(matches!(
        validate_agentfield_event(&oversized),
        Err(JournalError::Corrupt { .. })
    ));
    // Empty run id / digest fail closed.
    let empty_run_id = AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: String::new(),
        session_id: SID.into(),
        alias: "alias".into(),
        execute_target: "t".into(),
        catalog_revision: "rev".into(),
        input_digest: "d".repeat(64),
        execution_id: None,
        status: AgentFieldRunStatus::Queued,
        timestamp_ms: 1,
        summary: None,
        summary_truncated: false,
        last_error: None,
    };
    assert!(matches!(
        validate_agentfield_event(&empty_run_id),
        Err(JournalError::Corrupt { .. })
    ));
    // A well-formed intent passes.
    assert!(validate_agentfield_event(&intent("afrun_1")).is_ok());
}

#[test]
fn decode_gate_rejects_future_versions_before_payload_deserialization() {
    let sid = SessionId::from(SID);
    let line = serde_json::to_vec(&agentfield_envelope(&sid, 0, intent("afrun_1"))).unwrap();
    // Current version + agentfield record: accepted.
    assert!(decode_journal_envelope(&line).is_ok());

    let mut value: serde_json::Value = serde_json::from_slice(&line).unwrap();
    // A FUTURE schema version must be rejected before the payload parses —
    // even when the payload itself is well-formed.
    value["schema_version"] = serde_json::json!(3);
    let future = serde_json::to_vec(&value).unwrap();
    match decode_journal_envelope(&future) {
        Err(JournalError::SchemaUnsupported { expected, actual }) => {
            assert_eq!(expected, AGENTFIELD_JOURNAL_SCHEMA_VERSION);
            assert_eq!(actual, 3);
        }
        other => panic!("expected schema rejection, got {other:?}"),
    }

    // An agentfield payload hidden in a version-1 envelope fails closed.
    value["schema_version"] = serde_json::json!(1);
    let forged = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        decode_journal_envelope(&forged),
        Err(JournalError::SchemaUnsupported { .. })
    ));

    // Version 2 is reserved for agentfield records only.
    let plain = serde_json::json!({
        "schema_version": AGENTFIELD_JOURNAL_SCHEMA_VERSION,
        "record_id": format!("{SID}-record-0"),
        "session_id": SID,
        "turn_id": null,
        "journal_sequence": 0,
        "timestamp_ms": 0,
        "record": {"type": "session_started"},
    });
    assert!(matches!(
        decode_journal_envelope(&serde_json::to_vec(&plain).unwrap()),
        Err(JournalError::Corrupt { .. })
    ));
}

#[test]
fn replay_rebuilds_a_full_run_lifecycle() {
    let sid = SessionId::from(SID);
    let envelopes = vec![
        agentfield_envelope(&sid, 0, intent("afrun_1")),
        agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
        agentfield_envelope(&sid, 2, observed("afrun_1", AgentFieldRunStatus::Running)),
        agentfield_envelope(&sid, 3, terminal("afrun_1", AgentFieldRunStatus::Completed)),
    ];
    let projection = project_journal(&sid, &envelopes).unwrap();
    assert_eq!(projection.agentfield_runs.len(), 1);
    let run = &projection.agentfield_runs[0];
    assert_eq!(run.run_id, "afrun_1");
    assert_eq!(run.alias, "contract-review");
    assert_eq!(run.revision, "sha256:rev-1");
    assert_eq!(run.execution_id.as_deref(), Some("exec-1"));
    assert_eq!(run.status, AgentFieldRunStatus::Completed);
    assert_eq!(run.summary.as_deref(), Some("done"));
    assert_eq!(run.created_at_ms, 1_000);
    assert_eq!(run.updated_at_ms, 1_300);
}

#[test]
fn replay_fails_closed_on_illegal_agentfield_transitions() {
    let sid = SessionId::from(SID);
    // Missing intent: bind/status/cancel/terminal without a prior intent.
    for event in [
        bound("afrun_x", "exec-1"),
        observed("afrun_x", AgentFieldRunStatus::Running),
        AgentFieldJournalEvent::AgentFieldCancelRequested {
            run_id: "afrun_x".into(),
            session_id: SID.into(),
            alias: "contract-review".into(),
            execute_target: "legal.review_contract".into(),
            catalog_revision: "sha256:rev-1".into(),
            input_digest: "a".repeat(64),
            execution_id: None,
            status: AgentFieldRunStatus::Queued,
            timestamp_ms: 1,
            summary: None,
            summary_truncated: false,
            last_error: None,
        },
        terminal("afrun_x", AgentFieldRunStatus::Completed),
    ] {
        let error = project_journal(&sid, &[agentfield_envelope(&sid, 0, event)]).unwrap_err();
        assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
    }
    // Conflicting remote binding.
    let error = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
            agentfield_envelope(&sid, 2, bound("afrun_1", "exec-OTHER")),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
    // Terminal overwrite by a different terminal status.
    let error = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
            agentfield_envelope(&sid, 2, terminal("afrun_1", AgentFieldRunStatus::Completed)),
            agentfield_envelope(&sid, 3, terminal("afrun_1", AgentFieldRunStatus::Failed)),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
    // A non-terminal status in a terminal event.
    let error = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, terminal("afrun_1", AgentFieldRunStatus::Running)),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
    // A foreign session id inside the event.
    let foreign = AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: "afrun_9".into(),
        session_id: "other-session".into(),
        alias: "a".into(),
        execute_target: "t".into(),
        catalog_revision: "rev".into(),
        input_digest: "d".repeat(64),
        execution_id: None,
        status: AgentFieldRunStatus::Queued,
        timestamp_ms: 1,
        summary: None,
        summary_truncated: false,
        last_error: None,
    };
    let error = project_journal(&sid, &[agentfield_envelope(&sid, 0, foreign)]).unwrap_err();
    assert!(
        matches!(error, JournalError::SessionMismatch { .. }),
        "{error}"
    );
}

#[test]
fn replay_is_idempotent_for_duplicated_observations_and_late_arrivals() {
    let sid = SessionId::from(SID);
    // Late running observation after the committed terminal is ignored.
    let projection = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
            agentfield_envelope(&sid, 2, terminal("afrun_1", AgentFieldRunStatus::Completed)),
            agentfield_envelope(&sid, 3, observed("afrun_1", AgentFieldRunStatus::Running)),
        ],
    )
    .unwrap();
    assert_eq!(
        projection.agentfield_runs[0].status,
        AgentFieldRunStatus::Completed
    );
    // Byte-identical duplicate terminal replays idempotently.
    let projection = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
            agentfield_envelope(&sid, 2, terminal("afrun_1", AgentFieldRunStatus::Completed)),
            agentfield_envelope(&sid, 3, terminal("afrun_1", AgentFieldRunStatus::Completed)),
        ],
    )
    .unwrap();
    assert_eq!(
        projection.agentfield_runs[0].status,
        AgentFieldRunStatus::Completed
    );
    // Rebinding the SAME execution id is idempotent.
    let projection = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, bound("afrun_1", "exec-1")),
            agentfield_envelope(&sid, 2, bound("afrun_1", "exec-1")),
        ],
    )
    .unwrap();
    assert_eq!(
        projection.agentfield_runs[0].execution_id.as_deref(),
        Some("exec-1")
    );
}

#[test]
fn replay_is_idempotent_for_a_byte_identical_duplicated_intent() {
    let sid = SessionId::from(SID);
    // Exact duplicate (identical minimal fields): replayed idempotently.
    let projection = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, intent("afrun_1")),
        ],
    )
    .unwrap();
    assert_eq!(projection.agentfield_runs.len(), 1);
    assert_eq!(projection.agentfield_runs[0].run_id, "afrun_1");
}

#[test]
fn replay_rejects_a_conflicting_duplicated_intent() {
    let sid = SessionId::from(SID);
    // Same run id with a CONFLICTING field: fail closed.
    let mut conflicting = intent("afrun_1");
    if let AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        catalog_revision, ..
    } = &mut conflicting
    {
        *catalog_revision = "sha256:rev-OTHER".into();
    }
    let error = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, conflicting),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
    // A follow-up event whose identity fields conflict with the recorded
    // intent also fails closed.
    let mut conflicting_bind = bound("afrun_1", "exec-1");
    if let AgentFieldJournalEvent::AgentFieldExecutionBound { alias, .. } = &mut conflicting_bind {
        *alias = "other-alias".into();
    }
    let error = project_journal(
        &sid,
        &[
            agentfield_envelope(&sid, 0, intent("afrun_1")),
            agentfield_envelope(&sid, 1, conflicting_bind),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
}

#[test]
fn replay_of_an_intent_only_journal_restores_outcome_unknown_only_after_terminal() {
    let sid = SessionId::from(SID);
    // Intent-only: the run is still nonterminal queued in the projection —
    // the manager converts unbound nonterminal runs to outcome_unknown at
    // install time (crash recovery rules). The projection itself surfaces
    // the journaled truth.
    let projection =
        project_journal(&sid, &[agentfield_envelope(&sid, 0, intent("afrun_1"))]).unwrap();
    assert_eq!(
        projection.agentfield_runs[0].status,
        AgentFieldRunStatus::Queued
    );
    assert!(projection.agentfield_runs[0].execution_id.is_none());
}

/// D-7C3-03 evidence: every variant carries the COMPLETE minimal field
/// set with a stable serialized shape. This snapshot locks the exact
/// key set and order-independent shape of each event on the wire.
#[test]
fn five_event_schema_snapshot_covers_the_full_minimal_field_set() {
    let expected_keys = [
        "alias",
        "catalog_revision",
        "execute_target",
        "execution_id",
        "input_digest",
        "last_error",
        "run_id",
        "session_id",
        "status",
        "summary",
        "summary_truncated",
        "timestamp_ms",
        "type",
    ];
    let events = [
        intent("afrun_1"),
        bound("afrun_1", "exec-1"),
        observed("afrun_1", AgentFieldRunStatus::Running),
        AgentFieldJournalEvent::AgentFieldCancelRequested {
            run_id: "afrun_1".into(),
            session_id: SID.into(),
            alias: "contract-review".into(),
            execute_target: "legal.review_contract".into(),
            catalog_revision: "sha256:rev-1".into(),
            input_digest: "a".repeat(64),
            execution_id: Some("exec-1".into()),
            status: AgentFieldRunStatus::Running,
            timestamp_ms: 1_250,
            summary: None,
            summary_truncated: false,
            last_error: None,
        },
        terminal("afrun_1", AgentFieldRunStatus::Completed),
    ];
    for event in events {
        let value = serde_json::to_value(&event).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("event must serialize to an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, expected_keys, "snapshot for {}", event.run_id());
        // The bind event is the only variant required to carry a remote id.
        let is_bind = value["type"] == "agentfield_execution_bound";
        if is_bind {
            assert!(value["execution_id"].is_string());
        }
    }
}
