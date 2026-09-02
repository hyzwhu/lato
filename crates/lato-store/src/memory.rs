use async_trait::async_trait;
use lato_core::{
    EventStore, JournalDurability, JournalEnvelope, JournalError, JournalReplay, SessionId,
    project_journal,
};
use std::collections::BTreeMap;
use tokio::sync::Mutex;

use crate::validate_capacity;

#[derive(Default)]
pub struct MemoryEventStore {
    sessions: Mutex<BTreeMap<SessionId, Vec<JournalEnvelope>>>,
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
