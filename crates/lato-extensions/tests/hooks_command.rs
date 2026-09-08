use std::{collections::BTreeMap, fs, time::Duration};

use lato_extensions::hooks::{
    HandlerType, HookEventEnvelope, HookEventName, HookRunContext, HookRunError, HookSpec,
    run_command_hook,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn spec(command: String, root: &std::path::Path, timeout_ms: u64) -> HookSpec {
    HookSpec {
        id: "plugin:hook".into(), plugin_name: "plugin".into(),
        event: HookEventName::PreToolUse, handler_type: HandlerType::Command,
        matcher: None, command: Some(command), url: None, timeout_ms,
        source_dir: root.to_path_buf(), extra_env: BTreeMap::new(),
    }
}

fn envelope() -> HookEventEnvelope {
    HookEventEnvelope::new(HookEventName::PreToolUse, 1, "session", Some("turn".into()), serde_json::json!({"toolName":"read_file"})).unwrap()
}

#[tokio::test]
async fn command_receives_json_identity_and_workspace() {
    let root = tempdir().unwrap();
    let command = "read body; printf '%s|%s|%s|%s' \"$LATO_HOOK_EVENT\" \"$LATO_SESSION_ID\" \"$PWD\" \"$body\"";
    let result = run_command_hook(&spec(command.into(), root.path(), 2_000), &envelope(), &HookRunContext { session_id: "session", workspace_root: root.path(), cancellation: CancellationToken::new() }).await.unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.starts_with("PreToolUse|session|"));
    assert!(result.stdout.contains("\"schemaVersion\":1"));
}

#[cfg(unix)]
#[tokio::test]
async fn relative_executable_uses_source_dir_and_environment_cannot_spoof_identity() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir().unwrap();
    let script = root.path().join("hook.sh");
    fs::write(&script, "#!/bin/sh\nprintf '%s|%s' \"$LATO_SESSION_ID\" \"$EXTRA\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let mut value = spec("./hook.sh".into(), root.path(), 2_000);
    value.extra_env.insert("LATO_SESSION_ID".into(), "spoof".into());
    value.extra_env.insert("EXTRA".into(), "kept".into());
    let result = run_command_hook(&value, &envelope(), &HookRunContext { session_id: "real", workspace_root: root.path(), cancellation: CancellationToken::new() }).await.unwrap();
    assert_eq!(result.stdout, "real|kept");
}

#[tokio::test]
async fn timeout_and_cancellation_are_typed() {
    let root = tempdir().unwrap();
    let timeout = run_command_hook(&spec("sleep 2".into(), root.path(), 20), &envelope(), &HookRunContext { session_id: "s", workspace_root: root.path(), cancellation: CancellationToken::new() }).await.unwrap_err();
    assert!(matches!(timeout, HookRunError::Timeout { .. }));
    let token = CancellationToken::new();
    token.cancel();
    let cancelled = run_command_hook(&spec("sleep 2".into(), root.path(), 2_000), &envelope(), &HookRunContext { session_id: "s", workspace_root: root.path(), cancellation: token }).await.unwrap_err();
    assert!(matches!(cancelled, HookRunError::Cancelled));
    tokio::time::sleep(Duration::from_millis(10)).await;
}

#[tokio::test]
async fn combined_output_is_bounded() {
    let root = tempdir().unwrap();
    let error = run_command_hook(&spec("head -c 1048577 /dev/zero".into(), root.path(), 2_000), &envelope(), &HookRunContext { session_id: "s", workspace_root: root.path(), cancellation: CancellationToken::new() }).await.unwrap_err();
    assert!(matches!(error, HookRunError::OutputOverflow));
}
