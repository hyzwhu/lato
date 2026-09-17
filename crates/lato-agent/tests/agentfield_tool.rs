//! Phase 7C2 integration tests: the `agentfield` model tool end-to-end
//! against a fake control-plane client — registration gating, the Ask-mode
//! policy membrane with grant consumption, TOCTOU revision recheck, the
//! atomic 4-active concurrency cap with REAL concurrent starts,
//! exactly-one-send semantics with permanent `outcome_unknown`, ownership
//! isolation, and post-close behavior.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use lato_agent::agentfield::config::AgentFieldConfig;
use lato_agent::agentfield::types::{
    AsyncStartEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope, StatusEnvelope,
};
use lato_agent::agentfield::{
    AgentFieldCatalog, AgentFieldClient, AgentFieldError, AgentFieldManager, AgentFieldTool,
    CancelOutcome, RunStatus, SessionAgentFieldHandle, load_agentfield_config,
};
use lato_core::{PolicyDecision, PolicyMode, Retryability, SandboxProfile, Tool, ToolOutput};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---- shared fixtures -------------------------------------------------------

fn test_config_value() -> Value {
    json!({
        "enabled": true,
        "baseUrl": "https://agents.example.internal",
        "credential": "agentfield:primary",
        "capabilities": {
            "contract-review": {
                "target": "legal.review_contract",
                "description": "Review one contract",
                "inputSchema": {
                    "type": "object",
                    "properties": {"contract": {"type": "string"}},
                    "required": ["contract"],
                    "additionalProperties": false
                },
                "risk": "remote_read",
            }
        }
    })
}

fn catalog() -> AgentFieldCatalog {
    let config = AgentFieldConfig::parse(&test_config_value())
        .unwrap()
        .unwrap();
    AgentFieldCatalog::from_config(&config)
}

#[derive(Clone)]
enum StartBehavior {
    Ok(String),
    #[allow(dead_code)]
    EmptyId,
    Unauthorized,
    RemoteDenied,
    TransportFailure,
    DelayThenOk(String, u64),
}

#[derive(Clone)]
enum StatusBehavior {
    Ok(String),
    RemoteProtocol,
    Unavailable,
}

#[derive(Clone)]
enum CancelBehavior {
    Confirmed,
    #[allow(dead_code)]
    Conflict,
    Unavailable,
}

struct FakeClient {
    discovery_calls: AtomicUsize,
    start_calls: AtomicUsize,
    status_calls: AtomicUsize,
    cancel_calls: AtomicUsize,
    start_behavior: tokio::sync::Mutex<StartBehavior>,
    status_behavior: tokio::sync::Mutex<StatusBehavior>,
    cancel_behavior: tokio::sync::Mutex<CancelBehavior>,
}

impl FakeClient {
    fn new(start: StartBehavior) -> Self {
        Self {
            discovery_calls: AtomicUsize::new(0),
            start_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            cancel_calls: AtomicUsize::new(0),
            start_behavior: tokio::sync::Mutex::new(start),
            status_behavior: tokio::sync::Mutex::new(StatusBehavior::Ok("running".into())),
            cancel_behavior: tokio::sync::Mutex::new(CancelBehavior::Confirmed),
        }
    }
}

fn start_envelope(execution_id: &str) -> AsyncStartEnvelope {
    AsyncStartEnvelope::decode(&json!({
        "execution_id": execution_id,
        "status": "queued",
        "target": "legal.review_contract",
        "type": "reasoner",
        "run_id": "run-1",
        "workflow_id": "run-1",
        "created_at": "2026-09-17T00:00:00Z",
        "enqueued_at": "2026-09-17T00:00:00Z",
        "webhook_registered": false,
    }))
    .unwrap()
}

fn status_envelope(execution_id: &str, status: &str) -> StatusEnvelope {
    StatusEnvelope::decode(&json!({
        "execution_id": execution_id,
        "status": status,
        "run_id": "run-1",
        "started_at": "2026-09-17T00:00:01Z",
        "webhook_registered": false,
        "result": "findings: none",
    }))
    .unwrap()
}

