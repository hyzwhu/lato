use crate::{
    ActiveModelPort, Auth, Model, ModelApi, build_request, http_client_for_url,
    send_sampling_response,
};
use async_trait::async_trait;
use lato_core::{ModelError, ModelErrorKind, Retryability};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub const CONTEXT_HARD_LIMIT_BYTES: usize = 512_000;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCallReport {
    pub usage: Option<lato_core::ModelUsage>,
    pub generation: u64,
}

#[async_trait]
pub trait ModelStream: Send + Sync {
    fn active_model_port(&self) -> Option<ActiveModelPort> {
        None
    }

    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError>;

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        self.stream(prompt_bytes, context, tx).await?;
        Ok(ModelCallReport::default())
    }
}

pub struct SwitchableModelStream {
    inner: std::sync::RwLock<crate::ActiveModelStream>,
    generation: std::sync::atomic::AtomicU64,
}

impl SwitchableModelStream {
    pub fn new(mut initial: crate::ActiveModelStream) -> Self {
        initial.port.generation = 0;
        Self {
            inner: std::sync::RwLock::new(initial),
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn from_stream(stream: Arc<dyn ModelStream>) -> Self {
        let endpoint = if let Some(port) = stream.active_model_port() {
            crate::ActiveModelStream { stream, port }
        } else {
            crate::adapt_model_endpoint(
                "openai",
                "gpt-4.1",
                crate::ModelMetadata::default(),
                stream,
            )
            .expect("fallback model selection is statically valid")
        };
        Self::new(endpoint)
    }

    pub async fn set_active(&self, mut endpoint: crate::ActiveModelStream) {
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);
        endpoint.port.generation = generation;
        *self.inner.write().expect("model stream lock poisoned") = endpoint;
    }

    pub fn snapshot(&self) -> crate::ActiveModelStream {
        self.inner
            .read()
            .expect("model stream lock poisoned")
            .clone()
    }

    pub async fn set(&self, stream: Arc<dyn ModelStream>) {
        let endpoint = if let Some(port) = stream.active_model_port() {
            crate::ActiveModelStream { stream, port }
        } else {
            let current = self
                .inner
                .read()
                .expect("model stream lock poisoned")
                .port
                .clone();
            crate::adapt_model_endpoint(
                &current.selection.provider,
                &current.selection.model,
                current.metadata,
                stream,
            )
            .expect("active model selection remains valid")
        };
        self.set_active(endpoint).await;
    }
}

#[async_trait]
impl ModelStream for SwitchableModelStream {
    fn active_model_port(&self) -> Option<ActiveModelPort> {
        Some(
            self.inner
                .read()
                .expect("model stream lock poisoned")
                .port
                .clone(),
        )
    }

    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        let stream = self
            .inner
            .read()
            .expect("model stream lock poisoned")
            .stream
            .clone();
        stream.stream(prompt_bytes, context, tx).await
    }

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        let endpoint = self
            .inner
            .read()
            .expect("model stream lock poisoned")
            .clone();
        let mut report = endpoint
            .stream
            .stream_with_report(prompt_bytes, context, tx)
            .await?;
        report.generation = endpoint.port.generation;
        Ok(report)
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
    ) -> Result<(), ModelError> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err(context_overflow_error(
                "context exceeds hard limit; compact not implemented",
            ));
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
    ) -> Result<(), ModelError> {
        self.stream_with_report(prompt_bytes, context, tx)
            .await
            .map(|_| ())
    }

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err(context_overflow_error(
                "context exceeds hard limit; compact required",
            ));
        }
        if self.model.api == ModelApi::OpenaiCodexResponses {
            let request = crate::codex::build_codex_request(&self.model, &self.auth, &context)
                .map_err(legacy_model_error)?;
            crate::codex::stream_codex_with_report(&self.client, &request, tx).await
        } else {
            let request =
                build_request(&self.model, &self.auth, context).map_err(legacy_model_error)?;
            stream_http_request_with_tool_choice_fallback_with_report(&self.client, request, tx)
                .await
        }
    }
}

