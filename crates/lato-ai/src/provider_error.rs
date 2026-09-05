// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs
// License: Apache-2.0
// Lato changes: provider-neutral structured HTTP failure classification with bounded text compatibility

use lato_core::{ModelError, ModelErrorKind, Retryability};

const CONTEXT_CODES: &[&str] = &[
    "context_length_exceeded",
    "context_window_exceeded",
    "prompt_too_long",
    "request_too_large",
];

pub fn classify_provider_failure(status: u16, body: &str) -> ModelError {
    let value = serde_json::from_str::<serde_json::Value>(body).ok();
    let code = json_string(value.as_ref(), &["/error/code", "/code"]);
    let error_type = json_string(value.as_ref(), &["/error/type", "/type"]);
    let structured_message = json_string(value.as_ref(), &["/error/message", "/message"]);
    let message = bounded_message(structured_message.as_deref().unwrap_or(body));
    let combined = format!(
        "{} {} {}",
        code.as_deref().unwrap_or_default(),
        error_type.as_deref().unwrap_or_default(),
        message
    )
    .to_ascii_lowercase();

    let kind = if CONTEXT_CODES.iter().any(|needle| {
        code.as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(needle))
            || error_type
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(needle))
    }) || looks_like_context_overflow(&combined)
    {
        ModelErrorKind::ContextOverflow
    } else if status == 401 || combined.contains("unauthorized") {
        ModelErrorKind::Authentication
    } else if status == 402 || looks_like_credit_failure(&combined) {
        ModelErrorKind::Credit
    } else if status == 429 {
        ModelErrorKind::RateLimited
    } else if matches!(status, 400 | 404 | 409 | 413 | 422) {
        ModelErrorKind::InvalidRequest
    } else if status >= 500 || status == 408 {
        ModelErrorKind::Transport
    } else {
        ModelErrorKind::Other
    };

    let stable_code = match kind {
        ModelErrorKind::ContextOverflow => "model.context_overflow",
        ModelErrorKind::Authentication => "model.authentication",
        ModelErrorKind::Credit => "model.credit_exhausted",
        ModelErrorKind::RateLimited => "model.rate_limited",
        ModelErrorKind::InvalidRequest => "model.invalid_request",
        ModelErrorKind::Transport => "model.transport",
        ModelErrorKind::Cancelled => "model.cancelled",
        ModelErrorKind::Other => "model.provider_failed",
    };
    let retryability = match kind {
        ModelErrorKind::RateLimited | ModelErrorKind::Transport => Retryability::AfterBackoff,
        _ => Retryability::Never,
    };
    let mut error = ModelError::new(stable_code, message, retryability)
        .with_kind(kind)
        .with_status(status);
    if kind == ModelErrorKind::ContextOverflow
        && let Some(window) = structured_context_window(value.as_ref())
            .or_else(|| context_window_from_text(&combined))
    {
        error = error.with_context_window(window);
    }
    error
}

pub(crate) fn classify_transport_failure(message: impl Into<String>) -> ModelError {
    let message = message.into();
    if looks_like_context_overflow(&message.to_ascii_lowercase()) {
        let mut error = ModelError::new(
            "model.context_overflow",
            bounded_message(&message),
            Retryability::Never,
        )
        .with_kind(ModelErrorKind::ContextOverflow);
        if let Some(window) = context_window_from_text(&message.to_ascii_lowercase()) {
            error = error.with_context_window(window);
        }
        error
    } else {
        ModelError::new(
            "model.stream_interrupted",
            bounded_message(&message),
            Retryability::AfterBackoff,
        )
        .with_kind(ModelErrorKind::Transport)
    }
}

fn json_string(value: Option<&serde_json::Value>, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        value?
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    })
}

fn structured_context_window(value: Option<&serde_json::Value>) -> Option<u64> {
    [
        "/error/context_window",
        "/error/context_length",
        "/context_window",
        "/context_length",
    ]
    .iter()
    .find_map(|pointer| value?.pointer(pointer).and_then(serde_json::Value::as_u64))
    .filter(|window| *window > 0)
}

fn looks_like_context_overflow(value: &str) -> bool {
    let context_noun = value.contains("context length")
        || value.contains("context window")
        || value.contains("prompt tokens")
        || value.contains("prompt is too long");
    let exceeded = value.contains("exceed")
        || value.contains("too long")
        || value.contains("too large")
        || value.contains("maximum");
    context_noun && exceeded && !value.contains("max_output_tokens")
}

fn looks_like_credit_failure(value: &str) -> bool {
    [
        "spending limit",
        "spending-limit",
        "out of credits",
        "usage balance exhausted",
        "usage limit reached",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn context_window_from_text(value: &str) -> Option<u64> {
    if !looks_like_context_overflow(value) {
        return None;
    }
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .filter(|number| *number >= 1_024)
        .max()
}

fn bounded_message(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(4_096)
        .collect()
}
