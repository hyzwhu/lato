use lato_core::{
    Command, ExtensionAuditRecord, JOURNAL_SCHEMA_VERSION, JournalEnvelope, JournalError,
    JournalRecord, JournalRecordId, McpAuditOutcome, ModelContent, ModelSelection,
    PluginSnapshotSummary, Retryability, SessionId, SkillInvocationOrigin, ToolCallId, ToolError,
    ToolName, TurnId, journal_request_hash, project_journal,
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

#[test]
fn plugin_snapshot_adoption_round_trips_through_journal() {
    let record = JournalRecord::PluginSnapshotAdopted {
        summary: PluginSnapshotSummary {
            generation: 4,
            discovered: 2,
            active: 1,
            project_trusted: true,
        },
    };
    let encoded = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        serde_json::from_slice::<JournalRecord>(&encoded).unwrap(),
        record
    );
}

#[test]
fn journal_extension_audit_round_trips_without_projecting_conversation_state() {
    let sid = SessionId::from("audit-session");
    let audit = ExtensionAuditRecord::SkillInvoked {
        qualified_name: "demo:inspect".into(),
        origin: SkillInvocationOrigin::Model,
        body_hash: "sha256:v1:body".into(),
        allowed_tools_hash: Some("sha256:v1:tools".into()),
    };
    let record = JournalRecord::ExtensionAudit {
        audit: audit.clone(),
    };
    let encoded = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        serde_json::from_slice::<JournalRecord>(&encoded).unwrap(),
        record
    );

    let projection = project_journal(
        &sid,
        &[
            envelope(&sid, None, 0, JournalRecord::SessionStarted),
            envelope(&sid, None, 1, record),
        ],
    )
    .unwrap();
    assert!(projection.messages.is_empty());
    assert_eq!(projection.next_journal_sequence, 2);

    let serialized = serde_json::to_string(&audit).unwrap();
    assert!(!serialized.contains("secret body"));
    assert!(!serialized.contains("secret arguments"));
    assert!(!serialized.contains("expanded message"));
}

#[test]
fn journal_extension_audit_command_has_stable_snake_case_shape() {
    let command = Command::RecordExtensionAudit {
        audit: ExtensionAuditRecord::SkillCatalogMaterialized {
            generation: 3,
            visible_count: 2,
            omitted_count: 1,
            catalog_hash: "sha256:v1:catalog".into(),
        },
    };
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["type"], "record_extension_audit");
    assert_eq!(value["audit"]["type"], "skill_catalog_materialized");
    assert_eq!(serde_json::from_value::<Command>(value).unwrap(), command);
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

#[test]
fn mcp_tool_call_audit_round_trips_without_secrets_or_full_bodies() {
    let sid = SessionId::from("mcp-audit");
    let secret_args = r#"{"token":"super-secret-token","url":"https://user:pass@evil/mcp"}"#;
    let huge = "X".repeat(40_000);
    let audit = ExtensionAuditRecord::McpToolCall {
        generation: 3,
        server: "demo".into(),
        tool: "ping".into(),
        qualified_name: "demo__ping".into(),
        duration_ms: Some(12),
        outcome: McpAuditOutcome::Succeeded,
        args_hash: journal_request_hash(
            "mcp_tool_args",
            &serde_json::json!({"arguments": serde_json::from_str::<serde_json::Value>(secret_args).unwrap()}),
        ),
        result_hash: Some(journal_request_hash(
            "mcp_tool_result",
            &serde_json::json!({"content": huge}),
        )),
        truncated: true,
        error_code: None,
        redacted_reason: Some("mcp.result_spilled".into()),
    };
    let record = JournalRecord::ExtensionAudit {
        audit: audit.clone(),
    };
    let encoded = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        serde_json::from_slice::<JournalRecord>(&encoded).unwrap(),
        record
    );
    let serialized = serde_json::to_string(&audit).unwrap();
    assert!(!serialized.contains("super-secret-token"));
    assert!(!serialized.contains("user:pass"));
    assert!(!serialized.contains(&"X".repeat(100)));
    assert!(serialized.contains("args_hash"));
    assert!(serialized.contains("result_hash"));
    assert_eq!(
        serde_json::to_value(&audit).unwrap()["type"],
        "mcp_tool_call"
    );

    let projection = project_journal(
        &sid,
        &[
            envelope(&sid, None, 0, JournalRecord::SessionStarted),
            envelope(&sid, None, 1, record),
        ],
    )
    .unwrap();
    assert!(projection.messages.is_empty());
}
