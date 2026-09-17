use async_trait::async_trait;
use lato::doctor::{
    DoctorCheck, DoctorDependencies, DoctorOptions, DoctorReport, DoctorStatus, LiveProbe,
    exit_code, render_human, run,
};
use lato_ai::{CredentialStore, CustomModel, ModelApi, ProviderModelsEntry, ProviderModelsStore};
use std::{
    ffi::OsString,
    path::Path,
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn prepend_path(dir: &Path) -> OsString {
    let mut path = dir.as_os_str().to_owned();
    path.push(if cfg!(windows) { ";" } else { ":" });
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing);
    }
    path
}

fn lato(args: &[&str], home: &Path) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_lato"))
        .args(args)
        .env("LATO_HOME", home)
        .output()
        .unwrap()
}

const SECRET: &str = "doctor-super-secret-7319";

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct CountingProbe {
    calls: AtomicUsize,
    panic_on_call: bool,
}

impl CountingProbe {
    fn panic_if_called() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LiveProbe for CountingProbe {
    async fn probe(&self) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panic_on_call {
            panic!("live probe must not be called unless options.live is true");
        }
        Ok("ok".into())
    }
}

fn write_home_fixtures(home: &std::path::Path) {
    std::fs::write(
        home.join("config.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "default_model": "xai/grok-4"
        }))
        .unwrap(),
    )
    .unwrap();
    let mut store = CredentialStore::open(home).unwrap();
    store
        .modify(|data| {
            data.insert(
                "xai".into(),
                serde_json::json!({"type":"api_key","key": SECRET}),
            );
        })
        .unwrap();
}

fn write_selected_model(home: &Path, selection: &str) {
    std::fs::write(
        home.join("config.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"default_model": selection})).unwrap(),
    )
    .unwrap();
}

fn custom_model(provider: &str, id: &str) -> CustomModel {
    CustomModel {
        provider: provider.into(),
        id: id.into(),
        api: ModelApi::OpenaiCompletions,
        base_url: "http://127.0.0.1:8080/v1".into(),
        env: "LOCAL_KEY".into(),
        context_window: None,
        model_family: None,
    }
}

async fn offline_report(home: &Path) -> DoctorReport {
    let workspace = tempfile::tempdir().unwrap();
    let deps = DoctorDependencies {
        home: home.to_path_buf(),
        workspace: workspace.path().to_path_buf(),
        live_probe: CountingProbe::panic_if_called(),
    };
    run(DoctorOptions { live: false }, &deps).await
}

fn model_check(report: &DoctorReport) -> &DoctorCheck {
    report
        .checks
        .iter()
        .find(|check| check.id == "model")
        .unwrap()
}

#[tokio::test]
async fn doctor_recognizes_models_json_selection_offline() {
    let home = tempfile::tempdir().unwrap();
    write_selected_model(home.path(), "local/qwen");
    std::fs::write(
        home.path().join("models.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "models": [custom_model("local", "qwen")]
        }))
        .unwrap(),
    )
    .unwrap();
    let report = offline_report(home.path()).await;
    assert_eq!(model_check(&report).status, DoctorStatus::Ok);
    assert!(model_check(&report).message.contains("models.json"));
}

#[tokio::test]
async fn doctor_recognizes_provider_store_selection_offline() {
    let home = tempfile::tempdir().unwrap();
    write_selected_model(home.path(), "remote/discovered");
    ProviderModelsStore::open(home.path())
        .write(
            "remote",
            ProviderModelsEntry {
                models: vec![custom_model("remote", "discovered")],
                checked_at: 1,
                last_modified: 0,
                etag: None,
            },
        )
        .unwrap();
    let report = offline_report(home.path()).await;
    assert_eq!(model_check(&report).status, DoctorStatus::Ok);
    assert!(model_check(&report).message.contains("models-store.json"));
}

#[tokio::test]
async fn doctor_recognizes_compatibility_cache_selection_offline() {
    let home = tempfile::tempdir().unwrap();
    write_selected_model(home.path(), "compat/cached");
    std::fs::write(
        home.path().join("model-cache.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "models": [custom_model("compat", "cached")]
        }))
        .unwrap(),
    )
    .unwrap();
    let report = offline_report(home.path()).await;
    assert_eq!(model_check(&report).status, DoctorStatus::Ok);
    assert!(model_check(&report).message.contains("model-cache.json"));
}

#[tokio::test]
async fn doctor_reports_malformed_models_json_separately() {
    let home = tempfile::tempdir().unwrap();
    write_selected_model(home.path(), "local/qwen");
    std::fs::write(home.path().join("models.json"), b"{broken").unwrap();
    let report = offline_report(home.path()).await;
    assert_eq!(model_check(&report).status, DoctorStatus::Error);
    assert_eq!(
        model_check(&report).code.as_deref(),
        Some("doctor.check_failed")
    );
    assert!(model_check(&report).message.contains("models.json"));
    assert!(!model_check(&report).message.contains("unknown model"));
}

