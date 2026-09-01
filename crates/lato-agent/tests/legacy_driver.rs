use async_trait::async_trait;
use lato_agent::{HistoryItem, LegacyTurnDriver, default_fake_stream};
use lato_ai::{FakeModelStream, ModelStream, StreamPiece};
use lato_core::{
    Command, EventPayload, PolicyMode, SandboxProfile, SessionId, StartBehavior, StartTurn,
    ToolCallId, TurnId, UserInput,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_runtime::spawn_session;
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use lato_workspace::{FileLocks, SessionTrust};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::timeout;

fn driver_with_stream(
    stream: Arc<dyn ModelStream>,
) -> (
    Arc<LegacyTurnDriver>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let driver = LegacyTurnDriver::new(
        "legacy-session".into(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates_tx,
        None,
    );
    (Arc::new(driver), updates_rx)
}

fn driver() -> (
    Arc<LegacyTurnDriver>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    driver_with_stream(default_fake_stream())
}

struct RecordingTool {
    tx: mpsc::UnboundedSender<(SessionId, TurnId, ToolCallId, bool)>,
}

#[async_trait]
impl lato_core::Tool for RecordingTool {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        lato_core::ToolDescriptor {
            name: lato_core::ToolName::parse("session:record").unwrap(),
            version: semver::Version::new(1, 0, 0),
            description: "record typed tool context".into(),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![lato_core::ToolCapability::ExtensionInvoke],
            side_effect: lato_core::SideEffect::None,
            concurrency: lato_core::ToolConcurrency::Serial,
            idempotency: lato_core::ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: lato_core::ToolCancellation::Cooperative,
            source: lato_core::ToolSource {
                layer: lato_core::ToolLayer::SessionOverride,
                id: "test.recording".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: lato_core::ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, lato_core::ToolError> {
        self.tx
            .send((
                context.session_id,
                context.turn_id,
                context.call_id,
                context.cancellation.is_cancelled(),
            ))
            .unwrap();
        Ok(lato_core::ToolOutput {
            content: "recorded".into(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn driver_with_recording_tool(
    stream: Arc<dyn ModelStream>,
    recording_tx: mpsc::UnboundedSender<(SessionId, TurnId, ToolCallId, bool)>,
) -> Arc<LegacyTurnDriver> {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, _updates_rx) = mpsc::unbounded_channel();
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: cwd.clone(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register(Arc::new(RecordingTool { tx: recording_tx }))
        .unwrap();
    let runtime = Arc::new(builder.build().unwrap());
    Arc::new(LegacyTurnDriver::new_with_tool_runtime(
        "legacy-session".into(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates_tx,
        None,
        runtime,
    ))
}

async fn recv_until(
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
    predicate: impl Fn(&EventPayload) -> bool,
) -> lato_core::EventEnvelope {
    timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if predicate(&event.payload) {
                break event;
            }
        }
    })
    .await
    .expect("timed out waiting for runtime event")
}

#[tokio::test]
async fn tool_context_uses_runtime_session_turn_and_model_call_ids() {
    let stream = Arc::new(FakeModelStream::new(vec![
        vec![StreamPiece::ToolCall {
            id: "record-call-1".into(),
            name: "record".into(),
            arguments: serde_json::json!({}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (recording_tx, mut recordings) = mpsc::unbounded_channel();
    let driver = driver_with_recording_tool(stream, recording_tx);
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("record context"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnStarted)
    })
    .await;
    let started_turn_id = started
        .turn_id
        .expect("turn-started event must carry a turn id");
    let (session_id, turn_id, call_id, cancelled) =
        timeout(Duration::from_secs(2), recordings.recv())
            .await
            .expect("recording tool timed out")
            .expect("recording channel closed");

    assert_eq!(session_id, SessionId::from("legacy-session"));
    assert_eq!(turn_id, started_turn_id);
    assert_eq!(call_id, ToolCallId::from("record-call-1"));
    assert!(!cancelled);
    let completed = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnCompleted(_))
    })
    .await;
    assert!(matches!(completed.payload, EventPayload::TurnCompleted(_)));
}

struct GatedToolCallStream {
    release: Notify,
}

#[async_trait]
impl ModelStream for GatedToolCallStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        self.release.notified().await;
        tx.send(StreamPiece::ToolCall {
            id: "cancelled-record-call".into(),
            name: "record".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .map_err(|_| "stream receiver closed".to_string())
    }
}

#[tokio::test]
async fn cancellation_reaches_the_tool_membrane_before_dispatch() {
    let stream = Arc::new(GatedToolCallStream {
        release: Notify::new(),
    });
    let (recording_tx, mut recordings) = mpsc::unbounded_channel();
    let driver = driver_with_recording_tool(stream.clone(), recording_tx);
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("record after cancellation"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnStarted)
    })
    .await;
    session
        .submit(Command::CancelTurn {
            turn_id: started.turn_id.unwrap(),
        })
        .await
        .unwrap();

    let terminal = recv_until(&mut events, |payload| {
        matches!(
            payload,
            EventPayload::TurnCancelled { .. }
                | EventPayload::TurnCompleted(_)
                | EventPayload::TurnFailed { .. }
        )
    })
    .await;
    assert!(matches!(
        terminal.payload,
        EventPayload::TurnCancelled { .. }
    ));
    stream.release.notify_waiters();
    match timeout(Duration::from_millis(100), recordings.recv()).await {
        Err(_) | Ok(None) => {}
        Ok(Some((_, _, _, cancelled))) => assert!(cancelled),
    }
}

#[tokio::test]
async fn legacy_driver_emits_typed_text_and_returns_final_text() {
    let (driver, mut updates) = driver();
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hi"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut saw_delta = false;
    let final_text = timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            match event.payload {
                EventPayload::ModelDelta { text } => {
                    assert_eq!(text, "hi");
                    saw_delta = true;
                }
                EventPayload::TurnCompleted(output) => break output.final_text,
                EventPayload::TurnFailed { error } => panic!("turn failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("turn timed out");

    assert!(saw_delta);
    assert_eq!(final_text, "hi");
    assert!(matches!(
        updates.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn legacy_driver_history_can_be_hydrated_for_resume() {
    let (driver, _updates) = driver();
    driver
        .replace_history(vec![
            HistoryItem::User("old".into()),
            HistoryItem::AssistantText("answer".into()),
        ])
        .await;
    let history = driver.history_snapshot().await;
    assert_eq!(history.len(), 2);
    assert!(matches!(&history[0], HistoryItem::User(text) if text == "old"));
    assert!(matches!(&history[1], HistoryItem::AssistantText(text) if text == "answer"));
}

#[tokio::test]
async fn non_text_actor_updates_are_passed_through() {
    let stream = Arc::new(FakeModelStream::new(vec![
        vec![StreamPiece::ToolCall {
            id: "read-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "Cargo.toml", "limit": 1}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (driver, mut updates) = driver_with_stream(stream);
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("inspect"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let update = timeout(Duration::from_secs(2), updates.recv())
        .await
        .expect("tool update timed out")
        .expect("tool update channel closed");
    assert_eq!(update["method"], "session/tool_call");
    assert_eq!(update["params"]["name"], "read_file");

    let completed = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnCompleted(_))
    })
    .await;
    assert!(matches!(completed.payload, EventPayload::TurnCompleted(_)));
}

struct InterruptibleStream {
    calls: AtomicUsize,
    first_started: Notify,
    first_closed: Notify,
    contexts: Mutex<Vec<serde_json::Value>>,
}

struct SteeringRecordingStream {
    calls: AtomicUsize,
    first_started: Notify,
    first_closed: Notify,
}

impl SteeringRecordingStream {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            first_closed: Notify::new(),
        }
    }
}

#[async_trait]
impl ModelStream for SteeringRecordingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.first_started.notify_one();
                tx.closed().await;
                self.first_closed.notify_one();
            }
            1 => {
                tx.send(StreamPiece::ToolCall {
                    id: "steered-record-call".into(),
                    name: "record".into(),
                    arguments: serde_json::json!({}),
                })
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
            }
            _ => {
                tx.send(StreamPiece::Text("steered done".into()))
                    .await
                    .map_err(|_| "stream receiver closed".to_string())?;
            }
        }
        Ok(())
    }
}

impl InterruptibleStream {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            first_closed: Notify::new(),
            contexts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ModelStream for InterruptibleStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        self.contexts.lock().await.push(context);
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            tx.closed().await;
            self.first_closed.notify_one();
        } else {
            tx.send(StreamPiece::Text("new answer".into()))
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_token_interrupts_a_blocked_legacy_prompt() {
    let stream = Arc::new(InterruptibleStream::new());
    let (driver, _updates) = driver_with_stream(stream.clone());
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("old prompt"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnStarted)
    })
    .await;
    stream.first_started.notified().await;
    session
        .submit(Command::CancelTurn {
            turn_id: started.turn_id.unwrap(),
        })
        .await
        .unwrap();

    recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnCancelled { .. })
    })
    .await;
    timeout(Duration::from_secs(2), stream.first_closed.notified())
        .await
        .expect("cancelled stream task retained its receiver");
}

#[tokio::test]
async fn steering_keeps_the_runtime_owned_cancellation_token_live() {
    let stream = Arc::new(SteeringRecordingStream::new());
    let (recording_tx, mut recordings) = mpsc::unbounded_channel();
    let driver = driver_with_recording_tool(stream.clone(), recording_tx);
    let session = spawn_session(SessionId::from("legacy-session"), driver);
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("old prompt"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnStarted)
    })
    .await;
    let started_turn_id = started.turn_id.unwrap();
    stream.first_started.notified().await;
    session
        .submit(Command::SteerTurn(UserInput::text("record after steering")))
        .await
        .unwrap();

    let (session_id, turn_id, call_id, cancelled) =
        timeout(Duration::from_secs(2), recordings.recv())
            .await
            .expect("steered recording tool timed out")
            .expect("recording channel closed");
    assert_eq!(session_id, SessionId::from("legacy-session"));
    assert_eq!(turn_id, started_turn_id);
    assert_eq!(call_id, ToolCallId::from("steered-record-call"));
    assert!(
        !cancelled,
        "actor-local steering must not cancel the runtime token"
    );

    let completed = recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnCompleted(_))
    })
    .await;
    assert!(matches!(completed.payload, EventPayload::TurnCompleted(_)));
    timeout(Duration::from_secs(2), stream.first_closed.notified())
        .await
        .expect("steered stream task retained its receiver");
}

