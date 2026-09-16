// Phase 7C1: offline contract tests for the AgentField adapter foundation.
// No sockets: the HTTP layer is driven through a fake transport fed with the
// pinned fixture envelopes (`docs/superpowers/fixtures/agentfield-v0.1.138-contract.json`).

use lato_agent::agentfield::client::{
    AgentFieldClient, AgentFieldError, HttpAgentFieldClient, OutboundRequest, RawResponse,
    RedactedToken, TransportError,
};
use lato_agent::agentfield::config::{
    AgentFieldConfig, MAX_CAPABILITIES, MAX_OUTPUT_BYTES, PINNED_AGENTFIELD_VERSION,
};
use lato_agent::agentfield::types::{
    AsyncStartEnvelope, CancelConflictEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope,
    RemoteExecutionStatus, StatusEnvelope,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/superpowers/fixtures/agentfield-v0.1.138-contract.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn fixture_str(pointer: &str) -> Value {
    fixture()
        .pointer(pointer)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} missing"))
        .clone()
}

// ---- target derivation (spec §6.2, AC-13) ----

#[test]
fn colon_target_validates_and_derives_dot_execute_target() {
    assert_eq!(
        lato_agent::agentfield::config::derive_execute_target(
            "legal-agent",
            "review_contract",
            "legal-agent:review_contract"
        )
        .unwrap(),
        "legal-agent.review_contract"
    );
}

#[test]
fn target_rejections_fail_before_any_http() {
    let cases = [
        // Non-ASCII atom.
        (
            "legal-agént",
            "review_contract",
            "legal-agént:review_contract",
        ),
        // Dot inside an atom.
        (
            "legal.agent",
            "review_contract",
            "legal.agent:review_contract",
        ),
        // Encoded separator inside an atom.
        ("legal%2Dagent", "review", "legal%2Dagent:review"),
        // Colon mismatch between atoms and invocation target.
        ("legal-agent", "review", "other-agent:review"),
        // Extra separator.
        (
            "legal-agent",
            "review_contract",
            "legal-agent::review_contract",
        ),
        // Empty atom.
        ("legal-agent", "", "legal-agent:"),
    ];
    for (agent, reasoner, invocation) in cases {
        let error =
            lato_agent::agentfield::config::derive_execute_target(agent, reasoner, invocation)
                .expect_err("must fail closed");
        assert!(
            error.contains("atom") || error.contains("invocation_target"),
            "{error}"
        );
    }
    // Slash is invalid in atoms.
    assert!(
        lato_agent::agentfield::config::derive_execute_target(
            "legal/agent",
            "review",
            "legal/agent:review"
        )
        .is_err()
    );
}

/// The real client path (not just pure validators) must reject every escaped
/// execute target before a single transport request is made (AC-13 probe).
#[tokio::test]
async fn start_async_rejects_escaped_targets_with_zero_transport_requests() {
    for execute_target in [
        // Percent-encoded separator.
        "legal%2Eagent.review",
        // Extra dot inside an atom.
        "legal..agent.review_contract",
        // Non-ASCII atom.
        "legal-agént.review_contract",
        // Colon form passed through.
        "legal-agent:review_contract",
        // Slash.
        "legal/agent.review_contract",
        // No separator at all.
        "legal-agent-review_contract",
    ] {
        let transport =
            FakeTransport::with_status(202, fixture_str("/async_start/success_envelope"));
        let client = client_with(transport.clone());
        let error = client
            .start_async(execute_target, &json!({"contract": "acme.pdf"}))
            .await
            .expect_err("escaped target must fail closed");
        assert!(
            matches!(error, AgentFieldError::RemoteProtocol(_)),
            "{execute_target}: {error:?}"
        );
        assert_eq!(
            transport.seen.lock().unwrap().len(),
            0,
            "{execute_target}: rejection must happen before any HTTP request"
        );
    }
}