pub async fn stream_http_request_with_tool_choice_fallback(
    client: &reqwest::Client,
    request: crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), ModelError> {
    stream_http_request_with_tool_choice_fallback_with_report(client, request, tx)
        .await
        .map(|_| ())
}

pub(crate) async fn stream_http_request_with_tool_choice_fallback_with_report(
    client: &reqwest::Client,
    mut request: crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<ModelCallReport, ModelError> {
    match stream_http_request_with_report(client, &request, tx.clone()).await {
        Ok(report) => Ok(report),
        Err(error) if tool_choice_required_rejected(&error, &request) => {
            request.body["tool_choice"] = serde_json::json!("auto");
            stream_http_request_with_report(client, &request, tx).await
        }
        Err(error) => Err(error),
    }
}

fn tool_choice_required_rejected(error: &ModelError, request: &crate::HttpRequestSpec) -> bool {
    let lower = error.message.to_ascii_lowercase();
    request.body.get("tool_choice").and_then(|v| v.as_str()) == Some("required")
        && (error.status_code == Some(400) || lower.contains("http 400"))
        && (lower.contains("tool_choice") || lower.contains("tool choice"))
}

pub async fn stream_http_request(
    client: &reqwest::Client,
    request: &crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<(), ModelError> {
    stream_http_request_with_report(client, request, tx)
        .await
        .map(|_| ())
}

pub async fn stream_http_request_with_report(
    client: &reqwest::Client,
    request: &crate::HttpRequestSpec,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<ModelCallReport, ModelError> {
    let mut response = send_sampling_response(client, request).await?;
    let mut buffered = Vec::<u8>::new();
    let mut decoder = WireDecoder::default();
    let mut parser = ModelEventParser::default();
    'read: loop {
        let chunk = tokio::select! {
            _ = tx.closed() => return Err(legacy_model_error("stream receiver closed")),
            chunk = response.chunk() => chunk.map_err(legacy_model_error)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        buffered.extend_from_slice(&chunk);
        let mut start = 0;
        while let Some(end) = buffered[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + end;
            let line = std::str::from_utf8(&buffered[start..end])
                .map_err(|_| legacy_model_error("model response was not valid UTF-8"))?;
            if let Some(value) = decoder.line(line).map_err(legacy_model_error)? {
                send_pieces(&tx, parser.accept(value).map_err(legacy_model_error)?).await?;
            }
            start = end + 1;
            if decoder.terminal || parser.terminal {
                buffered.clear();
                break 'read;
            }
        }
        buffered.drain(..start);
    }
    if !buffered.is_empty() {
        let line = std::str::from_utf8(&buffered)
            .map_err(|_| legacy_model_error("model response was not valid UTF-8"))?;
        if let Some(value) = decoder.line(line).map_err(legacy_model_error)? {
            send_pieces(&tx, parser.accept(value).map_err(legacy_model_error)?).await?;
        }
    }
    if let Some(value) = decoder.finish().map_err(legacy_model_error)? {
        send_pieces(&tx, parser.accept(value).map_err(legacy_model_error)?).await?;
    }
    send_pieces(&tx, parser.finish().map_err(legacy_model_error)?).await?;
    Ok(ModelCallReport {
        usage: parser.usage,
        generation: 0,
    })
}

async fn send_pieces(
    tx: &mpsc::Sender<StreamPiece>,
    pieces: Vec<StreamPiece>,
) -> Result<(), ModelError> {
    for piece in pieces {
        tx.send(piece)
            .await
            .map_err(|_| legacy_model_error("stream receiver closed"))?;
    }
    Ok(())
}

fn legacy_model_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::new(
        "model.stream_interrupted",
        error.to_string(),
        Retryability::AfterBackoff,
    )
    .with_kind(ModelErrorKind::Transport)
}

fn context_overflow_error(message: impl Into<String>) -> ModelError {
    ModelError::new("model.context_overflow", message, Retryability::Never)
        .with_kind(ModelErrorKind::ContextOverflow)
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
    keys.sort_by_key(|key| (key.parse::<u64>().unwrap_or(u64::MAX), key.clone()));
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

// Decode SSE data fields incrementally, while retaining complete JSON documents.
#[derive(Default)]
struct WireDecoder {
    data: String,
    document: Option<String>,
    terminal: bool,
}

impl WireDecoder {
    fn line(&mut self, line: &str) -> Result<Option<serde_json::Value>, String> {
        let line = line.trim_end_matches('\r');
        if self.document.is_some() || line.trim_start().starts_with(['{', '[']) {
            let document = self.document.get_or_insert_default();
            document.push_str(line);
            document.push('\n');
            return Ok(None);
        }
        if let Some(raw) = line.strip_prefix("data:") {
            if raw.trim() == "[DONE]" {
                self.finish_data()?;
                self.terminal = true;
                return Ok(None);
            }
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(raw.trim_start());
            // Some compatible endpoints omit blank separators between events.
            if let Ok(value) = serde_json::from_str(&self.data) {
                self.data.clear();
                return Ok(Some(value));
            }
        } else if line.is_empty() {
            self.finish_data()?;
        }
        Ok(None)
    }

    fn finish_data(&self) -> Result<(), String> {
        if self.data.is_empty() {
            Ok(())
        } else {
            Err("invalid model SSE event".into())
        }
    }

    fn finish(&mut self) -> Result<Option<serde_json::Value>, String> {
        self.finish_data()?;
        self.document
            .take()
            .map(|document| {
                serde_json::from_str(&document)
                    .map_err(|error| format!("invalid model JSON response: {error}"))
            })
            .transpose()
    }
}

#[derive(Default)]
struct ModelEventParser {
    pending_tools: std::collections::HashMap<String, PendingToolCall>,
    responses: crate::codex::events::CodexEventMapper,
    terminal: bool,
    text: String,
    hidden_text: String,
    think: ThinkTagFilter,
    saw_tool: bool,
    usage: Option<lato_core::ModelUsage>,
}

impl ModelEventParser {
    fn accept(&mut self, value: serde_json::Value) -> Result<Vec<StreamPiece>, String> {
        if let Some(usage) = usage_from_event(&value) {
            merge_usage(&mut self.usage, usage);
        }
        let pieces = self.accept_event(value)?;
        for piece in &pieces {
            match piece {
                StreamPiece::Text(text) => self.text.push_str(text),
                StreamPiece::ToolCall { .. } => self.saw_tool = true,
            }
        }
        Ok(pieces)
    }

    fn push_visible_text(&mut self, text: &str, pieces: &mut Vec<StreamPiece>) {
        let visible = self.think.push(text);
        if !visible.is_empty() {
            pieces.push(StreamPiece::Text(visible));
        }
    }

    fn accept_event(&mut self, value: serde_json::Value) -> Result<Vec<StreamPiece>, String> {
        let mut pieces = Vec::new();
        self.terminal = matches!(
            value.get("type").and_then(|v| v.as_str()),
            Some(
                "response.completed"
                    | "response.done"
                    | "response.failed"
                    | "response.incomplete"
                    | "message_stop"
                    | "error"
            )
        );
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if value.get("error").is_some_and(|error| !error.is_null()) || event_type == "error" {
            return Err(value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("model provider returned an error")
                .into());
        }
        if event_type.starts_with("response.") {
            pieces.extend(self.responses.accept(value)?);
            return Ok(pieces);
        }
        if value.get("output").is_some() {
            let event_type = match value.get("status").and_then(|v| v.as_str()) {
                Some("failed") => "response.failed",
                Some("incomplete") => "response.incomplete",
                _ => "response.completed",
            };
            pieces.extend(
                self.responses
                    .accept(serde_json::json!({"type":event_type,"response":value}))?,
            );
            return Ok(pieces);
        }
        if event_type == "message" {
            for block in value
                .get("content")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            pieces.push(StreamPiece::Text(text.into()));
                        }
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_use missing id")?;
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_use missing name")?;
                        let arguments = block
                            .get("input")
                            .cloned()
                            .ok_or("tool_use missing input")?;
                        pieces.push(StreamPiece::ToolCall {
                            id: id.into(),
                            name: name.into(),
                            arguments,
                        });
                    }
                    _ => {}
                }
            }
            return Ok(pieces);
        }
        if let Some(text) = reasoning_field_text(&value) {
            self.hidden_text.push_str(text);
        }
        if let Some(text) = json_str_non_empty(&value, "/choices/0/delta/content")
            .or_else(|| json_str_non_empty(&value, "/choices/0/message/content"))
            .or_else(|| json_str_non_empty(&value, "/delta/text"))
            .or_else(|| {
                (event_type == "response.output_text.delta")
                    .then(|| value.get("delta").and_then(|v| v.as_str()))
                    .flatten()
            })
        {
            self.hidden_text.push_str(text);
            self.push_visible_text(text, &mut pieces);
        }
        if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
            self.hidden_text.push_str(text);
            self.push_visible_text(text, &mut pieces);
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
                self.pending_tools.insert(
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
                if let Some(call) = self.pending_tools.get_mut(&key) {
                    call.arguments.push_str(partial);
                }
            }
        } else if event_type == "content_block_stop" {
            let key = value
                .get("index")
                .map(ToString::to_string)
                .unwrap_or_else(|| "0".into());
            if let Some(call) = self.pending_tools.remove(&key)
                && !call.name.is_empty()
            {
                let arguments = parse_tool_arguments(&call.arguments, &call.id)?;
                pieces.push(StreamPiece::ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                });
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
                let pending = self.pending_tools.entry(key.clone()).or_default();
                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    pending.id = id.to_string();
                }
                if let Some(name) = call
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .or_else(|| call.get("name").and_then(|v| v.as_str()))
                {
                    pending.name = name.to_string();
                }
                append_tool_arguments(
                    pending,
                    call.pointer("/function/arguments")
                        .or_else(|| call.get("arguments")),
                );
            }
        }
        if let Some(function) = value
            .pointer("/choices/0/delta/function_call")
            .or_else(|| value.pointer("/choices/0/message/function_call"))
        {
            let pending = self.pending_tools.entry("legacy".to_string()).or_default();
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                pending.name = name.to_string();
            }
            append_tool_arguments(pending, function.get("arguments"));
        }
        if matches!(
            value
                .pointer("/choices/0/finish_reason")
                .and_then(|v| v.as_str()),
            Some("tool_calls") | Some("tool_call") | Some("function_call")
        ) {
            drain_complete_tool_calls(&mut self.pending_tools, &mut pieces)?;
        }
        Ok(pieces)
    }

    fn finish(&mut self) -> Result<Vec<StreamPiece>, String> {
        self.responses.ensure_complete()?;
        let mut pieces = Vec::new();
        let visible = self.think.finish();
        if !visible.is_empty() {
            pieces.push(StreamPiece::Text(visible));
        }
        drain_complete_tool_calls(&mut self.pending_tools, &mut pieces)?;
        let saw_tool = self.saw_tool
            || pieces
                .iter()
                .any(|piece| matches!(piece, StreamPiece::ToolCall { .. }));
        if !saw_tool {
            let mut scan = std::mem::take(&mut self.hidden_text);
            if scan.is_empty() {
                scan = self.text.clone();
            }
            pieces.extend(extract_text_embedded_tool_calls(&scan));
        }
        Ok(pieces)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedModelOutput {
    pub pieces: Vec<StreamPiece>,
    pub usage: Option<lato_core::ModelUsage>,
}

pub(crate) fn usage_from_event(value: &serde_json::Value) -> Option<lato_core::ModelUsage> {
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))
        .or_else(|| value.get("usageMetadata"))
        .or_else(|| value.pointer("/metadata/usage"))
        .or_else(|| value.pointer("/message/usage"))?;
    let token = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| usage.get(*key).and_then(serde_json::Value::as_u64))
    };
    let input_tokens = token(&[
        "input_tokens",
        "prompt_tokens",
        "promptTokenCount",
        "inputTokens",
    ]);
    let output_tokens = token(&[
        "output_tokens",
        "completion_tokens",
        "candidatesTokenCount",
        "outputTokens",
    ]);
    let reasoning_tokens = token(&["reasoning_tokens", "thoughtsTokenCount"]).or_else(|| {
        usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(serde_json::Value::as_u64)
            })
    });
    let cached_input_tokens = token(&[
        "cache_read_input_tokens",
        "cached_input_tokens",
        "cachedContentTokenCount",
        "cacheReadInputTokens",
    ])
    .or_else(|| {
        usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(serde_json::Value::as_u64)
            })
    });
    (input_tokens.is_some()
        || output_tokens.is_some()
        || reasoning_tokens.is_some()
        || cached_input_tokens.is_some())
    .then_some(lato_core::ModelUsage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cached_input_tokens,
    })
}

