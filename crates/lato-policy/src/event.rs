use lato_core::{SessionId, ToolCallId, ToolName, TurnId};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum PolicyEventKind {
    #[serde(rename = "policy.evaluated")]
    Evaluated,
    #[serde(rename = "policy.approval_requested")]
    ApprovalRequested,
    #[serde(rename = "policy.approval_consumed")]
    ApprovalConsumed,
    #[serde(rename = "policy.denied")]
    Denied,
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "tool.failed")]
    ToolFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PolicyEvent {
    pub kind: PolicyEventKind,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub call_id: Option<ToolCallId>,
    pub tool_name: Option<ToolName>,
    pub argument_digest: Option<String>,
    pub code: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub output_bytes: Option<usize>,
}

pub trait PolicyEventSink: Send + Sync {
    fn emit(&self, event: PolicyEvent);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopPolicyEventSink;

impl PolicyEventSink for NoopPolicyEventSink {
    fn emit(&self, _event: PolicyEvent) {}
}
