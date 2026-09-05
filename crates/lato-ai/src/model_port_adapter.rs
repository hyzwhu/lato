mod codec;
mod legacy_port;
mod stream_adapter;

use crate::ModelStream;
use lato_core::{ModelCapabilities, ModelPort, ModelSelection, ModelSelectionError};
use std::sync::Arc;

pub use legacy_port::LegacyModelPort;
pub use stream_adapter::ModelPortStreamAdapter;

#[derive(Clone)]
pub struct ActiveModelPort {
    pub selection: ModelSelection,
    pub metadata: ModelMetadata,
    pub capabilities: ModelCapabilities,
    pub generation: u64,
    pub port: Arc<dyn ModelPort>,
}

#[derive(Clone)]
pub struct ActiveModelStream {
    pub stream: Arc<dyn ModelStream>,
    pub port: ActiveModelPort,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelMetadata {
    pub context_window: Option<u64>,
    pub model_family: Option<String>,
}

pub struct SwitchableModelPort {
    inner: tokio::sync::RwLock<ActiveModelPort>,
}

impl SwitchableModelPort {
    pub fn new(selection: ModelSelection, port: Arc<dyn ModelPort>) -> Self {
        let capabilities = port.capabilities();
        Self {
            inner: tokio::sync::RwLock::new(ActiveModelPort {
                selection,
                metadata: ModelMetadata::default(),
                capabilities,
                generation: 0,
                port,
            }),
        }
    }

    pub fn from_active(active: ActiveModelPort) -> Self {
        Self {
            inner: tokio::sync::RwLock::new(active),
        }
    }

    pub async fn snapshot(&self) -> ActiveModelPort {
        self.inner.read().await.clone()
    }

    pub async fn set(&self, selection: ModelSelection, port: Arc<dyn ModelPort>) {
        let capabilities = port.capabilities();
        *self.inner.write().await = ActiveModelPort {
            selection,
            metadata: ModelMetadata::default(),
            capabilities,
            generation: 0,
            port,
        };
    }

    pub async fn set_active(&self, active: ActiveModelPort) {
        *self.inner.write().await = active;
    }
}

pub fn adapt_model_port(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<ActiveModelPort, ModelSelectionError> {
    adapt_model_endpoint(provider, model, ModelMetadata::default(), stream)
        .map(|endpoint| endpoint.port)
}

pub fn adapt_model_endpoint(
    provider: &str,
    model: &str,
    metadata: ModelMetadata,
    stream: Arc<dyn ModelStream>,
) -> Result<ActiveModelStream, ModelSelectionError> {
    let selection = ModelSelection::new(provider, model)?;
    let port: Arc<dyn ModelPort> = Arc::new(LegacyModelPort::new(selection.clone(), stream));
    let mut capabilities = port.capabilities();
    if metadata.context_window.is_some() {
        capabilities.context_window = metadata.context_window;
    }
    let active = ActiveModelPort {
        selection,
        metadata,
        capabilities,
        generation: 0,
        port,
    };
    let stream: Arc<dyn ModelStream> = Arc::new(ModelPortStreamAdapter::new(active.clone()));
    Ok(ActiveModelStream {
        stream,
        port: active,
    })
}

pub fn adapt_model_stream(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<Arc<dyn ModelStream>, ModelSelectionError> {
    Ok(adapt_model_endpoint(provider, model, ModelMetadata::default(), stream)?.stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeModelStream, StreamPiece, SwitchableModelStream};
    use serde_json::json;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn facade_round_trips_existing_provider_through_model_port() {
        let legacy: Arc<dyn ModelStream> = Arc::new(FakeModelStream::new(vec![vec![
            StreamPiece::Text("hello".into()),
            StreamPiece::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.rs"}),
            },
        ]]));
        let adapted = adapt_model_stream("openai", "gpt-test", legacy).unwrap();
        let (tx, mut rx) = mpsc::channel(16);

        adapted
            .stream(
                10,
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": []
                }),
                tx,
            )
            .await
            .unwrap();

        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "hello"));
        assert!(matches!(
            rx.recv().await,
            Some(StreamPiece::ToolCall { id, name, arguments })
                if id == "call-1"
                    && name == "read_file"
                    && arguments == json!({"path": "a.rs"})
        ));
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn facade_rejects_empty_selection() {
        let legacy: Arc<dyn ModelStream> = Arc::new(FakeModelStream::new(Vec::new()));
        assert!(adapt_model_stream("", "gpt-test", legacy).is_err());
    }

    #[tokio::test]
    async fn switch_generation_identifies_the_endpoint_that_completed() {
        let first_endpoint = adapt_model_endpoint(
            "p",
            "m1",
            ModelMetadata {
                context_window: Some(4_000),
                model_family: Some("family-a".into()),
            },
            Arc::new(FakeModelStream::new(Vec::new())),
        )
        .unwrap();
        let switchable = SwitchableModelStream::new(first_endpoint);
        let first = switchable.active_model_port().unwrap().generation;
        let second_endpoint = adapt_model_endpoint(
            "p",
            "m2",
            ModelMetadata {
                context_window: Some(2_000),
                model_family: Some("family-b".into()),
            },
            Arc::new(FakeModelStream::new(Vec::new())),
        )
        .unwrap();

        switchable.set_active(second_endpoint).await;

        let second = switchable.active_model_port().unwrap().generation;
        assert_eq!(second, first + 1);
    }
}
