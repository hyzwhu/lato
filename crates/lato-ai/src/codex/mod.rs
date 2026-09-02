pub(crate) mod events;
pub(crate) mod sse;
pub(crate) mod websocket;

use crate::StreamPiece;
use crate::{Auth, Model, responses_input, responses_tools};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransportOutcome {
    pub events_started: bool,
    pub terminal: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CodexTransportError {
    pub message: String,
    pub events_started: bool,
}

impl CodexTransportError {
    pub(crate) fn before_stream(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            events_started: false,
        }
    }

    pub(crate) fn with_mapper(
        message: impl Into<String>,
        mapper: &events::CodexEventMapper,
    ) -> Self {
        Self {
            message: message.into(),
            events_started: mapper.started(),
        }
    }
}

fn websocket_fallback_sessions() -> &'static Mutex<HashSet<String>> {
    static SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) async fn stream_codex(
    client: &reqwest::Client,
    request: &CodexRequest,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    let fallback_active = request.session_key.as_ref().is_some_and(|session| {
        websocket_fallback_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(session)
    });
    if fallback_active {
        return sse::stream_sse(client, request, tx)
            .await
            .map(|_| ())
            .map_err(|error| error.message);
    }
    let mut websocket_result = websocket::stream_websocket(request, tx.clone()).await;
    if websocket_result
        .as_ref()
        .is_err_and(|error| !error.events_started && websocket_retryable(&error.message))
    {
        websocket_result = websocket::stream_websocket(request, tx.clone()).await;
    }
    match websocket_result {
        Ok(_) => Ok(()),
        Err(error) if !error.events_started => {
            if let Some(session) = &request.session_key {
                websocket_fallback_sessions()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(session.clone());
            }
            sse::stream_sse(client, request, tx)
                .await
                .map(|_| ())
                .map_err(|sse_error| {
                    format!(
                        "Codex WebSocket failed before streaming ({}); SSE fallback failed: {}",
                        error.message, sse_error.message
                    )
                })
        }
        Err(error) => Err(error.message),
    }
}

fn websocket_retryable(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("websocket_connection_limit_reached")
        || message.contains("previous_response_not_found")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    pub session_key: Option<String>,
}

impl CodexRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub fn build_codex_request(
    model: &Model,
    auth: &Auth,
    context: &serde_json::Value,
) -> Result<CodexRequest, String> {
    let token = auth
        .api_key
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("OpenAI Codex requires an OAuth access token")?;
    let account_id = auth
        .account_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("OpenAI Codex credential is missing its ChatGPT account ID")?;
    let base = auth
        .base_url
        .as_deref()
        .or(model.base_url)
        .ok_or("missing base_url")?
        .trim_end_matches('/');
    let messages = context
        .get("messages")
        .cloned()
        .unwrap_or_else(|| context.clone());
    let (instructions, input_messages) = split_system_messages(messages);
    let tools = responses_tools(
        context
            .get("tools")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
    );
    let session_key = context
        .get("session_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(clamp_prompt_cache_key);
    let request_id = context
        .get("turn_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .or(session_key.as_deref());
    let mut headers = auth.headers.clone();
    for (key, value) in [
        ("authorization".into(), format!("Bearer {token}")),
        ("chatgpt-account-id".into(), account_id.into()),
        ("originator".into(), "lato".into()),
        (
            "user-agent".into(),
            format!("lato/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("openai-beta".into(), "responses=experimental".into()),
        ("accept".into(), "text/event-stream".into()),
        ("content-type".into(), "application/json".into()),
    ] {
        set_header(&mut headers, key, value);
    }
    if let Some(session_key) = &session_key {
        headers.push(("session-id".into(), session_key.clone()));
    }
    if let Some(request_id) = request_id {
        headers.push(("x-client-request-id".into(), request_id.into()));
    }
    let mut body = serde_json::json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": if instructions.is_empty() { "You are a helpful assistant." } else { &instructions },
        "input": responses_input(input_messages),
        "text": { "verbosity": context.get("text_verbosity").and_then(|value| value.as_str()).unwrap_or("low") },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": context.get("tool_choice").cloned().unwrap_or_else(|| serde_json::json!("auto")),
        "parallel_tool_calls": true,
    });
    if !tools.as_array().is_none_or(Vec::is_empty) {
        body["tools"] = tools;
    }
    if let Some(session_key) = &session_key {
        body["prompt_cache_key"] = serde_json::json!(session_key);
    }
    if let Some(effort) = context
        .get("reasoning_effort")
        .and_then(|value| value.as_str())
    {
        body["reasoning"] = serde_json::json!({
            "effort": effort,
            "summary": context.get("reasoning_summary").and_then(|value| value.as_str()).unwrap_or("auto")
        });
    }
    if let Some(temperature) = context.get("temperature") {
        body["temperature"] = temperature.clone();
    }
    Ok(CodexRequest {
        url: format!("{base}/codex/responses"),
        headers,
        body,
        session_key,
    })
}

fn set_header(headers: &mut Vec<(String, String)>, key: String, value: String) {
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&key));
    headers.push((key, value));
}

fn split_system_messages(messages: serde_json::Value) -> (String, serde_json::Value) {
    let Some(messages) = messages.as_array() else {
        return (String::new(), messages);
    };
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        if message.get("role").and_then(|value| value.as_str()) == Some("system") {
            if let Some(text) = message.get("content").and_then(|value| value.as_str()) {
                instructions.push(text.to_string());
            }
        } else {
            input.push(message.clone());
        }
    }
    (instructions.join("\n\n"), serde_json::Value::Array(input))
}

fn clamp_prompt_cache_key(value: &str) -> String {
    const MAX: usize = 64;
    value.chars().take(MAX).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelApi;

    #[test]
    fn builds_pi_compatible_codex_request_contract() {
        let model = Model {
            provider: "openai-codex",
            id: "gpt-5-codex",
            api: ModelApi::OpenaiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api"),
        };
        let auth = Auth {
            api_key: Some("secret".into()),
            account_id: Some("acct-7".into()),
            ..Default::default()
        };
        let request = build_codex_request(
            &model,
            &auth,
            &serde_json::json!({
                "messages": [
                    {"role":"system","content":"system text"},
                    {"role":"user","content":"hello"}
                ],
                "tools": [],
                "session_id": "session-7",
                "turn_id": "turn-2"
            }),
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(request.header("chatgpt-account-id"), Some("acct-7"));
        assert_eq!(request.header("originator"), Some("lato"));
        assert_eq!(request.header("session-id"), Some("session-7"));
        assert_eq!(request.header("x-client-request-id"), Some("turn-2"));
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["instructions"], "system text");
        assert_eq!(request.body["prompt_cache_key"], "session-7");
        assert_eq!(
            request.body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert_eq!(request.body["input"][0]["role"], "user");
    }

    #[test]
    fn rejects_incomplete_codex_oauth_identity() {
        let model = Model {
            provider: "openai-codex",
            id: "gpt-5-codex",
            api: ModelApi::OpenaiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api"),
        };
        assert!(build_codex_request(&model, &Auth::default(), &serde_json::json!([])).is_err());
    }
}