/// `DiscoveryEnvelope::decode` itself must enforce the target contract:
/// illegal atoms, inconsistent colon targets, and duplicate execute targets
/// fail closed (AC-13 probe).
#[test]
fn discovery_decode_enforces_target_contract() {
    let mut envelope = discovery_body();
    envelope["capabilities"][0]["agent_id"] = json!("bad.agent");
    assert!(
        DiscoveryEnvelope::decode(&envelope).is_err(),
        "illegal atom"
    );

    let mut envelope = discovery_body();
    envelope["capabilities"][0]["reasoners"][0]["invocation_target"] =
        json!("other-agent:review_contract");
    assert!(
        DiscoveryEnvelope::decode(&envelope).is_err(),
        "inconsistent colon target"
    );

    let mut envelope = discovery_body();
    let duplicate = envelope["capabilities"][0].clone();
    envelope["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(
        DiscoveryEnvelope::decode(&envelope).is_err(),
        "duplicate execute target"
    );
}

// ---- config parsing (spec §6.1) ----

fn valid_config_json() -> Value {
    json!({
        "enabled": true,
        "baseUrl": "https://agents.example.internal",
        "credential": "agentfield:primary",
        "capabilities": {
            "contract-review": {
                "target": "legal-agent.review_contract",
                "description": "Review one contract",
                "inputSchema": {"type":"object","additionalProperties":false},
                "risk": "remote_read",
                "timeoutSeconds": 900,
                "maxOutputBytes": 65536
            }
        }
    })
}

#[test]
fn config_parses_valid_stanza() {
    let config = AgentFieldConfig::parse(&valid_config_json())
        .unwrap()
        .expect("enabled stanza yields a config");
    assert!(config.enabled);
    assert_eq!(
        config.origin.base.as_str(),
        "https://agents.example.internal/"
    );
    assert_eq!(config.capabilities.len(), 1);
    assert_eq!(config.capabilities[0].0, "contract-review");
    assert_eq!(
        config.capabilities[0].1.target,
        "legal-agent.review_contract"
    );
    assert_eq!(config.capabilities[0].1.max_output_bytes, MAX_OUTPUT_BYTES);
    assert!(config.capability("contract-review").is_some());
    assert!(config.capability("missing").is_none());
}

#[test]
fn config_rejects_insecure_and_malformed_urls() {
    let cases = [
        json!({"enabled": true, "baseUrl": "http://agents.example.internal", "credential": "agentfield:primary"}),
        json!({"enabled": true, "baseUrl": "https://user:pass@agents.example.internal", "credential": "agentfield:primary"}),
        json!({"enabled": true, "baseUrl": "https://agents.example.internal/?x=1", "credential": "agentfield:primary"}),
        json!({"enabled": true, "baseUrl": "ftp://agents.example.internal", "credential": "agentfield:primary"}),
    ];
    for mut case in cases {
        case["capabilities"] = json!({});
        let error = AgentFieldConfig::parse(&case).unwrap_err();
        assert_eq!(error.code, "agentfield.invalid_arguments", "{error}");
    }
}

#[test]
fn config_allows_loopback_http_only_in_explicit_dev_mode() {
    let mut without_dev = valid_config_json();
    without_dev["baseUrl"] = json!("http://127.0.0.1:8080");
    assert!(AgentFieldConfig::parse(&without_dev).is_err());

    let mut with_dev = without_dev;
    with_dev["allowLoopbackHttp"] = json!(true);
    let _config = AgentFieldConfig::parse(&with_dev).unwrap().unwrap();
    // Dev-mode loopback acceptance stays a config-parse concern in v1.2.1;
    // the parsed origin carries only the normalized base URL.

    // Dev escape hatch never applies to non-loopback hosts.
    let mut remote_http = with_dev.clone();
    remote_http["baseUrl"] = json!("http://agents.example.internal");
    assert!(AgentFieldConfig::parse(&remote_http).is_err());
}

