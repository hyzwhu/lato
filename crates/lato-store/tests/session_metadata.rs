use lato_core::{
    EventStore, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope, JournalRecord,
    JournalRecordId, SessionId, TurnId, UserInput,
};
use lato_store::{FileEventStore, TitleSource};
use std::sync::Arc;

fn input_envelope(session_id: &SessionId, text: &str, timestamp_ms: u64) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("record-{timestamp_ms}")),
        session_id: session_id.clone(),
        turn_id: Some(TurnId::from("turn-1")),
        journal_sequence: 0,
        timestamp_ms,
        record: JournalRecord::TurnInputAccepted {
            input: UserInput::text(text),
        },
    }
}

#[tokio::test]
async fn legacy_journal_derives_an_automatic_title() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    store
        .append(
            input_envelope(&id, "  修复\n登录\u{1b}[31m 流程并添加回归测试  ", 42),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();

    let summaries = store.list_session_summaries().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, id);
    assert_eq!(summaries[0].title, "修复 登录[31m 流程并添加回归测试");
    assert_eq!(summaries[0].title_source, TitleSource::Automatic);
    assert_eq!(summaries[0].created_at_ms, 42);
    assert_eq!(summaries[0].updated_at_ms, 42);
}

#[tokio::test]
async fn manual_title_wins_over_later_automatic_initialization() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    store
        .append(
            input_envelope(&id, "Automatic title", 42),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    store
        .ensure_automatic_title(&id, "Automatic title")
        .await
        .unwrap();
    store.rename_session(&id, "  手动\n标题  ").await.unwrap();
    store.ensure_automatic_title(&id, "Ignored").await.unwrap();

    let summary = store.list_session_summaries().await.unwrap().remove(0);
    assert_eq!(summary.title, "手动 标题");
    assert_eq!(summary.title_source, TitleSource::Manual);
}

#[tokio::test]
async fn concurrent_automatic_and_manual_updates_preserve_manual_title() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(FileEventStore::open(home.path()).unwrap());
    let id = SessionId::from("session-1");
    store
        .append(
            input_envelope(&id, "Automatic title", 42),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let automatic = tokio::spawn({
        let store = store.clone();
        let id = id.clone();
        async move {
            store
                .ensure_automatic_title(&id, "Automatic title")
                .await
                .unwrap()
        }
    });
    let manual = tokio::spawn({
        let store = store.clone();
        let id = id.clone();
        async move { store.rename_session(&id, "Manual title").await.unwrap() }
    });
    automatic.await.unwrap();
    manual.await.unwrap();

    let summary = store.list_session_summaries().await.unwrap().remove(0);
    assert_eq!(summary.title, "Manual title");
    assert_eq!(summary.title_source, TitleSource::Manual);
}

#[tokio::test]
async fn invalid_manual_titles_are_rejected() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    store
        .append(
            input_envelope(&id, "Automatic title", 42),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    assert!(store.rename_session(&id, " \n\u{1b}\u{7f} ").await.is_err());
}

#[tokio::test]
async fn corrupt_or_newer_metadata_blocks_mutation() {
    for body in [b"{".as_slice(), br#"{"schema_version":2}"#.as_slice()] {
        let home = tempfile::tempdir().unwrap();
        let store = FileEventStore::open(home.path()).unwrap();
        let id = SessionId::from("session-1");
        store
            .append(
                input_envelope(&id, "Automatic title", 42),
                JournalDurability::SyncData,
            )
            .await
            .unwrap();
        let metadata = store
            .journal_path(&id)
            .unwrap()
            .with_file_name("metadata.json");
        std::fs::write(metadata, body).unwrap();
        assert!(store.rename_session(&id, "Manual").await.is_err());
    }
}

#[tokio::test]
async fn delete_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    store
        .append(
            input_envelope(&id, "Automatic title", 42),
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    store.delete_session(&id).await.unwrap();
    store.delete_session(&id).await.unwrap();
    assert!(store.list_session_summaries().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn delete_rejects_a_symlinked_session_directory() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    let session_dir = store
        .journal_path(&id)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(outside.path(), &session_dir).unwrap();
    assert!(store.delete_session(&id).await.is_err());
    assert!(outside.path().exists());
}