#[async_trait]
impl AgentFieldClient for FakeClient {
    async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
        self.discovery_calls.fetch_add(1, Ordering::SeqCst);
        DiscoveryEnvelope::decode(&json!({
            "discovered_at": "2026-09-17T00:00:00Z",
            "total_agents": 1,
            "total_reasoners": 1,
            "total_skills": 0,
            "pagination": {"limit": 100, "offset": 0, "has_more": false},
            "capabilities": [{
                "agent_id": "legal",
                "group_id": "",
                "base_url": "https://agentfield.invalid",
                "version": "v0.1.138",
                "health_status": "healthy",
                "deployment_type": "service",
                "last_heartbeat": "2026-09-17T00:00:00Z",
                "reasoners": [{
                    "id": "review_contract",
                    "invocation_target": "legal:review_contract"
                }],
                "skills": []
            }]
        }))
        .map_err(AgentFieldError::RemoteProtocol)
    }

    async fn start_async(
        &self,
        _execute_target: &str,
        _input: &Value,
    ) -> Result<AsyncStartEnvelope, AgentFieldError> {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.start_behavior.lock().await.clone();
        match behavior {
            StartBehavior::Ok(id) => Ok(start_envelope(&id)),
            StartBehavior::DelayThenOk(id, ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(start_envelope(&id))
            }
            StartBehavior::EmptyId => Ok(start_envelope("")),
            StartBehavior::Unauthorized => Err(AgentFieldError::Unauthorized),
            StartBehavior::RemoteDenied => Err(AgentFieldError::RemoteDenied),
            StartBehavior::TransportFailure => {
                Err(AgentFieldError::Unavailable("connection reset".into()))
            }
        }
    }

    async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.status_behavior.lock().await.clone();
        match behavior {
            StatusBehavior::Ok(status) => Ok(status_envelope(execution_id, &status)),
            StatusBehavior::RemoteProtocol => Err(AgentFieldError::RemoteProtocol(
                "unknown status enum `devolved`".into(),
            )),
            StatusBehavior::Unavailable => {
                Err(AgentFieldError::Unavailable("control plane down".into()))
            }
        }
    }

    async fn cancel(
        &self,
        _execution_id: &str,
        _reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.cancel_behavior.lock().await.clone();
        match behavior {
            CancelBehavior::Confirmed => Ok(Some(
                CancelSuccessEnvelope::decode(&json!({
                    "execution_id": "exec-1",
                    "status": "cancelled",
                    "previous_status": "running",
                    "cancelled_at": "2026-09-17T00:00:02Z",
                }))
                .unwrap(),
            )),
            CancelBehavior::Conflict => Ok(None),
            CancelBehavior::Unavailable => Err(AgentFieldError::Unavailable("timeout".into())),
        }
    }
}

type SharedFake = Arc<FakeClient>;

fn manager_with_fake(start: StartBehavior) -> (Arc<AgentFieldManager>, SharedFake) {
    let fake: SharedFake = Arc::new(FakeClient::new(start));
    let shared: Arc<dyn AgentFieldClient> = fake.clone();
    let manager = AgentFieldManager::with_factory(
        "session-int",
        catalog(),
        Some(Arc::new(move || {
            let shared = shared.clone();
            Box::pin(async move { Ok(shared.clone()) })
        })),
    );
    (Arc::new(manager), fake)
}