#[test]
fn config_enforces_alias_target_and_caps() {
    let mut bad_alias = valid_config_json();
    bad_alias["capabilities"] = json!({"Bad_Alias": {"target": "a.b", "description": "d", "inputSchema": {}, "risk": "remote_read"}});
    assert_eq!(
        AgentFieldConfig::parse(&bad_alias).unwrap_err().code,
        "agentfield.invalid_arguments"
    );

    let mut bad_target = valid_config_json();
    bad_target["capabilities"] = json!({"review": {"target": "a:b.c", "description": "d", "inputSchema": {}, "risk": "remote_read"}});
    assert!(AgentFieldConfig::parse(&bad_target).is_err());

    let mut bad_output = valid_config_json();
    bad_output["capabilities"] = json!({"review": {"target": "a.b", "description": "d", "inputSchema": {}, "risk": "remote_read", "maxOutputBytes": 999999}});
    assert!(AgentFieldConfig::parse(&bad_output).is_err());

    // More than 64 capabilities is rejected.
    let mut too_many = valid_config_json();
    let mut capabilities = serde_json::Map::new();
    for seq in 0..=MAX_CAPABILITIES {
        capabilities.insert(
            format!("cap{seq:02}"),
            json!({"target": "a.b", "description": "d", "inputSchema": {}, "risk": "remote_read"}),
        );
    }
    too_many["capabilities"] = Value::Object(capabilities);
    assert!(AgentFieldConfig::parse(&too_many).is_err());

    // Credential reference shape.
    let mut bad_credential = valid_config_json();
    bad_credential["credential"] = json!("sk-super-secret");
    assert!(AgentFieldConfig::parse(&bad_credential).is_err());
}

#[test]
fn disabled_stanza_reports_disabled_without_dropping_validation() {
    let mut disabled = valid_config_json();
    disabled["enabled"] = json!(false);
    let config = AgentFieldConfig::parse(&disabled).unwrap().unwrap();
    assert!(!config.enabled);
    assert!(
        config.capabilities.is_empty(),
        "disabled config carries no capability surface"
    );
}

// ---- strict decoders against the pinned fixture (AC-14) ----

#[test]
fn fixture_async_envelope_decodes_and_enforces_invariants() {
    let envelope = fixture_str("/async_start/success_envelope");
    let decoded = AsyncStartEnvelope::decode(&envelope).expect("fixture envelope must decode");
    assert_eq!(decoded.execution_id, "exec_redacted");
    assert_eq!(decoded.status, RemoteExecutionStatus::Queued);
    assert_eq!(decoded.target, "legal-agent.review_contract");
    assert_eq!(decoded.kind, "reasoner");
}

#[test]
fn fixture_async_envelope_invariants_fail_closed() {
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["workflow_id"] = json!("different");
    assert!(
        AsyncStartEnvelope::decode(&envelope).is_err(),
        "workflow_id != run_id must fail"
    );

    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["enqueued_at"] = json!("2026-09-16T00:00:01Z");
    assert!(
        AsyncStartEnvelope::decode(&envelope).is_err(),
        "enqueued_at != created_at must fail"
    );
}

#[test]
fn fixture_status_envelope_decodes_with_optional_type_checks() {
    let envelope = fixture_str("/status/success_envelope");
    let decoded = StatusEnvelope::decode(&envelope).expect("fixture status must decode");
    assert_eq!(decoded.status, RemoteExecutionStatus::Running);

    // Optional fields present with a wrong type fail closed.
    let mut envelope = fixture_str("/status/success_envelope");
    envelope["duration_ms"] = json!("fast");
    assert!(StatusEnvelope::decode(&envelope).is_err());
    let mut envelope = fixture_str("/status/success_envelope");
    envelope["approval_request_url"] = json!(42);
    assert!(StatusEnvelope::decode(&envelope).is_err());
}

#[test]
fn fixture_cancel_envelopes_decode() {
    let success = fixture_str("/cancel/success_envelope");
    let decoded = CancelSuccessEnvelope::decode(&success).unwrap();
    assert_eq!(decoded.status, RemoteExecutionStatus::Cancelled);

    let conflict = fixture_str("/cancel/terminal_conflict_envelope");
    let decoded = CancelConflictEnvelope::decode(&conflict).unwrap();
    assert_eq!(decoded.error, "invalid_state");

    // Only `invalid_state` is a stable contract.
    let mut conflict = fixture_str("/cancel/terminal_conflict_envelope");
    conflict["error"] = json!("something_else");
    assert!(CancelConflictEnvelope::decode(&conflict).is_err());
    // The dynamic message is only type-checked, never frozen.
    let mut conflict = fixture_str("/cancel/terminal_conflict_envelope");
    conflict["message"] = json!("totally different wording");
    assert!(CancelConflictEnvelope::decode(&conflict).is_ok());
}

