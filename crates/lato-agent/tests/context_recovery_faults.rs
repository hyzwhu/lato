use async_trait::async_trait;
use lato_agent::{
    AcpHost, HistoryItem, LegacyTurnDriver, PromptKind, REQUIRED_SECTIONS, SessionActor,
    TurnOutcome, history_to_model_messages,
};
use lato_ai::{ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventPayload, EventStore,
    HistoryProjectionMetadata, HistoryProjectionStore, HistoryReplacementReason, JournalDurability,
    JournalEnvelope, JournalError, JournalReplay, ModelError, ModelErrorKind, ModelMessage,
    PolicyMode, ProjectionError, Retryability, SandboxProfile, SessionId, SideEffect,
    StartBehavior, StartTurn, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolOutput, ToolSource, TurnId,
    TurnOutput, UserInput,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_protocol::JsonRpcReq;
use lato_runtime::{
    CompactionControl, CompactionRequest, SessionBootstrap, TurnControl, TurnDriver,
    TurnEventEmitter, TurnRequest, spawn_session, spawn_session_with_store,
};
use lato_store::MemoryEventStore;
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use lato_workspace::{FileLocks, SessionTrust};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct FaultStep {
    pieces: Vec<StreamPiece>,
    terminal: Result<(), ModelError>,
}

struct FaultStream {
    steps: tokio::sync::Mutex<VecDeque<FaultStep>>,
    contexts: Mutex<Vec<serde_json::Value>>,
    calls: AtomicUsize,
}

impl FaultStream {
    fn new(steps: Vec<FaultStep>) -> Self {
        Self {
            steps: tokio::sync::Mutex::new(steps.into()),
            contexts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn contexts(&self) -> Vec<serde_json::Value> {
        self.contexts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelStream for FaultStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context);
        let Some(step) = self.steps.lock().await.pop_front() else {
            return Err(ModelError::new(
                "test_fixture.exhausted",
                "model request consumed more fault steps than provided",
                Retryability::Never,
            ));
        };
        for piece in step.pieces {
            tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        }
        step.terminal
    }
}

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn success(pieces: Vec<StreamPiece>) -> FaultStep {
    FaultStep {
        pieces,
        terminal: Ok(()),
    }
}

fn failure(pieces: Vec<StreamPiece>, error: ModelError) -> FaultStep {
    FaultStep {
        pieces,
        terminal: Err(error),
    }
}

fn overflow(message: &str) -> ModelError {
    ModelError::new("model.context_overflow", message, Retryability::Never)
        .with_kind(ModelErrorKind::ContextOverflow)
        .with_context_window(100_000)
}

fn summary() -> String {
    let detail = "preserve verified implementation state, decisions, and pending work ".repeat(2);
    REQUIRED_SECTIONS
        .iter()
        .enumerate()
        .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn make_host(
    workspace: &tempfile::TempDir,
    home: &tempfile::TempDir,
    stream: Arc<FaultStream>,
) -> (AcpHost, mpsc::UnboundedReceiver<serde_json::Value>) {
    let raw: Arc<dyn ModelStream> = stream;
    let endpoint = adapt_model_endpoint(
        "fixture",
        "context-recovery-faults",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    (
        AcpHost::new_with_home(
            workspace.path().to_path_buf(),
            SessionTrust::for_headless_prompt(workspace.path()),
            updates_tx,
            endpoint.stream,
            home.path().to_path_buf(),
        ),
        updates_rx,
    )
}

async fn new_session(host: &mut AcpHost) -> String {
    host.handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn seed(host: &mut AcpHost, sid: &str) {
    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"seed"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete", "{response}");
}

fn deltas(updates: &mut mpsc::UnboundedReceiver<serde_json::Value>) -> Vec<String> {
    std::iter::from_fn(|| updates.try_recv().ok())
        .filter(|update| update["method"] == "session/update")
        .filter_map(|update| update["params"]["delta"].as_str().map(str::to_owned))
        .collect()
}

fn terminal_message(response: &serde_json::Value) -> &str {
    response["error"]["message"].as_str().unwrap()
}

#[tokio::test]
async fn first_pre_output_overflow_compacts_and_resubmits_exactly_once() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("first overflow")),
        success(vec![StreamPiece::Text(summary())]),
        success(vec![StreamPiece::Text("recovered".into())]),
    ]));
    let (mut host, mut updates) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "recovered");
    assert_eq!(stream.calls(), 4);
    let contexts = stream.contexts();
    assert_eq!(contexts.len(), 4, "seed, overflow, compact, resubmit");
    assert!(!contexts[1]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[2]["tools"].as_array().unwrap().is_empty());
    assert!(!contexts[3]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[3].to_string().contains("conversation_summary"));
    let visible = deltas(&mut updates).join("");
    assert!(visible.ends_with("recovered"));
    assert_eq!(visible.matches("recovered").count(), 1);
    assert!(!visible.contains("first overflow"));
}