/// Manager backed by REAL user/project config files under `home`/`cwd`.
fn manager_with_sources(
    home: &std::path::Path,
    cwd: &std::path::Path,
    start: StartBehavior,
) -> (Arc<AgentFieldManager>, SharedFake) {
    let fake: SharedFake = Arc::new(FakeClient::new(start));
    let shared: Arc<dyn AgentFieldClient> = fake.clone();
    let sources = lato_agent::agentfield::catalog::catalog_sources(home, cwd);
    let config = lato_agent::agentfield::catalog::assemble_catalog_config(&sources)
        .expect("sources assemble");
    let catalog = AgentFieldCatalog::from_config(&config);
    let manager = AgentFieldManager::with_factory_and_sources(
        "session-src",
        catalog,
        Some(Arc::new(move || {
            let shared = shared.clone();
            Box::pin(async move { Ok(shared.clone()) })
        })),
        sources,
    );
    (Arc::new(manager), fake)
}

fn agentfield_runtime(
    root: &std::path::Path,
    manager: Arc<AgentFieldManager>,
) -> (lato_tools::ToolRuntime, SessionAgentFieldHandle) {
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let scope = PolicyScope {
        workspace_root: root.to_path_buf(),
        mode: PolicyMode::Ask,
        project_trusted: true,
        sandbox_profile: SandboxProfile::Workspace,
    };
    let mut builder = ToolRuntimeBuilder::new(policy, scope);
    builder.set_argument_error_code(
        lato_core::ToolName::parse("builtin:agentfield").unwrap(),
        "agentfield.invalid_arguments",
    );
    let handle = SessionAgentFieldHandle::new();
    handle.install(manager);
    builder
        .register(Arc::new(AgentFieldTool::new(handle.clone())))
        .unwrap();
    (builder.build().unwrap(), handle)
}

fn output_json(output: &ToolOutput) -> Value {
    serde_json::from_str(&output.content).unwrap()
}

async fn approved_start(
    runtime: &lato_tools::ToolRuntime,
    revision: &str,
    call_id: &str,
) -> Result<ToolOutput, lato_core::ToolError> {
    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from(call_id),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let prepared = runtime
        .prepare_scoped(
            context,
            "agentfield",
            json!({
                "action":"start",
                "name":"contract-review",
                "revision":revision,
                "input":{"contract":"acme.pdf"}
            }),
            None,
        )
        .expect("prepare succeeds");
    let grant = match runtime.decision(&prepared) {
        PolicyDecision::RequireApproval(request) => runtime.approve(request).unwrap(),
        PolicyDecision::Allow(grant) => grant.clone(),
        PolicyDecision::Deny(denial) => panic!("unexpected denial: {denial:?}"),
    };
    runtime.execute_authorized(prepared, grant).await
}

// ---- tests -----------------------------------------------------------------

