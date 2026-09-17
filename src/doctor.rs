use async_trait::async_trait;
use lato_ai::{CredentialStore, ProviderModelsStore, load_models_json, lookup_model};
use lato_core::{
    EnvironmentPolicy, NetworkPolicy, PolicyDecision, PolicyMode, PolicyRequest, SandboxObligation,
    SandboxProfile, SessionId, SideEffect, ToolCallId, ToolCapability, ToolName, TurnId,
};
use lato_extensions::{ManifestLoadResult, load_manifest};
use lato_policy::{ApprovalLedger, PolicyEngine, redact_text};
use lato_tools::{BuiltinToolEnvironment, ToolCatalog, builtin_tools};
use lato_workspace::{
    FileLocks, HostSandboxBackend, SandboxBackend, SandboxReadiness, SessionTrust,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const DOCTOR_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    pub live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: DoctorStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DoctorReport {
    pub schema_version: u16,
    pub status: DoctorStatus,
    pub checks: Vec<DoctorCheck>,
}

pub struct DoctorDependencies {
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub live_probe: Arc<dyn LiveProbe>,
}

#[async_trait]
pub trait LiveProbe: Send + Sync {
    async fn probe(&self) -> Result<String, String>;
}

#[derive(serde::Deserialize)]
struct DoctorSettings {
    default_model: String,
    #[serde(default)]
    agentfield: Option<serde_json::Value>,
}

pub async fn run(options: DoctorOptions, deps: &DoctorDependencies) -> DoctorReport {
    let secrets = env_secrets();
    let mut checks = Vec::new();

    checks.push(version_check());
    checks.push(binary_path_check());
    checks.push(home_check(&deps.home));

    let settings = load_settings(&deps.home);
    let (settings_check, default_model) = match &settings {
        Ok(Some(settings)) => (
            check(
                "settings",
                DoctorStatus::Ok,
                format!("parsed default_model {}", settings.default_model),
                None,
            ),
            Some(settings.default_model.clone()),
        ),
        Ok(None) => (
            check(
                "settings",
                DoctorStatus::Warn,
                "config.json is missing",
                None,
            ),
            None,
        ),
        Err(error) => (
            check(
                "settings",
                DoctorStatus::Error,
                error.clone(),
                Some("doctor.check_failed"),
            ),
            None,
        ),
    };
    checks.push(settings_check);

    let configured_provider = default_model.as_deref().and_then(split_model);
    checks.push(model_check(&deps.home, configured_provider));
    checks.push(credentials_check(
        &deps.home,
        configured_provider.map(|(provider, _)| provider),
    ));
    checks.push(tool_catalog_check(&deps.workspace));
    checks.push(policy_self_test());
    checks.push(sandbox_check());
    checks.push(trust_check(&deps.home, &deps.workspace));

    // Phase 7C1: optional AgentField adapter diagnostics. The offline doctor
    // only reports configuration state; the live probe additionally verifies
    // the pinned contract against the configured control plane.
    let agentfield_check = agentfield_check(
        &deps.home,
        settings.as_ref().ok().and_then(|s| s.as_ref()),
        options.live,
    )
    .await;
    checks.push(agentfield_check);

    if options.live {
        checks.push(live_check(deps).await);
    }

    let checks = checks
        .into_iter()
        .map(|check| redact_check(check, &secrets))
        .collect::<Vec<_>>();
    DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        status: overall_status(&checks),
        checks,
    }
}

pub fn render_human(report: &DoctorReport) -> String {
    let mut lines = vec![format!(
        "Lato doctor status: {}",
        status_label(report.status)
    )];
    for check in &report.checks {
        lines.push(format!(
            "[{}] {}: {}",
            status_label(check.status),
            check.id,
            check.message
        ));
    }
    let secrets = env_secrets();
    let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    redact_text(&lines.join("\n"), &secret_refs)
}

pub fn exit_code(report: &DoctorReport, strict: bool) -> i32 {
    match report.status {
        DoctorStatus::Ok => 0,
        DoctorStatus::Warn if !strict => 0,
        DoctorStatus::Warn | DoctorStatus::Error => 1,
    }
}

