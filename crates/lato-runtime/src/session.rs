// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values

use crate::driver::{
    CompactionControl, CompactionRequest, DriverEvent, DriverMessage, TurnControl, TurnDriver,
    TurnEventEmitter, TurnRequest,
};
use lato_core::{
    AgentError, CancelReason, Command, CompactSession, CompactionError, CompactionId,
    CompactionPolicy, ErrorCategory, EventEnvelope, EventId, EventPayload, JOURNAL_SCHEMA_VERSION,
    JournalDurability, JournalEnvelope, JournalError, JournalRecord, JournalRecordId,
    JournalReplay, Retryability, SessionId, SessionMachine, SessionPhase, SessionStore,
    StartDecision, StartTurn, TransitionError, TurnId, UserInput,
};
use lato_store::MemoryEventStore;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_COMPACTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct SessionHandle {
    command_tx: mpsc::Sender<SubmittedCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
}

impl SessionHandle {
    pub async fn submit(&self, command: Command) -> Result<(), AgentError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(SubmittedCommand { command, reply_tx })
            .await
            .map_err(|_| bus_closed("runtime.command_bus_closed", "runtime command bus closed"))?;
        reply_rx
            .await
            .map_err(|_| bus_closed("runtime.reply_bus_closed", "runtime reply bus closed"))?
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_tx.subscribe()
    }
}

pub fn spawn_session(session_id: SessionId, driver: Arc<dyn TurnDriver>) -> SessionHandle {
    let store: Arc<dyn SessionStore> = Arc::new(MemoryEventStore::new());
    spawn_session_with_store(
        session_id.clone(),
        driver,
        store,
        SessionBootstrap {
            replay: JournalReplay::empty(session_id),
        },
    )
}

#[derive(Clone)]
pub struct SessionBootstrap {
    pub replay: JournalReplay,
}

pub fn spawn_session_with_store(
    session_id: SessionId,
    driver: Arc<dyn TurnDriver>,
    store: Arc<dyn SessionStore>,
    bootstrap: SessionBootstrap,
) -> SessionHandle {
    debug_assert_eq!(bootstrap.replay.projection.session_id, session_id);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
    let session = SessionLoop::new(
        session_id,
        driver,
        store,
        bootstrap,
        command_rx,
        event_tx.clone(),
    );
    tokio::spawn(session.run());
    SessionHandle {
        command_tx,
        event_tx,
    }
}

struct SubmittedCommand {
    command: Command,
    reply_tx: oneshot::Sender<Result<(), AgentError>>,
}

struct ActiveRuntimeTurn {
    id: TurnId,
    cancellation: CancellationToken,
    steering: mpsc::UnboundedSender<UserInput>,
    cancel_reason: Option<CancelReason>,
    task: JoinHandle<()>,
}

