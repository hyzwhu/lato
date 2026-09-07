use async_trait::async_trait;
use lato_core::{
    CancelReason, Command, CompactSession, CompactionCandidate, CompactionError, CompactionId,
    CompactionTrigger, ContextUsage, EventPayload, EventStore, HistoryProjectionMetadata,
    HistoryProjectionStore, HistoryReplacementReason, JOURNAL_SCHEMA_VERSION, JournalDurability,
    JournalEnvelope, JournalError, JournalRecord, JournalRecordId, JournalReplay, ModelContent,
    ModelMessage, ModelRole, PluginSnapshotSummary, ProjectionError, SessionId, StartBehavior,
    StartTurn, TurnOutput, UserInput,
};
use lato_runtime::{
    AutomaticCompactionOutcome, AutomaticCompactionRequest, CompactionControl, CompactionRequest,
    PrefireCompactionRequest, PrefireCompactionResult, SessionBootstrap, TurnControl, TurnDriver,
    TurnEventEmitter, TurnRequest, spawn_session, spawn_session_with_store,
};
use lato_store::{FaultPoint, FileEventStore, FileFaultInjector, MemoryEventStore};
use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, oneshot};
use tokio::time::{Duration, timeout};

const PREFIRE_NOTE1_SENTINEL: &str = "NOTE1-SPECULATIVE-SENTINEL";

struct EchoDriver;

struct ControlledStore {
    inner: MemoryEventStore,
    block_kind: Option<&'static str>,
    fail_kind: Option<&'static str>,
    append_started: Notify,
    release: Notify,
    records: tokio::sync::Mutex<Vec<JournalEnvelope>>,
}

struct RuntimeFault {
    point: FaultPoint,
    remaining: AtomicUsize,
}

impl RuntimeFault {
    fn once(point: FaultPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            remaining: AtomicUsize::new(1),
        })
    }
}

impl FileFaultInjector for RuntimeFault {
    fn check(&self, point: FaultPoint) -> Result<(), JournalError> {
        if point == self.point
            && self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
        {
            return Err(JournalError::Io {
                message: format!("injected {point:?}"),
            });
        }
        Ok(())
    }
}

struct ReconciliationFailStore {
    inner: MemoryEventStore,
    fail_replay: AtomicBool,
}

#[async_trait]
impl EventStore for ReconciliationFailStore {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError> {
        self.inner.append(envelope, durability).await
    }

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError> {
        if self.fail_replay.load(Ordering::SeqCst) {
            return Err(JournalError::Io {
                message: "injected reconciliation replay failure".into(),
            });
        }
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
impl HistoryProjectionStore for ReconciliationFailStore {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        self.inner
            .replace_history(session_id, messages, reason)
            .await?;
        self.fail_replay.store(true, Ordering::SeqCst);
        Err(ProjectionError::WriteFailed {
            message: "injected post-marker publication failure".into(),
        })
    }
}

impl ControlledStore {
    fn new(block_kind: Option<&'static str>, fail_kind: Option<&'static str>) -> Self {
        Self {
            inner: MemoryEventStore::new(),
            block_kind,
            fail_kind,
            append_started: Notify::new(),
            release: Notify::new(),
            records: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl EventStore for ControlledStore {
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
                message: "injected append failure".into(),
            });
        }
        self.inner.append(envelope.clone(), durability).await?;
        self.records.lock().await.push(envelope);
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
impl HistoryProjectionStore for ControlledStore {
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

fn record_kind(record: &JournalRecord) -> &'static str {
    match record {
        JournalRecord::SessionStarted => "session_started",
        JournalRecord::TurnInputAccepted { .. } => "turn_input_accepted",
        JournalRecord::ConversationItemCommitted { .. } => "conversation_item_committed",
        JournalRecord::TurnCompleted { .. } => "turn_completed",
        _ => "other",
    }
}

fn bootstrap(session_id: &SessionId) -> SessionBootstrap {
    SessionBootstrap {
        replay: JournalReplay::empty(session_id.clone()),
    }
}

#[async_trait]
impl TurnDriver for EchoDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        events.model_delta(request.input.text.clone())?;
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }
}

#[tokio::test]
async fn plugin_snapshot_adoption_is_committed_before_publication() {
    let sid = SessionId::from("plugin-adoption-order");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        bootstrap(&sid),
    );
    let mut events = session.subscribe();
    let summary = PluginSnapshotSummary {
        generation: 3,
        discovered: 2,
        active: 1,
        project_trusted: true,
    };
    session
        .submit(Command::AdoptPluginSnapshot {
            summary: summary.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::PluginSnapshotAdopted {
            summary: summary.clone()
        }
    );
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes.len(), 2);
    assert_eq!(
        replay.envelopes[1].record,
        JournalRecord::PluginSnapshotAdopted { summary }
    );
}

struct BlockingDriver;

struct BlockingCompactionDriver;

struct SuccessfulCompactionDriver {
    source: Vec<ModelMessage>,
    replacement: Vec<ModelMessage>,
    installed: Arc<tokio::sync::Mutex<Vec<ModelMessage>>>,
}

struct AutomaticCompactionDriver {
    trigger: CompactionTrigger,
    source: Vec<ModelMessage>,
    replacement: Vec<ModelMessage>,
    installed_inside_turn: tokio::sync::Mutex<Vec<ModelMessage>>,
    compaction_started: Notify,
    release_compaction: Notify,
    block_compaction: bool,
    install_history_called: AtomicBool,
}

struct AutomaticFailureDriver {
    error: lato_core::AgentError,
    observed: tokio::sync::Mutex<Option<AutomaticCompactionOutcome>>,
}

#[async_trait]
impl TurnDriver for AutomaticFailureDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        let outcome = events
            .compact(AutomaticCompactionRequest {
                trigger: CompactionTrigger::Threshold,
                usage: ContextUsage {
                    estimated_input_tokens: 900,
                    context_window: 1_000,
                    utilization_percent: 90,
                },
                messages: vec![ModelMessage {
                    role: ModelRole::User,
                    content: vec![ModelContent::Text {
                        text: "old context".into(),
                    }],
                }],
                two_pass: None,
                prior_model_attempts: 0,
            })
            .await?;
        *self.observed.lock().await = Some(outcome);
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }

    async fn compact(
        &self,
        _request: CompactionRequest,
        _control: CompactionControl,
    ) -> Result<CompactionCandidate, lato_core::AgentError> {
        Err(self.error.clone())
    }
}