/// AC-01: unconfigured / disabled / invalid config ⇒ `None` (zero tool
/// registration, zero network); a valid enabled config loads.
#[test]
fn config_gate_is_closed_for_unconfigured_disabled_and_invalid() {
    let temp = TempDir::new().unwrap();
    let home = temp.path();

    // Missing file.
    assert!(load_agentfield_config(home).is_none());
    // Missing stanza.
    std::fs::write(
        home.join("config.json"),
        json!({"model": {"provider": "x"}}).to_string(),
    )
    .unwrap();
    assert!(load_agentfield_config(home).is_none());
    // Disabled.
    let mut disabled = test_config_value();
    disabled["enabled"] = json!(false);
    std::fs::write(
        home.join("config.json"),
        json!({"agentfield": disabled}).to_string(),
    )
    .unwrap();
    assert!(load_agentfield_config(home).is_none());
    // Invalid (bad alias).
    let mut invalid = test_config_value();
    invalid["capabilities"]["Bad..Alias"] = invalid["capabilities"]["contract-review"].clone();
    invalid["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("contract-review");
    std::fs::write(
        home.join("config.json"),
        json!({"agentfield": invalid}).to_string(),
    )
    .unwrap();
    assert!(load_agentfield_config(home).is_none());
    // Valid.
    std::fs::write(
        home.join("config.json"),
        json!({"agentfield": test_config_value()}).to_string(),
    )
    .unwrap();
    let config = load_agentfield_config(home).expect("valid config loads");
    assert!(config.enabled);
    assert_eq!(config.capabilities.len(), 1);
}

/// The model sees exactly one `agentfield` tool whose four actions share
/// the static external-mutation descriptor (spec §8.1, AC-02 shape).
#[test]
fn runtime_exposes_one_agentfield_tool_with_frozen_descriptor() {
    let temp = TempDir::new().unwrap();
    let (manager, _fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager);
    let definitions = runtime.model_definitions_scoped(None);
    let agentfield: Vec<&Value> = definitions
        .iter()
        .filter(|definition| definition["function"]["name"] == "agentfield")
        .collect();
    assert_eq!(agentfield.len(), 1, "exactly one agentfield tool");
    let descriptor =
        lato_agent::agentfield::AgentFieldTool::new(SessionAgentFieldHandle::new()).descriptor();
    assert_eq!(
        descriptor.side_effect,
        lato_core::SideEffect::ExternalMutation
    );
    assert!(
        descriptor
            .capabilities
            .contains(&lato_core::ToolCapability::NetworkWrite)
    );
    assert_eq!(
        descriptor.idempotency,
        lato_core::ToolIdempotency::NonIdempotent
    );
}

/// Spec §8.1 test 6 + issue test 4: Ask mode requires approval; the grant
/// binds the full arguments; an explicit deny produces zero manager runs,
/// zero remote requests, and `policy.approval_denied`.
#[tokio::test]
async fn denied_approval_has_zero_manager_and_network_side_effects() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();

    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-deny"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let prepared = runtime
        .prepare_scoped(
            context,
            "agentfield",
            json!({"action":"start","name":"contract-review","revision":revision,"input":{"contract":"acme.pdf"}}),
            None,
        )
        .unwrap();
    match runtime.decision(&prepared) {
        PolicyDecision::RequireApproval(_request) => {
            // The user declines: approve() is never called.
        }
        other => panic!("Ask mode must require approval, got {other:?}"),
    }
    assert_eq!(manager.run_count().await, 0, "no run reserved");
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 0, "zero network");
}

/// AC-04: approval后 revision mismatch ⇒ `agentfield.catalog_changed`,
/// grant consumed, zero remote requests, zero local runs.
#[tokio::test]
async fn stale_revision_after_approval_consumes_the_grant_with_zero_remote_requests() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let stale = format!("sha256:{}", "a".repeat(64));

    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-toctou"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let prepared = runtime
        .prepare_scoped(
            context,
            "agentfield",
            json!({"action":"start","name":"contract-review","revision":stale,"input":{"contract":"acme.pdf"}}),
            None,
        )
        .unwrap();
    let grant = match runtime.decision(&prepared) {
        PolicyDecision::RequireApproval(request) => runtime.approve(request).unwrap(),
        other => panic!("unexpected decision {other:?}"),
    };
    let error = runtime
        .execute_authorized(prepared, grant.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "agentfield.catalog_changed");
    assert_eq!(manager.run_count().await, 0, "no reservation");
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 0, "zero network");

    // The grant is consumed and can never be reused.
    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-reuse"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let second = runtime
        .prepare_scoped(context, "agentfield", json!({"action":"status"}), None)
        .unwrap();
    let reuse = runtime.execute_authorized(second, grant).await;
    assert!(reuse.is_err(), "a consumed grant must not be reusable");
}

/// Issue test 5: the grant is one-shot; a second use of the same grant
/// (even for an identical request) fails closed.
#[tokio::test]
async fn successful_start_consumes_the_grant_exactly_once() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();

    let output = approved_start(&runtime, &revision, "call-ok")
        .await
        .unwrap();
    let started = output_json(&output);
    assert_eq!(started["run"]["status"], "queued");
    assert_eq!(started["run"]["executionId"], "exec-1");
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.run_count().await, 1);
}

