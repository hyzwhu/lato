use crate::{
    AgentError, ApprovalFingerprint, CancelReason, CompactionId, CompactionTrigger,
    HistoryReplacementReason, JournalValidation, ModelContent, ModelMessage, ModelRole,
    ProjectionError, Retryability, SandboxObligation, SessionId, SideEffect, ToolCallId,
    ToolCapability, ToolError, ToolIdempotency, ToolName, ToolOutput, TurnId, TurnOutput,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::JournalRecordId;

pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalDurability {
    Flush,
    SyncData,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct JournalEnvelope {
    pub schema_version: u32,
    pub record_id: JournalRecordId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub journal_sequence: u64,
    pub timestamp_ms: u64,
    pub record: JournalRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditStage {
    Evaluated,
    ApprovalRequested,
    ApprovalResolved,
    GrantConsumed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditDecision {
    Allowed,
    ApprovalRequired,
    Approved,
    Denied { code: String },
    Consumed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PolicyAuditRecord {
    pub stage: PolicyAuditStage,
    pub decision: PolicyAuditDecision,
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub request_hash: String,
    pub approval_fingerprint: ApprovalFingerprint,
    pub capabilities: Vec<ToolCapability>,
    pub sandbox: SandboxObligation,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PreparedToolAudit {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub request_hash: String,
    pub approval_fingerprint: ApprovalFingerprint,
    pub idempotency: ToolIdempotency,
    pub side_effect: SideEffect,
    pub sandbox: SandboxObligation,
    pub capabilities: Vec<ToolCapability>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    SessionStarted,
    TurnInputAccepted {
        input: crate::UserInput,
    },
    ConversationItemCommitted {
        message: ModelMessage,
    },
    PolicyDecisionCommitted {
        audit: PolicyAuditRecord,
    },
    ToolCallRequested {
        call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        request_hash: String,
    },
    ToolCallPrepared {
        audit: PreparedToolAudit,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        request_hash: String,
        result: Result<ToolOutput, ToolError>,
    },
    ToolCallRejected {
        call_id: ToolCallId,
        request_hash: String,
        error: ToolError,
    },
    TurnCompleted {
        output: TurnOutput,
    },
    TurnFailed {
        error: AgentError,
    },
    TurnCancelled {
        reason: CancelReason,
    },
    CompactionRequested {
        compaction_id: CompactionId,
        trigger: CompactionTrigger,
        user_context: Option<String>,
    },
    CompactionFailed {
        compaction_id: CompactionId,
        error_code: String,
    },
    CompactionCancelled {
        compaction_id: CompactionId,
    },
    SessionStopped,
    LegacyTranscriptImported {
        source_version: u32,
        item_count: u64,
        content_digest: String,
    },
    HistoryProjectionReplaced {
        checkpoint_id: String,
        checkpoint_digest: String,
        replaced_through_sequence: u64,
        replaced_through_record_id: JournalRecordId,
        replacement_entry_count: u64,
        history_digest: String,
        reason: HistoryReplacementReason,
        prior_checkpoint_id: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct UnresolvedToolCall {
    pub call_id: ToolCallId,
    pub request_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalTerminal {
    TurnCompleted,
    TurnFailed,
    TurnCancelled,
    SessionStopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionProjection {
    pub session_id: SessionId,
    pub messages: Vec<ModelMessage>,
    pub next_journal_sequence: u64,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JournalReplay {
    pub exists: bool,
    pub envelopes: Vec<JournalEnvelope>,
    pub projection: SessionProjection,
}

impl JournalReplay {
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            exists: false,
            envelopes: Vec::new(),
            projection: SessionProjection {
                session_id,
                messages: Vec::new(),
                next_journal_sequence: 0,
                unresolved_tools: Vec::new(),
                terminal: None,
                active_checkpoint_id: None,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JournalError {
    #[error("journal io: {message}")]
    Io { message: String },
    #[error("journal full at sequence {sequence}: limit {limit}")]
    Full { sequence: u64, limit: u64 },
    #[error("journal parse at line {line}: {message}")]
    Parse { line: usize, message: String },
    #[error("unsafe journal restore: {message}")]
    UnsafeRestore { message: String },
    #[error("journal sequence mismatch: expected {expected}, found {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("duplicate journal record id {record_id}")]
    DuplicateRecord { record_id: JournalRecordId },
    #[error("unsupported journal schema {actual}; expected {expected}")]
    SchemaUnsupported { expected: u32, actual: u32 },
    #[error("journal session mismatch: expected {expected}, found {actual}")]
    SessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("journal replay divergence for tool call {call_id}: {message}")]
    Divergence {
        call_id: ToolCallId,
        message: String,
    },
    #[error("tool call {call_id} has an unknown side-effect outcome")]
    IncompleteSideEffect { call_id: ToolCallId },
    #[error("legacy journal migration failed: {message}")]
    MigrationFailed { message: String },
    #[error("corrupt journal: {message}")]
    Corrupt { message: String },
    #[error(transparent)]
    Projection(#[from] ProjectionError),
}

impl JournalError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "journal.io",
            Self::Full { .. } => "journal.full",
            Self::Parse { .. } => "journal.parse",
            Self::UnsafeRestore { .. } => "journal.unsafe_restore",
            Self::Sequence { .. } => "journal.sequence",
            Self::DuplicateRecord { .. } => "journal.duplicate_record",
            Self::SchemaUnsupported { .. } => "journal.schema_unsupported",
            Self::SessionMismatch { .. } => "journal.session_mismatch",
            Self::Divergence { .. } => "journal.divergence",
            Self::IncompleteSideEffect { .. } => "journal.incomplete_side_effect",
            Self::MigrationFailed { .. } => "journal.migration_failed",
            Self::Corrupt { .. } => "journal.corrupt",
            Self::Projection(error) => error.code(),
        }
    }

    pub fn retryability(&self) -> Retryability {
        match self {
            Self::Io { .. } => Retryability::AfterBackoff,
            Self::Projection(error) => error.retryability(),
            _ => Retryability::Never,
        }
    }
}

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError>;

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError>;

    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError>;

    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError>;

    async fn shutdown(&self, session_id: &SessionId) -> Result<(), JournalError>;
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

pub fn journal_request_hash(kind: &str, payload: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"lato-journal-request-v1\0");
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_json(payload).to_string().as_bytes());
    format!("sha256:v1:{:x}", hasher.finalize())
}

pub fn project_journal(
    session_id: &SessionId,
    envelopes: &[JournalEnvelope],
) -> Result<SessionProjection, JournalError> {
    #[derive(Clone)]
    struct Requested {
        name: ToolName,
        arguments: Value,
        hash: String,
        prepared: bool,
    }

    let mut record_ids = BTreeSet::new();
    let mut requested = BTreeMap::<ToolCallId, Requested>::new();
    let mut messages = Vec::new();
    let mut terminal = None;
    let mut active_checkpoint_id = None;
    let mut checkpoint_ids = BTreeSet::new();

    for (index, envelope) in envelopes.iter().enumerate() {
        if envelope.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::SchemaUnsupported {
                expected: JOURNAL_SCHEMA_VERSION,
                actual: envelope.schema_version,
            });
        }
        if &envelope.session_id != session_id {
            return Err(JournalError::SessionMismatch {
                expected: session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        let expected = index as u64;
        if envelope.journal_sequence != expected {
            return Err(JournalError::Sequence {
                expected,
                actual: envelope.journal_sequence,
            });
        }
        if !record_ids.insert(envelope.record_id.clone()) {
            return Err(JournalError::DuplicateRecord {
                record_id: envelope.record_id.clone(),
            });
        }

        match &envelope.record {
            JournalRecord::SessionStarted
            | JournalRecord::PolicyDecisionCommitted { .. }
            | JournalRecord::CompactionRequested { .. }
            | JournalRecord::CompactionFailed { .. }
            | JournalRecord::CompactionCancelled { .. }
            | JournalRecord::LegacyTranscriptImported { .. } => {}
            JournalRecord::HistoryProjectionReplaced { checkpoint_id, .. } => {
                if requested.values().any(|call| call.prepared) {
                    return Err(JournalError::Corrupt {
                        message: "history replaced with an unresolved prepared tool".into(),
                    });
                }
                if !checkpoint_ids.insert(checkpoint_id.clone()) {
                    return Err(JournalError::Corrupt {
                        message: format!("duplicate history checkpoint {checkpoint_id}"),
                    });
                }
                active_checkpoint_id = Some(checkpoint_id.clone());
            }
            JournalRecord::TurnInputAccepted { input } => messages.push(ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: input.text.clone(),
                }],
            }),
            JournalRecord::ConversationItemCommitted { message } => messages.push(message.clone()),
            JournalRecord::ToolCallRequested {
                call_id,
                name,
                arguments,
                request_hash,
            } => {
                if requested.contains_key(call_id) {
                    return Err(divergence(call_id, "tool call was requested twice"));
                }
                requested.insert(
                    call_id.clone(),
                    Requested {
                        name: name.clone(),
                        arguments: arguments.clone(),
                        hash: request_hash.clone(),
                        prepared: false,
                    },
                );
                messages.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }],
                });
            }
            JournalRecord::ToolCallPrepared { audit } => {
                let Some(call) = requested.get_mut(&audit.call_id) else {
                    return Err(divergence(&audit.call_id, "preparation has no request"));
                };
                if call.hash != audit.request_hash || call.prepared {
                    return Err(divergence(
                        &audit.call_id,
                        "preparation does not match the requested call",
                    ));
                }
                call.prepared = true;
            }
            JournalRecord::ToolCallCompleted {
                call_id,
                request_hash,
                result,
            } => {
                let Some(call) = requested.remove(call_id) else {
                    return Err(divergence(call_id, "completion has no request"));
                };
                if !call.prepared || call.hash != *request_hash {
                    return Err(divergence(call_id, "completion does not match preparation"));
                }
                let output = match result {
                    Ok(output) => output.content.clone(),
                    Err(error) => format!("ERROR [{}]: {}", error.code, error.message),
                };
                messages.push(tool_result(call_id.clone(), output));
            }
            JournalRecord::ToolCallRejected {
                call_id,
                request_hash,
                error,
            } => {
                let Some(call) = requested.remove(call_id) else {
                    return Err(divergence(call_id, "rejection has no request"));
                };
                if call.prepared || call.hash != *request_hash {
                    return Err(divergence(call_id, "rejection does not match request"));
                }
                messages.push(tool_result(
                    call_id.clone(),
                    format!("ERROR [{}]: {}", error.code, error.message),
                ));
            }
            JournalRecord::TurnCompleted { .. } => terminal = Some(JournalTerminal::TurnCompleted),
            JournalRecord::TurnFailed { .. } => terminal = Some(JournalTerminal::TurnFailed),
            JournalRecord::TurnCancelled { .. } => terminal = Some(JournalTerminal::TurnCancelled),
            JournalRecord::SessionStopped => terminal = Some(JournalTerminal::SessionStopped),
        }
    }

    let unresolved_tools = requested
        .into_iter()
        .filter_map(|(call_id, call)| {
            let _ = (&call.name, &call.arguments);
            call.prepared.then_some(UnresolvedToolCall {
                call_id,
                request_hash: call.hash,
            })
        })
        .collect();

    Ok(SessionProjection {
        session_id: session_id.clone(),
        messages,
        next_journal_sequence: envelopes.len() as u64,
        unresolved_tools,
        terminal,
        active_checkpoint_id,
    })
}

pub fn validate_journal(
    session_id: &SessionId,
    envelopes: &[JournalEnvelope],
) -> Result<JournalValidation, JournalError> {
    let projection = project_journal(session_id, envelopes)?;
    Ok(JournalValidation {
        session_id: session_id.clone(),
        next_journal_sequence: projection.next_journal_sequence,
        last_record_id: envelopes.last().map(|item| item.record_id.clone()),
        message_count: projection.messages.len() as u64,
        history_digest: crate::history_digest(&projection.messages)?,
        unresolved_tools: projection.unresolved_tools,
        terminal: projection.terminal,
        active_checkpoint_id: projection.active_checkpoint_id,
    })
}

pub fn projection_message(record: &JournalRecord) -> Option<ModelMessage> {
    match record {
        JournalRecord::TurnInputAccepted { input } => Some(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: input.text.clone(),
            }],
        }),
        JournalRecord::ConversationItemCommitted { message } => Some(message.clone()),
        JournalRecord::ToolCallRequested {
            call_id,
            name,
            arguments,
            ..
        } => Some(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }],
        }),
        JournalRecord::ToolCallCompleted {
            call_id, result, ..
        } => Some(tool_result(
            call_id.clone(),
            match result {
                Ok(output) => output.content.clone(),
                Err(error) => format!("ERROR [{}]: {}", error.code, error.message),
            },
        )),
        JournalRecord::ToolCallRejected { call_id, error, .. } => Some(tool_result(
            call_id.clone(),
            format!("ERROR [{}]: {}", error.code, error.message),
        )),
        _ => None,
    }
}

fn divergence(call_id: &ToolCallId, message: impl Into<String>) -> JournalError {
    JournalError::Divergence {
        call_id: call_id.clone(),
        message: message.into(),
    }
}

fn tool_result(call_id: ToolCallId, output: String) -> ModelMessage {
    ModelMessage {
        role: ModelRole::Tool,
        content: vec![ModelContent::ToolResult { call_id, output }],
    }
}