impl AutomaticCompactionDriver {
    fn new(trigger: CompactionTrigger, block_compaction: bool) -> Self {
        Self {
            trigger,
            source: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "old context".into(),
                }],
            }],
            replacement: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "compacted context".into(),
                }],
            }],
            installed_inside_turn: tokio::sync::Mutex::new(Vec::new()),
            compaction_started: Notify::new(),
            release_compaction: Notify::new(),
            block_compaction,
            install_history_called: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl TurnDriver for AutomaticCompactionDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        let outcome = events
            .compact(AutomaticCompactionRequest {
                trigger: self.trigger,
                usage: ContextUsage {
                    estimated_input_tokens: 900,
                    context_window: 1_000,
                    utilization_percent: 90,
                },
                messages: self.source.clone(),
                two_pass: None,
                prior_model_attempts: 0,
            })
            .await?;
        if let AutomaticCompactionOutcome::Compacted(messages) = outcome {
            *self.installed_inside_turn.lock().await = messages;
        }
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
        control: CompactionControl,
    ) -> Result<CompactionCandidate, lato_core::AgentError> {
        self.compaction_started.notify_one();
        if self.block_compaction {
            tokio::select! {
                _ = control.cancellation.cancelled() => {
                    return Err(CompactionError::Cancelled.into());
                }
                _ = self.release_compaction.notified() => {}
            }
        }
        Ok(CompactionCandidate {
            compaction_id: request.compaction_id,
            messages: self.replacement.clone(),
            before: lato_core::CompactionSize {
                message_count: request.messages.len() as u64,
                serialized_bytes: 10_000,
            },
            after: lato_core::CompactionSize {
                message_count: self.replacement.len() as u64,
                serialized_bytes: 1_000,
            },
            summary_chars: 800,
        })
    }

    async fn install_history(
        &self,
        _messages: Vec<ModelMessage>,
    ) -> Result<(), lato_core::AgentError> {
        self.install_history_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl TurnDriver for SuccessfulCompactionDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }

    async fn history_snapshot(&self) -> Result<Vec<ModelMessage>, lato_core::AgentError> {
        Ok(self.source.clone())
    }

    async fn compact(
        &self,
        request: CompactionRequest,
        _control: CompactionControl,
    ) -> Result<CompactionCandidate, lato_core::AgentError> {
        Ok(CompactionCandidate {
            compaction_id: request.compaction_id,
            messages: self.replacement.clone(),
            before: lato_core::CompactionSize {
                message_count: self.source.len() as u64,
                serialized_bytes: 10_000,
            },
            after: lato_core::CompactionSize {
                message_count: self.replacement.len() as u64,
                serialized_bytes: 1_000,
            },
            summary_chars: 800,
        })
    }

    async fn install_history(
        &self,
        messages: Vec<ModelMessage>,
    ) -> Result<(), lato_core::AgentError> {
        *self.installed.lock().await = messages;
        Ok(())
    }
}

#[async_trait]
impl TurnDriver for BlockingCompactionDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }

    async fn history_snapshot(&self) -> Result<Vec<ModelMessage>, lato_core::AgentError> {
        Ok(vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "context".repeat(400),
            }],
        }])
    }

    async fn compact(
        &self,
        _request: CompactionRequest,
        control: CompactionControl,
    ) -> Result<CompactionCandidate, lato_core::AgentError> {
        control.cancellation.cancelled().await;
        Err(CompactionError::Cancelled.into())
    }
}

struct CommitDriver;

#[async_trait]
impl TurnDriver for CommitDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        events
            .commit(
                JournalRecord::ConversationItemCommitted {
                    message: ModelMessage {
                        role: ModelRole::Assistant,
                        content: vec![ModelContent::Text {
                            text: "committed".into(),
                        }],
                    },
                },
                JournalDurability::SyncData,
            )
            .await?;
        Ok(TurnOutput {
            final_text: "done".into(),
        })
    }
}

#[async_trait]
impl TurnDriver for BlockingDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        mut control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(TurnOutput { final_text: String::new() }),
            steer = control.steering.recv() => Ok(TurnOutput {
                final_text: format!("steered:{}", steer.expect("steering channel closed").text),
            }),
        }
    }
}

struct StaleEventDriver {
    runs: AtomicUsize,
    first_emitter_tx: Mutex<Option<oneshot::Sender<TurnEventEmitter>>>,
}

#[async_trait]
impl TurnDriver for StaleEventDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        mut control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            let sent = self
                .first_emitter_tx
                .lock()
                .expect("first emitter mutex poisoned")
                .take()
                .expect("first emitter sender already taken")
                .send(events);
            assert!(sent.is_ok(), "first emitter receiver closed");
        }
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(TurnOutput { final_text: String::new() }),
            steer = control.steering.recv() => Ok(TurnOutput {
                final_text: format!("steered:{}", steer.expect("steering channel closed").text),
            }),
        }
    }
}

struct BurstDriver;

#[async_trait]
impl TurnDriver for BurstDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        for index in 0..300 {
            events.model_delta(format!("delta-{index}"))?;
        }
        Ok(TurnOutput {
            final_text: "done".into(),
        })
    }
}

struct CleanupDriver {
    emitter_tx: Mutex<Option<oneshot::Sender<TurnEventEmitter>>>,
    dropped_tx: Mutex<Option<oneshot::Sender<()>>>,
}

struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[async_trait]
impl TurnDriver for CleanupDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        let _drop_signal = DropSignal(
            self.dropped_tx
                .lock()
                .expect("dropped signal mutex poisoned")
                .take(),
        );
        let sent = self
            .emitter_tx
            .lock()
            .expect("emitter mutex poisoned")
            .take()
            .expect("emitter sender already taken")
            .send(events);
        assert!(sent.is_ok(), "emitter receiver closed");
        pending().await
    }
}

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
) -> lato_core::EventEnvelope {
    timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

async fn events_through_turn_terminal(
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
) -> Vec<lato_core::EventEnvelope> {
    timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        loop {
            let event = events
                .recv()
                .await
                .expect("event channel closed before turn terminal");
            let terminal = matches!(
                &event.payload,
                EventPayload::TurnCompleted(_)
                    | EventPayload::TurnFailed { .. }
                    | EventPayload::TurnCancelled { .. }
            );
            observed.push(event);
            if terminal {
                return observed;
            }
        }
    })
    .await
    .expect("turn event sequence exceeded its total deadline")
}

async fn shutdown_and_collect(
    session: &lato_runtime::SessionHandle,
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
) -> Vec<lato_core::EventEnvelope> {
    timeout(Duration::from_secs(1), session.submit(Command::Shutdown))
        .await
        .expect("shutdown command exceeded its deadline")
        .unwrap();
    let observed = timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        loop {
            let event = events
                .recv()
                .await
                .expect("event channel closed before session stop");
            let stopped = matches!(&event.payload, EventPayload::SessionStopped);
            observed.push(event);
            if stopped {
                return observed;
            }
        }
    })
    .await
    .expect("shutdown event sequence exceeded its total deadline");
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
    ));
    observed
}

