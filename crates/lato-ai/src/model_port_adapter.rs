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
    pub capabilities: ModelCapabilities,
    pub port: Arc<dyn ModelPort>,
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
                capabilities,
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
            capabilities,
            port,
        };
    }
}

pub fn adapt_model_port(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<ActiveModelPort, ModelSelectionError> {
    let selection = ModelSelection::new(provider, model)?;
    let port: Arc<dyn ModelPort> = Arc::new(LegacyModelPort::new(selection.clone(), stream));
    let capabilities = port.capabilities();
    Ok(ActiveModelPort {
        selection,
        capabilities,
        port,
    })
}

pub fn adapt_model_stream(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<Arc<dyn ModelStream>, ModelSelectionError> {
    let active = adapt_model_port(provider, model, stream)?;
    Ok(Arc::new(ModelPortStreamAdapter::new(active)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeModelStream, StreamPiece};
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
}