#[tokio::test]
async fn second_overflow_is_terminal_without_another_recovery_or_submission() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("first overflow")),
        success(vec![StreamPiece::Text(summary())]),
        failure(Vec::new(), overflow("second overflow")),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: second overflow"
    );
    assert_eq!(
        stream.calls(),
        4,
        "must not consume a nonexistent fifth step"
    );
}

#[tokio::test]
async fn text_before_overflow_is_visible_once_and_never_replayed() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![failure(
        vec![StreamPiece::Text("partial".into())],
        overflow("overflow after text"),
    )]));
    let (mut host, mut updates) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;

    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"produce text"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: overflow after text"
    );
    assert_eq!(stream.calls(), 1);
    assert_eq!(deltas(&mut updates), vec!["partial"]);
}

#[tokio::test]
async fn tool_call_before_overflow_is_never_replayed() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![failure(
        vec![StreamPiece::ToolCall {
            id: "fault-call-1".into(),
            name: "definitely_missing_test_tool".into(),
            arguments: serde_json::json!({}),
        }],
        overflow("overflow after tool call"),
    )]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;

    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"call a tool"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: overflow after tool call"
    );
    assert_eq!(stream.calls(), 1);
}

#[tokio::test]
async fn unchanged_recovery_keeps_the_provider_overflow_as_primary_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("primary provider failure")),
        success(vec![StreamPiece::Text("invalid summary one".into())]),
        success(vec![StreamPiece::Text("invalid summary two".into())]),
        success(vec![StreamPiece::Text("invalid summary three".into())]),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: primary provider failure"
    );
    assert_eq!(stream.calls(), 5);
}

#[tokio::test]
async fn fatal_recovery_appends_context_without_replacing_the_provider_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("primary provider failure")),
        failure(
            Vec::new(),
            ModelError::new(
                "model.auth",
                "compaction credentials expired",
                Retryability::Never,
            )
            .with_kind(ModelErrorKind::Authentication),
        ),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    let message = terminal_message(&response);
    assert!(
        message.starts_with("legacy.turn_failed: model.context_overflow: primary provider failure"),
        "{message}"
    );
    assert!(
        message.contains(
            "context recovery failed: compaction.model_failed: compaction model failed: \
             model.auth: compaction credentials expired"
        ),
        "{message}"
    );
    assert_eq!(stream.calls(), 3);
}

struct OversizedOutputTool;

#[async_trait]
impl lato_core::Tool for OversizedOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: lato_core::ToolName::parse("test:oversized_output").unwrap(),
            version: semver::Version::new(1, 0, 0),
            description: "return deterministic output large enough to cross preflight".into(),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::None,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 40_000,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::SessionOverride,
                id: "test.oversized-output".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: "oversized-tool-output".repeat(1_500),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