fn runtime_event_kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::SessionStarted => "session_started",
        EventPayload::TurnStarted => "turn_started",
        EventPayload::ModelDelta { .. } => "model_delta",
        EventPayload::ReasoningDelta { .. } => "reasoning_delta",
        EventPayload::TurnCompleted(_) => "turn_completed",
        EventPayload::TurnFailed { .. } => "turn_failed",
        EventPayload::TurnCancelled { .. } => "turn_cancelled",
        EventPayload::ContextUsageUpdated { .. } => "context_usage_updated",
        EventPayload::CompactionStarted { .. } => "compaction_started",
        EventPayload::CompactionCompleted { .. } => "compaction_completed",
        EventPayload::CompactionFailed { .. } => "compaction_failed",
        EventPayload::CompactionCancelled { .. } => "compaction_cancelled",
        EventPayload::PluginSnapshotAdopted { .. } => "plugin_snapshot_adopted",
        EventPayload::SessionStopped => "session_stopped",
    }
}

struct BlockingPrefireDriver {
    started: Notify,
    install_called: AtomicBool,
}

struct CompletedPrefireDriver {
    observed_note1: tokio::sync::Mutex<Option<String>>,
    install_called: AtomicBool,
}

#[async_trait]
impl TurnDriver for BlockingPrefireDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        events.model_delta("ordinary delta")?;
        let _ = events
            .prefire_compaction(PrefireCompactionRequest {
                messages: vec![ModelMessage {
                    role: ModelRole::User,
                    content: vec![ModelContent::Text {
                        text: request.input.text,
                    }],
                }],
                prefix_len: 1,
                policy: lato_core::CompactionPolicy::default(),
            })
            .await;
        Ok(TurnOutput {
            final_text: String::new(),
        })
    }

    async fn prefire_compaction(
        &self,
        _request: PrefireCompactionRequest,
        control: CompactionControl,
    ) -> Result<PrefireCompactionResult, lato_core::AgentError> {
        self.started.notify_one();
        control.cancellation.cancelled().await;
        Err(lato_core::ModelError::cancelled().into())
    }

    async fn install_history(
        &self,
        _messages: Vec<ModelMessage>,
    ) -> Result<(), lato_core::AgentError> {
        self.install_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl TurnDriver for CompletedPrefireDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        let result = events
            .prefire_compaction(PrefireCompactionRequest {
                messages: vec![ModelMessage {
                    role: ModelRole::User,
                    content: vec![ModelContent::Text {
                        text: request.input.text,
                    }],
                }],
                prefix_len: 1,
                policy: lato_core::CompactionPolicy::default(),
            })
            .await?;
        *self.observed_note1.lock().await = Some(result.note1);
        events.model_delta("ordinary delta")?;
        Ok(TurnOutput {
            final_text: "done".into(),
        })
    }

    async fn prefire_compaction(
        &self,
        _request: PrefireCompactionRequest,
        _control: CompactionControl,
    ) -> Result<PrefireCompactionResult, lato_core::AgentError> {
        Ok(PrefireCompactionResult {
            note1: PREFIRE_NOTE1_SENTINEL.into(),
        })
    }

    async fn install_history(
        &self,
        _messages: Vec<ModelMessage>,
    ) -> Result<(), lato_core::AgentError> {
        self.install_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn journal_contains_assistant_text(record: &JournalRecord, needle: &str) -> bool {
    let JournalRecord::ConversationItemCommitted { message } = record else {
        return false;
    };
    message.role == ModelRole::Assistant
        && message
            .content
            .iter()
            .any(|content| matches!(content, ModelContent::Text { text } if text.contains(needle)))
}

fn is_compaction_lifecycle(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::CompactionStarted { .. }
            | EventPayload::CompactionCompleted { .. }
            | EventPayload::CompactionFailed { .. }
            | EventPayload::CompactionCancelled { .. }
    )
}

#[tokio::test]
async fn prefire_is_non_installing_and_cancellable_without_compaction_events() {
    let sid = SessionId::from("session-prefire");
    let driver = Arc::new(BlockingPrefireDriver {
        started: Notify::new(),
        install_called: AtomicBool::new(false),
    });
    let store = Arc::new(MemoryEventStore::new());
    let session =
        spawn_session_with_store(sid.clone(), driver.clone(), store.clone(), bootstrap(&sid));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("large history"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    let started = next_event(&mut events).await;
    let turn_id = started.turn_id.clone().unwrap();
    assert!(matches!(started.payload, EventPayload::TurnStarted));
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::ModelDelta {
            text: "ordinary delta".into()
        }
    );
    timeout(Duration::from_secs(1), driver.started.notified())
        .await
        .expect("prefire did not start");
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(!driver.install_called.load(Ordering::SeqCst));
    session
        .submit(Command::CancelTurn { turn_id })
        .await
        .unwrap();
    let cancelled = next_event(&mut events).await;
    assert!(matches!(
        &cancelled.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::User
        }
    ));
    assert!(!is_compaction_lifecycle(&cancelled.payload));

    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("prefire replay timed out")
        .expect("prefire replay failed");
    assert!(!replay.envelopes.iter().any(|envelope| matches!(
        &envelope.record,
        JournalRecord::CompactionRequested { .. } | JournalRecord::HistoryProjectionReplaced { .. }
    )));
    assert!(!replay.envelopes.iter().any(|envelope| {
        journal_contains_assistant_text(&envelope.record, PREFIRE_NOTE1_SENTINEL)
    }));
    assert!(!driver.install_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn completed_prefire_is_never_emitted_installed_or_persisted() {
    let sid = SessionId::from("session-prefire-completed");
    let driver = Arc::new(CompletedPrefireDriver {
        observed_note1: tokio::sync::Mutex::new(None),
        install_called: AtomicBool::new(false),
    });
    let store = Arc::new(MemoryEventStore::new());
    let session =
        spawn_session_with_store(sid.clone(), driver.clone(), store.clone(), bootstrap(&sid));
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("large history"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut observed = events_through_turn_terminal(&mut events).await;
    observed.extend(shutdown_and_collect(&session, &mut events).await);
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::TurnCompleted(_)))
            .count(),
        1
    );
    assert!(
        observed
            .iter()
            .all(|event| !is_compaction_lifecycle(&event.payload))
    );
    assert!(observed.iter().all(|event| {
        !matches!(
            &event.payload,
            EventPayload::ModelDelta { text } if text.contains(PREFIRE_NOTE1_SENTINEL)
        )
    }));
    assert_eq!(
        observed
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::ModelDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["ordinary delta"]
    );
    assert_eq!(
        driver.observed_note1.lock().await.as_deref(),
        Some(PREFIRE_NOTE1_SENTINEL)
    );
    assert!(!driver.install_called.load(Ordering::SeqCst));
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("completed prefire replay timed out")
        .expect("completed prefire replay failed");
    assert!(!replay.envelopes.iter().any(|envelope| matches!(
        &envelope.record,
        JournalRecord::CompactionRequested { .. } | JournalRecord::HistoryProjectionReplaced { .. }
    )));
    assert!(!replay.envelopes.iter().any(|envelope| {
        journal_contains_assistant_text(&envelope.record, PREFIRE_NOTE1_SENTINEL)
    }));
}

