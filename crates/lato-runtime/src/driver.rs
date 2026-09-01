// Derived from: Grok-Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: generalized the child reporter channel into a foreground turn event emitter

use async_trait::async_trait;
use lato_core::{AgentError, ErrorCategory, Retryability, TurnId, TurnOutput, UserInput};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct TurnRequest {
    pub turn_id: TurnId,
    pub input: UserInput,
}

pub struct TurnControl {
    pub cancellation: CancellationToken,
    pub steering: mpsc::UnboundedReceiver<UserInput>,
}

#[derive(Clone)]
pub struct TurnEventEmitter {
    pub(crate) turn_id: TurnId,
    pub(crate) tx: mpsc::UnboundedSender<DriverMessage>,
}

impl TurnEventEmitter {
    pub fn model_delta(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.send(DriverEvent::ModelDelta(text.into()))
    }

    pub fn reasoning_delta(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.send(DriverEvent::ReasoningDelta(text.into()))
    }

    fn send(&self, event: DriverEvent) -> Result<(), AgentError> {
        self.tx
            .send(DriverMessage::Event {
                turn_id: self.turn_id.clone(),
                event,
            })
            .map_err(|_| {
                AgentError::new(
                    "runtime.event_bus_closed",
                    ErrorCategory::InternalInvariant,
                    "runtime event bus closed",
                    Retryability::Never,
                )
            })
    }
}

#[async_trait]
pub trait TurnDriver: Send + Sync + 'static {
    async fn run(
        &self,
        request: TurnRequest,
        control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, AgentError>;
}

#[derive(Debug)]
pub(crate) enum DriverEvent {
    ModelDelta(String),
    ReasoningDelta(String),
}

#[derive(Debug)]
pub(crate) enum DriverMessage {
    Event {
        turn_id: TurnId,
        event: DriverEvent,
    },
    Finished {
        turn_id: TurnId,
        result: Result<TurnOutput, AgentError>,
    },
}