async fn live_check(deps: &DoctorDependencies) -> DoctorCheck {
    match tokio::time::timeout(Duration::from_secs(5), deps.live_probe.probe()).await {
        Ok(Ok(detail)) => check(
            "live",
            DoctorStatus::Ok,
            format!("live probe succeeded: {detail}"),
            None,
        ),
        Ok(Err(error)) => check(
            "live",
            DoctorStatus::Error,
            format!("live probe failed: {error}"),
            Some("doctor.check_failed"),
        ),
        Err(_) => check(
            "live",
            DoctorStatus::Error,
            "live probe timed out after 5s",
            Some("doctor.check_failed"),
        ),
    }
}

fn version_check() -> DoctorCheck {
    check(
        "version",
        DoctorStatus::Ok,
        format!(
            "lato {} {} {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        None,
    )
}

fn binary_path_check() -> DoctorCheck {
    let current = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return check(
                "binary_path",
                DoctorStatus::Warn,
                format!("cannot resolve the running binary: {error}"),
                Some("doctor.path_unresolved"),
            );
        }
    };
    if !is_lato_executable(&current) {
        return check(
            "binary_path",
            DoctorStatus::Ok,
            format!("running process is {}", current.display()),
            None,
        );
    }
    let Some(path_lato) = find_lato_on_path() else {
        return check(
            "binary_path",
            DoctorStatus::Warn,
            format!("lato is not on PATH; this process is {}", current.display()),
            Some("doctor.path_missing"),
        );
    };
    let current_canon = current.canonicalize().unwrap_or_else(|_| current.clone());
    let path_canon = path_lato
        .canonicalize()
        .unwrap_or_else(|_| path_lato.clone());
    if current_canon == path_canon {
        return check(
            "binary_path",
            DoctorStatus::Ok,
            format!("PATH resolves lato to {}", path_lato.display()),
            None,
        );
    }
    check(
        "binary_path",
        DoctorStatus::Warn,
        format!(
            "PATH resolves lato to {} but this process is {}; an older ~/.local/bin/lato (or similar) may be shadowing cargo/bin",
            path_lato.display(),
            current.display()
        ),
        Some("doctor.path_shadow"),
    )
}

fn is_lato_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "lato" || name.eq_ignore_ascii_case("lato.exe"))
}

fn find_lato_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["lato.exe", "lato"]
    } else {
        &["lato"]
    };
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn home_check(home: &Path) -> DoctorCheck {
    if let Err(error) = std::fs::create_dir_all(home) {
        return check(
            "lato_home",
            DoctorStatus::Error,
            format!("cannot create {}: {error}", home.display()),
            Some("doctor.check_failed"),
        );
    }
    match std::fs::metadata(home) {
        Ok(metadata) if metadata.is_dir() => check(
            "lato_home",
            DoctorStatus::Ok,
            format!("{} accessible", home.display()),
            None,
        ),
        Ok(_) => check(
            "lato_home",
            DoctorStatus::Error,
            format!("{} is not a directory", home.display()),
            Some("doctor.check_failed"),
        ),
        Err(error) => check(
            "lato_home",
            DoctorStatus::Error,
            format!("{} is not accessible: {error}", home.display()),
            Some("doctor.check_failed"),
        ),
    }
}

