use std::{collections::BTreeMap, path::PathBuf};

use lato_agent::SessionHookRuntime;
use lato_core::{ExtensionAuditRecord, HookAuditOutcome, HookAuditPhase};
use lato_extensions::hooks::{HandlerType, HookDecision, HookEventName, HookRegistry, HookSpec};
use tokio_util::sync::CancellationToken;

fn spec(id: &str, event: HookEventName, command: &str) -> HookSpec {
    HookSpec {
        id: id.into(),
        plugin_name: "test-plugin".into(),
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

fn runtime(specs: Vec<HookSpec>) -> SessionHookRuntime {
    SessionHookRuntime::new(
        HookRegistry::from_specs(7, specs),
        PathBuf::from("."),
        "session".into(),
    )
}

#[tokio::test]
async fn prompt_block_is_explicit_and_runner_failure_fails_open() {
    let hooks = runtime(vec![
        spec(
            "broken",
            HookEventName::UserPromptSubmit,
            "/definitely/missing/hook",
        ),
        spec(
            "block",
            HookEventName::UserPromptSubmit,
            "cat >/dev/null; printf '%s' '{\"decision\":\"block\",\"reason\":\"no prompts\"}'",
        ),
    ]);
    let result = hooks
        .prompt_submit("turn", "secret prompt", CancellationToken::new())
        .await;
    assert_eq!(result.block.unwrap().reason, "no prompts");
    assert_eq!(result.runs.len(), 2);
}

#[tokio::test]
async fn pre_tool_rewrites_only_arguments_and_preserves_gate() {
    let hooks = runtime(vec![spec(
        "rewrite",
        HookEventName::PreToolUse,
        "cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"updatedInput\":{\"path\":\"safe.txt\"}}}'",
    )]);
    let result = hooks
        .pre_tool_use(
            "turn",
            "builtin:read_file",
            serde_json::json!({"path":"unsafe.txt"}),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.updated_input.unwrap()["path"], "safe.txt");
    assert!(matches!(result.decision, HookDecision::Ask { .. }));
}

#[tokio::test]
async fn post_tool_replacement_and_stop_force_stop_are_typed() {
    let hooks = runtime(vec![
        spec(
            "post",
            HookEventName::PostToolUse,
            "cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"updatedToolOutput\":{\"text\":\"bounded replacement\"}}}'",
        ),
        spec(
            "stop",
            HookEventName::Stop,
            "cat >/dev/null; printf '%s' '{\"decision\":\"block\",\"reason\":\"continue\",\"continue\":false,\"stopReason\":\"finished\"}'",
        ),
    ]);
    let post = hooks
        .post_tool_use(
            "turn",
            serde_json::json!({"toolName":"read_file","arguments":{},"output":"original"}),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(post.replacement.unwrap()["text"], "bounded replacement");
    let stop = hooks
        .stop(
            "turn",
            serde_json::json!({"reason":"complete"}),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(stop.prevent_continuation.unwrap().reason, "finished");
}

#[test]
fn hook_audit_wire_shape_contains_hashes_not_payload_secrets() {
    let record = ExtensionAuditRecord::Hook {
        generation: 7,
        hook_id: "plugin:hook".into(),
        event: "PreToolUse".into(),
        phase: HookAuditPhase::ArgumentsRewritten,
        outcome: HookAuditOutcome::Applied,
        duration_ms: Some(3),
        effective_timeout_ms: 5_000,
        input_hash: "sha256-input".into(),
        output_hash: Some("sha256-output".into()),
        replaced_prior_hook_id: None,
        truncated: false,
        redacted_reason: None,
    };
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(encoded.contains("arguments_rewritten"));
    for secret in [
        "secret prompt",
        "unsafe.txt",
        "environment-secret",
        "https://user:pass@",
    ] {
        assert!(!encoded.contains(secret));
    }
}
