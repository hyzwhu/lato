use crate::{AgentError, EventId, SessionId, TurnId};

pub const EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EventEnvelope {
    pub schema_version: u16,
    pub event_id: EventId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub parent_event_id: Option<EventId>,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub payload: EventPayload,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    SessionStarted,
    TurnStarted,
    ModelDelta { text: String },
    ReasoningDelta { text: String },
    TurnCompleted(TurnOutput),
    TurnFailed { error: AgentError },
    TurnCancelled { reason: CancelReason },
    SessionStopped,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TurnOutput {
    pub final_text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    User,
    Replaced,
    Shutdown,
}
