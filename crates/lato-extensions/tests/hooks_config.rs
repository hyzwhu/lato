use lato_extensions::hooks::{
    HookEventName, HookMatcher, HookMode, ParsedDecision, parse_hook_result,
};

#[test]
fn event_aliases_modes_and_timeouts_are_stable() {
    assert_eq!(HookEventName::parse("pre-tool-use"), Some(HookEventName::PreToolUse));
    assert_eq!(HookEventName::parse("BeforeTool"), Some(HookEventName::PreToolUse));
    assert_eq!(HookEventName::parse("post_compact"), Some(HookEventName::PostCompact));
    assert_eq!(HookEventName::SessionStart.mode(), HookMode::Observe);
    assert_eq!(HookEventName::PreToolUse.mode(), HookMode::Tool);
    assert_eq!(HookEventName::UserPromptSubmit.default_timeout_ms(), 30_000);
    assert_eq!(HookEventName::Stop.default_timeout_ms(), 600_000);
    assert_eq!(HookEventName::SessionEnd.default_timeout_ms(), 1_500);
}

#[test]
fn matcher_supports_all_exact_aliases_and_regex() {
    assert!(HookMatcher::compile("").unwrap().matches("read_file"));
    assert!(HookMatcher::compile("*").unwrap().matches("read_file"));
    assert!(HookMatcher::compile("read_file|search").unwrap().matches("read_file"));
    assert!(HookMatcher::compile("read_.*").unwrap().matches("read_file"));
    assert!(!HookMatcher::compile("^read$").unwrap().matches("read_file"));
    assert!(HookMatcher::compile("(").is_err());
}

#[test]
fn result_nested_precedence_bounds_and_event_isolation() {
    let parsed = parse_hook_result(
        HookEventName::PreToolUse,
        r#"{"decision":"block","reason":"legacy","hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"nested","updatedInput":{"path":"safe"},"additionalContext":"ctx"}}"#,
        Some(0),
    )
    .unwrap();
    assert_eq!(parsed.decision, ParsedDecision::Ask);
    assert_eq!(parsed.reason.as_deref(), Some("nested"));
    assert_eq!(parsed.updated_input.unwrap()["path"], "safe");

    let observer = parse_hook_result(
        HookEventName::SessionStart,
        r#"{"decision":"block","hookSpecificOutput":{"updatedInput":{"bad":true}},"systemMessage":"hello"}"#,
        Some(0),
    )
    .unwrap();
    assert_eq!(observer.decision, ParsedDecision::Allow);
    assert!(observer.updated_input.is_none());
    assert_eq!(observer.system_message.as_deref(), Some("hello"));
}

#[test]
fn exit_two_blocks_without_overriding_specific_decision() {
    let blocked = parse_hook_result(HookEventName::UserPromptSubmit, "", Some(2)).unwrap();
    assert_eq!(blocked.decision, ParsedDecision::Deny);
    let ask = parse_hook_result(
        HookEventName::PreToolUse,
        r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
        Some(2),
    )
    .unwrap();
    assert_eq!(ask.decision, ParsedDecision::Ask);
}