async fn preflight_driver(
    workspace: &tempfile::TempDir,
    stream: Arc<FaultStream>,
) -> (
    Arc<LegacyTurnDriver>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let raw: Arc<dyn ModelStream> = stream;
    let endpoint = adapt_model_endpoint(
        "fixture",
        "preflight-faults",
        ModelMetadata {
            context_window: Some(10_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut tools = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: workspace.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    tools.register(Arc::new(OversizedOutputTool)).unwrap();
    let (updates, updates_rx) = mpsc::unbounded_channel();
    let driver = Arc::new(LegacyTurnDriver::new_with_tool_runtime(
        "preflight-faults".into(),
        endpoint.stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(workspace.path()),
        workspace.path().to_path_buf(),
        updates,
        None,
        Arc::new(tools.build().unwrap()),
    ));
    driver
        .replace_history(vec![
            HistoryItem::System("deterministic test system".into()),
            HistoryItem::AssistantText("prior-context".repeat(1_800)),
        ])
        .await;
    (driver, updates_rx)
}

async fn run_preflight_turn(driver: Arc<LegacyTurnDriver>) -> Vec<lato_core::EventEnvelope> {
    let session = spawn_session(SessionId::from("preflight-faults"), driver);
    let mut events = session.subscribe();
    timeout(
        Duration::from_secs(2),
        session.submit(Command::StartTurn(StartTurn {
            input: UserInput::text("produce a deterministic oversized tool result"),
            behavior: StartBehavior::Reject,
        })),
    )
    .await
    .expect("runtime did not accept the preflight turn")
    .unwrap();

    timeout(Duration::from_secs(2), async {
        let mut observed = Vec::new();
        loop {
            let event = events.recv().await.unwrap();
            let terminal = matches!(
                event.payload,
                EventPayload::TurnCompleted(_)
                    | EventPayload::TurnFailed { .. }
                    | EventPayload::TurnCancelled { .. }
            );
            observed.push(event);
            if terminal {
                break observed;
            }
        }
    })
    .await
    .expect("preflight turn did not terminate")
}

fn oversized_tool_call_step() -> FaultStep {
    success(vec![StreamPiece::ToolCall {
        id: "oversized-output-call".into(),
        name: "oversized_output".into(),
        arguments: serde_json::json!({}),
    }])
}

#[tokio::test]
async fn preflight_overflow_compacts_before_rebuilt_ordinary_request() {
    let workspace = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        oversized_tool_call_step(),
        success(vec![StreamPiece::Text(summary())]),
        success(vec![StreamPiece::Text("recovered after preflight".into())]),
    ]));
    let (driver, _updates) = preflight_driver(&workspace, stream.clone()).await;
    let events = run_preflight_turn(driver).await;

    assert!(
        matches!(
            events.last().unwrap().payload,
            EventPayload::TurnCompleted(_)
        ),
        "unexpected terminal event: {:?}",
        events.last().unwrap()
    );
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::CompactionStarted {
            trigger: lato_core::CompactionTrigger::PreflightOverflow,
            ..
        }
    )));
    let contexts = stream.contexts();
    assert_eq!(contexts.len(), 3, "ordinary, compaction, rebuilt ordinary");
    assert!(!contexts[0]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[1]["tools"].as_array().unwrap().is_empty());
    assert!(!contexts[2]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[2].to_string().contains("conversation_summary"));
}

#[tokio::test]
async fn preflight_recovery_failure_never_submits_known_oversized_ordinary_request() {
    let workspace = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        oversized_tool_call_step(),
        success(vec![StreamPiece::Text("invalid summary one".into())]),
        success(vec![StreamPiece::Text("invalid summary two".into())]),
    ]));
    let (driver, mut updates) = preflight_driver(&workspace, stream.clone()).await;
    let events = run_preflight_turn(driver).await;

    let EventPayload::TurnFailed { error } = &events.last().unwrap().payload else {
        panic!(
            "preflight failure did not terminate the turn: {:?}",
            events.last()
        );
    };
    assert!(
        error.message.contains("context.preflight_recovery_failed"),
        "{error}"
    );
    let contexts = stream.contexts();
    assert_eq!(contexts.len(), 3, "events: {events:#?}");
    assert!(!contexts[0]["tools"].as_array().unwrap().is_empty());
    assert!(
        contexts[1..]
            .iter()
            .all(|context| context["tools"].as_array().unwrap().is_empty()),
        "no oversized ordinary request may follow the tool result"
    );
    let recovery_updates = std::iter::from_fn(|| updates.try_recv().ok())
        .filter(|update| update["method"] == "lato/session/recovery")
        .collect::<Vec<_>>();
    assert_eq!(recovery_updates.len(), 1);
    assert_eq!(
        recovery_updates[0]["params"]["automaticCompactionSuppression"],
        "sticky"
    );
}

