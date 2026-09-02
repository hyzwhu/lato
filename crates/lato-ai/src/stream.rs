use crate::{Auth, Model, build_request, http_client_for_url, send_request_response};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

pub const CONTEXT_HARD_LIMIT_BYTES: usize = 512_000;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum StreamPiece {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StreamError(pub String);

#[async_trait]
pub trait ModelStream: Send + Sync {
    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String>;
}

pub struct SwitchableModelStream {
    inner: RwLock<Arc<dyn ModelStream>>,
}

impl SwitchableModelStream {
    pub fn new(initial: Arc<dyn ModelStream>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }
    pub async fn set(&self, stream: Arc<dyn ModelStream>) {
        *self.inner.write().await = stream;
    }
}

#[async_trait]
impl ModelStream for SwitchableModelStream {
    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        let stream = self.inner.read().await.clone();
        stream.stream(prompt_bytes, context, tx).await
    }
}

pub struct FakeModelStream {
    pub script: Mutex<Vec<Vec<StreamPiece>>>,
}
impl FakeModelStream {
    pub fn new(script: Vec<Vec<StreamPiece>>) -> Self {
        Self {
            script: Mutex::new(script),
        }
    }
}

#[async_trait]
impl ModelStream for FakeModelStream {
    async fn stream(
        &self,
        prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err("context exceeds hard limit; compact not implemented".into());
        }
        let next = {
            let mut s = self.script.lock().await;
            if s.is_empty() {
                vec![StreamPiece::Text("ok".into())]
            } else {
                s.remove(0)
            }
        };
        for p in next {
            let _ = tx.send(p).await;
        }
        Ok(())
    }
}

pub struct HttpModelStream {
    model: Model,
    auth: Auth,
    client: reqwest::Client,
}

impl HttpModelStream {
    pub fn new(model: Model, auth: Auth) -> Self {
        let client = {
            let url = auth
                .base_url
                .as_deref()
                .or(model.base_url)
                .unwrap_or_default();
            http_client_for_url(url)
        };
        Self {
            model,
            auth,
            client,
        }
    }
}

#[async_trait]
impl ModelStream for HttpModelStream {
    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err("context exceeds hard limit; compact required".into());
        }
        let request = build_request(&self.model, &self.auth, context)?;
        stream_http_request_with_tool_choice_fallback(&self.client, request, tx).await
    }
}

pub(crate) async fn stream_http_request_with_tool_choice_fallback(
    client: &reqwest::Client,
    mut request: crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    match stream_http_request(client, &request, tx.clone()).await {
        Ok(()) => Ok(()),
        Err(error) if tool_choice_required_rejected(&error, &request) => {
            request.body["tool_choice"] = serde_json::json!("auto");
            stream_http_request(client, &request, tx).await
        }
        Err(error) => Err(error),
    }
}

fn tool_choice_required_rejected(error: &str, request: &crate::HttpRequestSpec) -> bool {
    let lower = error.to_ascii_lowercase();
    request.body.get("tool_choice").and_then(|v| v.as_str()) == Some("required")
        && lower.contains("http 400")
        && (lower.contains("tool_choice") || lower.contains("tool choice"))
}

