use async_trait::async_trait;
use lato_extensions::hooks::*;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Fake(Mutex<VecDeque<Result<RawHookRun, HookRunError>>>);
#[async_trait]
impl HookExecutor for Fake {
    async fn execute(
        &self,
        _spec: &HookSpec,
        _envelope: &HookEventEnvelope,
        _context: &HookRunContext<'_>,
    ) -> Result<RawHookRun, HookRunError> {
        self.0.lock().unwrap().pop_front().unwrap()
    }
}
fn raw(value: serde_json::Value) -> Result<RawHookRun, HookRunError> {
    Ok(RawHookRun {
        stdout: value.to_string(),
        stderr_preview: String::new(),
        exit_code: Some(0),
        elapsed: Duration::ZERO,
        truncated: false,
    })
}
fn spec(id: &str, event: HookEventName) -> HookSpec {
    HookSpec {
        id: id.into(),
        plugin_name: "p".into(),
        event,
        handler_type: HandlerType::Command,
        matcher: None,
        command: Some("true".into()),
        url: None,
        timeout_ms: 100,
        source_dir: PathBuf::from("."),
        extra_env: BTreeMap::new(),
    }
}
fn env(event: HookEventName) -> HookEventEnvelope {
    HookEventEnvelope::new(
        event,
        1,
        "s",
        Some("t".into()),
        serde_json::json!({"toolName":"read_file","arguments":{"path":"old"}}),
    )
    .unwrap()
}
fn ctx() -> HookRunContext<'static> {
    HookRunContext {
        session_id: "s",
        workspace_root: std::path::Path::new("."),
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn pre_tool_last_rewrite_and_ask_win_but_deny_discards_mutations() {
    let registry = HookRegistry::from_specs(
        1,
        vec![
            spec("a", HookEventName::PreToolUse),
            spec("b", HookEventName::PreToolUse),
        ],
    );
    let fake = Fake(Mutex::new(VecDeque::from([
        raw(
            serde_json::json!({"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"path":"one"},"additionalContext":"one"}}),
        ),
        raw(
            serde_json::json!({"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"path":"two"}}}),
        ),
    ])));
    let result =
        dispatch_pre_tool_use(&registry, &env(HookEventName::PreToolUse), &ctx(), &fake).await;
    assert_eq!(result.updated_input.unwrap()["path"], "two");
    assert!(matches!(result.decision, HookDecision::Ask { hook_id, .. } if hook_id == "b"));
    let fake = Fake(Mutex::new(VecDeque::from([
        raw(serde_json::json!({"hookSpecificOutput":{"updatedInput":{"path":"one"}}})),
        raw(
            serde_json::json!({"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no"}}),
        ),
    ])));
    let result =
        dispatch_pre_tool_use(&registry, &env(HookEventName::PreToolUse), &ctx(), &fake).await;
    assert!(result.updated_input.is_none() && result.additional_context.is_empty());
    assert!(matches!(result.decision, HookDecision::Deny { .. }));
}

#[tokio::test]
async fn failures_fail_open_and_stop_force_stop_dominates() {
    let registry = HookRegistry::from_specs(
        1,
        vec![
            spec("a", HookEventName::Stop),
            spec("b", HookEventName::Stop),
        ],
    );
    let fake = Fake(Mutex::new(VecDeque::from([
        Err(HookRunError::Spawn),
        raw(
            serde_json::json!({"decision":"block","reason":"more","continue":false,"stopReason":"done","hookSpecificOutput":{"additionalContext":"ctx"}}),
        ),
    ])));
    let result = dispatch_stop(&registry, &env(HookEventName::Stop), &ctx(), &fake).await;
    assert_eq!(result.runs[0].outcome, HookRunOutcome::Failed);
    assert_eq!(result.prevent_continuation.unwrap().reason, "done");
    assert_eq!(result.blocks.len(), 1);
}

#[tokio::test]
async fn post_tool_aggregates_and_last_replacement_wins() {
    let registry = HookRegistry::from_specs(
        1,
        vec![
            spec("a", HookEventName::PostToolUse),
            spec("b", HookEventName::PostToolUse),
        ],
    );
    let fake = Fake(Mutex::new(VecDeque::from([
        raw(
            serde_json::json!({"hookSpecificOutput":{"updatedToolOutput":{"text":"one"},"additionalContext":"a"}}),
        ),
        raw(
            serde_json::json!({"decision":"block","reason":"warn","hookSpecificOutput":{"updatedToolOutput":{"text":"two"}}}),
        ),
    ])));
    let result =
        dispatch_post_tool_use(&registry, &env(HookEventName::PostToolUse), &ctx(), &fake).await;
    assert_eq!(result.replacement.unwrap()["text"], "two");
    assert_eq!(result.additional_context.len(), 1);
    assert_eq!(result.blocks.len(), 1);
}
