use async_trait::async_trait;
use lato_agent::{LegacyTurnDriver, ToolApproval};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{
    ApprovalRequest, Command, EventPayload, EventStore, HistoryProjectionMetadata,
    HistoryProjectionStore, HistoryReplacementReason, JournalDurability, JournalEnvelope,
    JournalError, JournalRecord, JournalReplay, ModelMessage, PolicyMode, ProjectionError,
    SandboxProfile, SessionId, SideEffect, StartBehavior, StartTurn, Tool, ToolCancellation,
    ToolCapability, ToolConcurrency, ToolContext, ToolDescriptor, ToolError, ToolIdempotency,
    ToolLayer, ToolName, ToolOutput, ToolSource, UserInput,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_runtime::{SessionBootstrap, spawn_session_with_store};
use lato_store::MemoryEventStore;
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use lato_workspace::{FileLocks, SessionTrust};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::timeout;

struct RecordingStore {
    inner: MemoryEventStore,
    block_kind: Option<&'static str>,
    fail_kind: Option<&'static str>,
    append_started: Notify,
    release: Notify,
    log: Arc<Mutex<Vec<String>>>,
}

impl RecordingStore {
    fn new(
        block_kind: Option<&'static str>,
        fail_kind: Option<&'static str>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            inner: MemoryEventStore::new(),
            block_kind,
            fail_kind,
            append_started: Notify::new(),
            release: Notify::new(),
            log,
        }
    }
}

#[async_trait]
impl EventStore for RecordingStore {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError> {
        let kind = record_kind(&envelope.record);
        if self.block_kind == Some(kind) {
            self.append_started.notify_one();
            self.release.notified().await;
        }
        if self.fail_kind == Some(kind) {
            return Err(JournalError::Io {
                message: format!("injected {kind} failure"),
            });
        }
        self.inner.append(envelope, durability).await?;
        self.log.lock().await.push(format!("append:{kind}"));
        Ok(())
    }

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError> {
        self.inner.replay(session_id).await
    }

    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError> {
        self.inner.import_if_absent(session_id, envelopes).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError> {
        self.inner.list_sessions().await
    }

    async fn shutdown(&self, session_id: &SessionId) -> Result<(), JournalError> {
        self.inner.shutdown(session_id).await
    }
}

#[async_trait]
impl HistoryProjectionStore for RecordingStore {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        self.inner
            .replace_history(session_id, messages, reason)
            .await
    }
}