#[tokio::test]
async fn default_doctor_is_offline_and_redacts_seeded_secrets() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    write_home_fixtures(home.path());
    let _secret = EnvGuard::set("LATO_TEST_SECRET", SECRET);
    let _lato_home = EnvGuard::set("LATO_HOME", home.path().to_str().unwrap());

    let probe = CountingProbe::panic_if_called();
    let deps = DoctorDependencies {
        home: home.path().to_path_buf(),
        workspace: workspace.path().to_path_buf(),
        live_probe: probe.clone(),
    };
    let report = run(DoctorOptions { live: false }, &deps).await;
    let human = render_human(&report);
    let json = serde_json::to_string(&report).unwrap();

    assert!(!human.contains("doctor-super-secret-7319"));
    assert!(!json.contains("doctor-super-secret-7319"));
    assert_eq!(probe.calls(), 0);
    assert_eq!(report.schema_version, 1);
    assert!(report.checks.iter().any(|check| check.id == "tool_catalog"));
    assert!(matches!(
        report.status,
        DoctorStatus::Ok | DoctorStatus::Warn | DoctorStatus::Error
    ));
}

#[test]
fn exit_code_keeps_warnings_at_zero_unless_strict() {
    let warn = DoctorReport {
        schema_version: 1,
        status: DoctorStatus::Warn,
        checks: vec![DoctorCheck {
            id: "sandbox".into(),
            status: DoctorStatus::Warn,
            message: "workspace sandbox wrapper unavailable".into(),
            code: None,
        }],
    };
    assert_eq!(exit_code(&warn, false), 0);
    assert_eq!(exit_code(&warn, true), 1);

    let error = DoctorReport {
        schema_version: 1,
        status: DoctorStatus::Error,
        checks: vec![DoctorCheck {
            id: "policy".into(),
            status: DoctorStatus::Error,
            message: "policy self-test failed".into(),
            code: Some("doctor.check_failed".into()),
        }],
    };
    assert_eq!(exit_code(&error, false), 1);
    assert_eq!(exit_code(&error, true), 1);
}

#[test]
fn doctor_json_reports_schema_version_and_tool_catalog() {
    let home = tempfile::tempdir().unwrap();
    let output = lato(&["doctor", "--json"], home.path());
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "tool_catalog")
    );
}

#[test]
fn doctor_unknown_flags_return_two() {
    let home = tempfile::tempdir().unwrap();
    let unknown = lato(&["doctor", "--unknown"], home.path());
    assert_eq!(unknown.status.code(), Some(2));
    let duplicate = lato(&["doctor", "--json", "--json"], home.path());
    assert_eq!(duplicate.status.code(), Some(2));
}

#[test]
fn doctor_warnings_are_zero_unless_strict() {
    let home = tempfile::tempdir().unwrap();
    let ordinary = lato(&["doctor"], home.path());
    assert_eq!(
        ordinary.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
    let strict = lato(&["doctor", "--strict"], home.path());
    assert_eq!(
        strict.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&strict.stderr)
    );
}

#[test]
fn help_lists_all_four_doctor_forms() {
    let home = tempfile::tempdir().unwrap();
    let output = lato(&["--help"], home.path());
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("lato doctor [--json] [--strict] [--live]"),
        "help={help}"
    );
    assert!(help.contains("lato doctor"));
    assert!(help.contains("--json"));
    assert!(help.contains("--strict"));
    assert!(help.contains("--live"));
}

fn doctor_binary_path_check(home: &Path, path: OsString) -> serde_json::Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lato"))
        .args(["doctor", "--json"])
        .env("LATO_HOME", home)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "binary_path")
        .cloned()
        .expect("binary_path check")
}