#[tokio::test]
async fn emits_ordered_start_delta_and_completion() {
    let session = spawn_session("session-1".into(), Arc::new(EchoDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let delta = next_event(&mut events).await;
    let completed = next_event(&mut events).await;

    assert!(matches!(started.payload, EventPayload::SessionStarted));
    assert!(matches!(turn_started.payload, EventPayload::TurnStarted));
    assert_eq!(
        delta.payload,
        EventPayload::ModelDelta {
            text: "hello".into()
        }
    );
    assert_eq!(
        completed.payload,
        EventPayload::TurnCompleted(TurnOutput {
            final_text: "hello".into()
        }),
    );
    assert_eq!(
        [
            started.sequence,
            turn_started.sequence,
            delta.sequence,
            completed.sequence
        ],
        [1, 2, 3, 4],
    );
}

#[tokio::test]
async fn steer_is_delivered_to_the_active_turn() {
    let session = spawn_session("session-2".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("start"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::SteerTurn(UserInput::text("more")))
        .await
        .unwrap();

    let mut final_text = None;
    for _ in 0..4 {
        if let EventPayload::TurnCompleted(output) = next_event(&mut events).await.payload {
            final_text = Some(output.final_text);
            break;
        }
    }
    assert_eq!(final_text.as_deref(), Some("steered:more"));
}

#[tokio::test]
async fn replace_cancels_before_starting_the_pending_turn() {
    let session = spawn_session("session-3".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Replace,
        }))
        .await
        .unwrap();

    let session_started = next_event(&mut events).await;
    let first_started = next_event(&mut events).await;
    assert!(matches!(
        session_started.payload,
        EventPayload::SessionStarted
    ));
    assert!(matches!(first_started.payload, EventPayload::TurnStarted));
    let first_turn_id = first_started.turn_id.expect("first turn id missing");

    let mut cancelled_sequence = None;
    let mut second_start = None;
    for _ in 0..6 {
        let event = next_event(&mut events).await;
        match event.payload {
            EventPayload::TurnCancelled {
                reason: CancelReason::Replaced,
            } => {
                assert_eq!(event.turn_id.as_ref(), Some(&first_turn_id));
                cancelled_sequence = Some(event.sequence);
            }
            EventPayload::TurnStarted if cancelled_sequence.is_some() => {
                second_start = Some(event);
                break;
            }
            _ => {}
        }
    }
    let cancelled_sequence = cancelled_sequence.expect("replacement cancellation missing");
    let second_start = second_start.expect("pending turn did not start after cancellation");
    assert_eq!(second_start.sequence, cancelled_sequence + 1);
    assert_ne!(second_start.turn_id.as_ref(), Some(&first_turn_id));
}

#[tokio::test]
async fn event_identity_is_stable_and_unique_within_a_session() {
    let session = spawn_session("session-identity".into(), Arc::new(EchoDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut seen_ids = std::collections::HashSet::new();
    let mut turn_id = None;
    for expected_sequence in 1..=4 {
        let event = next_event(&mut events).await;
        assert_eq!(event.session_id.as_str(), "session-identity");
        assert_eq!(event.sequence, expected_sequence);
        assert!(seen_ids.insert(event.event_id.clone()));
        if matches!(event.payload, EventPayload::TurnStarted) {
            turn_id = event.turn_id.clone();
        } else if matches!(
            event.payload,
            EventPayload::ModelDelta { .. } | EventPayload::TurnCompleted(_)
        ) {
            assert_eq!(event.turn_id, turn_id);
        }
    }
}

#[tokio::test]
async fn dropping_the_last_handle_aborts_the_driver_and_closes_its_event_bus() {
    let (emitter_tx, emitter_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let driver = CleanupDriver {
        emitter_tx: Mutex::new(Some(emitter_tx)),
        dropped_tx: Mutex::new(Some(dropped_tx)),
    };
    let session = spawn_session("session-cleanup".into(), Arc::new(driver));
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let emitter = timeout(Duration::from_secs(1), emitter_rx)
        .await
        .expect("driver did not expose its emitter")
        .expect("driver dropped its emitter sender");

    drop(session);

    timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("driver task was not cleaned up")
        .expect("driver dropped signal sender unexpectedly");
    let error = emitter
        .model_delta("after shutdown")
        .expect_err("closed runtime event bus should reject driver events");
    assert_eq!(error.code, "runtime.event_bus_closed");
}

#[tokio::test]
async fn reject_mode_does_not_start_a_second_turn() {
    let session = spawn_session("session-reject".into(), Arc::new(BlockingDriver));
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.invalid_transition");
}

#[tokio::test]
async fn cancel_targets_the_active_turn_and_emits_user_reason() {
    let session = spawn_session("session-cancel".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let turn_id = turn_started.turn_id.unwrap();
    assert!(matches!(started.payload, EventPayload::SessionStarted));
    session
        .submit(Command::CancelTurn { turn_id })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::User,
        },
    );
}

#[tokio::test]
async fn shutdown_stops_the_session_and_rejects_future_commands() {
    let session = spawn_session("session-stop".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::TurnStarted
    ));
    session.submit(Command::Shutdown).await.unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::Shutdown,
        },
    );
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStopped
    ));
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("after stop"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.command_bus_closed");
}

#[tokio::test]
async fn stale_driver_events_from_a_replaced_turn_are_ignored() {
    let (first_emitter_tx, first_emitter_rx) = oneshot::channel();
    let driver = StaleEventDriver {
        runs: AtomicUsize::new(0),
        first_emitter_tx: Mutex::new(Some(first_emitter_tx)),
    };
    let session = spawn_session("session-stale".into(), Arc::new(driver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let stale_emitter = timeout(Duration::from_secs(1), first_emitter_rx)
        .await
        .expect("first driver did not expose its emitter")
        .expect("first driver dropped its emitter sender");
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Replace,
        }))
        .await
        .unwrap();

    loop {
        let event = next_event(&mut events).await;
        if matches!(event.payload, EventPayload::TurnStarted) && event.sequence > 2 {
            break;
        }
    }
    stale_emitter
        .model_delta("stale")
        .expect("session should still accept driver messages");
    session
        .submit(Command::SteerTurn(UserInput::text("finish")))
        .await
        .unwrap();

    loop {
        match next_event(&mut events).await.payload {
            EventPayload::ModelDelta { text } => panic!("stale event leaked: {text}"),
            EventPayload::TurnCompleted(output) => {
                assert_eq!(output.final_text, "steered:finish");
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn slow_subscribers_observe_bounded_event_lag() {
    let session = spawn_session("session-lag".into(), Arc::new(BurstDriver));
    let mut lagged_events = session.subscribe();
    let mut completion_events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("burst"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    loop {
        let event = next_event(&mut completion_events).await;
        if matches!(event.payload, EventPayload::TurnCompleted(_)) {
            break;
        }
    }

    let lagged = timeout(Duration::from_secs(1), lagged_events.recv())
        .await
        .expect("lag check timed out")
        .expect_err("slow subscriber should receive a bounded lag signal");
    assert!(matches!(
        lagged,
        tokio::sync::broadcast::error::RecvError::Lagged(skipped) if skipped > 0
    ));
}

#[tokio::test]
async fn canonical_state_is_persisted_before_broadcast() {
    let sid = SessionId::from("session-barrier");
    let store = Arc::new(ControlledStore::new(Some("turn_input_accepted"), None));
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        bootstrap(&sid),
    );
    let mut events = session.subscribe();
    let submitted = {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .submit(Command::StartTurn(StartTurn {
                    input: UserInput::text("hello"),
                    behavior: StartBehavior::Reject,
                }))
                .await
        })
    };

    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    store.append_started.notified().await;
    assert!(
        timeout(Duration::from_millis(30), events.recv())
            .await
            .is_err()
    );
    store.release.notify_one();
    submitted.await.unwrap().unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::TurnStarted
    ));
}

