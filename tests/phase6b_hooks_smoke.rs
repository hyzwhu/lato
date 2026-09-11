use std::{collections::BTreeMap, path::PathBuf, process::Command};

use lato_agent::SessionHookRuntime;
use lato_extensions::hooks::{HandlerType, HookDecision, HookEventName, HookRegistry, HookSpec};
use tokio_util::sync::CancellationToken;

fn hook(id: &str, event: HookEventName, command: &str) -> HookSpec {
    HookSpec {
        id: id.into(),
        plugin_name: "smoke".into(),
        event,
        handler_type: HandlerType::Command,
        matcher: None,
        command: Some(command.into()),
        url: None,
        timeout_ms: 1_000,
        source_dir: PathBuf::from("."),
        extra_env: BTreeMap::new(),
    }
}

#[test]
fn phase6b_installed_command_smoke_exercises_hook_lifecycle() {
    let binary = std::env::var_os("LATO_SMOKE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lato")));
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let hooks = SessionHookRuntime::new(HookRegistry::from_specs(1, vec![
            hook("prompt", HookEventName::UserPromptSubmit, "cat >/dev/null; printf '%s' '{}'") ,
            hook("pre", HookEventName::PreToolUse, "cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"updatedInput\":{\"path\":\"safe.txt\"}}}'"),
            hook("post", HookEventName::PostToolUse, "cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"updatedToolOutput\":{\"text\":\"replacement\"}}}'"),
            hook("stop", HookEventName::Stop, "cat >/dev/null; printf '%s' '{\"continue\":false,\"stopReason\":\"done\"}'"),
        ]), PathBuf::from("."), "smoke-session".into());
        assert!(hooks.prompt_submit("turn", "hello", CancellationToken::new()).await.block.is_none());
        let pre = hooks.pre_tool_use("turn", "read_file", serde_json::json!({"path":"unsafe"}), CancellationToken::new()).await;
        assert_eq!(pre.updated_input.unwrap()["path"], "safe.txt");
        assert!(matches!(pre.decision, HookDecision::Ask { .. }));
        let post = hooks.post_tool_use("turn", serde_json::json!({"toolName":"read_file","arguments":{},"output":"old"}), CancellationToken::new()).await;
        assert_eq!(post.replacement.unwrap()["text"], "replacement");
        assert_eq!(hooks.stop("turn", serde_json::json!({}), CancellationToken::new()).await.prevent_continuation.unwrap().reason, "done");
    });
    println!("phase6b-hooks-smoke-ok");
}
