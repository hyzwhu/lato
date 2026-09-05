pub(crate) mod events;
pub mod models;
pub(crate) mod sse;
pub(crate) mod websocket;

use crate::StreamPiece;
use crate::{Auth, Model, responses_tools};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransportOutcome {
    pub events_started: bool,
    pub terminal: bool,
    pub usage: Option<lato_core::ModelUsage>,
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
            message: safe_error_excerpt(&message.into()),
            events_started: false,
        }
    }

    pub(crate) fn with_mapper(
        message: impl Into<String>,
        mapper: &events::CodexEventMapper,
    ) -> Self {
        Self {
            message: safe_error_excerpt(&message.into()),
            events_started: mapper.started(),
        }
    }
}

fn safe_error_excerpt(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(4096)
        .collect()
}

fn websocket_fallback_sessions() -> &'static Mutex<HashSet<String>> {
    static SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub async fn stream_codex(
    client: &reqwest::Client,
    request: &CodexRequest,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    stream_codex_with_report(client, request, tx)
        .await
        .map(|_| ())
}

pub(crate) async fn stream_codex_with_report(
    client: &reqwest::Client,
    request: &CodexRequest,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<crate::ModelCallReport, String> {
    let fallback_active = request.session_key.as_ref().is_some_and(|session| {
        websocket_fallback_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(session)
    });
    if fallback_active {
        return sse::stream_sse(client, request, tx)
            .await
            .map(|outcome| crate::ModelCallReport {
                usage: outcome.usage,
                generation: 0,
            })
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
        Ok(outcome) => Ok(crate::ModelCallReport {
            usage: outcome.usage,
            generation: 0,
        }),
        Err(error) if !error.events_started => {
            if let Some(session) = &request.session_key {
                websocket_fallback_sessions()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(session.clone());
            }
            sse::stream_sse(client, request, tx)
                .await
                .map(|outcome| crate::ModelCallReport {
                    usage: outcome.usage,
                    generation: 0,
                })
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
    build_codex_request_for_model(model.id, model.base_url, auth, context)
}

pub(crate) fn build_codex_request_for_model(
    model_id: &str,
    base_url: Option<&str>,
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
        .or(base_url)
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
        "model": model_id,
        "store": false,
        "stream": true,
        "instructions": if instructions.is_empty() { "You are a helpful assistant." } else { &instructions },
        "input": codex_input(input_messages),
        "text": { "verbosity": context.get("text_verbosity").and_then(|value| value.as_str()).unwrap_or("low") },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": context.get("tool_choice").cloned().unwrap_or_else(|| serde_json::json!("auto")),
        "parallel_tool_calls": true,
    });
    if !tools.as_array().is_none_or(Vec::is_empty) {
        body["tools"] = serde_json::Value::Array(
            tools
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|mut tool| {
                    tool["strict"] = serde_json::Value::Null;
                    tool
                })
                .collect(),
        );
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

fn codex_input(messages: serde_json::Value) -> serde_json::Value {
    let Some(messages) = messages.as_array() else {
        return messages;
    };
    let mut input = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message.get("role").and_then(|value| value.as_str()) {
            Some("user") => input.push(serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": message.get("content").and_then(|value| value.as_str()).unwrap_or("")
                }]
            })),
            Some("assistant") => {
                if let Some(text) = message
                    .get("content")
                    .and_then(|value| value.as_str())
                    .filter(|text| !text.is_empty())
                {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type":"output_text","text":text,"annotations":[]}],
                        "status": "completed",
                        "id": format!("msg_lato_{index}")
                    }));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                {
                    let Some(function) = call.get("function") else {
                        continue;
                    };
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.get("id").cloned().unwrap_or_default(),
                        "name": function.get("name").cloned().unwrap_or_default(),
                        "arguments": function.get("arguments").cloned().unwrap_or_else(|| serde_json::json!("{}"))
                    }));
                }
            }
            Some("tool") => input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": message.get("tool_call_id").cloned().unwrap_or_default(),
                "output": message.get("content").cloned().unwrap_or_default()
            })),
            _ => input.push(message.clone()),
        }
    }
    serde_json::Value::Array(input)
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
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[test]
    fn builds_pi_compatible_codex_request_contract() {
        let model = Model {
            provider: "openai-codex",
            id: "gpt-5-codex",
            api: ModelApi::OpenaiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api"),
            context_window: None,
            model_family: None,
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
            context_window: None,
            model_family: None,
        };
        assert!(build_codex_request(&model, &Auth::default(), &serde_json::json!([])).is_err());
    }

    fn local_request(base_url: &'static str, session: &str) -> CodexRequest {
        build_codex_request(
            &Model {
                provider: "openai-codex",
                id: "gpt-5-codex",
                api: ModelApi::OpenaiCodexResponses,
                base_url: Some(base_url),
                context_window: None,
                model_family: None,
            },
            &Auth {
                api_key: Some("token".into()),
                account_id: Some("acct".into()),
                ..Default::default()
            },
            &serde_json::json!({"messages":[],"session_id":session,"turn_id":"turn"}),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn codex_falls_back_to_sse_only_before_stream_start() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let _ = websocket.next().await;
            websocket.close(None).await.unwrap();

            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fallback-ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let base_url: &'static str = Box::leak(format!("http://{address}").into_boxed_str());
        let request = local_request(base_url, "fallback-before-stream-test");
        let (tx, mut rx) = mpsc::channel(2);
        stream_codex(&crate::http_client_for_url(&request.url), &request, tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "fallback-ok"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_does_not_replay_after_a_websocket_event() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let _ = websocket.next().await;
            websocket
                .send(Message::Text(
                    r#"{"type":"response.output_text.delta","delta":"started"}"#.into(),
                ))
                .await
                .unwrap();
            websocket.close(None).await.unwrap();
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok()
        });
        let base_url: &'static str = Box::leak(format!("http://{address}").into_boxed_str());
        let request = local_request(base_url, "fallback-after-stream-test");
        let (tx, mut rx) = mpsc::channel(2);
        let error = stream_codex(&crate::http_client_for_url(&request.url), &request, tx)
            .await
            .unwrap_err();
        assert!(error.contains("closed"));
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "started"));
        assert!(!server.await.unwrap(), "SSE replay must not be attempted");
    }
}
