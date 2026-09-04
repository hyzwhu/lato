use lato_core::{
    EventStore, HISTORY_PROJECTION_SCHEMA_VERSION, HistoryProjectionEntry,
    HistoryProjectionMetadata, HistoryProjectionStore, HistoryReplacementReason,
    JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord, JournalRecordId,
    ModelContent, ModelMessage, ModelRole, SessionId, history_digest,
};
use lato_store::FileEventStore;

#[tokio::test]
async fn missing_projection_is_built_from_canonical_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-rebuild");
    store
        .append(
            message_envelope(&sid, 0, "hello"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let history = store.history_path(&sid).unwrap();
    std::fs::remove_file(&history).unwrap();
    std::fs::remove_file(store.history_metadata_path(&sid).unwrap()).unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, vec![message("hello")]);
    assert!(history.is_file());
}

#[tokio::test]
async fn corrupt_projection_is_quarantined_then_rebuilt() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-corrupt");
    store
        .append(
            message_envelope(&sid, 0, "hello"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let history = store.history_path(&sid).unwrap();
    std::fs::write(&history, b"bad\n").unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, vec![message("hello")]);
    assert!(history.with_file_name("history.jsonl.corrupt").is_file());
}

#[tokio::test]
async fn canonical_corruption_is_not_hidden_by_valid_projection() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("canonical-corrupt");
    store
        .append(
            message_envelope(&sid, 0, "hello"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.journal_path(&sid).unwrap())
        .unwrap()
        .write_all(b"bad\n")
        .unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap_err().code(),
        "journal.parse"
    );
}

#[tokio::test]
async fn internally_consistent_but_divergent_projection_is_rebuilt() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-divergent");
    store
        .append(
            message_envelope(&sid, 0, "canonical"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let replacement = message("forged");
    let entry =
        HistoryProjectionEntry::new(0, JournalRecordId::from("record-0"), replacement.clone())
            .unwrap();
    let mut bytes = serde_json::to_vec(&entry).unwrap();
    bytes.push(b'\n');
    std::fs::write(store.history_path(&sid).unwrap(), &bytes).unwrap();
    let metadata = HistoryProjectionMetadata::new(
        sid.clone(),
        0,
        JournalRecordId::from("record-0"),
        1,
        bytes.len() as u64,
        history_digest(&[replacement]).unwrap(),
        None,
    );
    std::fs::write(
        store.history_metadata_path(&sid).unwrap(),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap().projection.messages,
        vec![message("canonical")]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn projection_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-permissions");
    store
        .append(
            message_envelope(&sid, 0, "hello"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    for path in [
        store.history_path(&sid).unwrap(),
        store.history_metadata_path(&sid).unwrap(),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn replacement_publishes_checkpoint_before_using_compacted_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-replacement");
    store
        .append(
            message_envelope(&sid, 0, "original"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let compacted = vec![message("summary")];
    let metadata = store
        .replace_history(&sid, compacted.clone(), HistoryReplacementReason::Repair)
        .await
        .unwrap();
    assert!(metadata.active_checkpoint_id.is_some());
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, compacted);
    assert_eq!(replay.envelopes.len(), 2);
    let checkpoint = store
        .journal_path(&sid)
        .unwrap()
        .parent()
        .unwrap()
        .join("compaction_checkpoints")
        .join(format!("{}.json", metadata.active_checkpoint_id.unwrap()));
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(checkpoint).unwrap()).unwrap();
    assert_eq!(value["schema_version"], HISTORY_PROJECTION_SCHEMA_VERSION);
}

#[tokio::test]
async fn missing_checkpoint_referenced_by_journal_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-checkpoint-missing");
    store
        .append(
            message_envelope(&sid, 0, "original"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let metadata = store
        .replace_history(
            &sid,
            vec![message("summary")],
            HistoryReplacementReason::Repair,
        )
        .await
        .unwrap();
    let checkpoint = store
        .journal_path(&sid)
        .unwrap()
        .parent()
        .unwrap()
        .join("compaction_checkpoints")
        .join(format!("{}.json", metadata.active_checkpoint_id.unwrap()));
    std::fs::remove_file(checkpoint).unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap_err().code(),
        "projection.checkpoint_missing"
    );
}

#[tokio::test]
async fn messages_after_replacement_extend_compacted_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-replacement-tail");
    store
        .append(
            message_envelope(&sid, 0, "original"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    store
        .replace_history(
            &sid,
            vec![message("summary")],
            HistoryReplacementReason::Repair,
        )
        .await
        .unwrap();
    store
        .append(
            message_envelope(&sid, 2, "tail"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap().projection.messages,
        vec![message("summary"), message("tail")]
    );
    let metadata: HistoryProjectionMetadata =
        serde_json::from_slice(&std::fs::read(store.history_metadata_path(&sid).unwrap()).unwrap())
            .unwrap();
    assert!(metadata.active_checkpoint_id.is_some());
    assert_eq!(metadata.generation, 1);
}

fn message(text: &str) -> ModelMessage {
    ModelMessage {
        role: ModelRole::Assistant,
        content: vec![ModelContent::Text { text: text.into() }],
    }
}

fn message_envelope(session_id: &SessionId, sequence: u64, text: &str) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("record-{sequence}")),
        session_id: session_id.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record: JournalRecord::ConversationItemCommitted {
            message: message(text),
        },
    }
}
