use super::codec::encode_legacy_request;
use crate::{ModelStream, StreamPiece};
use async_trait::async_trait;
use futures_util::Stream;
use lato_core::{
    ModelCapabilities, ModelError, ModelEventStream, ModelPort, ModelRequest, ModelSelection,
    ModelStopReason, ModelStreamEvent, Retryability, ToolCallDelta, ToolCallId, ToolName,
};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub struct LegacyModelPort {
    selection: ModelSelection,
    stream: Arc<dyn ModelStream>,
}

impl LegacyModelPort {
    pub fn new(selection: ModelSelection, stream: Arc<dyn ModelStream>) -> Self {
        Self { selection, stream }
    }
}

#[async_trait]
impl ModelPort for LegacyModelPort {
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        if request.selection != self.selection {
            return Err(ModelError::new(
                "model.selection_mismatch",
                format!(
                    "adapter is bound to {}/{} but request selected {}/{}",
                    self.selection.provider,
                    self.selection.model,
                    request.selection.provider,
                    request.selection.model
                ),
                Retryability::Never,
            ));
        }

        let context = encode_legacy_request(&request)?;
        let prompt_bytes = serde_json::to_vec(&context)
            .map_err(|error| {
                ModelError::new(
                    "model.invalid_request",
                    format!("failed to measure legacy request: {error}"),
                    Retryability::Never,
                )
            })?
            .len();
        let legacy = self.stream.clone();
        let bridge_cancellation = cancellation.child_token();
        let task_cancellation = bridge_cancellation.clone();
        let (event_tx, event_rx) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            run_legacy_bridge(legacy, prompt_bytes, context, event_tx, task_cancellation).await;
        });

        Ok(Box::pin(GuardedEventStream {
            inner: ReceiverStream::new(event_rx),
            cancellation: bridge_cancellation,
            task: Some(task),
        }))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_use: true,
            ..ModelCapabilities::default()
        }
    }
}

struct GuardedEventStream {
    inner: ReceiverStream<Result<ModelStreamEvent, ModelError>>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Stream for GuardedEventStream {
    type Item = Result<ModelStreamEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl Drop for GuardedEventStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_legacy_bridge(
    legacy: Arc<dyn ModelStream>,
    prompt_bytes: usize,
    context: serde_json::Value,
    event_tx: mpsc::Sender<Result<ModelStreamEvent, ModelError>>,
    cancellation: CancellationToken,
) {
    let (legacy_tx, mut legacy_rx) = mpsc::channel(16);
    let provider = legacy.stream_with_report(prompt_bytes, context, legacy_tx);
    tokio::pin!(provider);
    let mut next_tool_index = 0_u32;
    let mut saw_tool_call = false;

    let provider_result = loop {
        tokio::select! {
            _ = cancellation.cancelled() => return,
            result = &mut provider => break result,
            piece = legacy_rx.recv() => {
                if let Some(piece) = piece
                    && !forward_piece(
                        piece,
                        &event_tx,
                        &cancellation,
                        &mut next_tool_index,
                        &mut saw_tool_call,
                    ).await
                {
                    return;
                }
            }
        }
    };

    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return,
            piece = legacy_rx.recv() => match piece {
                Some(piece) => {
                    if !forward_piece(
                        piece,
                        &event_tx,
                        &cancellation,
                        &mut next_tool_index,
                        &mut saw_tool_call,
                    ).await
                    {
                        return;
                    }
                }
                None => break,
            }
        }
    }

    let final_event = match provider_result {
        Ok(report) => {
            if let Some(usage) = report.usage
                && !send_event(&event_tx, &cancellation, Ok(ModelStreamEvent::Usage(usage))).await
            {
                return;
            }
            Ok(ModelStreamEvent::Completed {
                reason: if saw_tool_call {
                    ModelStopReason::ToolCalls
                } else {
                    ModelStopReason::Completed
                },
            })
        }
        Err(message) => Err(ModelError::new(
            "model.stream_interrupted",
            message,
            Retryability::AfterBackoff,
        )),
    };
    let _ = send_event(&event_tx, &cancellation, final_event).await;
}

