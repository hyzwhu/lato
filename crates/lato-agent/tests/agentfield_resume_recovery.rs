//! Phase 7C3 cross-process recovery integration tests: repeated resume
//! rounds over a REAL on-disk journal keep run identity, status, and
//! sequence stable with zero duplicate remote side effects; the raw
//! journal bytes never contain secrets; and a resumed cancel reuses the
//! original remote execution id.

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use lato_agent::agentfield::journal::{AgentFieldJournalSink, JournalSinkError};
use lato_agent::agentfield::types::{
    AsyncStartEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope, StatusEnvelope,
};
use lato_agent::agentfield::{
    AgentFieldCatalog, AgentFieldClient, AgentFieldError, AgentFieldManager, RunStatus,
};
use lato_core::{
    AGENTFIELD_JOURNAL_SCHEMA_VERSION, AgentFieldJournalEvent, EventStore, JournalDurability,
    JournalEnvelope, JournalRecord, JournalRecordId, SessionId, project_journal,
};
use lato_store::FileEventStore;
use tempfile::TempDir;

const SECRET: &str = "sk-live-7c3-journal-secret-value";

// ---- durable sink over a real FileEventStore -------------------------------

/// Mirrors the production SessionLoop writer contract: the sink allocates
/// the next journal sequence for the session, stamps schema version 2 on
/// AgentField records, and resolves only after the durable SyncData
/// commit. Sequence continuity across "process restarts" is taken from
/// the replayed journal length, exactly like the real loop bootstrap.
struct FileJournalSink {
    store: Arc<FileEventStore>,
    session: SessionId,
    next_sequence: AtomicU64,
}

impl FileJournalSink {
    async fn resumed(store: &Arc<FileEventStore>, session: &SessionId) -> Self {
        let replay = store.replay(session).await.unwrap();
        Self {
            store: store.clone(),
            session: session.clone(),
            next_sequence: AtomicU64::new(replay.envelopes.len() as u64),
        }
    }
}

impl AgentFieldJournalSink for FileJournalSink {
    fn append(
        &self,
        event: AgentFieldJournalEvent,
    ) -> futures_util::future::BoxFuture<'static, Result<(), JournalSinkError>> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        let envelope = JournalEnvelope {
            schema_version: AGENTFIELD_JOURNAL_SCHEMA_VERSION,
            record_id: JournalRecordId::from(format!(
                "{}-journal-{}",
                self.session.as_str(),
                sequence
            )),
            session_id: self.session.clone(),
            turn_id: None,
            journal_sequence: sequence,
            timestamp_ms: sequence,
            record: JournalRecord::AgentField { event },
        };
        let store = self.store.clone();
        Box::pin(async move {
            store
                .append(envelope, JournalDurability::SyncData)
                .await
                .map_err(|error| JournalSinkError {
                    code: "journal.unavailable",
                    message: error.to_string(),
                })
        })
    }
}

// ---- fake client with a secret-carrying remote result ----------------------

struct FakeClient {
    start_calls: AtomicUsize,
    status_calls: AtomicUsize,
    cancel_calls: AtomicUsize,
    cancel_targets: std::sync::Mutex<Vec<String>>,
}

impl FakeClient {
    fn new() -> Self {
        Self {
            start_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            cancel_calls: AtomicUsize::new(0),
            cancel_targets: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AgentFieldClient for FakeClient {
    async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
        unreachable!("discovery is not part of these tests")
    }

    async fn start_async(
        &self,
        _execute_target: &str,
        _input: &serde_json::Value,
    ) -> Result<AsyncStartEnvelope, AgentFieldError> {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        Ok(AsyncStartEnvelope::decode(&serde_json::json!({
            "execution_id": "exec-original-1",
            "status": "queued",
            "target": "legal.review_contract",
            "type": "reasoner",
            "run_id": "run-remote-1",
            "workflow_id": "run-remote-1",
            "created_at": "2026-09-18T00:00:00Z",
            "enqueued_at": "2026-09-18T00:00:00Z",
            "webhook_registered": false,
        }))
        .unwrap())
    }

    async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        Ok(StatusEnvelope::decode(&serde_json::json!({
            "execution_id": execution_id,
            "status": "running",
            "target": "legal.review_contract",
            "created_at": "2026-09-18T00:00:00Z",
            "run_id": "run-remote-1",
            "started_at": "2026-09-18T00:00:01Z",
            "webhook_registered": false,
        }))
        .unwrap())
    }

    async fn cancel(
        &self,
        execution_id: &str,
        _reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        self.cancel_targets
            .lock()
            .unwrap()
            .push(execution_id.to_owned());
        Ok(Some(
            CancelSuccessEnvelope::decode(&serde_json::json!({
                "execution_id": execution_id,
                "status": "cancelled",
                "previous_status": "running",
                "cancelled_at": "2026-09-18T00:00:05Z",
                "run_id": "run-remote-1",
            }))
            .unwrap(),
        ))
    }
}

fn manager_with(
    session: &str,
    client: Option<Arc<dyn AgentFieldClient>>,
    sink: Arc<dyn AgentFieldJournalSink>,
) -> AgentFieldManager {
    let factory = client.map(|client| {
        let shared: Arc<dyn AgentFieldClient> = client;
        Arc::new(move || {
            let shared = shared.clone();
            Box::pin(async move { Ok(shared.clone()) })
                as futures_util::future::BoxFuture<
                    'static,
                    Result<Arc<dyn AgentFieldClient>, AgentFieldError>,
                >
        }) as lato_agent::agentfield::manager::ClientFactory
    });
    AgentFieldManager::with_factory(session, catalog(), factory, Some(sink))
}

fn catalog() -> AgentFieldCatalog {
    let config = lato_agent::agentfield::config::AgentFieldConfig::parse(&serde_json::json!({
        "enabled": true,
        "baseUrl": "https://agents.example.com",
        "credential": "agentfield:primary",
        "capabilities": {
            "contract-review": {
                "target": "legal.review_contract",
                "description": "Review one contract",
                "inputSchema": {
                    "type": "object",
                    "properties": {"contract": {"type": "string"}},
                    "required": ["contract"],
                    "additionalProperties": false
                },
                "risk": "remote_read",
            }
        },
    }))
    .unwrap()
    .unwrap();
    AgentFieldCatalog::from_config(&config)
}

fn journal_path(dir: &TempDir, session: &str) -> std::path::PathBuf {
    dir.path()
        .join("sessions")
        .join(session)
        .join("events.jsonl")
}

#[tokio::test]
async fn ten_round_resume_keeps_run_identity_status_sequence_and_side_effects_stable() {
    let directory = TempDir::new().unwrap();
    let session_id = "resume-stability";
    let session = SessionId::from(session_id);
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());