/// Phase 7C1: AgentField adapter diagnostics. Offline: configuration state
/// only (disabled / unconfigured / invalid). Phase 7C1.1: `--live` performs
/// an explicit opt-in discovery probe through the unique policy-enforcing
/// factory — disabled, unconfigured, and unresolvable-credential paths make
/// ZERO network requests and resolve zero credentials.
async fn agentfield_check(
    home: &Path,
    settings: Option<&DoctorSettings>,
    live: bool,
) -> DoctorCheck {
    use lato_agent::agentfield::{self as af};
    let raw = settings.and_then(|settings| settings.agentfield.clone());
    if raw.is_none() {
        return check(
            "agentfield",
            DoctorStatus::Ok,
            "agentfield not configured; adapter disabled",
            None,
        );
    }
    let config = match af::config::AgentFieldConfig::parse(&raw.unwrap()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            return check(
                "agentfield",
                DoctorStatus::Ok,
                "agentfield not configured; adapter disabled",
                None,
            );
        }
        Err(error) => {
            return check(
                "agentfield",
                DoctorStatus::Error,
                error.to_string(),
                Some(error.code),
            );
        }
    };
    if !config.enabled {
        return check(
            "agentfield",
            DoctorStatus::Ok,
            "agentfield present but disabled; adapter not registered",
            None,
        );
    }
    let store = lato_ai::CredentialStore::open(home).ok();
    let credential =
        af::resolve_agentfield_credential(store.as_ref(), &config.credential_reference);
    if credential.is_none() {
        return check(
            "agentfield",
            DoctorStatus::Warn,
            format!(
                "enabled but credential `{}` is not resolvable (credential store entry `agentfield` or LATO_AGENTFIELD_CREDENTIAL)",
                config.credential_reference
            ),
            Some("agentfield.unconfigured"),
        );
    }
    if !live {
        return check(
            "agentfield",
            DoctorStatus::Ok,
            format!(
                "enabled with {} capability(ies); run `lato doctor --live` for live AgentField diagnostics",
                config.capabilities.len()
            ),
            None,
        );
    }
    // Explicit opt-in live probe: one discovery request through the frozen
    // network policy, classified into stable diagnostics.
    match af::production_agentfield_client(store.as_ref(), &config).await {
        Err(error) => agentfield_live_error_check(error),
        Ok(client) => {
            let probe = af::AgentFieldProbe::new(std::sync::Arc::new(client));
            match tokio::time::timeout(Duration::from_secs(30), probe.probe()).await {
                Err(_) => check(
                    "agentfield",
                    DoctorStatus::Warn,
                    "agentfield.unavailable: live probe exceeded the 30s budget".to_string(),
                    Some("agentfield.unavailable"),
                ),
                Ok(Ok(snapshot)) => check(
                    "agentfield",
                    DoctorStatus::Ok,
                    format!(
                        "live probe ok: discovery answered {} agent(s), {} healthy, pinned contract {}",
                        snapshot.agent_count, snapshot.healthy_agent_count, snapshot.version
                    ),
                    None,
                ),
                Ok(Err(error)) => agentfield_live_error_check(error),
            }
        }
    }
}

/// Stable classification of a failed live probe. Error text comes from the
/// `agentfield.*` family only: never the server body, the token, or the
/// resolved address details.
fn agentfield_live_error_check(
    error: lato_agent::agentfield::client::AgentFieldError,
) -> DoctorCheck {
    use lato_agent::agentfield::client::AgentFieldError as Error;
    match &error {
        Error::Unauthorized => check(
            "agentfield",
            DoctorStatus::Warn,
            "agentfield.unauthorized: control plane rejected the credentials (401/403)".to_string(),
            Some("agentfield.unauthorized"),
        ),
        Error::Unavailable(_) => check(
            "agentfield",
            DoctorStatus::Warn,
            error.to_string(),
            Some("agentfield.unavailable"),
        ),
        Error::RemoteProtocol(_) => check(
            "agentfield",
            DoctorStatus::Error,
            error.to_string(),
            Some("agentfield.remote_protocol"),
        ),
        Error::BodyTooLarge(_) => check(
            "agentfield",
            DoctorStatus::Error,
            error.to_string(),
            Some("agentfield.output_too_large"),
        ),
        Error::RemoteDenied => check(
            "agentfield",
            DoctorStatus::Warn,
            "agentfield.remote_denied: remote policy denied the probe".to_string(),
            Some("agentfield.remote_denied"),
        ),
        Error::CredentialMissing(_) => check(
            "agentfield",
            DoctorStatus::Warn,
            error.to_string(),
            Some("agentfield.unconfigured"),
        ),
    }
}

fn load_settings(home: &Path) -> Result<Option<DoctorSettings>, String> {
    let path = home.join("config.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

#[derive(Clone, Copy)]
enum ModelSource {
    BuiltIn,
    ModelsJson,
    ProviderStore,
    CompatibilityCache,
}

impl ModelSource {
    fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in catalog",
            Self::ModelsJson => "models.json",
            Self::ProviderStore => "models-store.json",
            Self::CompatibilityCache => "model-cache.json",
        }
    }
}

