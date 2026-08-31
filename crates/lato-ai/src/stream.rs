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
        stream_http_request(&self.client, &request, tx).await
    }
}

pub async fn stream_http_request(
    client: &reqwest::Client,
    request: &crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    let mut response = send_request_response(client, request).await?;
    let mut body = String::new();
    let mut line_start = 0usize;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        body.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(relative_end) = body[line_start..].find('\n') {
            let line_end = line_start + relative_end;
            for piece in parse_stream_body(&body[line_start..line_end]) {
                if matches!(piece, StreamPiece::Text(_)) {
                    tx.send(piece)
                        .await
                        .map_err(|_| "stream receiver closed".to_string())?;
                }
            }
            line_start = line_end + 1;
        }
    }
    if line_start < body.len() {
        for piece in parse_stream_body(&body[line_start..]) {
            if matches!(piece, StreamPiece::Text(_)) {
                tx.send(piece)
                    .await
                    .map_err(|_| "stream receiver closed".to_string())?;
            }
        }
    }
    for piece in parse_stream_body(&body) {
        if matches!(piece, StreamPiece::ToolCall { .. }) {
            tx.send(piece)
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
    }
    Ok(())
}

pub fn parse_stream_body(body: &str) -> Vec<StreamPiece> {
    let mut pieces = Vec::new();
    let mut pending_tools: std::collections::HashMap<String, (String, String, String)> =
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
        if let Some(text) = value
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
            .or_else(|| value.pointer("/delta/text").and_then(|v| v.as_str()))
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
                pending_tools.insert(key, (id, name, initial));
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
                if let Some((_, _, args)) = pending_tools.get_mut(&key) {
                    args.push_str(partial);
                }
            }
        } else if event_type == "content_block_stop" {
            let key = value
                .get("index")
                .map(ToString::to_string)
                .unwrap_or_else(|| "0".into());
            if let Some((id, name, args)) = pending_tools.remove(&key) {
                let arguments =
                    serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({}));
                if !name.is_empty() {
                    pieces.push(StreamPiece::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
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
                (
                    item.get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("call")
                        .into(),
                    item.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    item.get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                ),
            );
        } else if event_type == "response.function_call_arguments.delta" {
            let key = value
                .get("item_id")
                .or_else(|| value.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call");
            if let Some((_, _, args)) = pending_tools.get_mut(key) {
                args.push_str(value.get("delta").and_then(|v| v.as_str()).unwrap_or(""));
            }
        } else if event_type == "response.function_call_arguments.done" {
            let key = value
                .get("item_id")
                .or_else(|| value.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call");
            if let Some((id, name, accumulated)) = pending_tools.remove(key) {
                let raw = value
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&accumulated);
                let arguments = serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({}));
                if !name.is_empty() {
                    pieces.push(StreamPiece::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
        }
        if let Some(call) = value.pointer("/choices/0/delta/tool_calls/0") {
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("call")
                .to_string();
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = call
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            if !name.is_empty() {
                pieces.push(StreamPiece::ToolCall {
                    id,
                    name,
                    arguments: args,
                });
            }
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let pieces = parse_stream_body(fixture);
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
        let pieces = parse_stream_body(fixture);
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. } if name == "grep" && arguments["pattern"] == "TODO")
        );
    }

    #[test]
    fn b1_3_vcr_sse_fixture_parses_text_and_tool_call_offline() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture);
        assert!(matches!(&pieces[0], StreamPiece::Text(v) if v == "hello"));
        assert!(matches!(&pieces[1], StreamPiece::ToolCall { name, .. } if name == "read_file"));
    }
}