    // Round 0: start a run and observe it running.
    let sink = FileJournalSink::resumed(&store, &session).await;
    let client = Arc::new(FakeClient::new());
    let manager = manager_with(session_id, Some(client.clone()), Arc::new(sink));
    let run = manager
        .start_run(
            "contract-review",
            "legal.review_contract",
            "sha256:rev-1",
            &serde_json::json!({"contract": "v1"}),
        )
        .await
        .unwrap();
    let observed = manager.run_status(Some(&run.run_id)).await.unwrap();
    assert_eq!(observed[0].status, RunStatus::Running);
    let expected_run_id = run.run_id.clone();
    assert_eq!(client.start_calls.load(Ordering::SeqCst), 1);
    drop(manager);

    // Rounds 1..=10: a restarted process replays, restores, and observes —
    // identity, status, sequence, and side-effect counts never move.
    for round in 1..=10usize {
        let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
        let replay = store.replay(&session).await.unwrap();
        let projection = project_journal(&session, &replay.envelopes).unwrap();
        assert_eq!(projection.agentfield_runs.len(), 1, "round {round}");
        let restored = &projection.agentfield_runs[0];
        assert_eq!(restored.run_id, expected_run_id, "round {round}");
        assert_eq!(
            restored.execution_id.as_deref(),
            Some("exec-original-1"),
            "round {round}"
        );
        assert_eq!(restored.status, lato_core::AgentFieldRunStatus::Running);
        // Exactly one durable start intent across every round: no resumed
        // round ever re-sent or re-created the remote execution.
        let intents = replay
            .envelopes
            .iter()
            .filter(|envelope| {
                matches!(
                    &envelope.record,
                    JournalRecord::AgentField {
                        event: AgentFieldJournalEvent::AgentFieldRunIntentRecorded { .. },
                    }
                )
            })
            .count();
        assert_eq!(intents, 1, "round {round}");
        // Resume itself is zero-network and zero-write: the journal bytes
        // do not grow while restoring and inspecting locally.
        let bytes_before = std::fs::metadata(journal_path(&directory, session_id))
            .unwrap()
            .len();
        let manager = manager_with(
            session_id,
            None,
            Arc::new(FileJournalSink::resumed(&store, &session).await),
        );
        manager
            .restore_runs(projection.agentfield_runs.clone())
            .unwrap();
        // Without a resolvable client the lazy reconcile fails closed with
        // the stable code while the last known state stays visible.
        let error = manager
            .run_status(Some(&expected_run_id))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "agentfield.unavailable", "round {round}");
        let restored_state = manager.run(&expected_run_id).await.expect("run visible");
        assert_eq!(restored_state.status, RunStatus::Running, "round {round}");
        drop(manager);
        let bytes_after = std::fs::metadata(journal_path(&directory, session_id))
            .unwrap()
            .len();
        assert_eq!(
            bytes_before, bytes_after,
            "round {round}: resume wrote nothing"
        );
        assert_eq!(
            replay.projection.next_journal_sequence,
            replay.envelopes.len() as u64,
            "round {round}"
        );
    }
}

