use lato_ai::classify_provider_failure;
use lato_core::ModelErrorKind;

#[test]
fn structured_context_length_error_is_typed() {
    let error = classify_provider_failure(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"maximum context length is 128000 tokens"}}"#,
    );
    assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
    assert_eq!(error.status_code, Some(400));
    assert_eq!(error.context_window, Some(128_000));
}

#[test]
fn structured_context_window_takes_priority() {
    let error = classify_provider_failure(
        413,
        r#"{"error":{"code":"prompt_too_long","message":"context window exceeded","context_window":200000}}"#,
    );
    assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
    assert_eq!(error.context_window, Some(200_000));
}

#[test]
fn generic_bad_request_is_not_context_overflow() {
    for body in [
        r#"{"error":{"message":"invalid tool schema"}}"#,
        r#"{"error":{"message":"max_output_tokens must be positive"}}"#,
        r#"{"error":{"code":"bad_request","message":"invalid request"}}"#,
    ] {
        let error = classify_provider_failure(400, body);
        assert_eq!(error.kind, ModelErrorKind::InvalidRequest, "{body}");
    }
}

#[test]
fn account_and_rate_failures_are_distinct() {
    assert_eq!(
        classify_provider_failure(401, "unauthorized").kind,
        ModelErrorKind::Authentication
    );
    assert_eq!(
        classify_provider_failure(402, "out of credits").kind,
        ModelErrorKind::Credit
    );
    assert_eq!(
        classify_provider_failure(429, "slow down").kind,
        ModelErrorKind::RateLimited
    );
}

#[test]
fn known_plain_text_overflow_is_supported_but_malformed_json_is_bounded() {
    let overflow = classify_provider_failure(
        400,
        "This model's maximum context length is 32768 tokens, but the prompt is too long",
    );
    assert_eq!(overflow.kind, ModelErrorKind::ContextOverflow);
    assert_eq!(overflow.context_window, Some(32_768));

    let malformed = classify_provider_failure(500, &"x".repeat(10_000));
    assert_eq!(malformed.kind, ModelErrorKind::Transport);
    assert_eq!(malformed.message.chars().count(), 4_096);
}
