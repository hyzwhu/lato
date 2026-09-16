use lato_agent::{AcpHost, default_fake_stream};
use lato_core::{EventStore, JournalEnvelope, JournalRecord, JournalRecordId, SessionId};
use lato_protocol::JsonRpcReq;
use lato_store::FileEventStore;
use lato_workspace::SessionTrust;

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn host(lato_home: &std::path::Path) -> AcpHost {
    let cwd = std::env::current_dir().unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    AcpHost::new_with_home(
        cwd.clone(),
        SessionTrust::for_headless_prompt(&cwd),
        tx,
        default_fake_stream(),
        lato_home.to_path_buf(),
    )
}

async fn handle(
    host: &mut AcpHost,
    id: i32,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    host.handle(req(id, method, params)).await.unwrap()
}

fn journal_path(home: &std::path::Path, sid: &str) -> std::path::PathBuf {
    home.join("sessions").join(sid).join("events.jsonl")
}

fn journal_bytes(home: &std::path::Path, sid: &str) -> Vec<u8> {
    std::fs::read(journal_path(home, sid)).unwrap()
}

/// One full `session/new -> owner disappears -> session/resume -> next turn`
/// cycle against hosts that share nothing but the on-disk home. The owner
/// host is dropped without an explicit close, which is the in-process shape
/// of a process restart and the exact reproduction of the historical
/// `journal sequence mismatch` failure.
async fn rebuild_resume_cycle(iteration: usize) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let _ = iteration;

    // Owner host: create the session and run one turn.
    let (sid, owner_journal) = {
        let mut host = host(&home);
        let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let prompt = handle(
            &mut host,
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "one"}),
        )
        .await;
        assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
        (sid.clone(), journal_bytes(&home, &sid))
    };
    assert!(!owner_journal.is_empty());

    // Rebuilt host over the same home: resume and continue.
    let mut host = host(&home);
    let resume = handle(
        &mut host,
        3,
        "session/resume",
        serde_json::json!({"sessionId": sid}),
    )
    .await;
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    let prompt = handle(
        &mut host,
        4,
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "two"}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");

    // The rebuilt host only appends; the owner's bytes are never rewritten.
    let final_journal = journal_bytes(&home, &sid);
    assert!(
        final_journal.starts_with(owner_journal.as_slice()),
        "journal history was rewritten on resume"
    );
}

#[tokio::test]
async fn rebuilt_host_resumes_and_continues_after_owner_drop_repeatedly() {
    for iteration in 0..10 {
        rebuild_resume_cycle(iteration).await;
    }
}

#[tokio::test]
async fn never_prompted_session_resumes_after_owner_drop() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();

    let sid = {
        let mut host = host(&home);
        let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
        new["result"]["sessionId"].as_str().unwrap().to_string()
    };

    let owner_journal = journal_bytes(&home, &sid);
    let mut host = host(&home);
    let resume = handle(
        &mut host,
        2,
        "session/resume",
        serde_json::json!({"sessionId": sid}),
    )
    .await;
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    let prompt = handle(
        &mut host,
        3,
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "first"}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
    assert!(journal_bytes(&home, &sid).starts_with(owner_journal.as_slice()));
}

#[tokio::test]
async fn repeated_resume_round_trips_keep_appending() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();

    let sid = {
        let mut host = host(&home);
        let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
        new["result"]["sessionId"].as_str().unwrap().to_string()
    };
    let after_new = journal_bytes(&home, &sid);

    for turn in 0..3 {
        let mut host = host(&home);
        let resume = handle(
            &mut host,
            2,
            "session/resume",
            serde_json::json!({"sessionId": sid}),
        )
        .await;
        assert!(
            resume.get("error").is_none(),
            "resume {turn} failed: {resume}"
        );
        let prompt = handle(
            &mut host,
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": format!("turn-{turn}")}),
        )
        .await;
        assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
        let current = journal_bytes(&home, &sid);
        assert!(current.starts_with(after_new.as_slice()));
    }
}

/// Corrupts the journal by rewriting one line's `journal_sequence`/`record_id`
/// and asserts `session/resume` fails closed with a structured journal error
/// while the original bytes stay untouched.
async fn corrupted_sequence_is_rejected_without_rewriting(home: &std::path::Path, sid: &str) {
    let original = journal_bytes(home, sid);
    let mut lines: Vec<String> = String::from_utf8(original.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(lines.len() >= 2);

    let mut corrupt: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    corrupt["journal_sequence"] = serde_json::json!(0);
    corrupt["record_id"] = serde_json::json!(format!("{sid}-journal-0"));
    lines[1] = serde_json::to_string(&corrupt).unwrap();
    let corrupted = lines.join("\n") + "\n";
    std::fs::write(journal_path(home, sid), &corrupted).unwrap();

    let mut host = host(home);
    let resume = handle(
        &mut host,
        1,
        "session/resume",
        serde_json::json!({"sessionId": sid}),
    )
    .await;
    let message = resume["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("journal sequence mismatch")
            || message.contains("duplicate journal record id"),
        "expected a structured journal failure, got: {resume}"
    );
    assert_eq!(
        journal_bytes(home, sid),
        corrupted.as_bytes(),
        "resume rewrote a corrupt journal"
    );
}

#[tokio::test]
async fn duplicated_sequence_fails_closed_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let mut host = host(&home);
    let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    handle(
        &mut host,
        2,
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "one"}),
    )
    .await;
    corrupted_sequence_is_rejected_without_rewriting(&home, &sid).await;
}

#[tokio::test]
async fn gapped_sequence_fails_closed_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let sid = {
        let mut host = host(&home);
        let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
        new["result"]["sessionId"].as_str().unwrap().to_string()
    };
    let sid = SessionId::from(sid);

    // Append a handcrafted record that skips a sequence: index N holds N+1.
    let replay = FileEventStore::open(&home)
        .unwrap()
        .replay(&sid)
        .await
        .unwrap();
    let next = replay.projection.next_journal_sequence + 1;
    let envelope = JournalEnvelope {
        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{sid}-journal-{next}")),
        session_id: sid.clone(),
        turn_id: None,
        journal_sequence: next,
        timestamp_ms: 0,
        record: JournalRecord::SessionStopped,
    };
    let mut line = serde_json::to_vec(&envelope).unwrap();
    line.push(b'\n');
    let path = journal_path(&home, sid.as_str());
    let mut bytes = journal_bytes(&home, sid.as_str());
    bytes.extend_from_slice(&line);
    std::fs::write(&path, &bytes).unwrap();

    let mut host = host(&home);
    let resume = handle(
        &mut host,
        2,
        "session/resume",
        serde_json::json!({"sessionId": sid.as_str()}),
    )
    .await;
    let message = resume["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("journal sequence mismatch"),
        "expected a structured sequence failure, got: {resume}"
    );
    assert_eq!(
        journal_bytes(&home, sid.as_str()),
        bytes,
        "resume rewrote the journal"
    );
}