/// Issue test 7 (AC-06): REAL concurrent starts contend for the 4-active
/// cap atomically; the losers get `agentfield.limit_exceeded` and make
/// zero remote requests.
#[tokio::test]
async fn concurrent_starts_are_atomically_capped_at_four() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::DelayThenOk("exec-1".into(), 80));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let runtime = Arc::new(runtime);
    let revision = manager.catalog().revision().to_owned();

    let mut tasks = Vec::new();
    for seq in 0..10 {
        let runtime = runtime.clone();
        let revision = revision.clone();
        tasks.push(tokio::spawn(async move {
            approved_start(&runtime, &revision, &format!("call-conc-{seq}")).await
        }));
    }
    let mut ok = 0usize;
    let mut limited = 0usize;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => ok += 1,
            Err(error) => {
                assert_eq!(error.code, "agentfield.limit_exceeded", "{error:?}");
                assert_eq!(error.retryability, Retryability::AfterBackoff);
                limited += 1;
            }
        }
    }
    assert_eq!(ok, 4, "exactly 4 admitted");
    assert_eq!(limited, 6);
    assert_eq!(
        fake.start_calls.load(Ordering::SeqCst),
        4,
        "losers never send"
    );
}

/// AC-05: exactly one send; an uncertain outcome without an execution ID is
/// a permanent `outcome_unknown` — no auto retry, no auto query.
#[tokio::test]
async fn uncertain_send_is_exactly_once_and_permanently_outcome_unknown() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::TransportFailure);
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();

    let error = approved_start(&runtime, &revision, "call-unknown")
        .await
        .expect_err("uncertain send surfaces the stable error");
    assert_eq!(error.code, "agentfield.outcome_unknown");
    assert_eq!(error.retryability, Retryability::Never);
    assert!(error.message.contains("control plane"), "{error:?}");
    assert_eq!(
        fake.start_calls.load(Ordering::SeqCst),
        1,
        "exactly one send"
    );

    // A follow-up status NEVER queries the remote for this run.
    let run = manager.run_status(None).await.unwrap();
    assert_eq!(run[0].status, RunStatus::OutcomeUnknown);
    assert_eq!(fake.status_calls.load(Ordering::SeqCst), 0);
}

/// Definitive remote rejections keep the run as `failed` and surface the
/// frozen code (401/403 → `agentfield.unauthorized`).
#[tokio::test]
async fn definitive_rejections_surface_frozen_codes() {
    for (behavior, expected) in [
        (StartBehavior::Unauthorized, "agentfield.unauthorized"),
        (StartBehavior::RemoteDenied, "agentfield.remote_denied"),
    ] {
        let temp = TempDir::new().unwrap();
        let (manager, fake) = manager_with_fake(behavior);
        let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
        let revision = manager.catalog().revision().to_owned();
        let error = approved_start(&runtime, &revision, "call-denied")
            .await
            .expect_err("rejection surfaces");
        assert_eq!(error.code, expected);
        assert_eq!(manager.run_count().await, 1, "failed run retained");
        assert_eq!(fake.start_calls.load(Ordering::SeqCst), 1);
    }
}

/// AC-07 (issue test 9): status/cancel only operate on runs owned by THIS
/// session's manager; unknown and foreign ids are indistinguishable.
#[tokio::test]
async fn ownership_isolation_is_indistinguishable() {
    let (manager, _fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));

    // Foreign session: a second manager owning its own run.
    let (foreign, _fake2) = manager_with_fake(StartBehavior::Ok("exec-2".into()));
    let foreign_run = foreign
        .start_run(
            "contract-review",
            "legal.review_contract",
            "sha256:x",
            &json!({}),
        )
        .await
        .unwrap();

    let tool = AgentFieldTool::new({
        let handle = SessionAgentFieldHandle::new();
        handle.install(manager.clone());
        handle
    });
    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-own"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };

    // Unknown id vs foreign id: identical observable result.
    let unknown = tool
        .invoke(
            context.clone(),
            json!({"action":"status","runId":"afrun_unknown-9"}),
        )
        .await
        .unwrap_err();
    let foreign_status = tool
        .invoke(
            context.clone(),
            json!({"action":"status","runId":foreign_run.run_id}),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown.code, "agentfield.not_found");
    assert_eq!(foreign_status.code, "agentfield.not_found");

    let unknown_cancel = tool
        .invoke(
            context.clone(),
            json!({"action":"cancel","runId":"afrun_unknown-9"}),
        )
        .await
        .unwrap_err();
    let foreign_cancel = tool
        .invoke(
            context,
            json!({"action":"cancel","runId":foreign_run.run_id}),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown_cancel.code, "agentfield.not_found");
    assert_eq!(foreign_cancel.code, "agentfield.not_found");
    assert_eq!(
        foreign_run.status,
        RunStatus::Queued,
        "foreign run untouched"
    );
}