pub async fn stream_http_request(
    client: &reqwest::Client,
    request: &crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    let mut response = send_request_response(client, request).await?;
    let mut body = Vec::<u8>::new();
    let mut line_start = 0usize;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        body.extend_from_slice(&chunk);
        while let Some(relative_end) = body[line_start..].iter().position(|byte| *byte == b'\n') {
            let line_end = line_start + relative_end;
            let line = std::str::from_utf8(&body[line_start..line_end])
                .map_err(|_| "model response was not valid UTF-8".to_string())?;
            for piece in parse_stream_text_line(line) {
                tx.send(piece)
                    .await
                    .map_err(|_| "stream receiver closed".to_string())?;
            }
            line_start = line_end + 1;
        }
    }
    let body =
        std::str::from_utf8(&body).map_err(|_| "model response was not valid UTF-8".to_string())?;
    if line_start < body.len() {
        for piece in parse_stream_text_line(&body[line_start..]) {
            tx.send(piece)
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
    }
    for piece in parse_stream_body(body)? {
        if matches!(piece, StreamPiece::ToolCall { .. }) {
            tx.send(piece)
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn json_str_non_empty<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    value
        .pointer(pointer)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn parse_stream_text_line(line: &str) -> Vec<StreamPiece> {
    let raw = line.trim();
    let data = raw.strip_prefix("data:").map(str::trim).unwrap_or(raw);
    if data.is_empty() || data == "[DONE]" {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let event_type = value
        .get("type")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let text = json_str_non_empty(&value, "/choices/0/delta/content")
        .or_else(|| json_str_non_empty(&value, "/choices/0/message/content"))
        .or_else(|| json_str_non_empty(&value, "/delta/text"))
        .or_else(|| {
            (event_type == "response.output_text.delta")
                .then(|| value.get("delta").and_then(|item| item.as_str()))
                .flatten()
        })
        .or_else(|| value.get("text").and_then(|item| item.as_str()));
    text.map(|text| vec![StreamPiece::Text(text.into())])
        .unwrap_or_default()
}

fn append_tool_arguments(pending: &mut PendingToolCall, arguments: Option<&serde_json::Value>) {
    match arguments {
        Some(serde_json::Value::String(raw)) => pending.arguments.push_str(raw),
        Some(other) if !other.is_null() => {
            pending.arguments = other.to_string();
        }
        _ => {}
    }
}

fn parse_tool_arguments(raw: &str, call_id: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|_| format!("invalid tool arguments for {call_id}"))
}

fn drain_complete_tool_calls(
    pending_tools: &mut std::collections::HashMap<String, PendingToolCall>,
    pieces: &mut Vec<StreamPiece>,
) -> Result<(), String> {
    let mut keys = pending_tools
        .iter()
        .filter_map(|(key, call)| (!call.name.is_empty()).then_some(key.clone()))
        .collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        if let Some(call) = pending_tools.remove(&key) {
            let call_id = if call.id.is_empty() { key } else { call.id };
            let arguments = parse_tool_arguments(&call.arguments, &call_id)?;
            pieces.push(StreamPiece::ToolCall {
                id: call_id,
                name: call.name,
                arguments,
            });
        }
    }
    Ok(())
}

pub fn parse_stream_body(body: &str) -> Result<Vec<StreamPiece>, String> {
    let mut pieces = Vec::new();
    let mut pending_tools: std::collections::HashMap<String, PendingToolCall> =
        std::collections::HashMap::new();
    for raw in body.lines() {
        let raw = raw.trim();
        let data = raw.strip_prefix("data:").map(str::trim).unwrap_or(raw);
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(text) = json_str_non_empty(&value, "/choices/0/delta/content")
            .or_else(|| json_str_non_empty(&value, "/choices/0/message/content"))
            .or_else(|| json_str_non_empty(&value, "/delta/text"))
            .or_else(|| {
                (event_type == "response.output_text.delta")
                    .then(|| value.get("delta").and_then(|v| v.as_str()))
                    .flatten()
            })
        {
            pieces.push(StreamPiece::Text(text.into()));
        }
        if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
            pieces.push(StreamPiece::Text(text.into()));
        }
        if event_type == "content_block_start" {
            if let Some(block) = value
                .get("content_block")
                .filter(|v| v.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            {
                let key = value
                    .get("index")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "0".into());
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("call")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let initial = block
                    .get("input")
                    .filter(|v| {
                        !v.is_null() && v.as_object().is_none_or(|object| !object.is_empty())
                    })
                    .map(ToString::to_string)
                    .unwrap_or_default();
                pending_tools.insert(
                    key,
                    PendingToolCall {
                        id,
                        name,
                        arguments: initial,
                    },
                );
            }
        } else if event_type == "content_block_delta" {
            if let Some(partial) = value
                .pointer("/delta/partial_json")
                .and_then(|v| v.as_str())
            {
                let key = value
                    .get("index")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "0".into());
                if let Some(call) = pending_tools.get_mut(&key) {
                    call.arguments.push_str(partial);
                }
            }
        } else if event_type == "content_block_stop" {
            let key = value
                .get("index")
                .map(ToString::to_string)
                .unwrap_or_else(|| "0".into());
            if let Some(call) = pending_tools.remove(&key)
                && !call.name.is_empty()
            {
                let arguments = parse_tool_arguments(&call.arguments, &call.id)?;
                pieces.push(StreamPiece::ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                });
            }
        } else if event_type == "response.output_item.added"
            && value.pointer("/item/type").and_then(|v| v.as_str()) == Some("function_call")
        {
            let item = &value["item"];
            let key = item
                .get("id")
                .or_else(|| item.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call")
                .to_string();
            pending_tools.insert(
                key.clone(),
                PendingToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("call")
                        .into(),
                    name: item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    arguments: item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                },
            );
        } else if event_type == "response.function_call_arguments.delta" {
            let key = value
                .get("item_id")
                .or_else(|| value.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call");
            if let Some(call) = pending_tools.get_mut(key) {
                call.arguments
                    .push_str(value.get("delta").and_then(|v| v.as_str()).unwrap_or(""));
            }
        } else if event_type == "response.function_call_arguments.done" {
            let key = value
                .get("item_id")
                .or_else(|| value.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call");
            if let Some(call) = pending_tools.remove(key) {
                let raw = value
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&call.arguments);
                if !call.name.is_empty() {
                    let arguments = parse_tool_arguments(raw, &call.id)?;
                    pieces.push(StreamPiece::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments,
                    });
                }
            }
        }
        if let Some(calls) = value
            .pointer("/choices/0/delta/tool_calls")
            .or_else(|| value.pointer("/choices/0/message/tool_calls"))
            .and_then(|v| v.as_array())
        {
            for (position, call) in calls.iter().enumerate() {
                let key = call
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| position.to_string());
                let pending = pending_tools.entry(key.clone()).or_default();
                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    pending.id = id.to_string();
                }
                if let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str()) {
                    pending.name = name.to_string();
                }
                append_tool_arguments(pending, call.pointer("/function/arguments"));
            }
        }
        if let Some(function) = value
            .pointer("/choices/0/delta/function_call")
            .or_else(|| value.pointer("/choices/0/message/function_call"))
        {
            let pending = pending_tools.entry("legacy".to_string()).or_default();
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                pending.name = name.to_string();
            }
            append_tool_arguments(pending, function.get("arguments"));
        }
        if matches!(
            value
                .pointer("/choices/0/finish_reason")
                .and_then(|v| v.as_str()),
            Some("tool_calls") | Some("function_call")
        ) {
            drain_complete_tool_calls(&mut pending_tools, &mut pieces)?;
        }
    }
    drain_complete_tool_calls(&mut pending_tools, &mut pieces)?;
    if !pieces
        .iter()
        .any(|piece| matches!(piece, StreamPiece::ToolCall { .. }))
    {
        let concatenated: String = pieces
            .iter()
            .filter_map(|piece| match piece {
                StreamPiece::Text(text) => Some(text.as_str()),
                StreamPiece::ToolCall { .. } => None,
            })
            .collect();
        pieces.extend(extract_text_embedded_tool_calls(&concatenated));
    }
    Ok(pieces)
}

