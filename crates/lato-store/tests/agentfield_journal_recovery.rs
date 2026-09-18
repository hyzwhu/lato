// Phase 7C3: crash matrix for AgentField journal events at the store
// layer. Every append fault boundary must leave the journal crash-
// consistent (all-or-nothing), and an interrupted agentfield tail must
// follow the frozen repair semantics WITHOUT silently skipping data of a
// readable version.

use lato_core::{
    AGENTFIELD_JOURNAL_SCHEMA_VERSION, AgentFieldJournalEvent, AgentFieldRunStatus, EventStore,
    JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord, JournalRecordId,
    SessionId, project_journal,
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
    fn once(point: FaultPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            remaining: AtomicUsize::new(1),
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

fn sid(name: &str) -> SessionId {
    SessionId::from(name)
}

fn agentfield_envelope(
    session: &SessionId,
    sequence: u64,
    event: AgentFieldJournalEvent,
) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: AGENTFIELD_JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-record-{sequence}", session.as_str())),
        session_id: session.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record: JournalRecord::AgentField { event },
    }
}

fn plain_envelope(session: &SessionId, sequence: u64) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-record-{sequence}", session.as_str())),
        session_id: session.clone(),
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

fn intent(session: &str) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: "afrun_crash-1".into(),
        session_id: session.to_owned(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        created_at_ms: 1_000,
    }
}

fn terminal(session: &str) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldRunTerminal {
        run_id: "afrun_crash-1".into(),
        session_id: session.to_owned(),
        execution_id: Some("exec-1".into()),
        status: AgentFieldRunStatus::Completed,
        observed_at_ms: 1_300,
        summary: Some("done".into()),
        summary_truncated: false,
        last_error: None,
    }
}

#[tokio::test]
async fn agentfield_appends_are_crash_consistent_at_every_fault_boundary() {
    for point in [
        FaultPoint::BeforeWrite,
        FaultPoint::AfterWrite,
        FaultPoint::BeforeFlush,
        FaultPoint::AfterFlush,
        FaultPoint::BeforeSyncData,
        FaultPoint::AfterSyncData,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let session = sid("crash-matrix");
        // Seed a healthy prefix, then crash during the agentfield append.
        let faulted =
            FileEventStore::open_with_fault_injector(directory.path(), CountingFault::once(point))
                .unwrap();
        faulted
            .append(plain_envelope(&session, 0), JournalDurability::SyncData)
            .await
            .unwrap();
        // The faulted append either lands or not — the process retries on a
        // clean store instance exactly like a restarted binary would.
        let result = faulted
            .append(
                agentfield_envelope(&session, 1, intent(session.as_str())),
                JournalDurability::SyncData,
            )
            .await;
        drop(faulted);
        let store = FileEventStore::open(directory.path()).unwrap();
        let replay = store.replay(&session).await.unwrap();
        match result {
            Ok(()) => assert_eq!(replay.envelopes.len(), 2, "fault point {point:?}"),
            Err(_) => assert_eq!(replay.envelopes.len(), 1, "fault point {point:?}"),
        }
        // The replay projection stays valid either way (all-or-nothing).
        let projection = project_journal(&session, &replay.envelopes).unwrap();
        assert_eq!(
            projection.next_journal_sequence,
            replay.envelopes.len() as u64
        );
    }
}

#[tokio::test]
async fn interrupted_agentfield_tail_is_repaired_and_never_half_read() {
    let directory = tempfile::tempdir().unwrap();
    let session = sid("crash-matrix-tail");
    let store = FileEventStore::open(directory.path()).unwrap();
    store
        .append(plain_envelope(&session, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    let intent_envelope = agentfield_envelope(&session, 1, intent(session.as_str()));
    store
        .append(intent_envelope, JournalDurability::SyncData)
        .await
        .unwrap();
    // Simulate a torn final line: the terminal event written without its
    // trailing newline and cut mid-JSON (hard exit during append).
    let journal_path = directory
        .path()
        .join("sessions")
        .join(session.as_str())
        .join("events.jsonl");
    let torn = r#"{"schema_version":2,"record_id":"crash-matrix-tail-record-2","s"#;
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .unwrap();
        file.write_all(torn.as_bytes()).unwrap();
        file.flush().unwrap();
    }
    // Reopen like a restarted process: the torn tail is repaired, the
    // committed prefix survives intact, and the projection is valid.
    let store = FileEventStore::open(directory.path()).unwrap();
    let replay = store.replay(&session).await.unwrap();
    assert_eq!(replay.envelopes.len(), 2);
    let projection = project_journal(&session, &replay.envelopes).unwrap();
    assert_eq!(projection.agentfield_runs.len(), 1);
    assert_eq!(
        projection.agentfield_runs[0].status,
        AgentFieldRunStatus::Queued
    );
    // Appending continues from the repaired tail without sequence conflict.
    store
        .append(
            agentfield_envelope(&session, 2, terminal(session.as_str())),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let projection =
        project_journal(&session, &store.replay(&session).await.unwrap().envelopes).unwrap();
    assert_eq!(
        projection.agentfield_runs[0].status,
        AgentFieldRunStatus::Completed
    );
}

#[tokio::test]
async fn concurrent_agentfield_appends_keep_exactly_one_sequence_per_event() {
    let directory = tempfile::tempdir().unwrap();
    let session = sid("crash-matrix-concurrent");
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    store
        .append(plain_envelope(&session, 0), JournalDurability::SyncData)
        .await
        .unwrap();
    // Two writers race for the SAME next sequence: the store serializes
    // candidates under the lock and exactly one commit per sequence wins.
    let run_intent = |index: usize| AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: format!("afrun_race-{index}"),
        session_id: session.as_str().to_owned(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        created_at_ms: 1_000 + index as u64,
    };
    for sequence in 1..=2usize {
        let handles: Vec<_> = (0..2usize)
            .map(|index| {
                let store = store.clone();
                let session = session.clone();
                let event = run_intent(index + sequence * 10);
                tokio::spawn(async move {
                    store
                        .append(
                            agentfield_envelope(&session, sequence as u64, event),
                            JournalDurability::SyncData,
                        )
                        .await
                })
            })
            .collect();
        let mut successes = 0;
        for handle in handles {
            if handle.await.unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1, "sequence {sequence}: exactly one winner");
    }
    let replay = store.replay(&session).await.unwrap();
    assert_eq!(replay.envelopes.len(), 3);
    let projection = project_journal(&session, &replay.envelopes).unwrap();
    assert_eq!(projection.agentfield_runs.len(), 2);
}
