use lato_core::{
    HistoryProjectionEntry, HistoryProjectionMetadata, JournalRecordId, ModelMessage, ModelRole,
    ProjectionError, SessionId, history_digest,
};

#[test]
fn history_digest_is_stable_and_metadata_round_trips() {
    let messages = vec![ModelMessage {
        role: ModelRole::User,
        content: vec![],
    }];
    let digest = history_digest(&messages).unwrap();
    let metadata = HistoryProjectionMetadata::new(
        SessionId::from("projection-contract"),
        0,
        JournalRecordId::from("r0"),
        1,
        12,
        digest.clone(),
        None,
    );
    let decoded: HistoryProjectionMetadata =
        serde_json::from_slice(&serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert_eq!(decoded.history_digest, digest);
    assert_eq!(decoded, metadata);
}

#[test]
fn projection_errors_have_stable_codes() {
    assert_eq!(
        ProjectionError::Corrupt {
            message: "bad".into()
        }
        .code(),
        "projection.corrupt"
    );
    assert_eq!(
        ProjectionError::CheckpointMissing {
            checkpoint_id: "c1".into()
        }
        .code(),
        "projection.checkpoint_missing"
    );
}

#[test]
fn projection_entry_hash_detects_changes() {
    let mut entry = HistoryProjectionEntry::new(
        1,
        JournalRecordId::from("r1"),
        ModelMessage {
            role: ModelRole::Assistant,
            content: vec![],
        },
    )
    .unwrap();
    entry.journal_sequence = 2;
    assert_eq!(
        entry.validate_hash().unwrap_err().code(),
        "projection.corrupt"
    );
}
