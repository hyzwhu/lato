use lato_core::{
    EventStore, JournalDurability, JournalEnvelope, JournalRecord, JournalRecordId, SessionId,
};
use lato_store::FileEventStore;
use std::sync::Arc;

fn envelope(session_id: &SessionId, sequence: u64, record: JournalRecord) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-journal-{sequence}", session_id.as_str())),
        session_id: session_id.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record,
    }
}

/// Regression for D-WIN26-01: two independent store instances (the in-process
/// shape of two processes, each with its own writer and in-memory mutex) race
/// to append the same next sequence behind a release barrier. The
/// validate-then-append critical section is cross-process locked, so exactly
/// one side may win and the journal must stay replay-valid afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_store_instances_appending_the_same_sequence_exactly_once_succeeds() {
    for round in 0..50 {
        let dir = tempfile::tempdir().unwrap();
        let sid = SessionId::from(format!("race-{round}"));
        let first_store = FileEventStore::open(dir.path()).unwrap();
        let second_store = FileEventStore::open(dir.path()).unwrap();
        first_store
            .append(
                envelope(&sid, 0, JournalRecord::SessionStarted),
                JournalDurability::SyncData,
            )
            .await
            .unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier_first = barrier.clone();
        let sid_first = sid.clone();
        let sid_second = sid.clone();
        let (first, second) = tokio::join!(
            async move {
                barrier_first.wait().await;
                first_store
                    .append(
                        envelope(&sid_first, 1, JournalRecord::SessionStopped),
                        JournalDurability::SyncData,
                    )
                    .await
            },
            async move {
                barrier.wait().await;
                second_store
                    .append(
                        envelope(&sid_second, 1, JournalRecord::SessionStopped),
                        JournalDurability::SyncData,
                    )
                    .await
            },
        );

        // Exactly one writer wins; the loser is rejected with a structured
        // sequence failure instead of corrupting the journal.
        let successes = first.is_ok() as u8 + second.is_ok() as u8;
        assert_eq!(
            successes, 1,
            "round {round}: first={first:?} second={second:?}"
        );
        let loser = if first.is_err() { first } else { second };
        assert!(
            matches!(loser.unwrap_err(), lato_core::JournalError::Sequence { .. }),
            "round {round}: loser must fail with a sequence error"
        );

        // A third independent instance must replay the journal successfully:
        // strictly monotonic sequences, no duplicates, no gaps.
        let replay = FileEventStore::open(dir.path())
            .unwrap()
            .replay(&sid)
            .await
            .unwrap();
        assert_eq!(replay.envelopes.len(), 2, "round {round}");
        assert_eq!(replay.projection.next_journal_sequence, 2, "round {round}");
        assert_eq!(replay.envelopes[0].journal_sequence, 0, "round {round}");
        assert_eq!(replay.envelopes[1].journal_sequence, 1, "round {round}");
        assert!(
            matches!(replay.envelopes[1].record, JournalRecord::SessionStopped),
            "round {round}"
        );
    }
}