async fn forward_piece(
    piece: StreamPiece,
    event_tx: &mpsc::Sender<Result<ModelStreamEvent, ModelError>>,
    cancellation: &CancellationToken,
    next_tool_index: &mut u32,
    saw_tool_call: &mut bool,
) -> bool {
    let event = match piece {
        StreamPiece::Text(text) => Ok(ModelStreamEvent::TextDelta { text }),
        StreamPiece::ToolCall {
            id,
            name,
            arguments,
        } => {
            let call_id = match ToolCallId::parse(id) {
                Ok(call_id) => call_id,
                Err(error) => {
                    let _ = send_event(
                        event_tx,
                        cancellation,
                        Err(invalid_response(format!(
                            "legacy provider returned an invalid tool call ID: {error}"
                        ))),
                    )
                    .await;
                    return false;
                }
            };
            let name = match ToolName::parse(format!("legacy:{name}")) {
                Ok(name) => name,
                Err(error) => {
                    let _ = send_event(
                        event_tx,
                        cancellation,
                        Err(invalid_response(format!(
                            "legacy provider returned an invalid tool name: {error}"
                        ))),
                    )
                    .await;
                    return false;
                }
            };
            let index = *next_tool_index;
            *next_tool_index = next_tool_index.saturating_add(1);
            *saw_tool_call = true;
            Ok(ModelStreamEvent::ToolCallDelta(ToolCallDelta {
                index,
                call_id: Some(call_id),
                name: Some(name),
                arguments_delta: arguments.to_string(),
            }))
        }
    };
    send_event(event_tx, cancellation, event).await
}

async fn send_event(
    event_tx: &mpsc::Sender<Result<ModelStreamEvent, ModelError>>,
    cancellation: &CancellationToken,
    event: Result<ModelStreamEvent, ModelError>,
) -> bool {
    tokio::select! {
        _ = cancellation.cancelled() => false,
        result = event_tx.send(event) => result.is_ok(),
    }
}

