use crate::{Auth, Model, build_request, send_request};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};

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
        Self {
            model,
            auth,
            client: reqwest::Client::new(),
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
        let body = send_request(&self.client, &request).await?;
        for piece in parse_stream_body(&body) {
            tx.send(piece)
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
        Ok(())
    }
}

pub fn parse_stream_body(body: &str) -> Vec<StreamPiece> {
    let mut pieces = Vec::new();
    for raw in body.lines() {
        let raw = raw.trim();
        let data = raw.strip_prefix("data:").map(str::trim).unwrap_or(raw);
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(text) = value
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("delta").and_then(|v| v.as_str()))
            .or_else(|| value.pointer("/delta/text").and_then(|v| v.as_str()))
        {
            pieces.push(StreamPiece::Text(text.into()));
        }
        if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
            pieces.push(StreamPiece::Text(text.into()));
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