#[cfg(unix)]
#[test]
fn doctor_warns_when_path_lato_shadows_this_binary() {
    let home = tempfile::tempdir().unwrap();
    let decoy_dir = tempfile::tempdir().unwrap();
    let decoy = decoy_dir.path().join("lato");
    std::fs::write(&decoy, b"#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755)).unwrap();

    let check = doctor_binary_path_check(home.path(), prepend_path(decoy_dir.path()));
    assert_eq!(check["status"], "warn");
    assert_eq!(check["code"], "doctor.path_shadow");
    let message = check["message"].as_str().unwrap();
    assert!(message.contains("shadow"), "message={message}");
    assert!(
        message.contains(&decoy.display().to_string()),
        "message={message}"
    );
}

#[test]
fn doctor_path_check_is_ok_when_path_matches_this_binary() {
    let home = tempfile::tempdir().unwrap();
    let bin = Path::new(env!("CARGO_BIN_EXE_lato"));
    let dir = bin.parent().expect("lato binary parent");
    let check = doctor_binary_path_check(home.path(), prepend_path(dir));
    assert_eq!(check["status"], "ok", "check={check}");
    assert!(check["code"].is_null());
}

fn agentfield_check(report: &DoctorReport) -> &DoctorCheck {
    report
        .checks
        .iter()
        .find(|check| check.id == "agentfield")
        .expect("agentfield check must always be present")
}

fn write_agentfield_config(home: &Path, agentfield: serde_json::Value) {
    std::fs::write(
        home.join("config.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({ "default_model": "xai/grok-4", "agentfield": agentfield }),
        )
        .unwrap(),
    )
    .unwrap();
}

fn valid_agentfield_config(credential: &str) -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "baseUrl": "https://agents.example.internal",
        "credential": credential,
        "capabilities": {
            "contract-review": {
                "target": "legal-agent.review_contract",
                "description": "Review one contract",
                "inputSchema": {"type": "object"},
                "risk": "remote_read"
            }
        }
    })
}

// ---- Phase 7C1.1: doctor --live opt-in AgentField probe ----

struct NoopProbe;

#[async_trait]
impl LiveProbe for NoopProbe {
    async fn probe(&self) -> Result<String, String> {
        Ok("catalog live probe ok".into())
    }
}

async fn live_report(home: &Path, probe: Arc<dyn LiveProbe>) -> DoctorReport {
    let workspace = tempfile::tempdir().unwrap();
    let deps = DoctorDependencies {
        home: home.to_path_buf(),
        workspace: workspace.path().to_path_buf(),
        live_probe: probe,
    };
    run(DoctorOptions { live: true }, &deps).await
}

async fn non_live_report(home: &Path) -> DoctorReport {
    let workspace = tempfile::tempdir().unwrap();
    let deps = DoctorDependencies {
        home: home.to_path_buf(),
        workspace: workspace.path().to_path_buf(),
        live_probe: Arc::new(NoopProbe),
    };
    run(DoctorOptions { live: false }, &deps).await
}

/// Minimal canned control plane: serves one JSON response per connection.
async fn spawn_agentfield_server(status: &str, content_type: &str, body: &str) -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let response = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        let mut queue = std::iter::once(response.into_bytes())
            .collect::<Vec<_>>()
            .into_iter();
        while let Ok((mut sock, _)) = listener.accept().await {
            // Drain request head, then answer.
            let mut buf = [0u8; 2048];
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf)).await;
            let Some(response) = queue.next() else { break };
            let _ = sock.write_all(&response).await;
            let _ = sock.shutdown().await;
        }
    });
    port
}

fn discovery_envelope() -> String {
    serde_json::json!({
        "discovered_at": "2026-09-16T00:00:00Z",
        "total_agents": 1,
        "total_reasoners": 1,
        "total_skills": 0,
        "pagination": {"limit": 100, "offset": 0, "has_more": false},
        "capabilities": [{
            "agent_id": "legal-agent",
            "group_id": "",
            "base_url": "https://agentfield.invalid",
            "version": "v0.1.138",
            "health_status": "healthy",
            "deployment_type": "service",
            "last_heartbeat": "2026-09-16T00:00:00Z",
            "reasoners": [{
                "id": "review_contract",
                "invocation_target": "legal-agent:review_contract"
            }],
            "skills": []
        }]
    })
    .to_string()
}

fn write_live_server_config(home: &Path, port: u16) {
    write_agentfield_config(
        home,
        serde_json::json!({
            "enabled": true,
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "allowLoopbackHttp": true,
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": "legal-agent.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type": "object"},
                    "risk": "remote_read"
                }
            }
        }),
    );
}

#[tokio::test]
async fn agentfield_live_disabled_and_unconfigured_stay_zero_network() {
    // Unconfigured: no agentfield stanza at all.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"default_model": "xai/grok-4"})).unwrap(),
    )
    .unwrap();
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Ok);
    assert!(check.message.contains("not configured"), "{check:?}");

    // Disabled stanza: parsed but disabled, zero network, zero credential
    // resolution.
    let home = tempfile::tempdir().unwrap();
    write_agentfield_config(
        home.path(),
        serde_json::json!({
            "enabled": false,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
        }),
    );
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Ok);
    assert!(check.message.contains("disabled"), "{check:?}");

    // Enabled but credential unresolvable: zero network, reference-only
    // message.
    let home = tempfile::tempdir().unwrap();
    write_agentfield_config(home.path(), valid_agentfield_config("agentfield:primary"));
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Warn);
    assert_eq!(check.code.as_deref(), Some("agentfield.unconfigured"));
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(
        !rendered.contains("https://agents.example.internal"),
        "{rendered}"
    );
}