/// Issue test 10: unknown remote enums fail closed (`agentfield.remote_protocol`)
/// preserving the last known state; cancel timeouts never report success.
#[tokio::test]
async fn unknown_remote_status_fails_closed_and_cancel_timeout_never_succeeds() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();
    approved_start(&runtime, &revision, "call-status")
        .await
        .unwrap();

    *fake.status_behavior.lock().await = StatusBehavior::RemoteProtocol;
    let error = manager
        .run_status(Some(&manager.run_status(None).await.unwrap()[0].run_id))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "agentfield.remote_protocol");

    // Cancel with a transport-level timeout → cancel_requested, NOT cancelled.
    *fake.cancel_behavior.lock().await = CancelBehavior::Unavailable;
    let runs = manager.run_status(None).await.unwrap();
    let (run, outcome) = manager.cancel_run(&runs[0].run_id, "user").await.unwrap();
    assert_eq!(outcome, CancelOutcome::CancelRequested);
    assert_ne!(run.status, RunStatus::Cancelled);
}

/// AC-09 (issue test 11): after session close, no remote cancel is sent and
/// every late call fails closed with `agentfield.unavailable`.
#[tokio::test]
async fn session_close_never_cancels_remote_and_late_calls_fail() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();
    approved_start(&runtime, &revision, "call-close")
        .await
        .unwrap();

    manager.close();
    let runs = manager.run_status(None).await;
    assert!(runs.is_err(), "late status fails closed");
    assert_eq!(
        fake.cancel_calls.load(Ordering::SeqCst),
        0,
        "no remote cancel"
    );
    let start = manager
        .start_run(
            "contract-review",
            "legal.review_contract",
            "sha256:x",
            &json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(start.code(), "agentfield.unavailable");
}

/// Issue test 12 residual: malformed JSON and oversized remote bodies stay
/// frozen at the client seam (regression through the tool surface).
#[tokio::test]
async fn remote_protocol_violations_surface_stably_through_status() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::Ok("exec-1".into()));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();
    approved_start(&runtime, &revision, "call-proto")
        .await
        .unwrap();

    *fake.status_behavior.lock().await = StatusBehavior::Unavailable;
    let run_id = manager.run_status(None).await.unwrap()[0].run_id.clone();
    let runs = manager.run_status(Some(&run_id)).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Unavailable);
    assert!(!runs[0].status.is_terminal(), "unavailable is not terminal");
    assert_eq!(runs[0].last_error, Some("agentfield.unavailable"));
}

