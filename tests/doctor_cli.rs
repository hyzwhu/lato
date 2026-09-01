use async_trait::async_trait;
use lato::doctor::{
    DoctorCheck, DoctorDependencies, DoctorOptions, DoctorReport, DoctorStatus, LiveProbe,
    exit_code, render_human, run,
};
use lato_ai::CredentialStore;
use std::{
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

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
    let report = run(
        DoctorOptions {
            live: false,
            ..DoctorOptions::default()
        },
        &deps,
    )
    .await;
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