#[tokio::test]
async fn journal_raw_bytes_never_contain_secrets() {
    let directory = TempDir::new().unwrap();
    let session_id = "secret-scan";
    let session = SessionId::from(session_id);
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    let sink = FileJournalSink::resumed(&store, &session).await;
    let client = Arc::new(SecretLeakingClient::new());
    let manager = manager_with(
        session_id,
        Some(client as Arc<dyn AgentFieldClient>),
        Arc::new(sink),
    );
    let run = manager
        .start_run(
            "contract-review",
            "legal.review_contract",
            "sha256:rev-1",
            &serde_json::json!({"token": SECRET}),
        )
        .await
        .unwrap();
    let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Completed);
    drop(manager);

    // Scan the raw journal bytes, plus every printable debug surface of the
    // recovered state, for the secret and the credential header.
    let bytes = std::fs::read(journal_path(&directory, session_id)).unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(!text.contains(SECRET), "secret leaked into journal bytes");
    assert!(
        !text.to_lowercase().contains("authorization: bearer"),
        "credential header leaked into journal bytes"
    );
    let store = FileEventStore::open(directory.path()).unwrap();
    let replay = store.replay(&session).await.unwrap();
    let projection = project_journal(&session, &replay.envelopes).unwrap();
    assert_eq!(
        projection.agentfield_runs[0].status,
        lato_core::AgentFieldRunStatus::Completed
    );
    let debug_surface = format!("{:?}", projection.agentfield_runs);
    assert!(!debug_surface.contains(SECRET));
}

struct SecretLeakingClient {
    start_calls: AtomicUsize,
    status_calls: AtomicUsize,
}

impl SecretLeakingClient {
    fn new() -> Self {
        Self {
            start_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AgentFieldClient for SecretLeakingClient {
    async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
        unreachable!()
    }

    async fn start_async(
        &self,
        _execute_target: &str,
        _input: &serde_json::Value,
    ) -> Result<AsyncStartEnvelope, AgentFieldError> {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        Ok(AsyncStartEnvelope::decode(&serde_json::json!({
            "execution_id": "exec-secret-1",
            "status": "queued",
            "target": "legal.review_contract",
            "type": "reasoner",
            "run_id": "run-remote-1",
            "workflow_id": "run-remote-1",
            "created_at": "2026-09-18T00:00:00Z",
            "enqueued_at": "2026-09-18T00:00:00Z",
            "webhook_registered": false,
        }))
        .unwrap())
    }

    async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        Ok(StatusEnvelope::decode(&serde_json::json!({
            "execution_id": execution_id,
            "status": "completed",
            "target": "legal.review_contract",
            "created_at": "2026-09-18T00:00:00Z",
            "run_id": "run-remote-1",
            "started_at": "2026-09-18T00:00:01Z",
            "webhook_registered": false,
            "completed_at": "2026-09-18T00:00:04Z",
            "duration_ms": 4000,
            "result": format!("review done with {SECRET}"),
            "error": format!("Authorization: Bearer {SECRET}"),
        }))
        .unwrap())
    }

    async fn cancel(
        &self,
        _execution_id: &str,
        _reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
        unreachable!()
    }
}

#[tokio::test]
async fn resumed_cancel_reuses_the_original_remote_execution_id() {
    let directory = TempDir::new().unwrap();
    let session_id = "resume-cancel";
    let session = SessionId::from(session_id);
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    // Seed: start a run, observe it running, then "exit" without closing.
    {
        let sink = FileJournalSink::resumed(&store, &session).await;
        let client = Arc::new(FakeClient::new());
        let manager = manager_with(session_id, Some(client), Arc::new(sink));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:rev-1",
                &serde_json::json!({"contract": "v1"}),
            )
            .await
            .unwrap();
        manager.run_status(Some(&run.run_id)).await.unwrap();
        // Hard exit: no close(), no implicit cancel — the manager is simply
        // dropped, exactly like a killed process.
    }
    // Resume in a fresh "process": restore, then explicitly cancel.
    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    let replay = store.replay(&session).await.unwrap();
    let projection = project_journal(&session, &replay.envelopes).unwrap();
    let client = Arc::new(FakeClient::new());
    let sink = FileJournalSink::resumed(&store, &session).await;
    let manager = manager_with(session_id, Some(client.clone()), Arc::new(sink));
    manager
        .restore_runs(projection.agentfield_runs.clone())
        .unwrap();
    let restored = &projection.agentfield_runs[0];
    let (run, outcome) = manager.cancel_run(&restored.run_id, "user").await.unwrap();
    assert_eq!(outcome, lato_agent::agentfield::CancelOutcome::Cancelled);
    assert_eq!(run.status, RunStatus::Cancelled);
    // The cancel went to the ORIGINAL remote execution id, exactly once.
    assert_eq!(client.cancel_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.cancel_targets.lock().unwrap().as_slice(),
        ["exec-original-1"]
    );
    // No second execution was ever created.
    assert_eq!(client.start_calls.load(Ordering::SeqCst), 0);
    // The durable journal ends with the cancelled terminal.
    let replay = store.replay(&session).await.unwrap();
    assert!(matches!(
        replay.envelopes.last().unwrap().record,
        JournalRecord::AgentField {
            event: AgentFieldJournalEvent::AgentFieldRunTerminal {
                status: lato_core::AgentFieldRunStatus::Cancelled,
                ..
            },
        }
    ));
}