struct ActiveRuntimeCompaction {
    id: CompactionId,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

struct PendingStart {
    id: TurnId,
    start: StartTurn,
}

struct SessionLoop {
    session_id: SessionId,
    driver: Arc<dyn TurnDriver>,
    store: Arc<dyn SessionStore>,
    machine: SessionMachine,
    command_rx: mpsc::Receiver<SubmittedCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
    driver_tx: mpsc::UnboundedSender<DriverMessage>,
    driver_rx: mpsc::UnboundedReceiver<DriverMessage>,
    active: Option<ActiveRuntimeTurn>,
    active_compaction: Option<ActiveRuntimeCompaction>,
    pending_start: Option<PendingStart>,
    sequence: u64,
    journal_sequence: u64,
    journal_exists: bool,
    started_emitted: bool,
}

impl SessionLoop {
    fn new(
        session_id: SessionId,
        driver: Arc<dyn TurnDriver>,
        store: Arc<dyn SessionStore>,
        bootstrap: SessionBootstrap,
        command_rx: mpsc::Receiver<SubmittedCommand>,
        event_tx: broadcast::Sender<EventEnvelope>,
    ) -> Self {
        let (driver_tx, driver_rx) = mpsc::unbounded_channel();
        Self {
            session_id,
            driver,
            store,
            machine: SessionMachine::new(),
            command_rx,
            event_tx,
            driver_tx,
            driver_rx,
            active: None,
            active_compaction: None,
            pending_start: None,
            sequence: 0,
            journal_sequence: bootstrap.replay.projection.next_journal_sequence,
            journal_exists: bootstrap.replay.exists,
            started_emitted: false,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        let _ = self.shutdown().await;
                        break;
                    };
                    let result = match self.emit_session_started_once().await {
                        Ok(()) => self.handle_command(command.command).await,
                        Err(error) => Err(error),
                    };
                    let _ = command.reply_tx.send(result);
                    if matches!(self.machine.phase(), SessionPhase::Stopped) {
                        break;
                    }
                }
                message = self.driver_rx.recv() => {
                    let Some(message) = message else {
                        let _ = self.shutdown().await;
                        break;
                    };
                    self.handle_driver_message(message).await;
                    if matches!(self.machine.phase(), SessionPhase::Stopped) {
                        break;
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, command: Command) -> Result<(), AgentError> {
        match command {
            Command::StartTurn(start) => self.request_start(start).await,
            Command::SteerTurn(input) => {
                let active = self.active.as_ref().ok_or_else(|| {
                    invalid_state("runtime.no_active_turn", "cannot steer an idle session")
                })?;
                active
                    .steering
                    .send(input)
                    .map_err(|_| bus_closed("runtime.steer_bus_closed", "turn steering bus closed"))
            }
            Command::CancelTurn { turn_id } => {
                self.machine
                    .request_cancel(&turn_id)
                    .map_err(transition_error)?;
                let active = self.active.as_mut().ok_or_else(|| {
                    invalid_state("runtime.no_active_turn", "cannot cancel an idle session")
                })?;
                if active.id != turn_id {
                    return Err(invalid_state(
                        "runtime.turn_identity_mismatch",
                        "state machine and runtime turn IDs differ",
                    ));
                }
                active.cancel_reason = Some(CancelReason::User);
                active.cancellation.cancel();
                Ok(())
            }
            Command::CompactSession(request) => self.request_compaction(request).await,
            Command::CancelCompaction { compaction_id } => {
                self.machine
                    .request_compaction_cancel(&compaction_id)
                    .map_err(transition_error)?;
                let active = self.active_compaction.as_ref().ok_or_else(|| {
                    invalid_state(
                        "compaction.not_active",
                        "cannot cancel an inactive compaction",
                    )
                })?;
                if active.id != compaction_id {
                    return Err(invalid_state(
                        "compaction.not_active",
                        "the requested compaction is not active",
                    ));
                }
                active.cancellation.cancel();
                Ok(())
            }
            Command::Shutdown => self.shutdown().await,
        }
    }

    async fn request_compaction(&mut self, request: CompactSession) -> Result<(), AgentError> {
        let compaction_id = next_compaction_id();
        let previous_machine = self.machine.clone();
        self.machine
            .request_compaction(compaction_id.clone())
            .map_err(|error| match error {
                TransitionError::TurnAlreadyActive => CompactionError::ActiveTurn.into(),
                other => transition_error(other),
            })?;

        let messages = match self.driver.history_snapshot().await {
            Ok(messages) => messages,
            Err(error) => {
                self.machine = previous_machine;
                return Err(error);
            }
        };
        if let Err(error) = self
            .commit(
                None,
                JournalRecord::CompactionRequested {
                    compaction_id: compaction_id.clone(),
                    trigger: request.trigger,
                    user_context: request.user_context.clone(),
                },
                JournalDurability::SyncData,
            )
            .await
        {
            self.machine = previous_machine;
            return Err(error);
        }

        let cancellation = CancellationToken::new();
        let control = CompactionControl {
            cancellation: cancellation.clone(),
        };
        let driver_request = CompactionRequest {
            compaction_id: compaction_id.clone(),
            request: request.clone(),
            messages,
            policy: CompactionPolicy::default(),
        };
        let driver = self.driver.clone();
        let driver_tx = self.driver_tx.clone();
        let finished_id = compaction_id.clone();
        let task = tokio::spawn(async move {
            let result = driver.compact(driver_request, control).await;
            let _ = driver_tx.send(DriverMessage::CompactionFinished {
                compaction_id: finished_id,
                result,
            });
        });
        self.active_compaction = Some(ActiveRuntimeCompaction {
            id: compaction_id.clone(),
            cancellation,
            task,
        });
        self.emit(
            None,
            EventPayload::CompactionStarted {
                compaction_id,
                trigger: request.trigger,
            },
        );
        Ok(())
    }

    async fn request_start(&mut self, start: StartTurn) -> Result<(), AgentError> {
        let requested_id = next_turn_id();
        let previous_machine = self.machine.clone();
        match self
            .machine
            .request_start(requested_id.clone(), start.behavior)
            .map_err(transition_error)?
        {
            StartDecision::StartNow => {
                if let Err(error) = self.launch(requested_id, start).await {
                    self.machine = previous_machine;
                    return Err(error);
                }
                Ok(())
            }
            StartDecision::CancelThenStart { active, pending } => {
                let current = self.active.as_mut().ok_or_else(|| {
                    invalid_state(
                        "runtime.active_turn_missing",
                        "state machine has no runtime turn",
                    )
                })?;
                if current.id != active {
                    return Err(invalid_state(
                        "runtime.turn_identity_mismatch",
                        "state machine and runtime turn IDs differ",
                    ));
                }
                current.cancel_reason = Some(CancelReason::Replaced);
                current.cancellation.cancel();
                self.pending_start = Some(PendingStart { id: pending, start });
                Ok(())
            }
        }
    }

    async fn launch(&mut self, turn_id: TurnId, start: StartTurn) -> Result<(), AgentError> {
        if self.active.is_some() {
            return Err(invalid_state(
                "runtime.active_turn_exists",
                "cannot launch a second foreground turn",
            ));
        }
        self.commit(
            Some(turn_id.clone()),
            JournalRecord::TurnInputAccepted {
                input: start.input.clone(),
            },
            JournalDurability::Flush,
        )
        .await?;
        let cancellation = CancellationToken::new();
        let (steering, steering_rx) = mpsc::unbounded_channel();
        let driver = self.driver.clone();
        let driver_tx = self.driver_tx.clone();
        let emitter = TurnEventEmitter {
            turn_id: turn_id.clone(),
            tx: driver_tx.clone(),
        };
        let request = TurnRequest {
            turn_id: turn_id.clone(),
            input: start.input,
        };
        let control = TurnControl {
            cancellation: cancellation.clone(),
            steering: steering_rx,
        };
        let finished_turn_id = turn_id.clone();
        let task = tokio::spawn(async move {
            let result = driver.run(request, control, emitter).await;
            let _ = driver_tx.send(DriverMessage::Finished {
                turn_id: finished_turn_id,
                result,
            });
        });
        self.active = Some(ActiveRuntimeTurn {
            id: turn_id.clone(),
            cancellation,
            steering,
            cancel_reason: None,
            task,
        });
        self.emit(Some(turn_id), EventPayload::TurnStarted);
        Ok(())
    }

    async fn handle_driver_message(&mut self, message: DriverMessage) {
        match message {
            DriverMessage::LiveEvent { turn_id, event } => {
                if self.active.as_ref().map(|active| &active.id) != Some(&turn_id) {
                    return;
                }
                let payload = match event {
                    DriverEvent::ModelDelta(text) => EventPayload::ModelDelta { text },
                    DriverEvent::ReasoningDelta(text) => EventPayload::ReasoningDelta { text },
                };
                self.emit(Some(turn_id), payload);
            }
            DriverMessage::Commit {
                turn_id,
                record,
                durability,
                ack,
            } => {
                let result = if self.active.as_ref().map(|active| &active.id) == Some(&turn_id) {
                    self.commit(Some(turn_id), record, durability).await
                } else {
                    Err(invalid_state(
                        "runtime.stale_turn_commit",
                        "cannot commit canonical state for an inactive turn",
                    ))
                };
                let _ = ack.send(result);
            }
            DriverMessage::Finished { turn_id, result } => {
                let Some(active) = self.active.take() else {
                    return;
                };
                if active.id != turn_id {
                    self.active = Some(active);
                    return;
                }
                if self.machine.finish(&turn_id).is_err() {
                    self.machine.stop();
                    return;
                }
                if let Some(reason) = active.cancel_reason {
                    if self
                        .commit(
                            Some(turn_id.clone()),
                            JournalRecord::TurnCancelled { reason },
                            JournalDurability::SyncData,
                        )
                        .await
                        .is_err()
                    {
                        self.pending_start = None;
                        self.machine.stop();
                        return;
                    }
                    self.emit(Some(turn_id), EventPayload::TurnCancelled { reason });
                } else {
                    match result {
                        Ok(output) => {
                            if self
                                .commit(
                                    Some(turn_id.clone()),
                                    JournalRecord::TurnCompleted {
                                        output: output.clone(),
                                    },
                                    JournalDurability::SyncData,
                                )
                                .await
                                .is_err()
                            {
                                self.pending_start = None;
                                self.machine.stop();
                                return;
                            }
                            self.emit(Some(turn_id), EventPayload::TurnCompleted(output));
                        }
                        Err(error) => {
                            if self
                                .commit(
                                    Some(turn_id.clone()),
                                    JournalRecord::TurnFailed {
                                        error: error.clone(),
                                    },
                                    JournalDurability::SyncData,
                                )
                                .await
                                .is_err()
                            {
                                self.pending_start = None;
                                self.machine.stop();
                                return;
                            }
                            self.emit(Some(turn_id), EventPayload::TurnFailed { error });
                        }
                    }
                }
                if let Some(pending) = self.pending_start.take() {
                    match self
                        .machine
                        .request_start(pending.id.clone(), pending.start.behavior)
                    {
                        Ok(StartDecision::StartNow) => {
                            if let Err(error) = self.launch(pending.id, pending.start).await {
                                self.emit(None, EventPayload::TurnFailed { error });
                            }
                        }
                        Ok(StartDecision::CancelThenStart { .. }) => {
                            self.emit(
                                Some(pending.id),
                                EventPayload::TurnFailed {
                                    error: invalid_state(
                                        "runtime.invalid_transition",
                                        "pending turn unexpectedly requested another replacement",
                                    ),
                                },
                            );
                        }
                        Err(error) => self.emit(
                            Some(pending.id),
                            EventPayload::TurnFailed {
                                error: transition_error(error),
                            },
                        ),
                    }
                }
            }
            DriverMessage::CompactionFinished {
                compaction_id,
                result,
            } => {
                self.handle_compaction_finished(compaction_id, result).await;
            }
        }
    }

    async fn handle_compaction_finished(
        &mut self,
        compaction_id: CompactionId,
        result: Result<lato_core::CompactionCandidate, AgentError>,
    ) {
        let Some(active) = self.active_compaction.take() else {
            return;
        };
        if active.id != compaction_id {
            self.active_compaction = Some(active);
            return;
        }
        let cancel_requested = self
            .machine
            .active_compaction()
            .is_some_and(|state| state.cancel_requested);
        if cancel_requested {
            if self
                .commit(
                    None,
                    JournalRecord::CompactionCancelled {
                        compaction_id: compaction_id.clone(),
                    },
                    JournalDurability::SyncData,
                )
                .await
                .is_err()
            {
                self.machine.stop();
                return;
            }
            let _ = self.machine.finish_compaction(&compaction_id);
            self.emit(None, EventPayload::CompactionCancelled { compaction_id });
            return;
        }

        let error = match result {
            Ok(_) => AgentError::new(
                "compaction.persistence_not_connected",
                ErrorCategory::InternalInvariant,
                "compaction candidate is ready but persistence is not connected",
                Retryability::Never,
            ),
            Err(error) => error,
        };
        if self
            .commit(
                None,
                JournalRecord::CompactionFailed {
                    compaction_id: compaction_id.clone(),
                    error_code: error.code.clone(),
                },
                JournalDurability::SyncData,
            )
            .await
            .is_err()
        {
            self.machine.stop();
            return;
        }
        let _ = self.machine.finish_compaction(&compaction_id);
        self.emit(
            None,
            EventPayload::CompactionFailed {
                compaction_id,
                error,
            },
        );
    }

    async fn emit_session_started_once(&mut self) -> Result<(), AgentError> {
        if !self.started_emitted {
            if !self.journal_exists {
                self.commit(
                    None,
                    JournalRecord::SessionStarted,
                    JournalDurability::SyncData,
                )
                .await?;
                self.journal_exists = true;
            }
            self.started_emitted = true;
            self.emit(None, EventPayload::SessionStarted);
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), AgentError> {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            active.task.abort();
            self.commit(
                Some(active.id.clone()),
                JournalRecord::TurnCancelled {
                    reason: CancelReason::Shutdown,
                },
                JournalDurability::SyncData,
            )
            .await?;
            self.emit(
                Some(active.id),
                EventPayload::TurnCancelled {
                    reason: CancelReason::Shutdown,
                },
            );
        }
        if let Some(active) = self.active_compaction.take() {
            active.cancellation.cancel();
            active.task.abort();
            self.commit(
                None,
                JournalRecord::CompactionCancelled {
                    compaction_id: active.id.clone(),
                },
                JournalDurability::SyncData,
            )
            .await?;
            self.emit(
                None,
                EventPayload::CompactionCancelled {
                    compaction_id: active.id,
                },
            );
        }
        self.machine.stop();
        self.commit(
            None,
            JournalRecord::SessionStopped,
            JournalDurability::SyncData,
        )
        .await?;
        self.emit(None, EventPayload::SessionStopped);
        self.store
            .shutdown(&self.session_id)
            .await
            .map_err(journal_error)?;
        Ok(())
    }

    async fn commit(
        &mut self,
        turn_id: Option<TurnId>,
        record: JournalRecord,
        durability: JournalDurability,
    ) -> Result<(), AgentError> {
        let sequence = self.journal_sequence;
        let envelope = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            record_id: JournalRecordId::from(format!("{}-journal-{sequence}", self.session_id)),
            session_id: self.session_id.clone(),
            turn_id,
            journal_sequence: sequence,
            timestamp_ms: now_ms(),
            record,
        };
        self.store
            .append(envelope, durability)
            .await
            .map_err(journal_error)?;
        self.journal_sequence += 1;
        Ok(())
    }