#[tokio::test]
async fn agentfield_live_success_reports_discovery_snapshot() {
    let port = spawn_agentfield_server("200 OK", "application/json", &discovery_envelope()).await;
    let home = tempfile::tempdir().unwrap();
    write_live_server_config(home.path(), port);
    let mut store = CredentialStore::open(home.path()).unwrap();
    store
        .modify(|data| {
            data.insert(
                "agentfield".into(),
                serde_json::json!({"type": "api_key", "key": "live-secret"}),
            );
        })
        .unwrap();
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Ok, "{check:?}");
    assert!(check.message.contains("live probe ok"), "{check:?}");
    assert!(
        check.message.contains("pinned contract v0.1.138"),
        "{check:?}"
    );
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(
        !rendered.contains("live-secret"),
        "token leaked into the report"
    );
}

#[tokio::test]
async fn agentfield_live_401_reports_unauthorized() {
    let port = spawn_agentfield_server(
        "401 Unauthorized",
        "application/json",
        r#"{"error":"invalid_credential"}"#,
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    write_live_server_config(home.path(), port);
    let mut store = CredentialStore::open(home.path()).unwrap();
    store
        .modify(|data| {
            data.insert(
                "agentfield".into(),
                serde_json::json!({"type": "api_key", "key": "wrong-secret"}),
            );
        })
        .unwrap();
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Warn, "{check:?}");
    assert_eq!(check.code.as_deref(), Some("agentfield.unauthorized"));
}

#[tokio::test]
async fn agentfield_live_unreachable_reports_unavailable() {
    // Production HTTPS to a loopback literal: refused by the frozen address
    // policy before any socket or DNS — deterministic and zero network.
    let home = tempfile::tempdir().unwrap();
    write_agentfield_config(
        home.path(),
        serde_json::json!({
            "enabled": true,
            "baseUrl": "https://127.0.0.1:9",
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": "legal-agent.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type": "object"},
                    "risk": "remote_read"
                }
            }
        }),
    );
    let mut store = CredentialStore::open(home.path()).unwrap();
    store
        .modify(|data| {
            data.insert(
                "agentfield".into(),
                serde_json::json!({"type": "api_key", "key": "live-secret"}),
            );
        })
        .unwrap();
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Warn, "{check:?}");
    assert_eq!(
        check.code.as_deref(),
        Some("agentfield.unavailable"),
        "{check:?}"
    );
}

#[tokio::test]
async fn agentfield_live_protocol_mismatch_reports_error() {
    let port = spawn_agentfield_server(
        "200 OK",
        "text/html",
        "<html><body>login page</body></html>",
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    write_live_server_config(home.path(), port);
    let mut store = CredentialStore::open(home.path()).unwrap();
    store
        .modify(|data| {
            data.insert(
                "agentfield".into(),
                serde_json::json!({"type": "api_key", "key": "live-secret"}),
            );
        })
        .unwrap();
    let report = live_report(home.path(), Arc::new(NoopProbe)).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Error, "{check:?}");
    assert_eq!(
        check.code.as_deref(),
        Some("agentfield.remote_protocol"),
        "{check:?}"
    );
}

#[tokio::test]
async fn agentfield_non_live_stays_offline_without_network() {
    let home = tempfile::tempdir().unwrap();
    write_agentfield_config(home.path(), valid_agentfield_config("agentfield:primary"));
    let mut store = CredentialStore::open(home.path()).unwrap();
    store
        .modify(|data| {
            data.insert(
                "agentfield".into(),
                serde_json::json!({"type": "api_key", "key": "offline-secret"}),
            );
        })
        .unwrap();
    let report = non_live_report(home.path()).await;
    let check = agentfield_check(&report);
    assert_eq!(check.status, DoctorStatus::Ok, "{check:?}");
    assert!(
        check.message.contains("run `lato doctor --live`"),
        "{check:?}"
    );
    assert!(!check.message.contains("deferred_to_7c1_1"));
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(!rendered.contains("offline-secret"));
}

/// 7C1.1 AC-08: the doctor live path builds its client only through the
/// unique policy-enforcing factory.
#[test]
fn agentfield_live_probe_uses_the_policy_factory() {
    let source = include_str!("../src/doctor.rs");
    assert!(
        source.contains("production_agentfield_client"),
        "doctor --live must construct the AgentField client via the policy factory"
    );
    let factory_idx = source
        .find("production_agentfield_client")
        .expect("factory call present");
    let probe_idx = source
        .find("AgentFieldProbe::new")
        .expect("live probe present");
    assert!(factory_idx < probe_idx, "factory must run before probing");
}
