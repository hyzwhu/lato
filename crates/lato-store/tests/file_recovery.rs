use lato_core::{
    EventStore, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord,
    JournalRecordId, SessionId,
};
use lato_store::{FaultPoint, FileEventStore, FileFaultInjector, MAX_JOURNAL_BYTES};
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct CountingFault {
    point: FaultPoint,
    remaining: AtomicUsize,
}

impl CountingFault {
    fn once(point: FaultPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            remaining: AtomicUsize::new(1),
        })
    }

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
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
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

#[tokio::test]
async fn every_append_fault_boundary_recovers_to_one_acknowledged_record() {
    for point in [
        FaultPoint::BeforeWrite,
        FaultPoint::AfterWrite,
        FaultPoint::BeforeFlush,
        FaultPoint::AfterFlush,
        FaultPoint::BeforeSyncData,
        FaultPoint::AfterSyncData,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store =
            FileEventStore::open_with_fault_injector(directory.path(), CountingFault::once(point))
                .unwrap();
        let sid = SessionId::from("fault-session");
        store
            .append(envelope(&sid, 0), JournalDurability::SyncData)
            .await
            .unwrap_or_else(|error| panic!("{point:?} did not recover: {error}"));
        let replay = store.replay(&sid).await.unwrap();
        assert_eq!(replay.envelopes.len(), 1, "fault point {point:?}");
        assert_eq!(replay.envelopes[0].journal_sequence, 0);
    }
}

#[tokio::test]
async fn failed_retry_does_not_advance_the_persisted_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileEventStore::open_with_fault_injector(
        directory.path(),
        CountingFault::times(FaultPoint::BeforeWrite, 2),
    )
    .unwrap();
    let sid = SessionId::from("failed-retry-session");
    assert!(
        store
            .append(envelope(&sid, 0), JournalDurability::SyncData)
            .await
            .is_err()
    );
    assert!(store.replay(&sid).await.unwrap().envelopes.is_empty());
    store
        .append(envelope(&sid, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    assert_eq!(store.replay(&sid).await.unwrap().envelopes.len(), 1);
}

#[tokio::test]
async fn import_faults_never_replace_an_existing_authoritative_journal() {
    let sid = SessionId::from("import-fault-session");

    let before_directory = tempfile::tempdir().unwrap();
    let before = FileEventStore::open_with_fault_injector(
        before_directory.path(),
        CountingFault::once(FaultPoint::BeforeRename),
    )
    .unwrap();
    assert!(
        before
            .import_if_absent(&sid, vec![envelope(&sid, 0)])
            .await
            .is_err()
    );
    assert!(!before.replay(&sid).await.unwrap().exists);
    assert_eq!(
        before
            .import_if_absent(&sid, vec![envelope(&sid, 0)])
            .await
            .unwrap()
            .envelopes
            .len(),
        1
    );

    let after_directory = tempfile::tempdir().unwrap();
    let after = FileEventStore::open_with_fault_injector(
        after_directory.path(),
        CountingFault::once(FaultPoint::AfterRename),
    )
    .unwrap();
    assert!(
        after
            .import_if_absent(&sid, vec![envelope(&sid, 0)])
            .await
            .is_err()
    );
    let reopened = FileEventStore::open(after_directory.path()).unwrap();
    assert_eq!(reopened.replay(&sid).await.unwrap().envelopes.len(), 1);
    let different = vec![envelope(&sid, 0), envelope(&sid, 1)];
    assert_eq!(
        reopened
            .import_if_absent(&sid, different)
            .await
            .unwrap()
            .envelopes
            .len(),
        1
    );
}

#[tokio::test]
async fn concurrent_imports_converge_without_overwriting() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    let sid = SessionId::from("concurrent-import-session");
    let first = {
        let store = store.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            store
                .import_if_absent(&sid, vec![envelope(&sid, 0)])
                .await
                .unwrap()
        })
    };
    let second = {
        let store = store.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            store
                .import_if_absent(&sid, vec![envelope(&sid, 0), envelope(&sid, 1)])
                .await
                .unwrap()
        })
    };
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.envelopes, second.envelopes);
    assert_eq!(store.replay(&sid).await.unwrap().envelopes, first.envelopes);
}

#[cfg(unix)]
#[test]
fn symlinked_sessions_directory_is_rejected() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), directory.path().join("sessions")).unwrap();
    let error = match FileEventStore::open(directory.path()) {
        Ok(_) => panic!("symlinked sessions directory must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "journal.unsafe_restore");
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
