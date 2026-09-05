use lato_core::{
    JOURNAL_SCHEMA_VERSION, JournalEnvelope, JournalError, JournalRecord, JournalRecordId,
    ModelContent, ModelSelection, Retryability, SessionId, ToolCallId, ToolError, ToolName, TurnId,
    journal_request_hash, project_journal,
};

#[test]
fn canonical_hash_ignores_object_key_order() {
    let a = serde_json::json!({"b": 2, "a": {"d": 4, "c": 3}});
    let b = serde_json::json!({"a": {"c": 3, "d": 4}, "b": 2});
    assert_eq!(
        journal_request_hash("tool", &a),
        journal_request_hash("tool", &b)
    );
}

#[test]
fn projection_restores_the_latest_model_selection() {
    let sid = SessionId::from("s1");
    let first = ModelSelection::new("fixture", "large-a").unwrap();
    let latest = ModelSelection::new("fixture", "small-b").unwrap();
    let records = vec![
        envelope(&sid, None, 0, JournalRecord::SessionStarted),
        envelope(
            &sid,
            None,
            1,
            JournalRecord::ModelSelected {
                selection: first,
                model_family: Some("family-a".into()),
                context_window: Some(8_000),
            },
        ),
        envelope(
            &sid,
            None,
            2,
            JournalRecord::ModelSelected {
                selection: latest.clone(),
                model_family: Some("family-b".into()),
                context_window: Some(4_000),
            },
        ),
    ];

    let projection = project_journal(&sid, &records).unwrap();
    assert_eq!(projection.model_selection, Some(latest));
    assert_eq!(projection.model_family.as_deref(), Some("family-b"));
    assert_eq!(projection.model_context_window, Some(4_000));
}

#[test]
fn projection_restores_requested_and_rejected_tool_messages() {
    let sid = SessionId::from("s1");
    let tid = TurnId::from("t1");
    let call_id = ToolCallId::from("c1");
    let request_hash = "sha256:v1:test".to_string();
    let records = vec![
        envelope(&sid, None, 0, JournalRecord::SessionStarted),
        envelope(
            &sid,
            Some(&tid),
            1,
            JournalRecord::ToolCallRequested {
                call_id: call_id.clone(),
                name: ToolName::parse("builtin:read_file").unwrap(),
                arguments: serde_json::json!({"path": "README.md"}),
                request_hash: request_hash.clone(),
            },
        ),
        envelope(
            &sid,
            Some(&tid),
            2,
            JournalRecord::ToolCallRejected {
                call_id,
                request_hash,
                error: ToolError::new("policy.denied", "denied", Retryability::Never),
            },
        ),
    ];
    let projection = project_journal(&sid, &records).unwrap();
    assert!(matches!(
        projection.messages[0].content[0],
        ModelContent::ToolCall { .. }
    ));
    assert!(matches!(
        projection.messages[1].content[0],
        ModelContent::ToolResult { .. }
    ));
    assert!(projection.unresolved_tools.is_empty());
    assert_eq!(projection.next_journal_sequence, 3);
}

#[test]
fn projection_rejects_sequence_gaps_without_partial_state() {
    let sid = SessionId::from("s1");
    let records = vec![envelope(&sid, None, 1, JournalRecord::SessionStarted)];
    let error = project_journal(&sid, &records).unwrap_err();
    assert_eq!(error.code(), "journal.sequence");
}

#[test]
fn journal_errors_have_stable_codes() {
    let error = JournalError::IncompleteSideEffect {
        call_id: ToolCallId::from("call-1"),
    };
    assert_eq!(error.code(), "journal.incomplete_side_effect");
    assert_eq!(error.retryability(), Retryability::Never);
}

fn envelope(
    sid: &SessionId,
    tid: Option<&TurnId>,
    sequence: u64,
    record: JournalRecord,
) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-record-{sequence}", sid.as_str())),
        session_id: sid.clone(),
        turn_id: tid.cloned(),
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record,
    }
}