fn invalid_response(message: impl Into<String>) -> ModelError {
    ModelError::new("model.invalid_response", message, Retryability::Never)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamPiece;
    use futures_util::StreamExt;
    use lato_core::{
        ModelCallId, ModelStopReason, ModelStreamEvent, Retryability, SamplingParameters,
        ToolCallId, ToolName,
    };
    use serde_json::json;
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };
    use tokio::sync::mpsc;

    #[derive(Clone)]
    enum Behavior {
        Pieces(Vec<StreamPiece>),
        Fail(String),
        Block(Arc<AtomicBool>),
    }

    struct ScriptedLegacyStream {
        behavior: Behavior,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelStream for ScriptedLegacyStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            tx: mpsc::Sender<StreamPiece>,
        ) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::Pieces(pieces) => {
                    for piece in pieces.clone() {
                        tx.send(piece).await.map_err(|error| error.to_string())?;
                    }
                    Ok(())
                }
                Behavior::Fail(message) => Err(message.clone()),
                Behavior::Block(dropped) => {
                    struct DropMarker(Arc<AtomicBool>);
                    impl Drop for DropMarker {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                    let _marker = DropMarker(dropped.clone());
                    pending::<()>().await;
                    Ok(())
                }
            }
        }
    }

    fn selection() -> ModelSelection {
        ModelSelection::new("openai", "gpt-test").unwrap()
    }

    fn request(selection: ModelSelection) -> ModelRequest {
        ModelRequest {
            call_id: ModelCallId::from("call-1"),
            selection,
            messages: Vec::new(),
            tools: Vec::new(),
            parameters: SamplingParameters {
                temperature: None,
                max_output_tokens: None,
                tool_choice: None,
                response_schema: None,
            },
        }
    }

    fn port(behavior: Behavior, calls: Arc<AtomicUsize>) -> LegacyModelPort {
        LegacyModelPort::new(
            selection(),
            Arc::new(ScriptedLegacyStream { behavior, calls }),
        )
    }

    #[tokio::test]
    async fn preserves_text_tool_order_and_completes_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let port = port(
            Behavior::Pieces(vec![
                StreamPiece::Text("a".into()),
                StreamPiece::ToolCall {
                    id: "tool-7".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.rs"}),
                },
                StreamPiece::Text("b".into()),
            ]),
            calls.clone(),
        );

        let events = port
            .stream(request(selection()), CancellationToken::new())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[0],
            Ok(ModelStreamEvent::TextDelta { text }) if text == "a"
        ));
        assert!(matches!(
            &events[1],
            Ok(ModelStreamEvent::ToolCallDelta(delta))
                if delta.index == 0
                    && delta.call_id == Some(ToolCallId::from("tool-7"))
                    && delta.name == Some(ToolName::parse("legacy:read_file").unwrap())
                    && delta.arguments_delta == "{\"path\":\"a.rs\"}"
        ));
        assert!(matches!(
            &events[2],
            Ok(ModelStreamEvent::TextDelta { text }) if text == "b"
        ));
        assert!(matches!(
            &events[3],
            Ok(ModelStreamEvent::Completed {
                reason: ModelStopReason::ToolCalls
            })
        ));
        let capabilities = port.capabilities();
        assert!(capabilities.tool_use);
        assert!(!capabilities.parallel_tool_calls);
        assert!(!capabilities.reasoning);
        assert_eq!(capabilities.context_window, None);
    }

    #[tokio::test]
    async fn rejects_selection_mismatch_before_calling_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let port = port(Behavior::Pieces(Vec::new()), calls.clone());
        let mismatch = ModelSelection::new("anthropic", "claude-test").unwrap();

        let error = match port
            .stream(request(mismatch), CancellationToken::new())
            .await
        {
            Ok(_) => panic!("selection mismatch unexpectedly streamed"),
            Err(error) => error,
        };

        assert_eq!(error.code, "model.selection_mismatch");
        assert_eq!(error.retryability, Retryability::Never);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_pre_cancelled_request_before_calling_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let port = port(Behavior::Pieces(Vec::new()), calls.clone());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = match port.stream(request(selection()), cancellation).await {
            Ok(_) => panic!("pre-cancelled request unexpectedly streamed"),
            Err(error) => error,
        };

        assert_eq!(error.code, "model.cancelled");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_failure_is_a_typed_stream_item() {
        let port = port(
            Behavior::Fail("wire broke".into()),
            Arc::new(AtomicUsize::new(0)),
        );

        let events = port
            .stream(request(selection()), CancellationToken::new())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err();
        assert_eq!(error.code, "model.stream_interrupted");
        assert!(error.message.contains("wire broke"));
    }

    #[tokio::test]
    async fn invalid_provider_tool_call_stops_without_completion() {
        let port = port(
            Behavior::Pieces(vec![StreamPiece::ToolCall {
                id: "".into(),
                name: "read_file".into(),
                arguments: json!({}),
            }]),
            Arc::new(AtomicUsize::new(0)),
        );

        let events = port
            .stream(request(selection()), CancellationToken::new())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].as_ref().unwrap_err().code,
            "model.invalid_response"
        );
    }

    #[tokio::test]
    async fn dropping_event_stream_aborts_blocked_provider_within_250ms() {
        let provider_dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let port = port(Behavior::Block(provider_dropped.clone()), calls.clone());
        let events = port
            .stream(request(selection()), CancellationToken::new())
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(events);
        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while !provider_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocked provider future was detached");
    }

    #[tokio::test]
    async fn cancellation_aborts_blocked_provider_within_250ms() {
        let provider_dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let port = port(Behavior::Block(provider_dropped.clone()), calls.clone());
        let cancellation = CancellationToken::new();
        let _events = port
            .stream(request(selection()), cancellation.clone())
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while !provider_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled provider future was detached");
    }
}
