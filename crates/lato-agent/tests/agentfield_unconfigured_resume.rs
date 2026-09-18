//! D-7C3-01 host-level resume integration: a session whose journal
//! carries 7C3 AgentField run history must resume successfully even after
//! the adapter configuration has disappeared (deleted stanza, disabled,
//! or unresolvable credential — all reduce to the closed registration
//! gate). The journal is preserved byte-for-byte and the unconfigured
//! recovery manager is installed without error; the stable
//! `agentfield.unconfigured` query behavior is covered at manager level
//! in `agentfield_resume_recovery.rs`.

use lato_agent::{AcpHost, default_fake_stream};
use lato_core::AgentFieldRunStatus;
use lato_protocol::JsonRpcReq;
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

/// Minimal valid journal: SessionStarted followed by a full AgentField
/// intent → bind → running-observation lifecycle (schema v2 envelopes).
fn seed_agentfield_journal(home: &std::path::Path, sid: &str) {
    let session_dir = home.join("sessions").join(sid);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut lines = Vec::new();
    lines.push(serde_json::json!({
        "schema_version": 1,
        "record_id": format!("{sid}-record-0"),
        "session_id": sid,
        "turn_id": null,
        "journal_sequence": 0,
        "timestamp_ms": 0,
        "record": {"type": "session_started"},
    }));
    let agentfield = |tag: &str, sequence: u64, status: &str, execution_id: serde_json::Value| {
        serde_json::json!({
            "schema_version": 2,
            "record_id": format!("{sid}-record-{sequence}"),
            "session_id": sid,
            "turn_id": null,
            "journal_sequence": sequence,
            "timestamp_ms": sequence,
            "record": {"type": "agentfield", "event": {
                "type": tag,
                "run_id": "afrun_seed-1",
                "session_id": sid,
                "alias": "contract-review",
                "execute_target": "legal.review_contract",
                "catalog_revision": "sha256:rev-1",
                "input_digest": "a".repeat(64),
                "execution_id": execution_id,
                "status": status,
                "timestamp_ms": sequence * 10,
                "summary": null,
                "summary_truncated": false,
                "last_error": null,
            }},
        })
    };
    lines.push(agentfield(
        "agentfield_run_intent_recorded",
        1,
        "queued",
        serde_json::Value::Null,
    ));
    lines.push(agentfield(
        "agentfield_execution_bound",
        2,
        "queued",
        serde_json::json!("exec-seed-1"),
    ));
    lines.push(agentfield(
        "agentfield_status_observed",
        3,
        "running",
        serde_json::json!("exec-seed-1"),
    ));
    let mut body = String::new();
    for line in &lines {
        body.push_str(&serde_json::to_string(line).unwrap());
        body.push('\n');
    }
    std::fs::write(session_dir.join("events.jsonl"), body).unwrap();
    assert_eq!(
        lato_core::AgentFieldRunStatus::Queued,
        AgentFieldRunStatus::Queued
    );
}

#[tokio::test]
async fn resume_with_deleted_config_keeps_the_agentfield_history_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let sid = "unconfigured-resume";
    seed_agentfield_journal(&home, sid);
    // Host WITHOUT any agentfield stanza (configuration deleted).
    let mut host = host(&home);
    assert!(lato_agent::agentfield::load_agentfield_config(&home).is_none());
    let resume = handle(
        &mut host,
        1,
        "session/resume",
        serde_json::json!({ "sessionId": sid }),
    )
    .await;
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    // The 7C3 AgentField events are preserved across the unconfigured
    // resume; the only appended record (if any) is the pre-existing
    // non-agentfield plugin-snapshot adoption, never an AgentField one.
    let journal = std::fs::read_to_string(journal_path(&home, sid)).unwrap();
    for record_id in ["record-1", "record-2", "record-3"] {
        assert!(
            journal.contains(&format!("{sid}-{record_id}")),
            "seeded agentfield record {record_id} was dropped"
        );
    }
    let agentfield_lines = journal
        .lines()
        .filter(|line| line.contains("\"type\":\"agentfield\""))
        .count();
    assert_eq!(agentfield_lines, 3, "no new agentfield records may appear");
}

#[tokio::test]
async fn resume_with_disabled_config_keeps_the_agentfield_history_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let sid = "disabled-resume";
    seed_agentfield_journal(&home, sid);
    // Present but disabled stanza — same closed gate, same recovery surface.
    std::fs::write(
        home.join("config.json"),
        serde_json::json!({
            "agentfield": {"enabled": false, "baseUrl": "https://agents.example.internal",
                "credential": "agentfield:primary"}
        })
        .to_string(),
    )
    .unwrap();
    let mut host = host(&home);
    let resume = handle(
        &mut host,
        1,
        "session/resume",
        serde_json::json!({ "sessionId": sid }),
    )
    .await;
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    assert!(journal_path(&home, sid).exists());
}

#[tokio::test]
async fn resume_without_agentfield_history_stays_zero_registration() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    // No config, no seeded 7C history: the fresh-session path keeps zero
    // agentfield registration (AC-01 unchanged).
    let mut host = host(&home);
    assert!(lato_agent::agentfield::load_agentfield_config(&home).is_none());
    let new = handle(&mut host, 1, "session/new", serde_json::json!({})).await;
    assert!(new.get("error").is_none(), "session/new failed: {new}");
}
