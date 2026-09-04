use crate::{ActiveModelPort, ModelStream, StreamPiece};
use async_trait::async_trait;
use futures_util::StreamExt;
use lato_core::{
    ModelCallId, ModelError, ModelPort, ModelSelection, ModelStreamEvent, ToolCallId, ToolName,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct ModelPortStreamAdapter {
    selection: ModelSelection,
    port: Arc<dyn ModelPort>,
    next_call_id: AtomicU64,
}

impl ModelPortStreamAdapter {
    pub fn new(active: ActiveModelPort) -> Self {
        Self {
            selection: active.selection,
            port: active.port,
            next_call_id: AtomicU64::new(1),
        }
    }
}

#[async_trait]
impl ModelStream for ModelPortStreamAdapter {
    fn active_model_port(&self) -> Option<ActiveModelPort> {
        Some(ActiveModelPort {
            selection: self.selection.clone(),
            capabilities: self.port.capabilities(),
            port: self.port.clone(),
        })
    }

    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        let sequence = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let request = super::codec::decode_legacy_request(
            ModelCallId::from(format!("legacy-model-call-{sequence}")),
            self.selection.clone(),
            context,
        )
        .map_err(|error| error.to_string())?;
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let mut events = tokio::select! {
            _ = tx.closed() => return Ok(()),
            result = self.port.stream(request, cancellation.clone()) => {
                result.map_err(|error| error.to_string())?
            }
        };
        let mut pending_tools = BTreeMap::<u32, PendingToolCall>::new();

        loop {
            let event = tokio::select! {
                _ = tx.closed() => return Ok(()),
                event = events.next() => event,
            };
            match event {
                Some(Ok(ModelStreamEvent::TextDelta { text })) => {
                    if !send_piece(&tx, StreamPiece::Text(text)).await {
                        return Ok(());
                    }
                }
                Some(Ok(ModelStreamEvent::ToolCallDelta(delta))) => {
                    merge_tool_delta(&mut pending_tools, delta)
                        .map_err(|error| error.to_string())?;
                }
                Some(Ok(ModelStreamEvent::ReasoningDelta { .. }))
                | Some(Ok(ModelStreamEvent::Usage(_))) => {}
                Some(Ok(ModelStreamEvent::Completed { .. })) => {
                    return flush_tool_calls(sequence, pending_tools, &tx).await;
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => {
                    return Err(ModelError::new(
                        "model.stream_interrupted",
                        "canonical model stream ended before completion",
                        lato_core::Retryability::AfterBackoff,
                    )
                    .to_string());
                }
            }
        }
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Default)]
struct PendingToolCall {
    call_id: Option<ToolCallId>,
    name: Option<ToolName>,
    arguments: String,
}

fn merge_tool_delta(
    pending: &mut BTreeMap<u32, PendingToolCall>,
    delta: lato_core::ToolCallDelta,
) -> Result<(), ModelError> {
    let call = pending.entry(delta.index).or_default();
    if let Some(call_id) = delta.call_id {
        if let Some(existing) = &call.call_id
            && existing != &call_id
        {
            return Err(invalid_response(format!(
                "tool call {} changed ID from {} to {}",
                delta.index, existing, call_id
            )));
        }
        call.call_id = Some(call_id);
    }
    if let Some(name) = delta.name {
        if let Some(existing) = &call.name
            && existing != &name
        {
            return Err(invalid_response(format!(
                "tool call {} changed name from {} to {}",
                delta.index, existing, name
            )));
        }
        call.name = Some(name);
    }
    call.arguments.push_str(&delta.arguments_delta);
    Ok(())
}

async fn flush_tool_calls(
    sequence: u64,
    pending: BTreeMap<u32, PendingToolCall>,
    tx: &mpsc::Sender<StreamPiece>,
) -> Result<(), String> {
    for (index, call) in pending {
        let call_id = call
            .call_id
            .map(|call_id| call_id.to_string())
            .unwrap_or_else(|| format!("legacy-tool-call-{sequence}-{index}"));
        let name = call.name.ok_or_else(|| {
            invalid_response(format!("tool call {index} completed without a name")).to_string()
        })?;
        if name.namespace() != "legacy" {
            return Err(
                invalid_response(format!("tool call {index} used non-legacy tool {name}"))
                    .to_string(),
            );
        }
        let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
            invalid_response(format!(
                "tool call {index} completed with invalid arguments: {error}"
            ))
            .to_string()
        })?;
        if !send_piece(
            tx,
            StreamPiece::ToolCall {
                id: call_id,
                name: name.local_name().to_owned(),
                arguments,
            },
        )
        .await
        {
            return Ok(());
        }
    }
    Ok(())
}

async fn send_piece(tx: &mpsc::Sender<StreamPiece>, piece: StreamPiece) -> bool {
    tokio::select! {
        _ = tx.closed() => false,
        result = tx.send(piece) => result.is_ok(),
    }
}