fn model_check(home: &Path, configured: Option<(&str, &str)>) -> DoctorCheck {
    let Some((provider, model_id)) = configured else {
        return check(
            "model",
            DoctorStatus::Warn,
            "no configured model to look up",
            None,
        );
    };
    if lookup_model(provider, model_id).is_some() {
        return model_found(provider, model_id, ModelSource::BuiltIn);
    }

    let models_path = home.join("models.json");
    if models_path.exists() {
        match load_models_json(&models_path) {
            Ok(models)
                if models
                    .iter()
                    .any(|model| model.provider == provider && model.id == model_id) =>
            {
                return model_found(provider, model_id, ModelSource::ModelsJson);
            }
            Ok(_) => {}
            Err(error) => return model_source_error(&models_path, error),
        }
    }

    let provider_store_path = home.join("models-store.json");
    if provider_store_path.exists() {
        match ProviderModelsStore::open(home).read(provider) {
            Ok(Some(entry))
                if entry
                    .models
                    .iter()
                    .any(|model| model.provider == provider && model.id == model_id) =>
            {
                return model_found(provider, model_id, ModelSource::ProviderStore);
            }
            Ok(_) => {}
            Err(error) => return model_source_error(&provider_store_path, error),
        }
    }

    let compatibility_path = home.join("model-cache.json");
    if compatibility_path.exists() {
        match load_models_json(&compatibility_path) {
            Ok(models)
                if models
                    .iter()
                    .any(|model| model.provider == provider && model.id == model_id) =>
            {
                return model_found(provider, model_id, ModelSource::CompatibilityCache);
            }
            Ok(_) => {}
            Err(error) => return model_source_error(&compatibility_path, error),
        }
    }

    check(
        "model",
        DoctorStatus::Warn,
        format!(
            "unknown model {provider}/{model_id}; run lato and select the model again, or repair the local model configuration"
        ),
        None,
    )
}

fn model_found(provider: &str, model_id: &str, source: ModelSource) -> DoctorCheck {
    check(
        "model",
        DoctorStatus::Ok,
        format!("{provider}/{model_id} found in {}", source.label()),
        None,
    )
}

fn model_source_error(path: &Path, error: String) -> DoctorCheck {
    check(
        "model",
        DoctorStatus::Error,
        format!("cannot parse {}: {error}", path.display()),
        Some("doctor.check_failed"),
    )
}

fn credentials_check(home: &Path, configured_provider: Option<&str>) -> DoctorCheck {
    let store = match CredentialStore::open(home) {
        Ok(store) => store,
        Err(error) => {
            return check(
                "credentials",
                DoctorStatus::Error,
                format!("cannot open credential store: {error}"),
                Some("doctor.check_failed"),
            );
        }
    };
    let Some(provider) = configured_provider else {
        return check(
            "credentials",
            DoctorStatus::Warn,
            "no configured provider to check",
            None,
        );
    };
    if store.contains(provider) {
        check(
            "credentials",
            DoctorStatus::Ok,
            format!("{provider}: present"),
            None,
        )
    } else {
        check(
            "credentials",
            DoctorStatus::Warn,
            format!("{provider}: missing"),
            None,
        )
    }
}

fn tool_catalog_check(workspace: &Path) -> DoctorCheck {
    let environment = BuiltinToolEnvironment {
        cwd: workspace.to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(workspace),
        skill_resolver: None,
    };
    let tools = match builtin_tools(environment) {
        Ok(tools) => tools,
        Err(error) => {
            return check(
                "tool_catalog",
                DoctorStatus::Error,
                error.to_string(),
                Some("doctor.check_failed"),
            );
        }
    };
    let mut catalog = ToolCatalog::new();
    for tool in tools {
        if let Err(error) = catalog.register(tool) {
            return check(
                "tool_catalog",
                DoctorStatus::Error,
                error.to_string(),
                Some("doctor.check_failed"),
            );
        }
    }
    check(
        "tool_catalog",
        DoctorStatus::Ok,
        format!("{} tools registered", catalog.len()),
        None,
    )
}

fn policy_self_test() -> DoctorCheck {
    let engine = PolicyEngine::new(Arc::new(ApprovalLedger::new(Duration::from_secs(60))));
    let allow = engine.evaluate(&self_test_request(
        PolicyMode::Ask,
        vec![ToolCapability::FileRead],
        SideEffect::ReadOnly,
        true,
    ));
    let ask_write = engine.evaluate(&self_test_request(
        PolicyMode::Ask,
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
        true,
    ));
    let always_write = engine.evaluate(&self_test_request(
        PolicyMode::Always,
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
        true,
    ));
    let denied = engine.evaluate(&self_test_request(
        PolicyMode::Ask,
        vec![ToolCapability::ExtensionInvoke],
        SideEffect::ReadOnly,
        false,
    ));
    let passed = matches!(allow, PolicyDecision::Allow(_))
        && matches!(ask_write, PolicyDecision::RequireApproval(_))
        && matches!(always_write, PolicyDecision::Allow(_))
        && matches!(
            denied,
            PolicyDecision::Deny(ref denial) if denial.code == "policy.untrusted_extension"
        );
    if passed {
        check(
            "policy",
            DoctorStatus::Ok,
            "fixed policy self-test matrix passed",
            None,
        )
    } else {
        check(
            "policy",
            DoctorStatus::Error,
            "fixed policy self-test matrix failed",
            Some("doctor.check_failed"),
        )
    }
}

