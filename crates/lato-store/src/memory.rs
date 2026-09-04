use async_trait::async_trait;
use lato_core::{
    EventStore, HistoryProjectionMetadata, HistoryProjectionStore, HistoryReplacementReason,
    JournalDurability, JournalEnvelope, JournalError, JournalRecord, JournalRecordId,
    JournalReplay, ModelMessage, ProjectionError, SessionId, history_digest, project_journal,
    projection_message,
};
use std::collections::BTreeMap;
use tokio::sync::Mutex;

use crate::validate_capacity;

#[derive(Default)]
pub struct MemoryEventStore {
    sessions: Mutex<BTreeMap<SessionId, Vec<JournalEnvelope>>>,
    replacements: Mutex<BTreeMap<SessionId, Vec<ModelMessage>>>,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        _durability: JournalDurability,
    ) -> Result<(), JournalError> {
        let session_id = envelope.session_id.clone();
        let mut sessions = self.sessions.lock().await;
        let existing = sessions.entry(session_id.clone()).or_default();
        let mut candidate = existing.clone();
        candidate.push(envelope);
        let sequence = candidate.last().map_or(0, |item| item.journal_sequence);
        validate_capacity(&candidate, sequence)?;
        project_journal(&session_id, &candidate)?;
        *existing = candidate;
        Ok(())
    }

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError> {
        let sessions = self.sessions.lock().await;
        let Some(envelopes) = sessions.get(session_id).cloned() else {
            return Ok(JournalReplay::empty(session_id.clone()));
        };
        let projection = project_journal(session_id, &envelopes)?;
        let mut projection = projection;
        if let Some(replacement) = self.replacements.lock().await.get(session_id).cloned()
            && let Some(marker_index) = envelopes.iter().rposition(|envelope| {
                matches!(
                    envelope.record,
                    JournalRecord::HistoryProjectionReplaced { .. }
                )
            })
        {
            projection.messages = replacement;
            for envelope in &envelopes[marker_index + 1..] {
                if let Some(message) = projection_message(&envelope.record) {
                    projection.messages.push(message);
                }
            }
        }
        Ok(JournalReplay {
            exists: true,
            envelopes,
            projection,
        })
    }

    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(session_id).cloned() {
            let projection = project_journal(session_id, &existing)?;
            return Ok(JournalReplay {
                exists: true,
                envelopes: existing,
                projection,
            });
        }
        let sequence = envelopes.last().map_or(0, |item| item.journal_sequence);
        validate_capacity(&envelopes, sequence)?;
        let projection = project_journal(session_id, &envelopes)?;
        sessions.insert(session_id.clone(), envelopes.clone());
        Ok(JournalReplay {
            exists: true,
            envelopes,
            projection,
        })
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError> {
        Ok(self.sessions.lock().await.keys().cloned().collect())
    }

    async fn shutdown(&self, _session_id: &SessionId) -> Result<(), JournalError> {
        Ok(())
    }
}

#[async_trait]
impl HistoryProjectionStore for MemoryEventStore {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        let replay =
            self.replay(session_id)
                .await
                .map_err(|error| ProjectionError::WriteFailed {
                    message: error.to_string(),
                })?;
        if replay.envelopes.is_empty() || !replay.projection.unresolved_tools.is_empty() {
            return Err(ProjectionError::Divergent {
                message: "history replacement is not safe".into(),
            });
        }
        let previous = replay.envelopes.last().expect("checked non-empty");
        let sequence = replay.projection.next_journal_sequence;
        let checkpoint_id = format!("memory-cp-{sequence}");
        let digest = history_digest(&messages)?;
        let record_id =
            JournalRecordId::from(format!("{session_id}-history-replacement-{sequence}"));
        self.append(
            JournalEnvelope {
                schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
                record_id: record_id.clone(),
                session_id: session_id.clone(),
                turn_id: None,
                journal_sequence: sequence,
                timestamp_ms: sequence,
                record: JournalRecord::HistoryProjectionReplaced {
                    checkpoint_id: checkpoint_id.clone(),
                    checkpoint_digest: digest.clone(),
                    replaced_through_sequence: previous.journal_sequence,
                    replaced_through_record_id: previous.record_id.clone(),
                    replacement_entry_count: messages.len() as u64,
                    history_digest: digest.clone(),
                    reason,
                    prior_checkpoint_id: replay.projection.active_checkpoint_id,
                },
            },
            JournalDurability::SyncData,
        )
        .await
        .map_err(|error| ProjectionError::WriteFailed {
            message: error.to_string(),
        })?;
        self.replacements
            .lock()
            .await
            .insert(session_id.clone(), messages.clone());
        Ok(HistoryProjectionMetadata::new(
            session_id.clone(),
            sequence,
            record_id,
            messages.len() as u64,
            0,
            digest,
            Some(checkpoint_id),
        ))
    }
}
