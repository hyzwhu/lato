use crate::{HistoryItem, TranscriptStore};
use lato_core::{
    EventStore, JOURNAL_SCHEMA_VERSION, JournalEnvelope, JournalError, JournalRecord,
    JournalRecordId, JournalReplay, ModelContent, ModelMessage, ModelRole, SessionId, ToolCallId,
    ToolName, journal_request_hash,
};

pub fn history_to_model_messages(
    history: &[HistoryItem],
) -> Result<Vec<ModelMessage>, JournalError> {
    history.iter().map(history_item_to_message).collect()
}

pub fn model_messages_to_history(
    messages: &[ModelMessage],
) -> Result<Vec<HistoryItem>, JournalError> {
    let mut history = Vec::new();
    for message in messages {
        if message.content.is_empty() {
            return Err(migration_error("model message has no content"));
        }
        for content in &message.content {
            let item = match (&message.role, content) {
                (ModelRole::System, ModelContent::Text { text }) => {
                    HistoryItem::System(text.clone())
                }
                (ModelRole::User, ModelContent::Text { text }) => HistoryItem::User(text.clone()),
                (ModelRole::Assistant, ModelContent::Text { text }) => {
                    HistoryItem::AssistantText(text.clone())
                }
                (
                    ModelRole::Assistant,
                    ModelContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                    },
                ) => HistoryItem::ToolCall {
                    id: call_id.to_string(),
                    name: name.local_name().to_owned(),
                    arguments: arguments.clone(),
                },
                (ModelRole::Tool, ModelContent::ToolResult { call_id, output }) => {
                    HistoryItem::ToolResult {
                        id: call_id.to_string(),
                        output: output.clone(),
                    }
                }
                _ => {
                    return Err(migration_error(format!(
                        "model content cannot be represented in legacy history: {content:?}"
                    )));
                }
            };
            history.push(item);
        }
    }
    Ok(history)
}

pub async fn import_legacy_if_needed(
    session_id: &SessionId,
    transcripts: Option<&TranscriptStore>,
    events: &dyn EventStore,
) -> Result<JournalReplay, JournalError> {
    let replay = events.replay(session_id).await?;
    if replay.exists {
        return Ok(replay);
    }
    let Some(transcripts) = transcripts else {
        return Ok(replay);
    };
    let Some(history) = transcripts
        .load_optional(session_id.as_str())
        .map_err(migration_error)?
    else {
        return Ok(replay);
    };
    let messages = history_to_model_messages(&history)?;
    let digest_payload = serde_json::to_value(&history).map_err(|error| {
        migration_error(format!("serialize legacy transcript for digest: {error}"))
    })?;
    let digest = journal_request_hash("legacy_transcript", &digest_payload);
    let mut envelopes = Vec::with_capacity(messages.len() + 2);
    push_record(&mut envelopes, session_id, JournalRecord::SessionStarted);
    for message in messages {
        push_record(
            &mut envelopes,
            session_id,
            JournalRecord::ConversationItemCommitted { message },
        );
    }
    push_record(
        &mut envelopes,
        session_id,
        JournalRecord::LegacyTranscriptImported {
            source_version: 1,
            item_count: history.len() as u64,
            content_digest: digest,
        },
    );
    events.import_if_absent(session_id, envelopes).await
}

fn history_item_to_message(item: &HistoryItem) -> Result<ModelMessage, JournalError> {
    let (role, content) = match item {
        HistoryItem::System(text) => (ModelRole::System, ModelContent::Text { text: text.clone() }),
        HistoryItem::User(text) => (ModelRole::User, ModelContent::Text { text: text.clone() }),
        HistoryItem::AssistantText(text) => (
            ModelRole::Assistant,
            ModelContent::Text { text: text.clone() },
        ),
        HistoryItem::ToolCall {
            id,
            name,
            arguments,
        } => (
            ModelRole::Assistant,
            ModelContent::ToolCall {
                call_id: ToolCallId::parse(id.clone()).map_err(migration_error)?,
                name: legacy_tool_name(name)?,
                arguments: arguments.clone(),
            },
        ),
        HistoryItem::ToolResult { id, output } => (
            ModelRole::Tool,
            ModelContent::ToolResult {
                call_id: ToolCallId::parse(id.clone()).map_err(migration_error)?,
                output: output.clone(),
            },
        ),
        HistoryItem::CompactionSummary(_) => {
            return Err(migration_error(
                "legacy compaction summaries cannot be losslessly imported",
            ));
        }
    };
    Ok(ModelMessage {
        role,
        content: vec![content],
    })
}

