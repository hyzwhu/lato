use lato_core::{
    EventStore, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord,
    JournalRecordId, SessionId,
};
use lato_store::{FileEventStore, MemoryEventStore};
use std::sync::Arc;

#[tokio::test]
async fn memory_store_satisfies_contract() {
    run_contract(Arc::new(MemoryEventStore::new())).await;
}

#[tokio::test]
async fn file_store_satisfies_contract() {
    let directory = tempfile::tempdir().unwrap();
    run_contract(Arc::new(FileEventStore::open(directory.path()).unwrap())).await;
}

async fn run_contract(store: Arc<dyn EventStore>) {
    let sid = SessionId::from("contract-session");
    assert!(!store.replay(&sid).await.unwrap().exists);
    store
        .append(envelope(&sid, 0), JournalDurability::Flush)
        .await
        .unwrap();
    store
        .append(envelope(&sid, 1), JournalDurability::SyncData)
        .await
        .unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert!(replay.exists);
    assert_eq!(replay.envelopes.len(), 2);
    assert_eq!(replay.projection.next_journal_sequence, 2);
    assert_eq!(store.list_sessions().await.unwrap(), vec![sid.clone()]);
    store.shutdown(&sid).await.unwrap();

    let gap = envelope(&sid, 3);
    assert_eq!(
        store
            .append(gap, JournalDurability::Flush)
            .await
            .unwrap_err()
            .code(),
        "journal.sequence"
    );
    assert_eq!(store.replay(&sid).await.unwrap().envelopes.len(), 2);

    let imported = SessionId::from("imported-session");
    let first = vec![envelope(&imported, 0)];
    let first_replay = store.import_if_absent(&imported, first).await.unwrap();
    assert_eq!(first_replay.envelopes.len(), 1);
    let second = vec![envelope(&imported, 0), envelope(&imported, 1)];
    let second_replay = store.import_if_absent(&imported, second).await.unwrap();
    assert_eq!(second_replay.envelopes.len(), 1);

    let duplicate = SessionId::from("duplicate-session");
    let first = envelope(&duplicate, 0);
    store
        .append(first.clone(), JournalDurability::Flush)
        .await
        .unwrap();
    let mut repeated_id = envelope(&duplicate, 1);
    repeated_id.record_id = first.record_id;
    assert_eq!(
        store
            .append(repeated_id, JournalDurability::Flush)
            .await
            .unwrap_err()
            .code(),
        "journal.duplicate_record"
    );
    assert_eq!(store.replay(&duplicate).await.unwrap().envelopes.len(), 1);
}

fn envelope(session_id: &SessionId, sequence: u64) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("record-{sequence}")),
        session_id: session_id.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record: if sequence == 0 {
            JournalRecord::SessionStarted
        } else {
            JournalRecord::SessionStopped
        },
    }
}
