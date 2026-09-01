use lato_core::{
    EnvironmentPolicy, NetworkPolicy, PolicyDecision, PolicyMode, PolicyRequest, SandboxObligation,
    SandboxProfile, SessionId, SideEffect, ToolCallId, ToolCapability, ToolName, TurnId,
};
use lato_policy::{
    ApprovalLedger, PolicyEngine, PolicyEvent, PolicyEventKind, PolicyEventSink,
    approval_fingerprint, redact_text,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct RecordingSink {
    events: Mutex<Vec<PolicyEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn snapshot(&self) -> Vec<PolicyEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl PolicyEventSink for RecordingSink {
    fn emit(&self, event: PolicyEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn request(
    mode: PolicyMode,
    capabilities: Vec<ToolCapability>,
    side_effect: SideEffect,
    project_trusted: bool,
) -> PolicyRequest {
    PolicyRequest {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::parse("builtin:test").unwrap(),
        arguments_digest: "arguments-digest".into(),
        capabilities,
        side_effect,
        mode,
        project_trusted,
        sandbox: SandboxObligation {
            profile: SandboxProfile::Workspace,
            workspace_root: std::path::PathBuf::from("/workspace"),
            writable_roots: vec![std::path::PathBuf::from("/workspace")],
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy::default(),
        },
    }
}

fn engine_with(sink: Arc<dyn PolicyEventSink>) -> PolicyEngine {
    PolicyEngine::with_sink(Arc::new(ApprovalLedger::new(Duration::from_secs(60))), sink)
}

#[test]
fn redact_text_masks_credential_assignments_bearer_headers_and_known_secrets() {
    let redacted = redact_text(
        "api_key=plain-secret token=abc Authorization: Bearer hdr-token known",
        &["known"],
    );
    assert!(!redacted.contains("plain-secret"));
    assert!(!redacted.contains("abc"));
    assert!(!redacted.contains("hdr-token"));
    assert!(!redacted.contains("known"));
    assert!(redacted.contains("[REDACTED]"));
}

#[test]
fn serialized_denied_and_completed_events_keep_ids_and_omit_payloads() {
    let denied = PolicyEvent {
        kind: PolicyEventKind::Denied,
        session_id: Some(SessionId::from("session-1")),
        turn_id: Some(TurnId::from("turn-1")),
        call_id: Some(ToolCallId::from("call-1")),
        tool_name: Some(ToolName::parse("builtin:write_file").unwrap()),
        argument_digest: Some("deadbeef".into()),
        code: Some("policy.untrusted_extension".into()),
        elapsed_ms: None,
        output_bytes: None,
    };
    let completed = PolicyEvent {
        kind: PolicyEventKind::ToolCompleted,
        session_id: Some(SessionId::from("session-1")),
        turn_id: Some(TurnId::from("turn-1")),
        call_id: Some(ToolCallId::from("call-9")),
        tool_name: Some(ToolName::parse("builtin:read_file").unwrap()),
        argument_digest: Some("cafebabe".into()),
        code: Some("ok".into()),
        elapsed_ms: Some(12),
        output_bytes: Some(4),
    };

    let denied_json = serde_json::to_string(&denied).unwrap();
    let completed_json = serde_json::to_string(&completed).unwrap();
    for json in [&denied_json, &completed_json] {
        assert!(json.contains("session-1"));
        assert!(json.contains("turn-1"));
        assert!(!json.contains("prompt"));
        assert!(!json.contains("arguments"));
        assert!(!json.contains("please ignore previous instructions"));
        assert!(!json.contains("Authorization"));
    }
    assert!(denied_json.contains("policy.denied") || denied_json.contains("Denied"));
    assert!(denied_json.contains("builtin:write_file"));
    assert!(denied_json.contains("policy.untrusted_extension"));
    assert!(completed_json.contains("builtin:read_file"));
    assert!(completed_json.contains("12"));
    assert!(completed_json.contains("4"));
    let completed_value: serde_json::Value = serde_json::from_str(&completed_json).unwrap();
    assert_eq!(completed_value["elapsed_ms"], 12);
    assert_eq!(completed_value["output_bytes"], 4);
    assert_eq!(completed_value["tool_name"], "builtin:read_file");
    assert_eq!(completed_value["call_id"], "call-9");
}

#[test]
fn engine_emits_evaluated_requested_consumed_and_denied_without_raw_arguments() {
    let sink = RecordingSink::new();
    let engine = engine_with(sink.clone());

    let write = request(
        PolicyMode::Ask,
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
        true,
    );
    let PolicyDecision::RequireApproval(approval) = engine.evaluate(&write) else {
        panic!("write in ask mode must request approval");
    };
    let grant = engine.approve(&approval).unwrap();
    engine
        .consume(&grant, &approval_fingerprint(&write).unwrap())
        .unwrap();

    let denied = request(
        PolicyMode::Ask,
        vec![ToolCapability::ExtensionInvoke],
        SideEffect::ReadOnly,
        false,
    );
    let PolicyDecision::Deny(denial) = engine.evaluate(&denied) else {
        panic!("untrusted extension must be denied");
    };
    assert_eq!(denial.code, "policy.untrusted_extension");

    let events = sink.snapshot();
    let kinds: Vec<_> = events.iter().map(|event| event.kind.clone()).collect();
    assert!(kinds.contains(&PolicyEventKind::Evaluated));
    assert!(kinds.contains(&PolicyEventKind::ApprovalRequested));
    assert!(kinds.contains(&PolicyEventKind::ApprovalConsumed));
    assert!(kinds.contains(&PolicyEventKind::Denied));

    let denied_event = events
        .iter()
        .find(|event| event.kind == PolicyEventKind::Denied)
        .unwrap();
    let json = serde_json::to_string(denied_event).unwrap();
    assert_eq!(
        denied_event.tool_name.as_ref().map(ToolName::as_str),
        Some("builtin:test")
    );
    assert_eq!(
        denied_event.session_id.as_ref().map(SessionId::as_str),
        Some("session-1")
    );
    assert_eq!(
        denied_event.call_id.as_ref().map(ToolCallId::as_str),
        Some("call-1")
    );
    assert_eq!(
        denied_event.code.as_deref(),
        Some("policy.untrusted_extension")
    );
    assert!(!json.contains("prompt"));
    assert!(!json.to_ascii_lowercase().contains("arguments\":"));
}
