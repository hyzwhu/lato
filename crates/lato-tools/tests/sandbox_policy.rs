use lato_core::{
    PolicyMode, SandboxObligation, SandboxProfile, SessionId, ToolCallId, ToolContext, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine, validate_sandbox_obligation};
use lato_tools::{
    BuiltinToolEnvironment, PolicyScope, ToolRuntimeBuilder, run_terminal_command_with_backend,
};
use lato_workspace::{FileLocks, HostSandboxBackend, SessionTrust};
use serde_json::json;
use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct EnvRestore {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvRestore {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let previous = vars
            .iter()
            .map(|(key, value)| {
                let existing = std::env::var_os(key);
                unsafe {
                    std::env::set_var(key, value);
                }
                (*key, existing)
            })
            .collect();
        Self { previous }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, previous) in self.previous.drain(..) {
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn context() -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

#[test]
fn read_only_obligation_rejects_writable_roots() {
    let mut obligation = SandboxObligation::read_only("/workspace");
    obligation.writable_roots = vec![PathBuf::from("/workspace")];
    let error = validate_sandbox_obligation(&obligation).unwrap_err();
    assert_eq!(error.code, "sandbox.unsupported");
}

#[test]
fn workspace_obligation_rejects_root_outside_workspace() {
    let mut obligation = SandboxObligation::workspace("/workspace");
    obligation.writable_roots = vec![PathBuf::from("/tmp/outside")];
    let error = validate_sandbox_obligation(&obligation).unwrap_err();
    assert_eq!(error.code, "sandbox.unsupported");
}

#[test]
fn well_formed_workspace_and_read_only_obligations_are_accepted() {
    assert!(validate_sandbox_obligation(&SandboxObligation::workspace("/workspace")).is_ok());
    assert!(validate_sandbox_obligation(&SandboxObligation::read_only("/workspace")).is_ok());
}

#[test]
fn off_is_accepted_only_when_obligation_profile_is_off() {
    assert!(validate_sandbox_obligation(&SandboxObligation::off("/workspace")).is_ok());

    let mut disguised = SandboxObligation::off("/workspace");
    disguised.profile = SandboxProfile::Workspace;
    disguised.writable_roots = vec![PathBuf::from("/tmp/outside")];
    let error = validate_sandbox_obligation(&disguised).unwrap_err();
    assert_eq!(error.code, "sandbox.unsupported");
}

#[tokio::test]
async fn missing_wrapper_does_not_execute_workspace_command() {
    let temp = tempfile::tempdir().unwrap();
    let backend = HostSandboxBackend::with_wrapper_override("/definitely/missing/lato-sandbox");
    let error = run_terminal_command_with_backend(
        "printf forbidden > forbidden",
        temp.path(),
        &SandboxObligation::workspace(temp.path()),
        &backend,
    )
    .await
    .unwrap_err();
    assert!(error.contains("sandbox.unavailable"));
    assert!(!temp.path().join("forbidden").exists());
}

#[tokio::test]
async fn read_only_grant_cannot_search_replace_a_file_into_existence() {
    let workspace = tempfile::tempdir().unwrap();
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: workspace.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::ReadOnly,
        },
    );
    builder
        .register_builtin_tools(BuiltinToolEnvironment {
            cwd: workspace.path().to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(workspace.path()),
            skill_resolver: None,
        })
        .unwrap();
    let runtime = builder.build().unwrap();
    let created = workspace.path().join("created.txt");
    let error = runtime
        .invoke(
            context(),
            "search_replace",
            json!({"path": "created.txt", "old": "old", "new": "new"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.policy_denied");
    assert!(error.message.contains("read-only"), "{}", error.message);
    assert!(!created.exists());
}

#[tokio::test]
async fn process_tool_consumes_grant_sandbox_not_session_trust() {
    let workspace = tempfile::tempdir().unwrap();
    let mut trust = SessionTrust::for_headless_prompt(workspace.path());
    trust.sandbox = SandboxProfile::Off;
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: workspace.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::ReadOnly,
        },
    );
    builder
        .register_builtin_tools(BuiltinToolEnvironment {
            cwd: workspace.path().to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust,
            skill_resolver: None,
        })
        .unwrap();
    let runtime = builder.build().unwrap();
    let result = runtime
        .invoke(
            context(),
            "run_terminal_command",
            json!({"command": "printf forbidden > forbidden"}),
        )
        .await;
    assert!(result.is_err());
    assert!(!workspace.path().join("forbidden").exists());
}

#[tokio::test]
async fn grant_path_does_not_forward_secret_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let _env = EnvRestore::set(&[
        ("LATO_TEST_TOKEN", "leaked-token-value"),
        ("LATO_TEST_SECRET", "leaked-secret-value"),
        ("LATO_TEST_PASSWORD", "leaked-password-value"),
        ("LATO_TEST_API_KEY", "leaked-api-key-value"),
        ("LATO_TEST_AUTHORIZATION", "leaked-authorization-value"),
    ]);
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: workspace.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register_builtin_tools(BuiltinToolEnvironment {
            cwd: workspace.path().to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(workspace.path()),
            skill_resolver: None,
        })
        .unwrap();
    let runtime = builder.build().unwrap();
    let command = if cfg!(windows) {
        "Write-Output \"$env:LATO_TEST_TOKEN|$env:LATO_TEST_SECRET|$env:LATO_TEST_PASSWORD|$env:LATO_TEST_API_KEY|$env:LATO_TEST_AUTHORIZATION|$env:PATH\""
    } else {
        "printf '%s' \"$LATO_TEST_TOKEN|$LATO_TEST_SECRET|$LATO_TEST_PASSWORD|$LATO_TEST_API_KEY|$LATO_TEST_AUTHORIZATION|$PATH\""
    };
    let output = runtime
        .invoke(
            context(),
            "run_terminal_command",
            json!({"command": command}),
        )
        .await
        .unwrap();
    assert!(
        !output.content.contains("leaked-token-value"),
        "{}",
        output.content
    );
    assert!(!output.content.contains("leaked-secret-value"));
    assert!(!output.content.contains("leaked-password-value"));
    assert!(!output.content.contains("leaked-api-key-value"));
    assert!(!output.content.contains("leaked-authorization-value"));
    assert!(
        output.content.contains(':')
            || output.content.contains(';')
            || output.content.contains('/'),
        "PATH should still be forwarded: {}",
        output.content
    );
}