    fn emit(&mut self, turn_id: Option<TurnId>, payload: EventPayload) {
        self.sequence += 1;
        let sequence = self.sequence;
        let envelope = EventEnvelope {
            schema_version: lato_core::EVENT_SCHEMA_VERSION,
            event_id: EventId::from(format!("{}-event-{sequence}", self.session_id)),
            session_id: self.session_id.clone(),
            turn_id,
            parent_event_id: None,
            sequence,
            timestamp_ms: now_ms(),
            payload,
        };
        let _ = self.event_tx.send(envelope);
    }
}

fn next_turn_id() -> TurnId {
    TurnId::from(format!(
        "turn-{}",
        NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn next_compaction_id() -> CompactionId {
    CompactionId::from(format!(
        "compaction-{}",
        NEXT_COMPACTION_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn transition_error(error: TransitionError) -> AgentError {
    match error {
        TransitionError::CompactionAlreadyActive => CompactionError::AlreadyActive.into(),
        TransitionError::NotActiveCompaction => invalid_state(
            "compaction.not_active",
            "the requested compaction is not active",
        ),
        other => invalid_state("runtime.invalid_transition", other.to_string()),
    }
}

fn invalid_state(code: &str, message: impl Into<String>) -> AgentError {
    AgentError::new(
        code,
        ErrorCategory::InvalidInput,
        message,
        Retryability::RequiresDecision,
    )
}

fn bus_closed(code: &str, message: &str) -> AgentError {
    AgentError::new(
        code,
        ErrorCategory::InternalInvariant,
        message,
        Retryability::Never,
    )
}

fn journal_error(error: JournalError) -> AgentError {
    AgentError::new(
        error.code(),
        ErrorCategory::Storage,
        error.to_string(),
        error.retryability(),
    )
}
