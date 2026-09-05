// Derived from: Grok-Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: generalized the child reporter channel into a foreground turn event emitter

use async_trait::async_trait;
use lato_core::{
    AgentError, CompactSession, CompactionCandidate, CompactionId, CompactionPolicy,
    CompactionTrigger, ContextUsage, ErrorCategory, JournalDurability, JournalRecord, ModelMessage,
    Retryability, TurnId, TurnOutput, UserInput,
};
use tokio::sync::{mpsc, oneshot};
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

#[derive(Clone, Debug)]
pub struct CompactionRequest {
    pub compaction_id: CompactionId,
    pub request: CompactSession,
    pub messages: Vec<ModelMessage>,
    pub policy: CompactionPolicy,
    pub two_pass: Option<TwoPassCompactionInput>,
}

pub struct CompactionControl {
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct AutomaticCompactionRequest {
    pub trigger: CompactionTrigger,
    pub usage: ContextUsage,
    pub messages: Vec<ModelMessage>,
    pub two_pass: Option<TwoPassCompactionInput>,
}

#[derive(Clone, Debug)]
pub struct PrefireCompactionRequest {
    pub messages: Vec<ModelMessage>,
    pub prefix_len: usize,
    pub policy: CompactionPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefireCompactionResult {
    pub note1: String,
}

#[derive(Clone, Debug)]
pub struct TwoPassCompactionInput {
    pub note1: String,
    pub prefix_len: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutomaticCompactionOutcome {
    Compacted(Vec<ModelMessage>),
    ContinueUnchanged,
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

    pub fn context_usage(&self, usage: ContextUsage) -> Result<(), AgentError> {
        self.send(DriverEvent::ContextUsageUpdated(usage))
    }

    pub async fn commit(
        &self,
        record: JournalRecord,
        durability: JournalDurability,
    ) -> Result<(), AgentError> {
        let (ack, result) = oneshot::channel();
        self.tx
            .send(DriverMessage::Commit {
                turn_id: self.turn_id.clone(),
                record,
                durability,
                ack,
            })
            .map_err(|_| event_bus_closed())?;
        result.await.map_err(|_| event_bus_closed())?
    }

    pub async fn compact(
        &self,
        request: AutomaticCompactionRequest,
    ) -> Result<AutomaticCompactionOutcome, AgentError> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(DriverMessage::AutomaticCompactionRequested {
                turn_id: self.turn_id.clone(),
                request,
                reply,
            })
            .map_err(|_| event_bus_closed())?;
        result.await.map_err(|_| event_bus_closed())?
    }

    pub async fn prefire_compaction(
        &self,
        request: PrefireCompactionRequest,
    ) -> Result<PrefireCompactionResult, AgentError> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(DriverMessage::PrefireCompactionRequested {
                turn_id: self.turn_id.clone(),
                request,
                reply,
            })
            .map_err(|_| event_bus_closed())?;
        result.await.map_err(|_| event_bus_closed())?
    }

    fn send(&self, event: DriverEvent) -> Result<(), AgentError> {
        self.tx
            .send(DriverMessage::LiveEvent {
                turn_id: self.turn_id.clone(),
                event,
            })
            .map_err(|_| event_bus_closed())
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

    async fn history_snapshot(&self) -> Result<Vec<ModelMessage>, AgentError> {
        Err(unsupported_compaction())
    }

    async fn compact(
        &self,
        _request: CompactionRequest,
        _control: CompactionControl,
    ) -> Result<CompactionCandidate, AgentError> {
        Err(unsupported_compaction())
    }

    async fn prefire_compaction(
        &self,
        _request: PrefireCompactionRequest,
        _control: CompactionControl,
    ) -> Result<PrefireCompactionResult, AgentError> {
        Err(unsupported_compaction())
    }

    async fn install_history(&self, _messages: Vec<ModelMessage>) -> Result<(), AgentError> {
        Err(unsupported_compaction())
    }
}

#[derive(Debug)]
pub(crate) enum DriverEvent {
    ModelDelta(String),
    ReasoningDelta(String),
    ContextUsageUpdated(ContextUsage),
}

#[derive(Debug)]
pub(crate) enum DriverMessage {
    LiveEvent {
        turn_id: TurnId,
        event: DriverEvent,
    },
    Commit {
        turn_id: TurnId,
        record: JournalRecord,
        durability: JournalDurability,
        ack: oneshot::Sender<Result<(), AgentError>>,
    },
    Finished {
        turn_id: TurnId,
        result: Result<TurnOutput, AgentError>,
    },
    CompactionFinished {
        compaction_id: CompactionId,
        result: Result<CompactionCandidate, AgentError>,
    },
    AutomaticCompactionRequested {
        turn_id: TurnId,
        request: AutomaticCompactionRequest,
        reply: oneshot::Sender<Result<AutomaticCompactionOutcome, AgentError>>,
    },
    PrefireCompactionRequested {
        turn_id: TurnId,
        request: PrefireCompactionRequest,
        reply: oneshot::Sender<Result<PrefireCompactionResult, AgentError>>,
    },
}

fn event_bus_closed() -> AgentError {
    AgentError::new(
        "runtime.event_bus_closed",
        ErrorCategory::InternalInvariant,
        "runtime event bus closed",
        Retryability::Never,
    )
}

fn unsupported_compaction() -> AgentError {
    AgentError::new(
        "compaction.unsupported_driver",
        ErrorCategory::InternalInvariant,
        "turn driver does not support context compaction",
        Retryability::Never,
    )
}