#[tokio::test]
async fn journal_failure_stops_the_turn_without_broadcasting_committed_state() {
    let sid = SessionId::from("session-journal-failure");
    let store = Arc::new(ControlledStore::new(None, Some("turn_input_accepted")));
    let session =
        spawn_session_with_store(sid.clone(), Arc::new(EchoDriver), store, bootstrap(&sid));
    let mut events = session.subscribe();
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "journal.io");
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    assert!(
        timeout(Duration::from_millis(30), events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn driver_commit_waits_for_store_ack() {
    let sid = SessionId::from("session-driver-commit");
    let store = Arc::new(ControlledStore::new(
        Some("conversation_item_committed"),
        None,
    ));
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(CommitDriver),
        store.clone(),
        bootstrap(&sid),
    );
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("commit"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::TurnStarted
    ));
    store.append_started.notified().await;
    assert!(
        timeout(Duration::from_millis(30), events.recv())
            .await
            .is_err()
    );
    store.release.notify_one();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCompleted(_)
    ));
}

#[tokio::test]
async fn model_deltas_are_live_and_never_appended() {
    let sid = SessionId::from("session-live-delta");
    let store = Arc::new(ControlledStore::new(None, None));
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        bootstrap(&sid),
    );
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let mut saw_delta = false;
    loop {
        match next_event(&mut events).await.payload {
            EventPayload::ModelDelta { .. } => saw_delta = true,
            EventPayload::TurnCompleted(_) => break,
            _ => {}
        }
    }
    assert!(saw_delta);
    let kinds: Vec<_> = store
        .records
        .lock()
        .await
        .iter()
        .map(|envelope| record_kind(&envelope.record))
        .collect();
    assert_eq!(
        kinds,
        vec!["session_started", "turn_input_accepted", "turn_completed"]
    );
}

#[tokio::test]
async fn bootstrap_resumes_the_next_journal_sequence_without_duplicate_start() {
    let sid = SessionId::from("session-resume");
    let store = Arc::new(MemoryEventStore::new());
    store
        .append(
            JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                record_id: JournalRecordId::from("session-resume-journal-0"),
                session_id: sid.clone(),
                turn_id: None,
                journal_sequence: 0,
                timestamp_ms: 0,
                record: JournalRecord::SessionStarted,
            },
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    let replay = store.replay(&sid).await.unwrap();
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        SessionBootstrap { replay },
    );
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("resume"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes[0].record, JournalRecord::SessionStarted);
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|item| matches!(item.record, JournalRecord::SessionStarted))
            .count(),
        1
    );
    assert_eq!(replay.envelopes[1].journal_sequence, 1);
}

#[tokio::test]
async fn old_journal_without_phase_4c3_metadata_replays_and_runs_the_next_turn() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("session-old-journal");
    let old_turn = lato_core::TurnId::from("old-turn");
    let seed_store = FileEventStore::open(directory.path()).unwrap();
    let old_assistant = ModelMessage {
        role: ModelRole::Assistant,
        content: vec![ModelContent::Text {
            text: "old answer".into(),
        }],
    };
    let old_records = vec![
        (None, JournalRecord::SessionStarted),
        (
            Some(old_turn.clone()),
            JournalRecord::TurnInputAccepted {
                input: UserInput::text("old question"),
            },
        ),
        (
            Some(old_turn.clone()),
            JournalRecord::ConversationItemCommitted {
                message: old_assistant.clone(),
            },
        ),
        (
            Some(old_turn),
            JournalRecord::TurnCompleted {
                output: TurnOutput {
                    final_text: "old answer".into(),
                },
            },
        ),
    ];
    for (sequence, (turn_id, record)) in old_records.into_iter().enumerate() {
        seed_store
            .append(
                JournalEnvelope {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    record_id: JournalRecordId::from(format!("old-journal-{sequence}")),
                    session_id: sid.clone(),
                    turn_id,
                    journal_sequence: sequence as u64,
                    timestamp_ms: sequence as u64,
                    record,
                },
                JournalDurability::SyncData,
            )
            .await
            .unwrap();
    }
    seed_store.shutdown(&sid).await.unwrap();
    drop(seed_store);

    let store = Arc::new(FileEventStore::open(directory.path()).unwrap());
    let bootstrap_replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("old journal replay timed out")
        .expect("old journal replay failed");
    assert_eq!(
        bootstrap_replay.projection.messages,
        vec![
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "old question".into(),
                }],
            },
            old_assistant,
        ]
    );
    assert!(bootstrap_replay.projection.active_checkpoint_id.is_none());
    assert!(bootstrap_replay.projection.model_selection.is_none());
    assert!(bootstrap_replay.projection.model_family.is_none());
    assert!(bootstrap_replay.projection.model_context_window.is_none());

    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        SessionBootstrap {
            replay: bootstrap_replay,
        },
    );
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("next question"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let mut observed = events_through_turn_terminal(&mut events).await;
    observed.extend(shutdown_and_collect(&session, &mut events).await);
    assert_eq!(
        observed
            .iter()
            .map(|event| runtime_event_kind(&event.payload))
            .collect::<Vec<_>>(),
        vec![
            "session_started",
            "turn_started",
            "model_delta",
            "turn_completed",
            "session_stopped",
        ]
    );
    assert_eq!(
        observed[2].payload,
        EventPayload::ModelDelta {
            text: "next question".into(),
        }
    );
    assert_eq!(
        observed[3].payload,
        EventPayload::TurnCompleted(TurnOutput {
            final_text: "next question".into(),
        })
    );

    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("continued old journal replay timed out")
        .expect("continued old journal replay failed");
    assert_eq!(replay.envelopes.len(), 7);
    assert_eq!(replay.projection.next_journal_sequence, 7);
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|envelope| matches!(&envelope.record, JournalRecord::SessionStarted))
            .count(),
        1
    );
    assert!(!replay.envelopes.iter().any(|envelope| matches!(
        &envelope.record,
        JournalRecord::CompactionRequested { .. }
            | JournalRecord::CompactionFailed { .. }
            | JournalRecord::CompactionCancelled { .. }
            | JournalRecord::HistoryProjectionReplaced { .. }
            | JournalRecord::ModelSelected { .. }
    )));
}

