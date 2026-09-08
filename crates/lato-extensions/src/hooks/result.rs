use serde_json::Value;

use super::{HookEventName, MAX_FEEDBACK_CHARS, MAX_REASON_CHARS, MAX_REPLACEMENT_CHARS};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedDecision {
    Allow,
    Ask,
    Defer,
    Deny,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedHookResult {
    pub decision: ParsedDecision,
    pub reason: Option<String>,
    pub updated_input: Option<Value>,
    pub additional_context: Option<String>,
    pub updated_tool_output: Option<Value>,
    pub continue_: Option<bool>,
    pub stop_reason: Option<String>,
    pub system_message: Option<String>,
}

impl Default for ParsedHookResult {
    fn default() -> Self {
        Self { decision: ParsedDecision::Allow, reason: None, updated_input: None, additional_context: None, updated_tool_output: None, continue_: None, stop_reason: None, system_message: None }
    }
}

pub fn parse_hook_result(event: HookEventName, stdout: &str, exit_code: Option<i32>) -> Result<ParsedHookResult, HookResultError> {
    let mut result = if stdout.trim().is_empty() {
        ParsedHookResult::default()
    } else {
        let value: Value = serde_json::from_str(stdout).map_err(|_| HookResultError::MalformedJson)?;
        parse_value(event, &value)
    };
    if exit_code == Some(2) && result.decision == ParsedDecision::Allow {
        result.decision = ParsedDecision::Deny;
        result.reason = Some("hook exited with blocking status".to_owned());
    }
    Ok(result)
}

fn parse_value(event: HookEventName, value: &Value) -> ParsedHookResult {
    let nested = value.get("hookSpecificOutput").and_then(Value::as_object);
    let nested_decision = nested.and_then(|nested| nested.get("permissionDecision")).and_then(Value::as_str);
    let top_decision = value.get("decision").and_then(Value::as_str);
    let decision = parse_decision(nested_decision.or(top_decision));
    let nested_reason = nested.and_then(|nested| nested.get("permissionDecisionReason")).and_then(Value::as_str);
    let reason = nested_reason.or_else(|| value.get("reason").and_then(Value::as_str)).map(|value| bounded_chars(value, MAX_REASON_CHARS));
    let mut result = ParsedHookResult {
        decision,
        reason,
        updated_input: nested.and_then(|nested| nested.get("updatedInput")).filter(|value| value.is_object()).cloned(),
        additional_context: nested.and_then(|nested| nested.get("additionalContext")).and_then(Value::as_str).map(|value| bounded_chars(value, MAX_FEEDBACK_CHARS)),
        updated_tool_output: nested.and_then(|nested| nested.get("updatedToolOutput")).filter(|value| serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= MAX_REPLACEMENT_CHARS)).cloned(),
        continue_: value.get("continue").and_then(Value::as_bool),
        stop_reason: value.get("stopReason").and_then(Value::as_str).map(|value| bounded_chars(value, MAX_REASON_CHARS)),
        system_message: value.get("systemMessage").and_then(Value::as_str).map(|value| bounded_chars(value, MAX_FEEDBACK_CHARS)),
    };
    match event.mode() {
        super::HookMode::Observe => {
            result.decision = ParsedDecision::Allow;
            result.reason = None;
            result.updated_input = None;
            result.additional_context = None;
            result.updated_tool_output = None;
            result.continue_ = None;
            result.stop_reason = None;
        }
        super::HookMode::Prompt => {
            result.updated_input = None;
            result.additional_context = None;
            result.updated_tool_output = None;
            result.continue_ = None;
        }
        super::HookMode::Tool => {
            result.updated_tool_output = None;
            result.continue_ = None;
            result.stop_reason = None;
        }
        super::HookMode::PostTool => {
            result.updated_input = None;
            result.continue_ = None;
            result.stop_reason = None;
        }
        super::HookMode::Stop => {
            result.updated_input = None;
            result.updated_tool_output = None;
        }
    }
    result
}

fn parse_decision(value: Option<&str>) -> ParsedDecision {
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("ask") => ParsedDecision::Ask,
        Some("defer") => ParsedDecision::Defer,
        Some("deny" | "block") => ParsedDecision::Deny,
        _ => ParsedDecision::Allow,
    }
}

fn bounded_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HookResultError {
    #[error("hook returned malformed JSON")]
    MalformedJson,
}