/// Round-1 P1-3 regression: a REAL user-config change after approval makes
/// the post-approval recheck fail closed — `agentfield.catalog_changed`,
/// grant consumed, zero additional remote requests, zero new runs.
#[tokio::test]
async fn real_user_config_change_after_approval_fails_closed() {
    let temp_home = TempDir::new().unwrap();
    let temp_cwd = TempDir::new().unwrap();
    let user_config = temp_home.path().join("config.json");
    let config_body = |target: &str| {
        json!({"agentfield": {
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": target,
                    "description": "Review one contract",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"contract": {"type": "string"}},
                        "required": ["contract"],
                        "additionalProperties": false
                    },
                    "risk": "remote_read",
                }
            }
        }})
    };
    std::fs::write(
        &user_config,
        config_body("legal.review_contract").to_string(),
    )
    .unwrap();
    let (manager, fake) = manager_with_sources(
        temp_home.path(),
        temp_cwd.path(),
        StartBehavior::Ok("exec-1".into()),
    );
    let (runtime, _handle) = agentfield_runtime(temp_cwd.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();

    // Baseline start with the frozen revision succeeds (exactly one send).
    approved_start(&runtime, &revision, "call-src-ok")
        .await
        .unwrap();
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.run_count().await, 1);

    // REAL config change on disk (user source): the execution target moves.
    std::fs::write(&user_config, config_body("legal.review_other").to_string()).unwrap();

    let error = approved_start(&runtime, &revision, "call-src-changed")
        .await
        .expect_err("changed sources must fail closed");
    assert_eq!(error.code, "agentfield.catalog_changed");
    assert_eq!(
        fake.start_calls.load(Ordering::SeqCst),
        1,
        "zero additional remote requests"
    );
    assert_eq!(manager.run_count().await, 1, "no new run reserved");
}

/// Round-1 P1-3 regression: a REAL project-config contribution after
/// approval is also detected.
#[tokio::test]
async fn real_project_config_contribution_after_approval_fails_closed() {
    let temp_home = TempDir::new().unwrap();
    let temp_cwd = TempDir::new().unwrap();
    let project_dir = temp_cwd.path().join(".lato");
    let project_config = project_dir.join("config.json");
    std::fs::create_dir_all(&project_dir).unwrap();
    let config_body = |target: &str, alias: &str| {
        json!({"agentfield": {
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {
                alias: {
                    "target": target,
                    "description": "Review one contract",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"contract": {"type": "string"}},
                        "required": ["contract"],
                        "additionalProperties": false
                    },
                    "risk": "remote_read",
                }
            }
        }})
    };
    std::fs::write(
        temp_home.path().join("config.json"),
        config_body("legal.review_contract", "contract-review").to_string(),
    )
    .unwrap();
    let (manager, fake) = manager_with_sources(
        temp_home.path(),
        temp_cwd.path(),
        StartBehavior::Ok("exec-1".into()),
    );
    let (runtime, _handle) = agentfield_runtime(temp_cwd.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();

    approved_start(&runtime, &revision, "call-proj-ok")
        .await
        .unwrap();
    // A new project capability appears after approval.
    std::fs::write(
        &project_config,
        config_body("alpha.task", "alpha-task").to_string(),
    )
    .unwrap();
    let error = approved_start(&runtime, &revision, "call-proj-changed")
        .await
        .expect_err("project contribution must fail closed");
    assert_eq!(error.code, "agentfield.catalog_changed");
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 1);
}

/// Round-1 P1-1 regression through the REAL ToolRuntime: aborting the
/// execute future mid-send leaves NO stranded `queued` run — the drop guard
/// records the permanent `outcome_unknown` terminal.
#[tokio::test]
async fn runtime_abort_mid_send_marks_the_run_outcome_unknown() {
    let temp = TempDir::new().unwrap();
    let (manager, fake) = manager_with_fake(StartBehavior::DelayThenOk("exec-1".into(), 5_000));
    let (runtime, _handle) = agentfield_runtime(temp.path(), manager.clone());
    let revision = manager.catalog().revision().to_owned();
    let runtime = Arc::new(runtime);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let revision = revision.clone();
        async move { approved_start(&runtime, &revision, "call-abort").await }
    });
    while fake.start_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    let runs = manager.run_status(None).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::OutcomeUnknown);
    assert!(runs[0].execution_id.is_none());
    assert_eq!(runs[0].last_error, Some("agentfield.outcome_unknown"));
}