fn manual_compaction() -> Command {
    Command::CompactSession(CompactSession {
        user_context: None,
        trigger: CompactionTrigger::Manual,
    })
}

#[tokio::test]
async fn automatic_compaction_persists_and_returns_history_inside_the_same_turn() {
    let sid = SessionId::from("session-automatic-threshold");
    let driver = Arc::new(AutomaticCompactionDriver::new(
        CompactionTrigger::Threshold,
        false,
    ));
    let replacement = driver.replacement.clone();
    let store = Arc::new(MemoryEventStore::new());
    let session =
        spawn_session_with_store(sid.clone(), driver.clone(), store.clone(), bootstrap(&sid));
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut observed_turn_id = None;
    let mut compaction_triggers = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if matches!(event.payload, EventPayload::TurnStarted) {
            observed_turn_id = event.turn_id.clone();
        }
        match event.payload {
            EventPayload::CompactionStarted { trigger, .. } => {
                assert_eq!(event.turn_id, observed_turn_id);
                compaction_triggers.push(trigger);
            }
            EventPayload::CompactionCompleted { .. } => {
                assert_eq!(event.turn_id, observed_turn_id);
            }
            EventPayload::TurnCompleted(_) => {
                assert_eq!(event.turn_id, observed_turn_id);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(compaction_triggers, vec![CompactionTrigger::Threshold]);
    assert_eq!(
        store.replay(&sid).await.unwrap().projection.messages,
        replacement
    );
    assert_eq!(*driver.installed_inside_turn.lock().await, replacement);
    assert!(!driver.install_history_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn automatic_compaction_preserves_model_switch_trigger_and_turn_identity() {
    let driver = Arc::new(AutomaticCompactionDriver::new(
        CompactionTrigger::ModelSwitch,
        false,
    ));
    let session = spawn_session("session-automatic-model-switch".into(), driver);
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("switch"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut turn_id = None;
    let mut saw_model_switch = false;
    loop {
        let event = next_event(&mut events).await;
        if matches!(event.payload, EventPayload::TurnStarted) {
            turn_id = event.turn_id.clone();
        }
        if let EventPayload::CompactionStarted { trigger, .. } = event.payload {
            assert_eq!(trigger, CompactionTrigger::ModelSwitch);
            assert_eq!(event.turn_id, turn_id);
            saw_model_switch = true;
        } else if matches!(event.payload, EventPayload::TurnCompleted(_)) {
            assert_eq!(event.turn_id, turn_id);
            break;
        }
    }
    assert!(saw_model_switch);
}

#[tokio::test]
async fn automatic_compaction_is_cancelled_with_its_turn_and_rejects_manual_overlap() {
    let driver = Arc::new(AutomaticCompactionDriver::new(
        CompactionTrigger::Threshold,
        true,
    ));
    let session = spawn_session("session-automatic-cancel".into(), driver.clone());
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    timeout(Duration::from_secs(1), driver.compaction_started.notified())
        .await
        .expect("automatic compaction did not start");

    let error = session.submit(manual_compaction()).await.unwrap_err();
    assert_eq!(error.code, "compaction.already_active");

    let mut turn_id = None;
    let mut compaction_id = None;
    while compaction_id.is_none() {
        let event = next_event(&mut events).await;
        match event.payload {
            EventPayload::TurnStarted => turn_id = event.turn_id,
            EventPayload::CompactionStarted {
                compaction_id: id, ..
            } => {
                assert_eq!(event.turn_id, turn_id);
                compaction_id = Some(id);
            }
            _ => {}
        }
    }
    let turn_id = turn_id.expect("turn start missing");
    let compaction_id = compaction_id.unwrap();
    session
        .submit(Command::CancelTurn {
            turn_id: turn_id.clone(),
        })
        .await
        .unwrap();

    let mut saw_compaction_cancel = false;
    loop {
        let event = next_event(&mut events).await;
        match event.payload {
            EventPayload::CompactionCancelled {
                compaction_id: cancelled,
            } => {
                assert_eq!(cancelled, compaction_id);
                assert_eq!(event.turn_id.as_ref(), Some(&turn_id));
                saw_compaction_cancel = true;
            }
            EventPayload::TurnCancelled {
                reason: CancelReason::User,
            } => {
                assert_eq!(event.turn_id.as_ref(), Some(&turn_id));
                break;
            }
            _ => {}
        }
    }
    assert!(saw_compaction_cancel);
}

#[tokio::test]
async fn automatic_compaction_pre_marker_failure_retains_old_history() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("automatic-pre-marker-failure");
    let driver = Arc::new(AutomaticCompactionDriver::new(
        CompactionTrigger::Threshold,
        false,
    ));
    let old_history = driver.source.clone();
    let replay = seeded_file_store_with_messages(directory.path(), &sid, &old_history).await;
    let store = Arc::new(
        FileEventStore::open_with_fault_injector(
            directory.path(),
            RuntimeFault::once(FaultPoint::BeforeCheckpointPublish),
        )
        .unwrap(),
    );
    let session = spawn_session_with_store(
        sid.clone(),
        driver.clone(),
        store.clone(),
        SessionBootstrap { replay },
    );
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut observed = events_through_turn_terminal(&mut events).await;
    observed.extend(shutdown_and_collect(&session, &mut events).await);
    assert_eq!(
        observed
            .iter()
            .map(|event| runtime_event_kind(&event.payload))
            .collect::<Vec<_>>(),
        vec![
            "session_started",
            "turn_started",
            "compaction_started",
            "compaction_failed",
            "turn_failed",
            "session_stopped",
        ]
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::CompactionStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::CompactionFailed { .. }))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::TurnFailed { .. }))
            .count(),
        1
    );
    let EventPayload::CompactionFailed { error, .. } = &observed[3].payload else {
        unreachable!("exact event ordering asserted above")
    };
    assert_eq!(error.category, lato_core::ErrorCategory::Storage);
    let EventPayload::TurnFailed { error } = &observed[4].payload else {
        unreachable!("exact event ordering asserted above")
    };
    assert_eq!(error.code, "projection.write_failed");

    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("pre-checkpoint replay timed out")
        .expect("pre-checkpoint replay failed");
    assert!(replay.projection.active_checkpoint_id.is_none());
    let mut expected_history = old_history;
    expected_history.push(ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text {
            text: "continue".into(),
        }],
    });
    assert_eq!(replay.projection.messages, expected_history);
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|envelope| matches!(
                &envelope.record,
                JournalRecord::CompactionRequested { .. }
            ))
            .count(),
        1
    );
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|envelope| matches!(&envelope.record, JournalRecord::TurnFailed { .. }))
            .count(),
        1
    );
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|envelope| matches!(&envelope.record, JournalRecord::CompactionFailed { .. }))
            .count(),
        1
    );
    assert!(!replay.envelopes.iter().any(|envelope| matches!(
        &envelope.record,
        JournalRecord::HistoryProjectionReplaced { .. }
    )));
    assert!(driver.installed_inside_turn.lock().await.is_empty());
    assert!(!driver.install_history_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn automatic_compaction_post_marker_reconciliation_returns_committed_history() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("automatic-post-marker-failure");
    let driver = Arc::new(AutomaticCompactionDriver::new(
        CompactionTrigger::Threshold,
        false,
    ));
    let old_history = driver.source.clone();
    let replay = seeded_file_store_with_messages(directory.path(), &sid, &old_history).await;
    let store = Arc::new(
        FileEventStore::open_with_fault_injector(
            directory.path(),
            RuntimeFault::once(FaultPoint::BeforeMetadataPublish),
        )
        .unwrap(),
    );
    let replacement = driver.replacement.clone();
    let session = spawn_session_with_store(
        sid.clone(),
        driver.clone(),
        store.clone(),
        SessionBootstrap { replay },
    );
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut observed = events_through_turn_terminal(&mut events).await;
    observed.extend(shutdown_and_collect(&session, &mut events).await);
    assert_eq!(
        observed
            .iter()
            .map(|event| runtime_event_kind(&event.payload))
            .collect::<Vec<_>>(),
        vec![
            "session_started",
            "turn_started",
            "compaction_started",
            "compaction_completed",
            "turn_completed",
            "session_stopped",
        ]
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::CompactionStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::CompactionCompleted { .. }))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::TurnCompleted(_)))
            .count(),
        1
    );
    let EventPayload::CompactionCompleted {
        checkpoint_id: event_checkpoint_id,
        warning,
        ..
    } = &observed[3].payload
    else {
        unreachable!("exact event ordering asserted above")
    };
    assert_eq!(
        warning.as_ref().map(|warning| warning.code.as_str()),
        Some("projection.write_failed")
    );
    let EventPayload::TurnCompleted(output) = &observed[4].payload else {
        unreachable!("exact event ordering asserted above")
    };
    assert_eq!(output.final_text, "continue");
    assert_eq!(*driver.installed_inside_turn.lock().await, replacement);
    let replay = timeout(Duration::from_secs(1), store.replay(&sid))
        .await
        .expect("post-marker replay timed out")
        .expect("post-marker replay failed");
    assert_eq!(replay.projection.messages, replacement);
    let replacement_checkpoint_ids = replay
        .envelopes
        .iter()
        .filter_map(|envelope| match &envelope.record {
            JournalRecord::HistoryProjectionReplaced { checkpoint_id, .. } => {
                Some(checkpoint_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replacement_checkpoint_ids,
        vec![event_checkpoint_id.as_str()]
    );
    assert_eq!(
        replay.projection.active_checkpoint_id.as_deref(),
        Some(event_checkpoint_id.as_str())
    );
    assert_eq!(
        replay
            .envelopes
            .iter()
            .filter(|envelope| matches!(&envelope.record, JournalRecord::TurnCompleted { .. }))
            .count(),
        1
    );
    assert!(!replay.envelopes.iter().any(|envelope| matches!(
        &envelope.record,
        JournalRecord::CompactionFailed { .. } | JournalRecord::TurnFailed { .. }
    )));
    assert!(!driver.install_history_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn automatic_compaction_ordinary_failure_continues_the_turn_unchanged() {
    let driver = Arc::new(AutomaticFailureDriver {
        error: CompactionError::NothingToCompact.into(),
        observed: tokio::sync::Mutex::new(None),
    });
    let session = spawn_session("automatic-ordinary-failure".into(), driver.clone());
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut saw_failure = false;
    loop {
        match next_event(&mut events).await.payload {
            EventPayload::CompactionFailed { error, .. } => {
                assert_eq!(error.code, "compaction.nothing_to_compact");
                saw_failure = true;
            }
            EventPayload::TurnCompleted(_) => break,
            _ => {}
        }
    }
    assert!(saw_failure);
    assert!(matches!(
        *driver.observed.lock().await,
        Some(AutomaticCompactionOutcome::ContinueUnchanged { .. })
    ));
}

#[tokio::test]
async fn automatic_compaction_auth_failure_stops_the_turn() {
    let driver = Arc::new(AutomaticFailureDriver {
        error: lato_core::AgentError::new(
            "model.auth",
            lato_core::ErrorCategory::Model,
            "HTTP 401 from provider",
            lato_core::Retryability::RequiresDecision,
        ),
        observed: tokio::sync::Mutex::new(None),
    });
    let session = spawn_session("automatic-auth-failure".into(), driver.clone());
    let mut events = session.subscribe();

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("continue"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut saw_compaction_failure = false;
    loop {
        match next_event(&mut events).await.payload {
            EventPayload::CompactionFailed { error, .. } => {
                assert_eq!(error.code, "model.auth");
                saw_compaction_failure = true;
            }
            EventPayload::TurnFailed { error } => {
                assert_eq!(error.code, "model.auth");
                break;
            }
            _ => {}
        }
    }
    assert!(saw_compaction_failure);
    assert_eq!(*driver.observed.lock().await, None);
}

#[tokio::test]
async fn compaction_blocks_turns_and_can_be_cancelled() {
    let session = spawn_session(
        "session-compaction-cancel".into(),
        Arc::new(BlockingCompactionDriver),
    );
    let mut events = session.subscribe();

    session.submit(manual_compaction()).await.unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    let started = next_event(&mut events).await;
    let EventPayload::CompactionStarted { compaction_id, .. } = started.payload else {
        panic!("expected compaction start");
    };
    assert!(started.turn_id.is_none());

    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("blocked"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "compaction.already_active");

    session
        .submit(Command::CancelCompaction {
            compaction_id: compaction_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::CompactionCancelled { compaction_id }
    );
}

#[tokio::test]
async fn compaction_rejects_an_active_turn_and_duplicate_request() {
    let running = spawn_session(
        "session-compaction-running".into(),
        Arc::new(BlockingDriver),
    );
    running
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let error = running.submit(manual_compaction()).await.unwrap_err();
    assert_eq!(error.code, "compaction.active_turn");

    let compacting = spawn_session(
        "session-compaction-duplicate".into(),
        Arc::new(BlockingCompactionDriver),
    );
    compacting.submit(manual_compaction()).await.unwrap();
    let error = compacting.submit(manual_compaction()).await.unwrap_err();
    assert_eq!(error.code, "compaction.already_active");
}

#[tokio::test]
async fn cancel_compaction_requires_the_matching_operation_id() {
    let session = spawn_session(
        "session-compaction-identity".into(),
        Arc::new(BlockingCompactionDriver),
    );
    session.submit(manual_compaction()).await.unwrap();
    let error = session
        .submit(Command::CancelCompaction {
            compaction_id: CompactionId::from("wrong-compaction"),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, "compaction.not_active");
}

#[tokio::test]
async fn shutdown_cancels_compaction_before_stopping_the_session() {
    let session = spawn_session(
        "session-compaction-shutdown".into(),
        Arc::new(BlockingCompactionDriver),
    );
    let mut events = session.subscribe();
    session.submit(manual_compaction()).await.unwrap();
    let _session_started = next_event(&mut events).await;
    let started = next_event(&mut events).await;
    let EventPayload::CompactionStarted { compaction_id, .. } = started.payload else {
        panic!("expected compaction start");
    };

    session.submit(Command::Shutdown).await.unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::CompactionCancelled { compaction_id }
    );
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStopped
    ));
}

#[tokio::test]
async fn compaction_persistence_installs_checkpoint_and_resynchronizes_sequence() {
    let sid = SessionId::from("session-compaction-persistence");
    let source = vec![ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text {
            text: "old context".into(),
        }],
    }];
    let replacement = vec![ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text {
            text: "compacted context".into(),
        }],
    }];
    let installed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let driver = SuccessfulCompactionDriver {
        source,
        replacement: replacement.clone(),
        installed: installed.clone(),
    };
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(driver),
        store.clone(),
        bootstrap(&sid),
    );
    let mut events = session.subscribe();
    session.submit(manual_compaction()).await.unwrap();
    let _session_started = next_event(&mut events).await;
    let _compaction_started = next_event(&mut events).await;
    let completed = next_event(&mut events).await;
    let EventPayload::CompactionCompleted { checkpoint_id, .. } = completed.payload else {
        panic!(
            "expected compaction completion, got {:?}",
            completed.payload
        );
    };
    assert!(completed.turn_id.is_none());

    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(
        replay.projection.active_checkpoint_id.as_deref(),
        Some(checkpoint_id.as_str())
    );
    assert_eq!(replay.projection.messages, replacement);
    assert_eq!(*installed.lock().await, replacement);

    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("after compact"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.payload,
            EventPayload::TurnCompleted(_)
        ) {
            break;
        }
    }
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(
        replay.envelopes.last().unwrap().journal_sequence + 1,
        replay.projection.next_journal_sequence
    );
}