#[tokio::test]
async fn steering_restarts_without_deadlock_or_stale_output() {
    let stream = Arc::new(InterruptibleStream::new());
    let (driver, _updates) = driver_with_stream(stream.clone());
    let session = spawn_session(SessionId::from("legacy-session"), driver.clone());
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("old prompt"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    recv_until(&mut events, |payload| {
        matches!(payload, EventPayload::TurnStarted)
    })
    .await;
    stream.first_started.notified().await;
    session
        .submit(Command::SteerTurn(UserInput::text("new prompt")))
        .await
        .unwrap();

    let mut deltas = String::new();
    let output = timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await.unwrap().payload {
                EventPayload::ModelDelta { text } => deltas.push_str(&text),
                EventPayload::TurnCompleted(output) => break output,
                EventPayload::TurnFailed { error } => panic!("turn failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("steered turn deadlocked");

    assert_eq!(deltas, "new answer");
    assert_eq!(output.final_text, "new answer");
    timeout(Duration::from_secs(2), stream.first_closed.notified())
        .await
        .expect("steered stream task retained its receiver");

    let contexts = stream.contexts.lock().await;
    let latest_messages = contexts.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(latest_messages.last().unwrap()["content"], "new prompt");
    drop(contexts);

    let history = driver.history_snapshot().await;
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, HistoryItem::User(text) if text == "new prompt"))
            .count(),
        1
    );
    assert!(!output.final_text.contains("old"));
}
