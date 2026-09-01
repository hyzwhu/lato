use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use lato_core::{
    AgentError, ErrorCategory, ModelCapabilities, ModelContent, ModelError, ModelEventStream,
    ModelMessage, ModelPort, ModelRequest, ModelRole, ModelSelection, ModelStopReason,
    ModelStreamEvent, ModelUsage, Retryability, SamplingParameters, ToolChoice,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct ScriptedPort;

struct InterruptedPort;

#[async_trait]
impl ModelPort for ScriptedPort {
    async fn stream(
        &self,
        _request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(ModelStreamEvent::TextDelta { text: "hi".into() }),
            Ok(ModelStreamEvent::Usage(ModelUsage {
                input_tokens: Some(3),
                output_tokens: Some(1),
                reasoning_tokens: None,
                cached_input_tokens: None,
            })),
            Ok(ModelStreamEvent::Completed {
                reason: ModelStopReason::Completed,
            }),
        ])))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[async_trait]
impl ModelPort for InterruptedPort {
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        Ok(Box::pin(stream::iter(vec![Err(ModelError::new(
            "model.stream_interrupted",
            "connection closed",
            Retryability::Safe,
        ))])))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        call_id: "model-call-1".into(),
        selection: ModelSelection::new("openai", "gpt-test").unwrap(),
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "hello".into(),
            }],
        }],
        tools: Vec::new(),
        parameters: SamplingParameters {
            temperature: None,
            max_output_tokens: Some(32),
            tool_choice: Some(ToolChoice::Auto),
            response_schema: None,
        },
    }
}

#[tokio::test]
async fn model_port_is_object_safe_and_streams_canonical_events() {
    let port: Arc<dyn ModelPort> = Arc::new(ScriptedPort);
    let events = port
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<_> = events.collect().await;
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], Ok(ModelStreamEvent::TextDelta { .. })));
    assert!(matches!(events[2], Ok(ModelStreamEvent::Completed { .. })));
}

#[tokio::test]
async fn pre_cancelled_requests_fail_with_typed_model_error() {
    let token = CancellationToken::new();
    token.cancel();
    let error = match ScriptedPort.stream(request(), token).await {
        Ok(_) => panic!("pre-cancelled request unexpectedly produced a stream"),
        Err(error) => error,
    };
    assert_eq!(error.code, "model.cancelled");
    let agent: AgentError = error.into();
    assert_eq!(agent.category, ErrorCategory::Model);
}

#[tokio::test]
async fn established_stream_reports_interruptions_as_items() {
    let mut events = InterruptedPort
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    assert_eq!(error.code, "model.stream_interrupted");
    assert_eq!(error.retryability, Retryability::Safe);
    assert!(events.next().await.is_none());
}

#[test]
fn model_events_have_stable_tagged_json() {
    let event = ModelStreamEvent::Completed {
        reason: ModelStopReason::ToolCalls,
    };
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["type"], "completed");
    assert_eq!(value["reason"], "tool_calls");
}

#[test]
fn unknown_usage_is_not_serialized_as_zero() {
    let usage = ModelUsage {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        cached_input_tokens: None,
    };
    let value = serde_json::to_value(usage).unwrap();
    assert!(value["input_tokens"].is_null());
    assert!(value["output_tokens"].is_null());
}

#[test]
fn provider_specific_stop_reasons_remain_explicit() {
    let reason = ModelStopReason::Other("safety_review".into());
    let encoded = serde_json::to_string(&reason).unwrap();
    let decoded: ModelStopReason = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, reason);
}

#[test]
fn selection_rejects_empty_parts() {
    assert!(ModelSelection::new("", "model").is_err());
    assert!(ModelSelection::new("provider", " ").is_err());
}
