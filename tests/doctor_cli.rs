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