fn invalid_response(message: impl Into<String>) -> ModelError {
    ModelError::new(
        "model.invalid_response",
        message,
        lato_core::Retryability::Never,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::Stream;
    use lato_core::{
        ModelCapabilities, ModelError, ModelEventStream, ModelRequest, ModelStopReason,
        ModelStreamEvent, Retryability, ToolCallDelta, ToolCallId, ToolName,
    };
    use std::{
        pin::Pin,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    struct ScriptedPort {
        events: Vec<Result<ModelStreamEvent, ModelError>>,
        request: Arc<Mutex<Option<ModelRequest>>>,
    }

    #[async_trait]
    impl ModelPort for ScriptedPort {
        async fn stream(
            &self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            *self.request.lock().unwrap() = Some(request);
            Ok(Box::pin(tokio_stream::iter(self.events.clone())))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    struct PendingPort {
        stream_dropped: Arc<AtomicBool>,
    }

    struct PendingEventStream {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingEventStream {
        type Item = Result<ModelStreamEvent, ModelError>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PendingEventStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ModelPort for PendingPort {
        async fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            Ok(Box::pin(PendingEventStream {
                dropped: self.stream_dropped.clone(),
            }))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn selection() -> ModelSelection {
        ModelSelection::new("openai", "gpt-test").unwrap()
    }

    fn active(port: Arc<dyn ModelPort>) -> ActiveModelPort {
        ActiveModelPort {
            selection: selection(),
            capabilities: port.capabilities(),
            port,
        }
    }

    fn context() -> serde_json::Value {
        serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "tools": []
        })
    }

    #[tokio::test]
    async fn streams_text_and_reassembles_indexed_tool_deltas() {
        let request = Arc::new(Mutex::new(None));
        let port = Arc::new(ScriptedPort {
            request: request.clone(),
            events: vec![
                Ok(ModelStreamEvent::TextDelta { text: "hi".into() }),
                Ok(ModelStreamEvent::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    call_id: None,
                    name: Some(ToolName::parse("legacy:read_file").unwrap()),
                    arguments_delta: "{\"path\":".into(),
                })),
                Ok(ModelStreamEvent::ToolCallDelta(ToolCallDelta {
                    index: 1,
                    call_id: None,
                    name: Some(ToolName::parse("legacy:list_dir").unwrap()),
                    arguments_delta: "{}".into(),
                })),
                Ok(ModelStreamEvent::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    call_id: None,
                    name: None,
                    arguments_delta: "\"a.rs\"}".into(),
                })),
                Ok(ModelStreamEvent::Completed {
                    reason: ModelStopReason::ToolCalls,
                }),
            ],
        });
        let adapter = ModelPortStreamAdapter::new(active(port));
        let (tx, mut rx) = mpsc::channel(16);

        adapter.stream(5, context(), tx).await.unwrap();
        let mut pieces = Vec::new();
        while let Some(piece) = rx.recv().await {
            pieces.push(piece);
        }

        assert!(matches!(&pieces[0], StreamPiece::Text(text) if text == "hi"));
        let StreamPiece::ToolCall {
            id: first_id,
            name: first_name,
            arguments: first_arguments,
        } = &pieces[1]
        else {
            panic!("expected first tool call");
        };
        assert_eq!(first_name, "read_file");
        assert_eq!(first_arguments, &serde_json::json!({"path": "a.rs"}));
        let StreamPiece::ToolCall {
            id: second_id,
            name: second_name,
            arguments: second_arguments,
        } = &pieces[2]
        else {
            panic!("expected second tool call");
        };
        assert_eq!(second_name, "list_dir");
        assert_eq!(second_arguments, &serde_json::json!({}));
        assert_ne!(first_id, second_id);

        let captured = request.lock().unwrap();
        let captured = captured.as_ref().unwrap();
        assert_eq!(captured.selection, selection());
        assert_eq!(captured.messages.len(), 1);
    }

    #[tokio::test]
    async fn malformed_completed_tool_arguments_return_stable_error() {
        let port = Arc::new(ScriptedPort {
            request: Arc::new(Mutex::new(None)),
            events: vec![
                Ok(ModelStreamEvent::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    call_id: Some(ToolCallId::from("tool-1")),
                    name: Some(ToolName::parse("legacy:read_file").unwrap()),
                    arguments_delta: "{".into(),
                })),
                Ok(ModelStreamEvent::Completed {
                    reason: ModelStopReason::ToolCalls,
                }),
            ],
        });
        let adapter = ModelPortStreamAdapter::new(active(port));
        let (tx, _rx) = mpsc::channel(16);

        let error = adapter.stream(1, context(), tx).await.unwrap_err();

        assert!(error.starts_with("model.invalid_response:"));
    }

    #[tokio::test]
    async fn canonical_stream_error_keeps_stable_code_prefix() {
        let port = Arc::new(ScriptedPort {
            request: Arc::new(Mutex::new(None)),
            events: vec![Err(ModelError::new(
                "model.rate_limited",
                "slow down",
                Retryability::AfterBackoff,
            ))],
        });
        let adapter = ModelPortStreamAdapter::new(active(port));
        let (tx, _rx) = mpsc::channel(16);

        let error = adapter.stream(1, context(), tx).await.unwrap_err();

        assert_eq!(error, "model.rate_limited: slow down");
    }

    #[tokio::test]
    async fn receiver_drop_releases_pending_canonical_stream_within_250ms() {
        let stream_dropped = Arc::new(AtomicBool::new(false));
        let adapter = Arc::new(ModelPortStreamAdapter::new(active(Arc::new(PendingPort {
            stream_dropped: stream_dropped.clone(),
        }))));
        let (tx, rx) = mpsc::channel(1);
        let task = {
            let adapter = adapter.clone();
            tokio::spawn(async move { adapter.stream(1, context(), tx).await })
        };
        tokio::task::yield_now().await;
        drop(rx);

        tokio::time::timeout(std::time::Duration::from_millis(250), task)
            .await
            .expect("adapter ignored downstream receiver drop")
            .unwrap()
            .unwrap();
        assert!(stream_dropped.load(Ordering::SeqCst));
    }
}
