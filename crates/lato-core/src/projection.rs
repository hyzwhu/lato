use crate::{
    JournalRecordId, JournalTerminal, ModelMessage, Retryability, SessionId, UnresolvedToolCall,
    canonical_json,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HISTORY_PROJECTION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryProjectionEntry {
    pub schema_version: u32,
    pub journal_sequence: u64,
    pub record_id: JournalRecordId,
    pub message: ModelMessage,
    pub entry_hash: String,
}

impl HistoryProjectionEntry {
    pub fn new(
        journal_sequence: u64,
        record_id: JournalRecordId,
        message: ModelMessage,
    ) -> Result<Self, ProjectionError> {
        let mut entry = Self {
            schema_version: HISTORY_PROJECTION_SCHEMA_VERSION,
            journal_sequence,
            record_id,
            message,
            entry_hash: String::new(),
        };
        entry.entry_hash = digest_value(
            "lato-history-entry-v1",
            &serde_json::to_value((
                entry.schema_version,
                entry.journal_sequence,
                &entry.record_id,
                &entry.message,
            ))
            .map_err(|error| ProjectionError::Corrupt {
                message: error.to_string(),
            })?,
        );
        Ok(entry)
    }

    pub fn validate_hash(&self) -> Result<(), ProjectionError> {
        let expected = Self::new(
            self.journal_sequence,
            self.record_id.clone(),
            self.message.clone(),
        )?
        .entry_hash;
        if self.entry_hash != expected {
            return Err(ProjectionError::Corrupt {
                message: format!(
                    "history entry hash mismatch at sequence {}",
                    self.journal_sequence
                ),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryProjectionMetadata {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub generation: u64,
    pub last_journal_sequence: u64,
    pub last_record_id: JournalRecordId,
    pub entry_count: u64,
    pub byte_length: u64,
    pub history_digest: String,
    pub active_checkpoint_id: Option<String>,
}

impl HistoryProjectionMetadata {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        last_journal_sequence: u64,
        last_record_id: JournalRecordId,
        entry_count: u64,
        byte_length: u64,
        history_digest: String,
        active_checkpoint_id: Option<String>,
    ) -> Self {
        Self {
            schema_version: HISTORY_PROJECTION_SCHEMA_VERSION,
            session_id,
            generation: 0,
            last_journal_sequence,
            last_record_id,
            entry_count,
            byte_length,
            history_digest,
            active_checkpoint_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReplacementReason {
    ContextCompaction,
    Rewind,
    Repair,
}

#[async_trait]
pub trait HistoryProjectionStore: Send + Sync {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError>;
}

pub trait SessionStore: crate::EventStore + HistoryProjectionStore {}

impl<T> SessionStore for T where T: crate::EventStore + HistoryProjectionStore + ?Sized {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryCheckpoint {
    pub schema_version: u32,
    pub checkpoint_id: String,
    pub session_id: SessionId,
    pub replaced_through_sequence: u64,
    pub replaced_through_record_id: JournalRecordId,
    pub prior_checkpoint_id: Option<String>,
    pub messages: Vec<ModelMessage>,
    pub content_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub messages: Vec<ModelMessage>,
    pub next_journal_sequence: u64,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalValidation {
    pub session_id: SessionId,
    pub next_journal_sequence: u64,
    pub last_record_id: Option<JournalRecordId>,
    pub message_count: u64,
    pub history_digest: String,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProjectionError {
    #[error("corrupt history projection: {message}")]
    Corrupt { message: String },
    #[error("divergent history projection: {message}")]
    Divergent { message: String },
    #[error("projection checkpoint is missing: {checkpoint_id}")]
    CheckpointMissing { checkpoint_id: String },
    #[error("projection checkpoint mismatch: {message}")]
    CheckpointMismatch { message: String },
    #[error("projection limit exceeded: {message}")]
    LimitExceeded { message: String },
    #[error("projection quarantine failed: {message}")]
    QuarantineFailed { message: String },
    #[error("projection write failed: {message}")]
    WriteFailed { message: String },
}

impl ProjectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Corrupt { .. } => "projection.corrupt",
            Self::Divergent { .. } => "projection.divergent",
            Self::CheckpointMissing { .. } => "projection.checkpoint_missing",
            Self::CheckpointMismatch { .. } => "projection.checkpoint_mismatch",
            Self::LimitExceeded { .. } => "projection.limit_exceeded",
            Self::QuarantineFailed { .. } => "projection.quarantine_failed",
            Self::WriteFailed { .. } => "projection.write_failed",
        }
    }
    pub fn retryability(&self) -> Retryability {
        if matches!(self, Self::WriteFailed { .. }) {
            Retryability::AfterBackoff
        } else {
            Retryability::Never
        }
    }
}

pub fn history_digest(messages: &[ModelMessage]) -> Result<String, ProjectionError> {
    let value = serde_json::to_value(messages).map_err(|error| ProjectionError::Corrupt {
        message: error.to_string(),
    })?;
    Ok(digest_value("lato-history-v1", &value))
}

pub fn checkpoint_digest(
    session_id: &SessionId,
    sequence: u64,
    record_id: &JournalRecordId,
    prior: Option<&str>,
    messages: &[ModelMessage],
) -> Result<String, ProjectionError> {
    Ok(digest_value(
        "lato-history-checkpoint-v1",
        &serde_json::json!({"session_id": session_id, "sequence": sequence, "record_id": record_id, "prior": prior, "messages": messages}),
    ))
}

fn digest_value(domain: &str, value: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_json(value).to_string().as_bytes());
    format!("sha256:v1:{:x}", hasher.finalize())
}