pub(crate) fn merge_usage(
    current: &mut Option<lato_core::ModelUsage>,
    update: lato_core::ModelUsage,
) {
    let previous = current.take().unwrap_or(lato_core::ModelUsage {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        cached_input_tokens: None,
    });
    *current = Some(lato_core::ModelUsage {
        input_tokens: update.input_tokens.or(previous.input_tokens),
        output_tokens: update.output_tokens.or(previous.output_tokens),
        reasoning_tokens: update.reasoning_tokens.or(previous.reasoning_tokens),
        cached_input_tokens: update.cached_input_tokens.or(previous.cached_input_tokens),
    });
}

pub fn parse_stream_body(body: &str) -> Result<Vec<StreamPiece>, String> {
    Ok(parse_stream_body_with_report(body)?.pieces)
}

pub fn parse_stream_body_with_report(body: &str) -> Result<ParsedModelOutput, String> {
    let mut decoder = WireDecoder::default();
    let mut parser = ModelEventParser::default();
    let mut pieces = Vec::new();
    for line in body.lines() {
        if let Some(value) = decoder.line(line)? {
            pieces.extend(parser.accept(value)?);
        }
        if decoder.terminal || parser.terminal {
            break;
        }
    }
    if let Some(value) = decoder.finish()? {
        pieces.extend(parser.accept(value)?);
    }
    pieces.extend(parser.finish()?);
    Ok(ParsedModelOutput {
        pieces,
        usage: parser.usage,
    })
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Streaming filter that drops GLM/SenseNova chain-of-thought tags.
///
/// `glm-5.2` often streams reasoning as `<think>…</think>` in `content`
/// (or leaves a stray `</think>` after `reasoning_content`). Those tokens
/// must not be shown as assistant text, but the raw buffer is still
/// scanned for XML `<tool_call>` payloads.
#[derive(Clone, Debug, Default)]
pub struct ThinkTagFilter {
    in_think: bool,
    pending: String,
}

impl ThinkTagFilter {
    pub fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        self.pending.push_str(chunk);
        let mut visible = String::new();
        loop {
            if self.in_think {
                if let Some(pos) = self.pending.find(THINK_CLOSE) {
                    self.pending.replace_range(..pos + THINK_CLOSE.len(), "");
                    self.in_think = false;
                    continue;
                }
                let keep = suffix_that_is_tag_prefix(&self.pending, THINK_CLOSE);
                self.pending.replace_range(..self.pending.len() - keep, "");
                break;
            }
            let open_at = self.pending.find(THINK_OPEN);
            let close_at = self.pending.find(THINK_CLOSE);
            match (open_at, close_at) {
                (Some(open), Some(close)) if close < open => {
                    visible.push_str(&self.pending[..close]);
                    self.pending.replace_range(..close + THINK_CLOSE.len(), "");
                }
                (Some(open), _) => {
                    visible.push_str(&self.pending[..open]);
                    self.pending.replace_range(..open + THINK_OPEN.len(), "");
                    self.in_think = true;
                }
                (None, Some(close)) => {
                    visible.push_str(&self.pending[..close]);
                    self.pending.replace_range(..close + THINK_CLOSE.len(), "");
                }
                (None, None) => {
                    let keep = suffix_that_is_tag_prefix(&self.pending, THINK_OPEN)
                        .max(suffix_that_is_tag_prefix(&self.pending, THINK_CLOSE));
                    let emit_upto = self.pending.len() - keep;
                    visible.push_str(&self.pending[..emit_upto]);
                    self.pending.replace_range(..emit_upto, "");
                    break;
                }
            }
        }
        visible
    }

    pub fn finish(&mut self) -> String {
        if self.in_think {
            self.pending.clear();
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

fn suffix_that_is_tag_prefix(text: &str, tag: &str) -> usize {
    let start = text.len().saturating_sub(tag.len());
    let mut best = 0;
    for index in start..=text.len() {
        if !text.is_char_boundary(index) {
            continue;
        }
        let suffix = &text[index..];
        if !suffix.is_empty() && tag.starts_with(suffix) {
            best = suffix.len();
        }
    }
    best
}

pub fn strip_think_tags(text: &str) -> String {
    let mut filter = ThinkTagFilter::default();
    let mut visible = filter.push(text);
    visible.push_str(&filter.finish());
    visible
}

fn reasoning_field_text(value: &serde_json::Value) -> Option<&str> {
    json_str_non_empty(value, "/choices/0/delta/reasoning_content")
        .or_else(|| json_str_non_empty(value, "/choices/0/message/reasoning_content"))
        .or_else(|| json_str_non_empty(value, "/choices/0/delta/reasoning"))
        .or_else(|| json_str_non_empty(value, "/choices/0/message/reasoning"))
        .or_else(|| json_str_non_empty(value, "/choices/0/delta/thinking"))
        .or_else(|| json_str_non_empty(value, "/choices/0/message/thinking"))
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
#[path = "stream_repair_tests.rs"]
mod repair_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpRequestSpec;

    fn usage(input: u64, output: u64, reasoning: u64, cached: u64) -> lato_core::ModelUsage {
        lato_core::ModelUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            reasoning_tokens: Some(reasoning),
            cached_input_tokens: Some(cached),
        }
    }

    #[test]
    fn provider_usage_dialects_normalize_to_the_canonical_shape() {
        let cases = [
            vec![serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":80,"output_tokens":20,
                    "output_tokens_details":{"reasoning_tokens":5},
                    "input_tokens_details":{"cached_tokens":40}}}
            })],
            vec![serde_json::json!({
                "choices":[],
                "usage":{"prompt_tokens":80,"completion_tokens":20,
                    "completion_tokens_details":{"reasoning_tokens":5},
                    "prompt_tokens_details":{"cached_tokens":40}}
            })],
            vec![
                serde_json::json!({"type":"message_start","message":{"usage":{
                    "input_tokens":80,"cache_read_input_tokens":40}}}),
                serde_json::json!({"type":"message_delta","usage":{
                    "output_tokens":20,"reasoning_tokens":5}}),
            ],
            vec![serde_json::json!({
                "usageMetadata":{"promptTokenCount":80,"candidatesTokenCount":20,
                    "thoughtsTokenCount":5,"cachedContentTokenCount":40}
            })],
            vec![serde_json::json!({
                "metadata":{"usage":{"inputTokens":80,"outputTokens":20,
                    "reasoning_tokens":5,"cacheReadInputTokens":40}}
            })],
            vec![serde_json::json!({
                "usage":{"prompt_tokens":80,"completion_tokens":20,
                    "reasoning_tokens":5,"cached_input_tokens":40}
            })],
        ];

        for events in cases {
            let mut parser = ModelEventParser::default();
            for event in events {
                parser.accept(event).unwrap();
            }
            assert_eq!(parser.usage, Some(usage(80, 20, 5, 40)));
        }
    }

    #[test]
    fn stream_body_report_retains_terminal_responses_usage() {
        let output = parse_stream_body_with_report(
            r#"data: {"type":"response.completed","response":{"output":[],"usage":{"input_tokens":80,"output_tokens":20,"output_tokens_details":{"reasoning_tokens":5},"input_tokens_details":{"cached_tokens":40}}}}

data: [DONE]

"#,
        )
        .unwrap();

        assert_eq!(output.usage, Some(usage(80, 20, 5, 40)));
    }

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
                context_window: None,
                model_family: None,
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
        assert!(err.message.contains("compact"));
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
            &legacy_model_error("http 400: invalid tool_choice"),
            &request
        ));
        assert!(!tool_choice_required_rejected(
            &legacy_model_error("http 401: unauthorized"),
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
            &legacy_model_error("http 400: invalid model"),
            &request
        ));
        assert!(tool_choice_required_rejected(
            &legacy_model_error("http 400: unsupported tool_choice required"),
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
    fn stray_think_close_tag_is_not_user_visible_text() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"</think>\"}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(pieces.is_empty(), "think tags must not leak: {pieces:?}");
    }

    #[test]
    fn think_block_in_content_is_stripped_but_trailing_answer_remains() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>plan the write\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"</think>done\"}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert_eq!(
            pieces,
            vec![StreamPiece::Text("done".into())],
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn glm_xml_tool_call_inside_reasoning_content_is_extracted() {
        let fixture = r#"{"choices":[{"message":{"reasoning_content":"<tool_call>write_file<arg_key>path</arg_key><arg_value>hello.txt</arg_value><arg_key>contents</arg_key><arg_value>Hello, world!</arg_value></tool_call>","content":"</think>"},"finish_reason":"stop"}]}"#;
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            pieces
                .iter()
                .all(|piece| !matches!(piece, StreamPiece::Text(text) if text.contains("think"))),
            "think tags must not leak: {pieces:?}"
        );
        assert!(
            pieces.iter().any(|piece| matches!(
                piece,
                StreamPiece::ToolCall { name, arguments, .. }
                    if name == "write_file"
                        && arguments["path"] == "hello.txt"
                        && arguments["contents"] == "Hello, world!"
            )),
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn glm_xml_tool_call_inside_think_tags_is_extracted_and_hidden() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>need a file\\n\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"<tool_call>write_file<arg_key>path</arg_key><arg_value>hello.txt</arg_value><arg_key>contents</arg_key><arg_value>Hello, world!</arg_value></tool_call>\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"</think>\"}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            !pieces.iter().any(|piece| matches!(piece, StreamPiece::Text(text) if text.contains("think") || text.contains("tool_call"))),
            "pieces={pieces:?}"
        );
        assert!(
            pieces.iter().any(|piece| matches!(
                piece,
                StreamPiece::ToolCall { name, arguments, .. }
                    if name == "write_file" && arguments["path"] == "hello.txt"
            )),
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn split_think_close_tag_across_sse_chunks_does_not_leak() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"</th\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ink>\"}}]}\n",
            "data: [DONE]\n"
        );
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(pieces.is_empty(), "split think tag leaked: {pieces:?}");
    }

    #[test]
    fn flat_openai_compat_tool_call_without_function_wrapper_is_parsed() {
        let fixture = r#"{"choices":[{"message":{"tool_calls":[{"id":"c1","name":"write_file","arguments":"{\"path\":\"hello.txt\",\"contents\":\"Hello, world!\"}"}]},"finish_reason":"tool_call"}]}"#;
        let pieces = parse_stream_body(fixture).unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { name, arguments, .. }
                if name == "write_file" && arguments["path"] == "hello.txt"),
            "pieces={pieces:?}"
        );
    }

    #[test]
    fn think_tag_filter_holds_incomplete_prefixes() {
        let mut filter = ThinkTagFilter::default();
        assert_eq!(filter.push("hello <"), "hello ");
        assert_eq!(filter.push(" 2"), "< 2");
        assert_eq!(filter.finish(), "");
        assert_eq!(strip_think_tags("</think>\nvisible"), "\nvisible");
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