async fn seeded_file_store(directory: &std::path::Path, sid: &SessionId) -> JournalReplay {
    seeded_file_store_with_messages(directory, sid, &[]).await
}

async fn seeded_file_store_with_messages(
    directory: &std::path::Path,
    sid: &SessionId,
    messages: &[ModelMessage],
) -> JournalReplay {
    let store = FileEventStore::open(directory).unwrap();
    store
        .append(
            JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                record_id: JournalRecordId::from(format!("{sid}-journal-0")),
                session_id: sid.clone(),
                turn_id: None,
                journal_sequence: 0,
                timestamp_ms: 0,
                record: JournalRecord::SessionStarted,
            },
            JournalDurability::SyncData,
        )
        .await
        .unwrap();
    for (index, message) in messages.iter().enumerate() {
        let sequence = index as u64 + 1;
        store
            .append(
                JournalEnvelope {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    record_id: JournalRecordId::from(format!("{sid}-journal-{sequence}")),
                    session_id: sid.clone(),
                    turn_id: None,
                    journal_sequence: sequence,
                    timestamp_ms: sequence,
                    record: JournalRecord::ConversationItemCommitted {
                        message: message.clone(),
                    },
                },
                JournalDurability::SyncData,
            )
            .await
            .unwrap();
    }
    let replay = store.replay(sid).await.unwrap();
    store.shutdown(sid).await.unwrap();
    replay
}