/// Round-1 P2 regression: `list` returns ONLY the allowlist ∩ verified
/// discovery (a locally allowlisted target the control plane never
/// published is invisible) and each entry carries the locally frozen
/// `inputSchema`.
#[tokio::test]
async fn list_intersects_allowlist_with_verified_discovery_and_includes_input_schema() {
    let temp_home = TempDir::new().unwrap();
    let temp_cwd = TempDir::new().unwrap();
    // Two allowlisted capabilities; the fake discovery below only publishes
    // `legal.review_contract`.
    std::fs::write(
        temp_home.path().join("config.json"),
        json!({"agentfield": {
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","properties":{"contract":{"type":"string"}},"required":["contract"]},
                    "risk": "remote_read",
                },
                "other-missing": {
                    "target": "other.missing",
                    "description": "Not published by discovery",
                    "inputSchema": {"type":"object"},
                    "risk": "remote_read",
                }
            }
        }}).to_string(),
    )
    .unwrap();
    let (manager, _fake) = manager_with_sources(
        temp_home.path(),
        temp_cwd.path(),
        StartBehavior::Ok("exec-1".into()),
    );
    let handle = SessionAgentFieldHandle::new();
    handle.install(manager.clone());
    let tool = AgentFieldTool::new(handle);
    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-list"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let output = tool
        .invoke(context, json!({"action":"list"}))
        .await
        .unwrap();
    let listed = output_json(&output);
    let capabilities = listed["capabilities"].as_array().unwrap();
    assert_eq!(
        capabilities.len(),
        1,
        "only the allowlist ∩ verified discovery: {listed}"
    );
    assert_eq!(capabilities[0]["name"], "contract-review");
    assert_eq!(
        capabilities[0]["inputSchema"],
        json!({"type":"object","properties":{"contract":{"type":"string"}},"required":["contract"]}),
        "the locally frozen inputSchema is projected"
    );
    assert_eq!(capabilities[0]["available"], true);
}

/// With no verified discovery at all, the intersection is empty.
#[tokio::test]
async fn list_without_verified_discovery_is_empty() {
    struct DeadDiscovery;
    #[async_trait]
    impl AgentFieldClient for DeadDiscovery {
        async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
            Err(AgentFieldError::Unavailable("control plane down".into()))
        }
        async fn start_async(
            &self,
            _execute_target: &str,
            _input: &Value,
        ) -> Result<AsyncStartEnvelope, AgentFieldError> {
            unreachable!("list never starts")
        }
        async fn status(&self, _execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
            unreachable!("list never queries status")
        }
        async fn cancel(
            &self,
            _execution_id: &str,
            _reason: &str,
        ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
            unreachable!("list never cancels")
        }
    }
    let config = lato_agent::agentfield::config::AgentFieldConfig::parse(&test_config_value())
        .unwrap()
        .unwrap();
    let catalog = AgentFieldCatalog::from_config(&config);
    let client: Arc<dyn AgentFieldClient> = Arc::new(DeadDiscovery);
    let manager = AgentFieldManager::with_factory(
        "session-dead",
        catalog,
        Some(Arc::new(move || {
            let client = client.clone();
            Box::pin(async move { Ok(client.clone()) })
        })),
    );
    let manager = Arc::new(manager);
    let handle = SessionAgentFieldHandle::new();
    handle.install(manager);
    let tool = AgentFieldTool::new(handle);
    let context = lato_core::ToolContext {
        session_id: lato_core::SessionId::from("session"),
        turn_id: lato_core::TurnId::from("turn"),
        call_id: lato_core::ToolCallId::from("call-list-dead"),
        cancellation: tokio_util::sync::CancellationToken::new(),
        execution_grant: None,
    };
    let output = tool
        .invoke(context, json!({"action":"list"}))
        .await
        .unwrap();
    let listed = output_json(&output);
    assert_eq!(listed["capabilities"].as_array().unwrap().len(), 0);
    assert!(listed["revision"].as_str().unwrap().starts_with("sha256:"));
}