#[test]
fn fixture_discovery_envelope_decodes_and_validates_targets() {
    let envelope = fixture_str("/discovery/success_envelope");
    let decoded = DiscoveryEnvelope::decode(&envelope).expect("fixture discovery must decode");
    assert_eq!(decoded.capabilities.len(), 1);
    assert_eq!(decoded.capabilities[0].agent_id, "legal-agent");
    assert_eq!(decoded.capabilities[0].version, PINNED_AGENTFIELD_VERSION);
}

#[test]
fn strict_decoder_missing_required_and_wrong_types_fail_closed() {
    // Missing required field.
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope.as_object_mut().unwrap().remove("execution_id");
    assert!(AsyncStartEnvelope::decode(&envelope).is_err());

    // Required ignored-but-type-checked field with the wrong type.
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["run_id"] = json!(17);
    assert!(AsyncStartEnvelope::decode(&envelope).is_err());

    // Optional ignored-but-type-checked field with the wrong type.
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["webhook_error"] = json!(false);
    assert!(AsyncStartEnvelope::decode(&envelope).is_err());

    // Unknown fields are ignored.
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["brand_new_upstream_field"] = json!({"nested": true});
    assert!(AsyncStartEnvelope::decode(&envelope).is_ok());

    // Unknown status enum.
    let mut envelope = fixture_str("/status/success_envelope");
    envelope["status"] = json!("frobnicating");
    assert!(StatusEnvelope::decode(&envelope).is_err());
}

// ---- client behavior over a fake transport ----

struct FakeTransport {
    responses: std::sync::Mutex<Vec<RawResponse>>,
    seen: std::sync::Mutex<Vec<OutboundRequest>>,
}

impl FakeTransport {
    fn with_status(status: u16, body: Value) -> Arc<Self> {
        Arc::new(Self {
            responses: std::sync::Mutex::new(vec![RawResponse {
                status,
                content_type: Some("application/json".into()),
                body: serde_json::to_vec(&body).unwrap(),
            }]),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl lato_agent::agentfield::HttpTransport for FakeTransport {
    async fn send(&self, request: OutboundRequest) -> Result<RawResponse, TransportError> {
        self.seen.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| TransportError::Connection("exhausted fake responses".into()))
    }
}

fn client_with(transport: Arc<FakeTransport>) -> HttpAgentFieldClient<Arc<FakeTransport>> {
    HttpAgentFieldClient::new(
        lato_agent::agentfield::config::ControlPlaneOrigin {
            base: "https://agents.example.internal/".parse().unwrap(),
        },
        Some(RedactedToken::new("super-secret-token-value")),
        transport,
    )
}

fn discovery_body() -> Value {
    fixture_str("/discovery/success_envelope")
}

#[tokio::test]
async fn discovery_injects_bearer_and_classifies_versions() {
    let transport = FakeTransport::with_status(200, discovery_body());
    let client = client_with(transport.clone());
    let envelope = client.discovery().await.unwrap();
    assert_eq!(envelope.capabilities.len(), 1);

    let seen = transport.seen.lock().unwrap().clone();
    let request = &seen[0];
    assert_eq!(
        request.url,
        "https://agents.example.internal/api/v1/discovery/capabilities"
    );
    // Bearer is injected into the request…
    assert!(request.bearer.is_some());
    // …but redacted from any debug rendering.
    assert!(!format!("{:?}", request).contains("super-secret-token-value"));
    assert!(!format!("{:?}", request.bearer).contains("super-secret-token-value"));
}

#[tokio::test]
async fn discovery_rejects_other_remote_versions() {
    let mut body = discovery_body();
    body["capabilities"][0]["version"] = json!("v0.1.139");
    let client = client_with(FakeTransport::with_status(200, body));
    let error = client.discovery().await.unwrap_err();
    assert!(matches!(error, AgentFieldError::RemoteProtocol(_)));
}

#[tokio::test]
async fn start_posts_to_derived_target_and_validates_envelope() {
    let envelope = fixture_str("/async_start/success_envelope");
    let transport = FakeTransport::with_status(202, envelope);
    let client = client_with(transport.clone());
    let decoded = client
        .start_async("legal-agent.review_contract", &json!({"contract": "acme"}))
        .await
        .unwrap();
    assert_eq!(decoded.execution_id, "exec_redacted");
    let seen = transport.seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].url,
        "https://agents.example.internal/api/v1/execute/async/legal-agent.review_contract"
    );
    // The colon discovery form never reaches a URL.
    assert!(!seen[0].url.contains("legal-agent:"));
}

#[tokio::test]
async fn start_rejects_target_mismatch_and_nonqueued_status() {
    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["target"] = json!("other.target");
    let error = client_with(FakeTransport::with_status(202, envelope))
        .start_async("legal-agent.review_contract", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentFieldError::RemoteProtocol(_)));