pub fn extract_text_embedded_tool_calls(text: &str) -> Vec<StreamPiece> {
    let mut pieces = Vec::new();
    let mut rest = text;
    let mut index = 0usize;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let Some(end) = after.find("</tool_call>") else {
            break;
        };
        let inner = after[..end].trim();
        rest = &after[end + "</tool_call>".len()..];
        if let Some((name, arguments)) = parse_embedded_tool_call(inner) {
            index += 1;
            pieces.push(StreamPiece::ToolCall {
                id: format!("text-tool-{index}"),
                name,
                arguments,
            });
        }
    }
    pieces
}

fn parse_embedded_tool_call(inner: &str) -> Option<(String, serde_json::Value)> {
    if inner.starts_with('{') {
        let value = serde_json::from_str::<serde_json::Value>(inner).ok()?;
        let name = value.get("name")?.as_str()?.to_string();
        let arguments = match value.get("arguments") {
            Some(serde_json::Value::String(raw)) => parse_tool_arguments(raw, "embedded").ok()?,
            Some(other) => other.clone(),
            None => serde_json::json!({}),
        };
        return Some((name, arguments));
    }
    let name_end = inner.find("<arg_key>").unwrap_or(inner.len());
    let name = inner[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut arguments = serde_json::Map::new();
    let mut cursor = inner.get(name_end..).unwrap_or("");
    while let Some(key_start) = cursor.find("<arg_key>") {
        cursor = &cursor[key_start + "<arg_key>".len()..];
        let key_end = cursor.find("</arg_key>")?;
        let key = cursor[..key_end].trim().to_string();
        cursor = &cursor[key_end + "</arg_key>".len()..];
        let value_start = cursor.find("<arg_value>")?;
        cursor = &cursor[value_start + "<arg_value>".len()..];
        let value_end = cursor.find("</arg_value>")?;
        let raw = &cursor[..value_end];
        cursor = &cursor[value_end + "</arg_value>".len()..];
        let parsed = serde_json::from_str::<serde_json::Value>(raw)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
        arguments.insert(key, parsed);
    }
    Some((name, serde_json::Value::Object(arguments)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpRequestSpec;

    #[test]
    fn malformed_structured_tool_arguments_are_rejected() {
        let body = r#"data: {"choices":[{"message":{"tool_calls":[{"id":"call-bad","type":"function","function":{"name":"write_file","arguments":"{"}}]},"finish_reason":"tool_calls"}]}"#;
        let error = parse_stream_body(body).unwrap_err();
        assert_eq!(error, "invalid tool arguments for call-bad");
    }

    #[tokio::test]
    async fn http_stream_reassembles_utf8_split_across_network_chunks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n".as_bytes();
            let split = event.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
            for part in [&event[..split], &event[split..]] {
                socket
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(part).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let request = HttpRequestSpec {
            method: "POST",
            url: format!("http://{address}"),
            headers: vec![],
            body: serde_json::json!({}),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        stream_http_request(&http_client_for_url(&request.url), &request, tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "你"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_emits_first_delta_before_response_finishes() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).unwrap();
            let first = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
            let second =
                "data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\ndata: [DONE]\n\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}", first.len() + second.len()).unwrap();
            socket.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(1));
            socket.write_all(second.as_bytes()).unwrap();
        });
        let base_url: &'static str = Box::leak(format!("http://{address}/v1").into_boxed_str());
        let stream = HttpModelStream::new(
            Model {
                provider: "fixture",
                id: "model",
                api: crate::ModelApi::OpenaiCompletions,
                base_url: Some(base_url),
            },
            Auth {
                api_key: Some("key".into()),
                ..Default::default()
            },
        );
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(async move { stream.stream(1, serde_json::json!([]), tx).await });
        let first = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, StreamPiece::Text(text) if text == "first"));
        task.await.unwrap().unwrap();
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "second"));
    }

    #[tokio::test]
    async fn a2_6_oversize_fails_without_truncate() {
        let fake = FakeModelStream::new(vec![]);
        let (tx, _rx) = mpsc::channel(1);
        let err = fake
            .stream(CONTEXT_HARD_LIMIT_BYTES + 1, serde_json::json!([]), tx)
            .await
            .unwrap_err();
        assert!(err.contains("compact"));
    }

    #[test]
    fn b1_3_anthropic_tool_use_fixture_is_assembled() {
        let fixture = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"read_file\",\"input\":{}}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. } if name == "read_file" && arguments["path"] == "a.txt")
        );
    }

    #[test]
    fn b1_3_openai_responses_function_call_fixture_is_assembled() {
        let fixture = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"grep\",\"arguments\":\"\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"pattern\\\":\\\"TODO\\\"}\"}\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item-1\"}\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. } if name == "grep" && arguments["pattern"] == "TODO")
        );
    }

    #[test]
    fn openai_chat_non_stream_message_content_and_tool_call_parse() {
        let fixture = r#"{"choices":[{"message":{"content":"done","tool_calls":[{"id":"c1","type":"function","function":{"name":"run_terminal_command","arguments":"{\"command\":\"pwd\"}"}}]}}]}"#;
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(matches!(&pieces[0], StreamPiece::Text(text) if text == "done"));
        assert!(
            matches!(&pieces[1], StreamPiece::ToolCall { id, name, arguments }
                if id == "c1" && name == "run_terminal_command" && arguments["command"] == "pwd")
        );
    }

    #[test]
    fn openai_chat_reasoning_content_is_not_user_visible_text() {
        let fixture =
            r#"{"choices":[{"message":{"reasoning_content":"thinking text","content":""}}]}"#;
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            pieces.is_empty(),
            "reasoning must not be printed: {pieces:?}"
        );
    }

    #[test]
    fn openai_chat_delta_reasoning_content_is_not_user_visible_text() {
        let fixture = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"The user wants me to write a file.\"}}]}\n";
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            pieces.is_empty(),
            "reasoning must not be printed: {pieces:?}"
        );
    }

    #[test]
    fn openai_chat_tool_call_chunks_are_assembled_before_dispatch() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_terminal_command\",\"arguments\":\"\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"comm\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert_eq!(pieces.len(), 1, "pieces={pieces:?}");
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { id, name, arguments }
                if id == "c1" && name == "run_terminal_command" && arguments["command"] == "pwd")
        );
    }

    #[test]
    fn openai_chat_tool_call_is_not_emitted_with_empty_arguments_from_first_delta() {
        let fixture = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"run_terminal_command\",\"arguments\":\"\"}}]}}]}\n";
        let pieces = parse_stream_body(fixture).unwrap();
        assert_eq!(pieces.len(), 1, "whole-body parser flushes once at end");
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. }
                if name == "run_terminal_command" && arguments.as_object().is_some_and(|o| o.is_empty()))
        );
    }

    #[test]
    fn openai_chat_object_arguments_and_legacy_function_call_parse() {
        let object_args = r#"{"choices":[{"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"write_file","arguments":{"path":"hello.go","contents":"package main"}}}]},"finish_reason":"tool_calls"}]}"#;
        let pieces = parse_stream_body(object_args).unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. }
                if name == "write_file" && arguments["path"] == "hello.go" && arguments["contents"] == "package main"),
            "pieces={pieces:?}"
        );

        let legacy = r#"{"choices":[{"delta":{"function_call":{"name":"write_file","arguments":"{\"path\":\"hello.go\"}"}}}]}"#;
        let pieces = parse_stream_body(legacy).unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. }
                if name == "write_file" && arguments["path"] == "hello.go"),
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn required_tool_choice_http_400_is_retryable() {
        let request = HttpRequestSpec {
            method: "POST",
            url: "https://token.sensenova.cn/v1/chat/completions".into(),
            headers: vec![],
            body: serde_json::json!({"tool_choice":"required"}),
        };
        assert!(tool_choice_required_rejected(
            "http 400: invalid tool_choice",
            &request
        ));
        assert!(!tool_choice_required_rejected(
            "http 401: unauthorized",
            &request
        ));
    }

    #[test]
    fn unrelated_http_400_does_not_trigger_tool_choice_fallback() {
        let request = HttpRequestSpec {
            method: "POST",
            url: "http://example.invalid".into(),
            headers: vec![],
            body: serde_json::json!({"tool_choice":"required"}),
        };
        assert!(!tool_choice_required_rejected(
            "http 400: invalid model",
            &request
        ));
        assert!(tool_choice_required_rejected(
            "http 400: unsupported tool_choice required",
            &request
        ));
    }

    #[test]
    fn glm_xml_tool_call_in_content_is_extracted() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<tool_call>write_file\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"<arg_key>path</arg_key><arg_value>hello.go</arg_value>\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"<arg_key>contents</arg_key><arg_value>package main</arg_value></tool_call>\"}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            pieces.iter().any(|piece| matches!(
                piece,
                StreamPiece::ToolCall { name, arguments, .. }
                    if name == "write_file" && arguments["path"] == "hello.go" && arguments["contents"] == "package main"
            )),
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn glm_json_tool_call_xml_wrapper_is_extracted() {
        let pieces = extract_text_embedded_tool_calls(
            "<tool_call>{\"name\":\"write_file\",\"arguments\":{\"path\":\"hello.go\",\"contents\":\"package main\"}}</tool_call>",
        );
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. }
                if name == "write_file" && arguments["path"] == "hello.go")
        );
    }

    #[test]
    fn b1_3_vcr_sse_fixture_parses_text_and_tool_call_offline() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(matches!(&pieces[0], StreamPiece::Text(v) if v == "hello"));
        assert!(matches!(&pieces[1], StreamPiece::ToolCall { name, .. } if name == "read_file"));
    }
}
