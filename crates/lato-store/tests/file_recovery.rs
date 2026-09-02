use lato_core::{
    EventStore, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord,
    JournalRecordId, SessionId,
};
use lato_store::{FileEventStore, MAX_JOURNAL_BYTES};
use std::io::Write;

#[tokio::test]
async fn invalid_unterminated_tail_is_truncated() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("tail-session");
    store
        .append(envelope(&sid, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    let path = store.journal_path(&sid).unwrap();
    let clean_len = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"broken\"")
        .unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes.len(), 1);
    assert_eq!(std::fs::metadata(path).unwrap().len(), clean_len);
}

#[tokio::test]
async fn complete_bad_line_fails() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("bad-line-session");
    store
        .append(envelope(&sid, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    let path = store.journal_path(&sid).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap_err().code(),
        "journal.parse"
    );
}

#[tokio::test]
async fn valid_unterminated_tail_is_retained_and_terminated() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("valid-tail-session");
    store
        .append(envelope(&sid, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    let path = store.journal_path(&sid).unwrap();
    let bytes = serde_json::to_vec(&envelope(&sid, 1)).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    assert_eq!(store.replay(&sid).await.unwrap().envelopes.len(), 2);
    assert!(std::fs::read(path).unwrap().ends_with(b"\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_journal_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("symlink-session");
    let path = store.journal_path(&sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let target = directory.path().join("target.jsonl");
    std::fs::write(&target, b"").unwrap();
    symlink(target, path).unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap_err().code(),
        "journal.unsafe_restore"
    );
}

#[tokio::test]
async fn traversal_session_id_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("../escape");
    assert_eq!(
        store.journal_path(&sid).unwrap_err().code(),
        "journal.unsafe_restore"
    );
}

#[tokio::test]
async fn oversized_journal_is_rejected_before_reading() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("oversized-session");
    let path = store.journal_path(&sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();
    assert_eq!(
        store.replay(&sid).await.unwrap_err().code(),
        "journal.unsafe_restore"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn journal_and_session_directory_have_private_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(directory.path()).unwrap();
    let sid = SessionId::from("permissions-session");
    store
        .append(envelope(&sid, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    let path = store.journal_path(&sid).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
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