fn successful_driver() -> (SuccessfulCompactionDriver, Vec<ModelMessage>) {
    let source = vec![ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text {
            text: "old context".into(),
        }],
    }];
    let replacement = vec![ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text {
            text: "compacted context".into(),
        }],
    }];
    (
        SuccessfulCompactionDriver {
            source,
            replacement: replacement.clone(),
            installed: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        },
        replacement,
    )
}

#[tokio::test]
async fn compaction_persistence_pre_marker_failure_preserves_old_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("runtime-pre-marker-failure");
    let replay = seeded_file_store(directory.path(), &sid).await;
    let store = Arc::new(
        FileEventStore::open_with_fault_injector(
            directory.path(),
            RuntimeFault::once(FaultPoint::BeforeCheckpointPublish),
        )
        .unwrap(),
    );
    let (driver, _) = successful_driver();
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(driver),
        store.clone(),
        SessionBootstrap { replay },
    );
    let mut events = session.subscribe();
    session.submit(manual_compaction()).await.unwrap();
    let _session_started = next_event(&mut events).await;
    let _compaction_started = next_event(&mut events).await;
    let failed = next_event(&mut events).await;
    assert!(matches!(
        failed.payload,
        EventPayload::CompactionFailed { .. }
    ));
    let replay = store.replay(&sid).await.unwrap();
    assert!(replay.projection.active_checkpoint_id.is_none());
}

#[tokio::test]
async fn compaction_persistence_post_marker_failure_completes_with_warning() {
    let directory = tempfile::tempdir().unwrap();
    let sid = SessionId::from("runtime-post-marker-failure");
    let replay = seeded_file_store(directory.path(), &sid).await;
    let store = Arc::new(
        FileEventStore::open_with_fault_injector(
            directory.path(),
            RuntimeFault::once(FaultPoint::BeforeMetadataPublish),
        )
        .unwrap(),
    );
    let (driver, replacement) = successful_driver();
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(driver),
        store.clone(),
        SessionBootstrap { replay },
    );
    let mut events = session.subscribe();
    session.submit(manual_compaction()).await.unwrap();
    let _session_started = next_event(&mut events).await;
    let _compaction_started = next_event(&mut events).await;
    let completed = next_event(&mut events).await;
    let EventPayload::CompactionCompleted { warning, .. } = completed.payload else {
        panic!("expected reconciled compaction completion");
    };
    assert_eq!(warning.unwrap().code, "projection.write_failed");
    assert_eq!(
        store.replay(&sid).await.unwrap().projection.messages,
        replacement
    );
}

#[tokio::test]
async fn compaction_persistence_replay_failure_stops_without_false_completion() {
    let sid = SessionId::from("runtime-reconciliation-failure");
    let store = Arc::new(ReconciliationFailStore {
        inner: MemoryEventStore::new(),
        fail_replay: AtomicBool::new(false),
    });
    let (driver, _) = successful_driver();
    let session = spawn_session_with_store(sid.clone(), Arc::new(driver), store, bootstrap(&sid));
    let mut events = session.subscribe();
    session.submit(manual_compaction()).await.unwrap();
    let _session_started = next_event(&mut events).await;
    let _compaction_started = next_event(&mut events).await;
    let failed = next_event(&mut events).await;
    let EventPayload::CompactionFailed { error, .. } = failed.payload else {
        panic!("expected reconciliation failure");
    };
    assert_eq!(error.code, "compaction.reconciliation_failed");
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("must not run"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert!(matches!(
        error.code.as_str(),
        "runtime.command_bus_closed" | "runtime.reply_bus_closed"
    ));
}