#[tokio::test]
async fn suppression_policy_manual_bypass_has_a_real_acp_compaction_entry_point() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text(
            "manual compaction source ".repeat(2_000),
        )]),
        success(vec![StreamPiece::Text(summary())]),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = timeout(
        Duration::from_secs(2),
        host.handle(req(
            3,
            "lato/session/compact",
            serde_json::json!({"sessionId": sid}),
        )),
    )
    .await
    .expect("manual ACP compaction timed out")
    .unwrap();

    assert_eq!(response["result"]["status"], "complete", "{response}");
    let contexts = stream.contexts();
    assert_eq!(contexts.len(), 2, "ordinary seed and manual compaction");
    assert!(contexts[1]["tools"].as_array().unwrap().is_empty());
}

struct CheckpointCancellingStore {
    inner: MemoryEventStore,
    turn_cancellation: Arc<AsyncMutex<Option<CancellationToken>>>,
    checkpoint_committed: Notify,
}

#[async_trait]
impl EventStore for CheckpointCancellingStore {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError> {
        self.inner.append(envelope, durability).await
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
impl HistoryProjectionStore for CheckpointCancellingStore {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        let metadata = self
            .inner
            .replace_history(session_id, messages, reason)
            .await?;
        self.turn_cancellation
            .lock()
            .await
            .as_ref()
            .expect("active turn cancellation was not captured")
            .cancel();
        self.checkpoint_committed.notify_one();
        Ok(metadata)
    }
}

struct RuntimeActorDriver {
    actor: AsyncMutex<SessionActor>,
    compactor: Arc<LegacyTurnDriver>,
    turn_cancellation: Arc<AsyncMutex<Option<CancellationToken>>>,
    actor_observed_cancellation: Notify,
    release_finished: Notify,
}

#[async_trait]
impl TurnDriver for RuntimeActorDriver {
    async fn run(
        &self,
        request: TurnRequest,
        control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, AgentError> {
        *self.turn_cancellation.lock().await = Some(control.cancellation.clone());
        let mut actor = self.actor.lock().await;
        actor.set_journal_events(Some(events));
        let result = actor
            .prompt_with_context(
                PromptKind::Start,
                request.input.text,
                request.turn_id,
                control.cancellation,
            )
            .await
            .map_err(|message| {
                AgentError::new(
                    "test.turn_failed",
                    ErrorCategory::Task,
                    message,
                    Retryability::Never,
                )
            })?;
        if result == TurnOutcome::Cancelled {
            self.actor_observed_cancellation.notify_one();
            self.release_finished.notified().await;
        }
        Ok(TurnOutput {
            final_text: actor.latest_assistant_text(),
        })
    }

    async fn history_snapshot(&self) -> Result<Vec<ModelMessage>, AgentError> {
        history_to_model_messages(self.actor.lock().await.history()).map_err(|error| {
            AgentError::new(
                "test.history_projection_failed",
                ErrorCategory::Task,
                error.to_string(),
                Retryability::Never,
            )
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
        control: CompactionControl,
    ) -> Result<lato_core::CompactionCandidate, AgentError> {
        TurnDriver::compact(self.compactor.as_ref(), request, control).await
    }
}

#[tokio::test]
async fn cancellation_at_sampling_boundary_never_submits() {
    let workspace = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![success(vec![StreamPiece::Text(
        "must not be sampled".into(),
    )])]));
    let raw: Arc<dyn ModelStream> = stream.clone();
    let endpoint = adapt_model_endpoint(
        "fixture",
        "context-recovery-faults",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let mut actor = SessionActor::new(
        endpoint.stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(workspace.path()),
        workspace.path().to_path_buf(),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let outcome = timeout(
        Duration::from_secs(1),
        actor.prompt_with_context(
            PromptKind::Start,
            "continue after committed checkpoint".into(),
            TurnId::from("cancelled-sampling-boundary"),
            cancellation,
        ),
    )
    .await
    .expect("cancelled sampling boundary did not terminate")
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert_eq!(
        stream.calls(),
        0,
        "cancelled turns must not reach the model"
    );
}

#[tokio::test]
async fn cancellation_after_compaction_never_resubmits() {
    let workspace = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("overflow before cancellation")),
        success(vec![StreamPiece::Text(summary())]),
        success(vec![StreamPiece::Text("must not be sampled".into())]),
    ]));
    let raw: Arc<dyn ModelStream> = stream.clone();
    let endpoint = adapt_model_endpoint(
        "fixture",
        "context-recovery-faults",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let sid = SessionId::from("cancellation-after-compaction");
    let actor = SessionActor::new(
        endpoint.stream.clone(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(workspace.path()),
        workspace.path().to_path_buf(),
    );
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let compactor = Arc::new(LegacyTurnDriver::new_with_endpoint(
        sid.to_string(),
        endpoint,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(workspace.path()),
        workspace.path().to_path_buf(),
        updates,
        None,
    ));
    let turn_cancellation = Arc::new(AsyncMutex::new(None));
    let driver = Arc::new(RuntimeActorDriver {
        actor: AsyncMutex::new(actor),
        compactor,
        turn_cancellation: turn_cancellation.clone(),
        actor_observed_cancellation: Notify::new(),
        release_finished: Notify::new(),
    });
    let store = Arc::new(CheckpointCancellingStore {
        inner: MemoryEventStore::new(),
        turn_cancellation,
        checkpoint_committed: Notify::new(),
    });
    let session = spawn_session_with_store(
        sid.clone(),
        driver.clone(),
        store.clone(),
        SessionBootstrap {
            replay: JournalReplay::empty(sid.clone()),
        },
    );
    let mut events = session.subscribe();

    timeout(
        Duration::from_secs(1),
        session.submit(Command::StartTurn(StartTurn {
            input: UserInput::text("seed"),
            behavior: StartBehavior::Reject,
        })),
    )
    .await
    .expect("runtime did not accept seed turn")
    .unwrap();
    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.unwrap().payload,
                EventPayload::TurnCompleted(_)
            ) {
                break;
            }
        }
    })
    .await
    .expect("seed turn did not complete");

    timeout(
        Duration::from_secs(1),
        session.submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        })),
    )
    .await
    .expect("runtime did not accept recovery turn")
    .unwrap();
    let turn_id = timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.payload, EventPayload::TurnStarted) {
                break event.turn_id.expect("turn start must carry an id");
            }
        }
    })
    .await
    .expect("recovery turn did not start");

    timeout(
        Duration::from_secs(1),
        store.checkpoint_committed.notified(),
    )
    .await
    .expect("automatic compaction checkpoint was not committed");
    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.unwrap().payload,
                EventPayload::CompactionCompleted { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("compaction completion was not published");
    timeout(
        Duration::from_secs(1),
        driver.actor_observed_cancellation.notified(),
    )
    .await
    .expect("actor did not observe cancellation at the recovery boundary");

    timeout(
        Duration::from_secs(1),
        session.submit(Command::CancelTurn {
            turn_id: turn_id.clone(),
        }),
    )
    .await
    .expect("runtime did not accept turn cancellation")
    .unwrap();
    driver.release_finished.notify_one();

    let terminal = timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.payload,
                EventPayload::TurnCancelled { .. }
                    | EventPayload::TurnCompleted(_)
                    | EventPayload::TurnFailed { .. }
            ) {
                break event;
            }
        }
    })
    .await
    .expect("recovery turn did not terminate");

    assert!(matches!(
        terminal.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::User
        }
    ));
    assert_eq!(
        stream.calls(),
        3,
        "seed, rejected request, and compaction only; no recovery resubmit"
    );
    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("committed checkpoint replay timed out")
        .unwrap();
    assert!(replay.projection.active_checkpoint_id.is_some());
    assert!(
        serde_json::to_string(&replay.projection.messages)
            .unwrap()
            .contains("conversation_summary"),
        "the committed compacted history must remain authoritative"
    );
}