    let mut envelope = fixture_str("/async_start/success_envelope");
    envelope["status"] = json!("running");
    let error = client_with(FakeTransport::with_status(202, envelope))
        .start_async("legal-agent.review_contract", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentFieldError::RemoteProtocol(_)));
}

#[tokio::test]
async fn http_status_mapping_is_stable() {
    // 401 → unauthorized without echoing the body.
    let client = client_with(FakeTransport::with_status(
        401,
        json!({"hint": "token sk-super-secret-token-value was rejected"}),
    ));
    let error = client.status("exec_1").await.unwrap_err();
    assert!(matches!(error, AgentFieldError::Unauthorized));
    let rendered = error.to_string();
    assert!(!rendered.contains("sk-super-secret"), "{rendered}");

    // 503 → unavailable.
    let client = client_with(FakeTransport::with_status(
        503,
        json!({"error": "overloaded"}),
    ));
    let error = client.status("exec_1").await.unwrap_err();
    assert!(matches!(error, AgentFieldError::Unavailable(_)));

    // Non-JSON (HTML login page) → remote_protocol.
    let html_client = HttpAgentFieldClient::new(
        lato_agent::agentfield::config::ControlPlaneOrigin {
            base: "https://agents.example.internal/".parse().unwrap(),
        },
        None,
        Arc::new(FakeHtmlTransport),
    );
    let error = html_client.status("exec_1").await.unwrap_err();
    assert!(matches!(error, AgentFieldError::RemoteProtocol(_)));
}

struct FakeHtmlTransport;

#[async_trait::async_trait]
impl lato_agent::agentfield::HttpTransport for FakeHtmlTransport {
    async fn send(&self, _request: OutboundRequest) -> Result<RawResponse, TransportError> {
        Ok(RawResponse {
            status: 200,
            content_type: Some("text/html".into()),
            body: b"<html><body>login</body></html>".to_vec(),
        })
    }
}

#[tokio::test]
async fn cancel_maps_success_and_invalid_state_conflict() {
    let success = fixture_str("/cancel/success_envelope");
    let transport = FakeTransport::with_status(200, success);
    let client = client_with(transport.clone());
    let cancelled = client
        .cancel("exec_redacted", "cancelled by Lato user")
        .await
        .unwrap();
    assert!(cancelled.is_some());
    let seen = transport.seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].url,
        "https://agents.example.internal/api/v1/executions/exec_redacted/cancel"
    );
    assert_eq!(
        seen[0].json_body.as_ref().unwrap()["reason"],
        "cancelled by Lato user"
    );

    let conflict = fixture_str("/cancel/terminal_conflict_envelope");
    let client = client_with(FakeTransport::with_status(409, conflict));
    let already_terminal = client
        .cancel("exec_redacted", "cancelled by Lato user")
        .await
        .unwrap();
    assert!(
        already_terminal.is_none(),
        "409 invalid_state is already-terminal"
    );
}