fn sandbox_check() -> DoctorCheck {
    let backend = HostSandboxBackend::new();
    let off = backend.readiness(SandboxProfile::Off);
    let workspace = backend.readiness(SandboxProfile::Workspace);
    let read_only = backend.readiness(SandboxProfile::ReadOnly);
    if off != SandboxReadiness::Ready {
        return check(
            "sandbox",
            DoctorStatus::Error,
            format!("off profile is {off:?}"),
            Some("sandbox.unavailable"),
        );
    }
    if workspace != SandboxReadiness::Ready || read_only != SandboxReadiness::Ready {
        return check(
            "sandbox",
            DoctorStatus::Warn,
            format!("off ready; workspace={workspace:?} read_only={read_only:?}"),
            None,
        );
    }
    check(
        "sandbox",
        DoctorStatus::Ok,
        "off, workspace, and read-only profiles are ready",
        None,
    )
}

fn trust_check(home: &Path, workspace: &Path) -> DoctorCheck {
    let trust = SessionTrust::for_interactive(workspace, false);
    let project_plugins = count_plugins(workspace.join(".lato/plugins"));
    let user_plugins = count_plugins(home.join("plugins"));
    check(
        "trust",
        DoctorStatus::Ok,
        format!(
            "project_trusted={} project_plugins={project_plugins} user_plugins={user_plugins}",
            trust.cwd_trusted()
        ),
        None,
    )
}

fn count_plugins(root: PathBuf) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            matches!(
                load_manifest(&entry.path()),
                Ok(ManifestLoadResult::Found(_) | ManifestLoadResult::Convention(_))
            )
        })
        .count()
}

fn self_test_request(
    mode: PolicyMode,
    capabilities: Vec<ToolCapability>,
    side_effect: SideEffect,
    project_trusted: bool,
) -> PolicyRequest {
    PolicyRequest {
        session_id: SessionId::from("doctor-session"),
        turn_id: TurnId::from("doctor-turn"),
        call_id: ToolCallId::from("doctor-call"),
        tool_name: ToolName::parse("builtin:test").unwrap(),
        arguments_digest: "doctor-self-test".into(),
        capabilities,
        side_effect,
        mode,
        project_trusted,
        sandbox: SandboxObligation {
            profile: SandboxProfile::Workspace,
            workspace_root: PathBuf::from("/workspace"),
            writable_roots: vec![PathBuf::from("/workspace")],
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy::default(),
        },
        detail: None,
    }
}

fn split_model(selection: &str) -> Option<(&str, &str)> {
    selection.split_once('/')
}

fn env_secrets() -> Vec<String> {
    std::env::var("LATO_TEST_SECRET")
        .ok()
        .filter(|value| !value.is_empty())
        .into_iter()
        .collect()
}

fn redact_check(mut check: DoctorCheck, secrets: &[String]) -> DoctorCheck {
    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    check.id = redact_text(&check.id, &refs);
    check.message = redact_text(&check.message, &refs);
    if let Some(code) = check.code {
        check.code = Some(redact_text(&code, &refs));
    }
    check
}

fn overall_status(checks: &[DoctorCheck]) -> DoctorStatus {
    if checks
        .iter()
        .any(|check| check.status == DoctorStatus::Error)
    {
        DoctorStatus::Error
    } else if checks
        .iter()
        .any(|check| check.status == DoctorStatus::Warn)
    {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    }
}

fn check(
    id: &str,
    status: DoctorStatus,
    message: impl Into<String>,
    code: Option<&str>,
) -> DoctorCheck {
    DoctorCheck {
        id: id.to_owned(),
        status,
        message: message.into(),
        code: code.map(str::to_owned),
    }
}

fn status_label(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Ok => "ok",
        DoctorStatus::Warn => "warn",
        DoctorStatus::Error => "error",
    }
}