fn legacy_tool_name(name: &str) -> Result<ToolName, JournalError> {
    ToolName::parse(name)
        .or_else(|_| ToolName::parse(format!("legacy:{name}")))
        .map_err(migration_error)
}

fn push_record(
    envelopes: &mut Vec<JournalEnvelope>,
    session_id: &SessionId,
    record: JournalRecord,
) {
    let sequence = envelopes.len() as u64;
    envelopes.push(JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{session_id}-legacy-{sequence}")),
        session_id: session_id.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record,
    });
}

fn migration_error(error: impl std::fmt::Display) -> JournalError {
    JournalError::MigrationFailed {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::JournalDurability;
    use lato_store::FileEventStore;

    #[test]
    fn supported_history_round_trips_without_losing_tool_arguments() {
        let history = vec![
            HistoryItem::User("hello".into()),
            HistoryItem::AssistantText("checking".into()),
            HistoryItem::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md","limit":2}),
            },
            HistoryItem::ToolResult {
                id: "call-1".into(),
                output: "content".into(),
            },
        ];
        let messages = history_to_model_messages(&history).unwrap();
        assert_eq!(model_messages_to_history(&messages).unwrap(), history);
    }

    #[tokio::test]
    async fn legacy_import_is_atomic_idempotent_and_preserves_the_source() {
        let directory = tempfile::tempdir().unwrap();
        let transcripts = TranscriptStore::open(directory.path()).unwrap();
        let source = vec![
            HistoryItem::User("old".into()),
            HistoryItem::AssistantText("answer".into()),
        ];
        transcripts.append("legacy-1", &source).unwrap();
        let events = FileEventStore::open(directory.path()).unwrap();
        let sid = SessionId::from("legacy-1");
        let first = import_legacy_if_needed(&sid, Some(&transcripts), &events)
            .await
            .unwrap();
        let second = import_legacy_if_needed(&sid, Some(&transcripts), &events)
            .await
            .unwrap();
        assert_eq!(first.envelopes, second.envelopes);
        assert_eq!(
            model_messages_to_history(&first.projection.messages).unwrap(),
            source
        );
        assert_eq!(transcripts.load("legacy-1").unwrap(), source);
        assert!(events.journal_path(&sid).unwrap().is_file());
    }

    #[tokio::test]
    async fn existing_journal_wins_over_different_legacy_content() {
        let directory = tempfile::tempdir().unwrap();
        let transcripts = TranscriptStore::open(directory.path()).unwrap();
        transcripts
            .append("journal-wins", &[HistoryItem::User("legacy".into())])
            .unwrap();
        let events = FileEventStore::open(directory.path()).unwrap();
        let sid = SessionId::from("journal-wins");
        events
            .append(
                JournalEnvelope {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    record_id: JournalRecordId::from("journal-wins-0"),
                    session_id: sid.clone(),
                    turn_id: None,
                    journal_sequence: 0,
                    timestamp_ms: 0,
                    record: JournalRecord::SessionStarted,
                },
                JournalDurability::SyncData,
            )
            .await
            .unwrap();
        let replay = import_legacy_if_needed(&sid, Some(&transcripts), &events)
            .await
            .unwrap();
        assert!(replay.projection.messages.is_empty());
        assert_eq!(replay.envelopes.len(), 1);
    }

    #[tokio::test]
    async fn malformed_legacy_transcript_does_not_create_a_journal() {
        let directory = tempfile::tempdir().unwrap();
        let transcripts = TranscriptStore::open(directory.path()).unwrap();
        std::fs::write(
            directory.path().join("sessions/broken.jsonl"),
            b"not-json\n",
        )
        .unwrap();
        let events = FileEventStore::open(directory.path()).unwrap();
        let sid = SessionId::from("broken");
        assert!(
            import_legacy_if_needed(&sid, Some(&transcripts), &events)
                .await
                .is_err()
        );
        assert!(!events.replay(&sid).await.unwrap().exists);
    }
}
