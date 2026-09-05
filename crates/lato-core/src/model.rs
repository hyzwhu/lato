// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/model-provider/src/provider.rs
// License: Apache-2.0
// Lato changes: normalized provider requests, capabilities, stream events, and typed failures

use crate::{
    AgentError, ErrorCategory, ModelCallId, Retryability, ToolCallId, ToolDescriptor, ToolName,
};
use async_trait::async_trait;
use futures_core::Stream;
use serde_json::Value;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
}

impl ModelSelection {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelSelectionError> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() {
            return Err(ModelSelectionError::EmptyProvider);
        }
        if model.trim().is_empty() {
            return Err(ModelSelectionError::EmptyModel);
        }
        Ok(Self { provider, model })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelSelectionError {
    #[error("model provider must not be empty")]
    EmptyProvider,
    #[error("model ID must not be empty")]
    EmptyModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContent {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolCall {
        call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
    },
    ToolResult {
        call_id: ToolCallId,
        output: String,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ModelContent>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SamplingParameters {
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub response_schema: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific(ToolName),
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelRequest {
    pub call_id: ModelCallId,
    pub selection: ModelSelection,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDescriptor>,
    pub parameters: SamplingParameters,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelCapabilities {
    pub tool_use: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: bool,
    pub vision: bool,
    pub structured_output: bool,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    pub call_id: Option<ToolCallId>,
    pub name: Option<ToolName>,
    pub arguments_delta: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Completed,
    ToolCalls,
    Length,
    ContentFilter,
    Cancelled,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallDelta(ToolCallDelta),
    Usage(ModelUsage),
    Completed { reason: ModelStopReason },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    ContextOverflow,
    Authentication,
    Credit,
    RateLimited,
    InvalidRequest,
    Transport,
    Cancelled,
    #[default]
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ModelError {
    pub code: String,
    pub message: String,
    pub retryability: Retryability,
    #[serde(default)]
    pub kind: ModelErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub output_started: bool,
}

impl ModelError {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryability: Retryability,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryability,
            kind: ModelErrorKind::Other,
            status_code: None,
            context_window: None,
            output_started: false,
        }
    }

    pub fn with_kind(mut self, kind: ModelErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_status(mut self, status_code: u16) -> Self {
        self.status_code = Some(status_code);
        self
    }

    pub fn with_context_window(mut self, context_window: u64) -> Self {
        self.context_window = Some(context_window);
        self
    }

    pub fn with_output_started(mut self, output_started: bool) -> Self {
        self.output_started = output_started;
        self
    }

    pub fn cancelled() -> Self {
        Self::new(
            "model.cancelled",
            "model request cancelled",
            Retryability::Never,
        )
        .with_kind(ModelErrorKind::Cancelled)
    }
}

impl From<ModelError> for AgentError {
    fn from(error: ModelError) -> Self {
        AgentError::new(
            error.code,
            ErrorCategory::Model,
            error.message,
            error.retryability,
        )
    }
}

pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ModelError>> + Send + 'static>>;

#[async_trait]
pub trait ModelPort: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;

    fn capabilities(&self) -> ModelCapabilities;
}