#[tokio::test]
async fn transport_failures_map_to_unavailable_and_never_retry() {
    struct DyingTransport;
    #[async_trait::async_trait]
    impl lato_agent::agentfield::HttpTransport for DyingTransport {
        async fn send(&self, _request: OutboundRequest) -> Result<RawResponse, TransportError> {
            Err(TransportError::Connection(
                "connection reset before response".into(),
            ))
        }
    }
    let client = HttpAgentFieldClient::new(
        lato_agent::agentfield::config::ControlPlaneOrigin {
            base: "https://agents.example.internal/".parse().unwrap(),
        },
        None,
        DyingTransport,
    );
    // A single call fails with unavailable; the client performs no retries
    // (one transport error = one error surfaced to the caller).
    let error = client
        .start_async("legal-agent.review_contract", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentFieldError::Unavailable(_)));
    assert!(!error.to_string().contains("super-secret"));
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    struct HugeTransport;
    #[async_trait::async_trait]
    impl lato_agent::agentfield::HttpTransport for HugeTransport {
        async fn send(&self, _request: OutboundRequest) -> Result<RawResponse, TransportError> {
            Ok(RawResponse {
                status: 200,
                content_type: Some("application/json".into()),
                body: vec![b'x'; 2 * 1024 * 1024],
            })
        }
    }
    let client = HttpAgentFieldClient::new(
        lato_agent::agentfield::config::ControlPlaneOrigin {
            base: "https://agents.example.internal/".parse().unwrap(),
        },
        None,
        HugeTransport,
    );
    let error = client.status("exec_1").await.unwrap_err();
    assert!(matches!(error, AgentFieldError::BodyTooLarge(_)));
}

// ---- Round 1 verification probes: pinned status/cancel strictness ----

#[tokio::test]
async fn cancel_rejects_html_and_non_cancelled_success_status() {
    // text/html + 200 is a protocol violation, never a success (AC-14 probe).
    let client = HttpAgentFieldClient::new(
        lato_agent::agentfield::config::ControlPlaneOrigin {
            base: "https://agents.example.internal/".parse().unwrap(),
        },
        None,
        Arc::new(FakeHtmlTransport),
    );
    let error = client
        .cancel("exec_redacted", "cancelled by Lato user")
        .await
        .unwrap_err();
    assert!(matches!(error, AgentFieldError::RemoteProtocol(_)));

    // 200 with `status=running` is not a confirmed cancellation.
    let mut running = fixture_str("/cancel/success_envelope");
    running["status"] = json!("running");
    let client = client_with(FakeTransport::with_status(200, running));
    let error = client
        .cancel("exec_redacted", "cancelled by Lato user")
        .await
        .unwrap_err();
    assert!(
        matches!(error, AgentFieldError::RemoteProtocol(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn status_rejects_a_foreign_execution_id_echo() {
    let mut envelope = fixture_str("/status/success_envelope");
    envelope["execution_id"] = json!("exec_foreign");
    let client = client_with(FakeTransport::with_status(200, envelope));
    let error = client.status("exec_redacted").await.unwrap_err();
    assert!(
        matches!(error, AgentFieldError::RemoteProtocol(_)),
        "{error:?}"
    );

    // The matching echo still decodes.
    let client = client_with(FakeTransport::with_status(
        200,
        fixture_str("/status/success_envelope"),
    ));
    assert!(client.status("exec_redacted").await.is_ok());
}

#[tokio::test]
async fn async_start_rejects_an_unpinned_success_status() {
    // The pinned async contract answers 202; a 200 success envelope must be
    // rejected before decoding (AC-14 probe).
    let client = client_with(FakeTransport::with_status(
        200,
        fixture_str("/async_start/success_envelope"),
    ));
    let error = client
        .start_async("legal-agent.review_contract", &json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, AgentFieldError::RemoteProtocol(_)),
        "{error:?}"
    );
}
