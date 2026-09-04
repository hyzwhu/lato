use lato_core::{
    CompactionId, CompactionTrigger, EventStore, HISTORY_PROJECTION_SCHEMA_VERSION,
    HistoryProjectionEntry, HistoryProjectionMetadata, HistoryProjectionStore,
    HistoryReplacementReason, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope,
    JournalRecord, JournalRecordId, ModelContent, ModelMessage, ModelRole, SessionId,
    history_digest,
};
use lato_store::{FaultPoint, FileEventStore, FileFaultInjector};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct CountingFault {
    point: FaultPoint,
    remaining: AtomicUsize,
}

impl CountingFault {
    fn times(point: FaultPoint, times: usize) -> Arc<Self> {
        Arc::new(Self {
            point,
            remaining: AtomicUsize::new(times),
        })
    }
}

impl FileFaultInjector for CountingFault {
    fn check(&self, point: FaultPoint) -> Result<(), lato_core::JournalError> {
        if point == self.point
            && self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
        {
            return Err(lato_core::JournalError::Io {
                message: format!("injected {point:?}"),
            });
        }
        Ok(())
    }
}

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

#[tokio::test]
async fn checkpoint_failure_keeps_old_history_authoritative() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("projection-checkpoint-failure");
    seed_original(directory.path(), &sid).await;
    let store = FileEventStore::open_with_fault_injector(
        directory.path(),
        CountingFault::times(FaultPoint::BeforeCheckpointPublish, 1),
    )
    .unwrap();
    assert!(
        store
            .replace_history(
                &sid,
                vec![message("summary")],
                HistoryReplacementReason::ContextCompaction,
            )
            .await
            .is_err()
    );
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, vec![message("original")]);
    assert!(replay.projection.active_checkpoint_id.is_none());
}

#[tokio::test]
async fn marker_failure_keeps_old_history_authoritative() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("projection-marker-failure");
    seed_original(directory.path(), &sid).await;
    let store = FileEventStore::open_with_fault_injector(
        directory.path(),
        CountingFault::times(FaultPoint::BeforeWrite, 2),
    )
    .unwrap();
    assert!(
        store
            .replace_history(
                &sid,
                vec![message("summary")],
                HistoryReplacementReason::ContextCompaction,
            )
            .await
            .is_err()
    );
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, vec![message("original")]);
    assert!(replay.projection.active_checkpoint_id.is_none());
}

#[tokio::test]
async fn post_marker_publication_failures_rebuild_from_the_new_checkpoint() {
    for (index, point) in [
        FaultPoint::BeforeHistoryPublish,
        FaultPoint::BeforeMetadataPublish,
    ]
    .into_iter()
    .enumerate()
    {
        let directory = tempfile::tempdir().unwrap();
        let sid = SessionId::from(format!("projection-post-marker-{index}"));
        seed_original(directory.path(), &sid).await;
        let store = FileEventStore::open_with_fault_injector(
            directory.path(),
            CountingFault::times(point, 1),
        )
        .unwrap();
        assert!(
            store
                .replace_history(
                    &sid,
                    vec![message("summary")],
                    HistoryReplacementReason::ContextCompaction,
                )
                .await
                .is_err()
        );
        let replay = store.replay(&sid).await.unwrap();
        assert_eq!(replay.projection.messages, vec![message("summary")]);
        assert!(replay.projection.active_checkpoint_id.is_some());
        assert!(matches!(
            replay.envelopes.last().unwrap().record,
            JournalRecord::HistoryProjectionReplaced {
                reason: HistoryReplacementReason::ContextCompaction,
                ..
            }
        ));
    }
}

#[tokio::test]
async fn compaction_lifecycle_records_only_advance_projection_cursors() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("projection-compaction-lifecycle");
    store
        .append(
            message_envelope(&sid, 0, "original"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    for (sequence, record) in [
        JournalRecord::CompactionRequested {
            compaction_id: CompactionId::from("compact-1"),
            trigger: CompactionTrigger::Manual,
            user_context: None,
        },
        JournalRecord::CompactionFailed {
            compaction_id: CompactionId::from("compact-1"),
            error_code: "projection.write_failed".into(),
        },
        JournalRecord::CompactionCancelled {
            compaction_id: CompactionId::from("compact-2"),
        },
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append(
                JournalEnvelope {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    record_id: JournalRecordId::from(format!("lifecycle-{sequence}")),
                    session_id: sid.clone(),
                    turn_id: None,
                    journal_sequence: sequence as u64 + 1,
                    timestamp_ms: sequence as u64 + 1,
                    record,
                },
                JournalDurability::SyncData,
            )
            .await
            .unwrap();
    }
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages, vec![message("original")]);
    assert_eq!(replay.projection.next_journal_sequence, 4);
    assert!(replay.projection.active_checkpoint_id.is_none());
}

async fn seed_original(directory: &std::path::Path, sid: &SessionId) {
    let store = FileEventStore::open(directory).unwrap();
    store
        .append(
            message_envelope(sid, 0, "original"),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    store.shutdown(sid).await.unwrap();
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