struct CountingMutationTool {
    calls: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for CountingMutationTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("test:mutate").unwrap(),
            version: semver::Version::new(1, 0, 0),
            description: "count a non-idempotent mutation".into(),
            input_schema: serde_json::json!({"type":"object"}),
            capabilities: vec![ToolCapability::FileWrite],
            side_effect: SideEffect::WorkspaceMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::SessionOverride,
                id: "test.mutate".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().await.push("invoke".into());
        Ok(ToolOutput {
            content: "mutated".into(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

struct DenyApproval;

#[async_trait]
impl ToolApproval for DenyApproval {
    async fn approve(&self, _request: &ApprovalRequest) -> bool {
        false
    }
}

fn build_driver(
    cwd: &Path,
    calls: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<String>>>,
    mode: PolicyMode,
    approval: Option<Arc<dyn ToolApproval>>,
) -> Arc<LegacyTurnDriver> {
    let stream = Arc::new(FakeModelStream::new(vec![
        vec![StreamPiece::ToolCall {
            id: "mutation-1".into(),
            name: "mutate".into(),
            arguments: serde_json::json!({"value": 1}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: cwd.to_path_buf(),
            mode,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register(Arc::new(CountingMutationTool { calls, log }))
        .unwrap();
    let runtime = Arc::new(builder.build().unwrap());
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    Arc::new(LegacyTurnDriver::new_with_tool_runtime(
        "journal-session".into(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(cwd),
        cwd.to_path_buf(),
        updates,
        approval,
        runtime,
    ))
}

fn record_kind(record: &JournalRecord) -> &'static str {
    match record {
        JournalRecord::ToolCallRequested { .. } => "requested",
        JournalRecord::PolicyDecisionCommitted { .. } => "policy",
        JournalRecord::ToolCallPrepared { .. } => "prepared",
        JournalRecord::ToolCallCompleted { .. } => "completed",
        JournalRecord::ToolCallRejected { .. } => "rejected",
        JournalRecord::TurnFailed { .. } => "turn_failed",
        _ => "other",
    }
}

async fn start_and_wait_terminal(
    driver: Arc<LegacyTurnDriver>,
    store: Arc<RecordingStore>,
) -> EventPayload {
    let sid = SessionId::from("journal-session");
    let session = spawn_session_with_store(
        sid.clone(),
        driver,
        store,
        SessionBootstrap {
            replay: JournalReplay::empty(sid),
        },
    );
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("mutate once"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            let payload = events.recv().await.unwrap().payload;
            if matches!(
                payload,
                EventPayload::TurnCompleted(_)
                    | EventPayload::TurnFailed { .. }
                    | EventPayload::TurnCancelled { .. }
            ) {
                break payload;
            }
        }
    })
    .await
    .expect("turn terminal event timed out")
}

#[tokio::test]
async fn mutating_tool_is_surrounded_by_durable_journal_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingStore::new(None, None, log.clone()));
    let driver = build_driver(
        directory.path(),
        calls.clone(),
        log.clone(),
        PolicyMode::Always,
        None,
    );
    assert!(matches!(
        start_and_wait_terminal(driver, store).await,
        EventPayload::TurnCompleted(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let log = log.lock().await;
    let requested = log
        .iter()
        .position(|item| item == "append:requested")
        .unwrap();
    let policy = log.iter().position(|item| item == "append:policy").unwrap();
    let prepared = log
        .iter()
        .position(|item| item == "append:prepared")
        .unwrap();
    let invoked = log.iter().position(|item| item == "invoke").unwrap();
    let completed = log
        .iter()
        .position(|item| item == "append:completed")
        .unwrap();
    assert!(requested < policy && policy < prepared && prepared < invoked && invoked < completed);
}

#[tokio::test]
async fn prepared_barrier_blocks_tool_invocation_until_acknowledged() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingStore::new(Some("prepared"), None, log.clone()));
    let driver = build_driver(
        directory.path(),
        calls.clone(),
        log,
        PolicyMode::Always,
        None,
    );
    let running = tokio::spawn(start_and_wait_terminal(driver, store.clone()));
    store.append_started.notified().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    store.release.notify_one();
    assert!(matches!(
        running.await.unwrap(),
        EventPayload::TurnCompleted(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_prepared_barrier_never_invokes_tool() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingStore::new(None, Some("prepared"), log.clone()));
    let driver = build_driver(
        directory.path(),
        calls.clone(),
        log,
        PolicyMode::Always,
        None,
    );
    assert!(matches!(
        start_and_wait_terminal(driver, store).await,
        EventPayload::TurnFailed { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_completion_leaves_one_unknown_outcome_without_reinvocation() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingStore::new(None, Some("completed"), log.clone()));
    let driver = build_driver(
        directory.path(),
        calls.clone(),
        log,
        PolicyMode::Always,
        None,
    );
    assert!(matches!(
        start_and_wait_terminal(driver, store.clone()).await,
        EventPayload::TurnFailed { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let replay = store
        .replay(&SessionId::from("journal-session"))
        .await
        .unwrap();
    assert_eq!(replay.projection.unresolved_tools.len(), 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn denied_approval_records_rejection_without_preparation_or_invocation() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingStore::new(None, None, log.clone()));
    let driver = build_driver(
        directory.path(),
        calls.clone(),
        log.clone(),
        PolicyMode::Ask,
        Some(Arc::new(DenyApproval)),
    );
    assert!(matches!(
        start_and_wait_terminal(driver, store).await,
        EventPayload::TurnCompleted(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let log = log.lock().await;
    assert!(log.iter().any(|item| item == "append:rejected"));
    assert!(!log.iter().any(|item| item == "append:prepared"));
}
