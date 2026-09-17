// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values
// Compaction orchestration derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs

use crate::driver::{
    AutomaticCompactionOutcome, AutomaticCompactionRequest, CompactionControl, CompactionRequest,
    DriverEvent, DriverMessage, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
};
use lato_core::{
    AgentError, CancelReason, Command, CompactSession, CompactionError, CompactionId,
    CompactionPolicy, ErrorCategory, EventEnvelope, EventId, EventPayload,
    HistoryReplacementReason, JOURNAL_SCHEMA_VERSION, JournalDurability, JournalEnvelope,
    JournalError, JournalRecord, JournalRecordId, JournalReplay, ProjectionError, Retryability,
    SessionId, SessionMachine, SessionPhase, SessionStore, StartDecision, StartTurn,
    TransitionError, TurnId, UserInput,
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
    started: tokio::sync::watch::Receiver<Option<Result<(), AgentError>>>,
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

    /// Waits until the session loop has finished its startup journal bootstrap
    /// (writing the sole first record when the journal does not exist yet) and
    /// reports whether the journal contract still holds.
    pub async fn await_started(&self) -> Result<(), AgentError> {
        let mut started = self.started.clone();
        loop {
            if let Some(result) = started.borrow().as_ref() {
                return result.clone();
            }
            if started.changed().await.is_err() {
                return Err(runtime_stopped_error());
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_tx.subscribe()
    }
}

fn runtime_stopped_error() -> AgentError {
    AgentError::new(
        "runtime.session_stopped",
        ErrorCategory::InvalidInput,
        "session loop stopped before journal startup completed",
        Retryability::Never,
    )
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
    let (started_tx, started_rx) = tokio::sync::watch::channel(None);
    let session = SessionLoop::new(
        session_id,
        driver,
        store,
        bootstrap,
        command_rx,
        event_tx.clone(),
        Some(started_tx),
    );
    tokio::spawn(session.run());
    SessionHandle {
        command_tx,
        event_tx,
        started: started_rx,
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
    owner_turn: Option<TurnId>,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
    reply: Option<oneshot::Sender<Result<AutomaticCompactionOutcome, AgentError>>>,
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
    current_checkpoint_id: Option<String>,
    started_tx: Option<tokio::sync::watch::Sender<Option<Result<(), AgentError>>>>,
}

impl SessionLoop {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: SessionId,
        driver: Arc<dyn TurnDriver>,
        store: Arc<dyn SessionStore>,
        bootstrap: SessionBootstrap,
        command_rx: mpsc::Receiver<SubmittedCommand>,
        event_tx: broadcast::Sender<EventEnvelope>,
        started_tx: Option<tokio::sync::watch::Sender<Option<Result<(), AgentError>>>>,
    ) -> Self {
        let (driver_tx, driver_rx) = mpsc::unbounded_channel();
        let current_checkpoint_id = bootstrap.replay.projection.active_checkpoint_id.clone();
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
            current_checkpoint_id,
            started_tx,
        }
    }

    /// Single ownership of the journal's first record: the session loop writes
    /// `SessionStarted` at sequence 0 exactly once when no journal exists yet.
    /// Callers must never append it themselves; they seed the loop via the
    /// bootstrap replay instead.
    async fn ensure_journal_started(&mut self) -> Result<(), AgentError> {
        if !self.journal_exists {
            self.commit(
                None,
                JournalRecord::SessionStarted,
                JournalDurability::SyncData,
            )
            .await?;
            self.journal_exists = true;
        }
        Ok(())
    }

    async fn run(mut self) {
        let startup = self.ensure_journal_started().await;
        if let Some(started_tx) = self.started_tx.take() {
            let _ = started_tx.send(Some(startup.clone()));
        }
        if startup.is_err() {
            return;
        }
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        // The owner dropped the handle without an explicit
                        // shutdown command. Treat this like an abrupt process
                        // exit: stop without appending terminal records so a
                        // concurrently reopening reader never races an
                        // in-flight tail commit. Explicit `Command::Shutdown`
                        // remains the awaited path that commits `SessionStopped`.
                        self.stop_without_commit();
                        break;
                    };
                    self.emit_session_started_once();
                    let result = self.handle_command(command.command).await;
                    let _ = command.reply_tx.send(result);
                    if matches!(self.machine.phase(), SessionPhase::Stopped) {
                        break;
                    }
                }
                message = self.driver_rx.recv() => {
                    let Some(message) = message else {
                        self.stop_without_commit();
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

    /// Stops the loop on an ungraceful owner drop: in-flight work is cancelled
    /// but the journal is left exactly as last durably committed, so replay
    /// stays crash-consistent and no tail commit can collide with a session
    /// that reopened the journal.
    fn stop_without_commit(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            active.task.abort();
        }
        if let Some(active) = self.active_compaction.take() {
            active.cancellation.cancel();
            active.task.abort();
        }
        self.machine.stop();
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
                if let Some(compaction) = self.active_compaction.as_ref()
                    && compaction.owner_turn.as_ref() == Some(&turn_id)
                {
                    compaction.cancellation.cancel();
                }
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
            Command::SelectModel {
                selection,
                model_family,
                context_window,
            } => {
                if !matches!(self.machine.phase(), SessionPhase::Idle) {
                    return Err(invalid_state(
                        "runtime.session_busy",
                        "cannot switch models while the session is active",
                    ));
                }
                self.commit(
                    None,
                    JournalRecord::ModelSelected {
                        selection,
                        model_family,
                        context_window,
                    },
                    JournalDurability::SyncData,
                )
                .await
            }
            Command::AdoptPluginSnapshot { summary } => {
                if !matches!(self.machine.phase(), SessionPhase::Idle) {
                    return Err(invalid_state(
                        "plugin.snapshot_session_busy",
                        "plugin snapshot adoption requires an idle session",
                    ));
                }
                self.commit(
                    None,
                    JournalRecord::PluginSnapshotAdopted {
                        summary: summary.clone(),
                    },
                    JournalDurability::Flush,
                )
                .await?;
                self.emit(None, EventPayload::PluginSnapshotAdopted { summary });
                Ok(())
            }
            Command::RecordExtensionAudit { audit } => {
                let turn_id = self.active.as_ref().map(|active| active.id.clone());
                self.commit(
                    turn_id,
                    JournalRecord::ExtensionAudit { audit },
                    JournalDurability::Flush,
                )
                .await
            }
            Command::RecordPlanModeEvent { event } => {
                let record = match &event {
                    lato_core::PlanModeJournalEvent::Transitioned {
                        activation,
                        from,
                        to,
                        command,
                    } => JournalRecord::PlanModeTransitioned {
                        activation: *activation,
                        from: *from,
                        to: *to,
                        command: *command,
                    },
                    lato_core::PlanModeJournalEvent::ApprovalRecorded {
                        activation,
                        generation,
                        content_hash,
                        approver,
                        approved_at_ms,
                    } => JournalRecord::PlanApprovalRecorded {
                        activation: *activation,
                        generation: *generation,
                        content_hash: content_hash.clone(),
                        approver: approver.clone(),
                        approved_at_ms: *approved_at_ms,
                    },
                    lato_core::PlanModeJournalEvent::ApprovalRevoked {
                        activation,
                        generation,
                        reason,
                    } => JournalRecord::PlanApprovalRevoked {
                        activation: *activation,
                        generation: *generation,
                        reason: reason.clone(),
                    },
                };
                let turn_id = self.active.as_ref().map(|active| active.id.clone());
                self.commit(turn_id, record, JournalDurability::Flush).await
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
            two_pass: None,
            prior_model_attempts: 0,
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
            owner_turn: None,
            cancellation,
            task,
            reply: None,
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
                    DriverEvent::ContextUsageUpdated(usage) => {
                        EventPayload::ContextUsageUpdated { usage }
                    }
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
            DriverMessage::AutomaticCompactionRequested {
                turn_id,
                request,
                reply,
            } => {
                self.request_automatic_compaction(turn_id, request, reply)
                    .await;
            }
            DriverMessage::PrefireCompactionRequested {
                turn_id,
                request,
                reply,
            } => {
                let Some(active) = self.active.as_ref() else {
                    let _ = reply.send(Err(invalid_state(
                        "runtime.stale_turn_prefire",
                        "cannot prefire compaction for an inactive turn",
                    )));
                    return;
                };
                if active.id != turn_id {
                    let _ = reply.send(Err(invalid_state(
                        "runtime.stale_turn_prefire",
                        "cannot prefire compaction for a different turn",
                    )));
                    return;
                }
                let driver = self.driver.clone();
                let cancellation = active.cancellation.child_token();
                tokio::spawn(async move {
                    let result = driver
                        .prefire_compaction(request, CompactionControl { cancellation })
                        .await;
                    let _ = reply.send(result);
                });
            }
        }
    }

    async fn request_automatic_compaction(
        &mut self,
        turn_id: TurnId,
        request: AutomaticCompactionRequest,
        reply: oneshot::Sender<Result<AutomaticCompactionOutcome, AgentError>>,
    ) {
        if self.active.as_ref().map(|active| &active.id) != Some(&turn_id) {
            let _ = reply.send(Err(invalid_state(
                "runtime.stale_turn_compaction",
                "cannot compact history for an inactive turn",
            )));
            return;
        }
        let compaction_id = next_compaction_id();
        let previous_machine = self.machine.clone();
        if let Err(error) = self
            .machine
            .request_turn_compaction(&turn_id, compaction_id.clone())
        {
            let _ = reply.send(Err(transition_error(error)));
            return;
        }
        if let Err(error) = self
            .commit(
                Some(turn_id.clone()),
                JournalRecord::CompactionRequested {
                    compaction_id: compaction_id.clone(),
                    trigger: request.trigger,
                    user_context: None,
                },
                JournalDurability::SyncData,
            )
            .await
        {
            self.machine = previous_machine;
            let _ = reply.send(Err(error));
            return;
        }

        let cancellation = CancellationToken::new();
        let control = CompactionControl {
            cancellation: cancellation.clone(),
        };
        let driver_request = CompactionRequest {
            compaction_id: compaction_id.clone(),
            request: CompactSession {
                user_context: None,
                trigger: request.trigger,
            },
            messages: request.messages,
            policy: CompactionPolicy::default(),
            two_pass: request.two_pass,
            prior_model_attempts: request.prior_model_attempts,
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
            owner_turn: Some(turn_id.clone()),
            cancellation,
            task,
            reply: Some(reply),
        });
        self.emit(
            Some(turn_id),
            EventPayload::CompactionStarted {
                compaction_id,
                trigger: request.trigger,
            },
        );
    }

    async fn handle_compaction_finished(
        &mut self,
        compaction_id: CompactionId,
        result: Result<lato_core::CompactionCandidate, AgentError>,
    ) {
        let Some(mut active) = self.active_compaction.take() else {
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
            let cancellation_error: AgentError = CompactionError::Cancelled.into();
            if let Err(error) = self
                .commit(
                    active.owner_turn.clone(),
                    JournalRecord::CompactionCancelled {
                        compaction_id: compaction_id.clone(),
                    },
                    JournalDurability::SyncData,
                )
                .await
            {
                self.machine.stop();
                if let Some(reply) = active.reply.take() {
                    let _ = reply.send(Err(error));
                }
                return;
            }
            self.finish_runtime_compaction(active.owner_turn.as_ref(), &compaction_id);
            self.emit(
                active.owner_turn.clone(),
                EventPayload::CompactionCancelled { compaction_id },
            );
            if let Some(reply) = active.reply.take() {
                let _ = reply.send(Err(cancellation_error));
            }
            return;
        }

        if let Some(owner_turn) = active.owner_turn.clone() {
            self.handle_automatic_compaction_finished(active, owner_turn, result)
                .await;
            return;
        }

        let error = match result {
            Ok(candidate) => {
                self.persist_compaction(candidate).await;
                return;
            }
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

    async fn handle_automatic_compaction_finished(
        &mut self,
        mut active: ActiveRuntimeCompaction,
        owner_turn: TurnId,
        result: Result<lato_core::CompactionCandidate, AgentError>,
    ) {
        match result {
            Ok(candidate) => {
                let outcome = self
                    .persist_automatic_compaction(&owner_turn, candidate)
                    .await;
                if let Some(reply) = active.reply.take() {
                    let _ = reply.send(outcome);
                }
            }
            Err(error) => {
                let commit_result = self
                    .commit(
                        Some(owner_turn.clone()),
                        JournalRecord::CompactionFailed {
                            compaction_id: active.id.clone(),
                            error_code: error.code.clone(),
                        },
                        JournalDurability::SyncData,
                    )
                    .await;
                if let Err(storage_error) = commit_result {
                    self.machine.stop();
                    if let Some(reply) = active.reply.take() {
                        let _ = reply.send(Err(storage_error));
                    }
                    return;
                }
                self.finish_runtime_compaction(Some(&owner_turn), &active.id);
                self.emit(
                    Some(owner_turn),
                    EventPayload::CompactionFailed {
                        compaction_id: active.id,
                        error: error.clone(),
                    },
                );
                if let Some(reply) = active.reply.take() {
                    let outcome = if automatic_compaction_must_stop(&error) {
                        Err(error)
                    } else {
                        Ok(AutomaticCompactionOutcome::ContinueUnchanged { error })
                    };
                    let _ = reply.send(outcome);
                }
            }
        }
    }

    async fn persist_automatic_compaction(
        &mut self,
        owner_turn: &TurnId,
        candidate: lato_core::CompactionCandidate,
    ) -> Result<AutomaticCompactionOutcome, AgentError> {
        let before_checkpoint = self.current_checkpoint_id.clone();
        match self
            .store
            .replace_history(
                &self.session_id,
                candidate.messages.clone(),
                HistoryReplacementReason::ContextCompaction,
            )
            .await
        {
            Ok(metadata) => {
                self.journal_sequence = metadata.last_journal_sequence.saturating_add(1);
                self.current_checkpoint_id = metadata.active_checkpoint_id.clone();
                let Some(checkpoint_id) = metadata.active_checkpoint_id else {
                    return self.stop_automatic_after_reconciliation_failure(
                        owner_turn,
                        candidate.compaction_id,
                        "replacement returned no active checkpoint",
                    );
                };
                self.finish_runtime_compaction(Some(owner_turn), &candidate.compaction_id);
                self.emit(
                    Some(owner_turn.clone()),
                    EventPayload::CompactionCompleted {
                        compaction_id: candidate.compaction_id,
                        before: candidate.before,
                        after: candidate.after,
                        checkpoint_id,
                        warning: None,
                    },
                );
                Ok(AutomaticCompactionOutcome::Compacted(candidate.messages))
            }
            Err(error) => {
                self.reconcile_automatic_compaction_failure(
                    owner_turn,
                    candidate,
                    before_checkpoint,
                    error,
                )
                .await
            }
        }
    }

    async fn reconcile_automatic_compaction_failure(
        &mut self,
        owner_turn: &TurnId,
        candidate: lato_core::CompactionCandidate,
        before_checkpoint: Option<String>,
        replacement_error: ProjectionError,
    ) -> Result<AutomaticCompactionOutcome, AgentError> {
        let warning = projection_error(replacement_error);
        let replay = match self.store.replay(&self.session_id).await {
            Ok(replay) => replay,
            Err(error) => {
                return self.stop_automatic_after_reconciliation_failure(
                    owner_turn,
                    candidate.compaction_id,
                    format!("replay after replacement error: {error}"),
                );
            }
        };
        self.journal_sequence = replay.projection.next_journal_sequence;
        let active_checkpoint = replay.projection.active_checkpoint_id.clone();
        if active_checkpoint != before_checkpoint {
            let Some(checkpoint_id) = active_checkpoint.clone() else {
                return self.stop_automatic_after_reconciliation_failure(
                    owner_turn,
                    candidate.compaction_id,
                    "replacement changed checkpoint state without an active checkpoint",
                );
            };
            self.current_checkpoint_id = active_checkpoint;
            self.finish_runtime_compaction(Some(owner_turn), &candidate.compaction_id);
            self.emit(
                Some(owner_turn.clone()),
                EventPayload::CompactionCompleted {
                    compaction_id: candidate.compaction_id,
                    before: candidate.before,
                    after: candidate.after,
                    checkpoint_id,
                    warning: Some(warning),
                },
            );
            return Ok(AutomaticCompactionOutcome::Compacted(
                replay.projection.messages,
            ));
        }

        let storage_error = warning.clone();
        if let Err(error) = self
            .commit(
                Some(owner_turn.clone()),
                JournalRecord::CompactionFailed {
                    compaction_id: candidate.compaction_id.clone(),
                    error_code: warning.code.clone(),
                },
                JournalDurability::SyncData,
            )
            .await
        {
            self.machine.stop();
            return Err(error);
        }
        self.finish_runtime_compaction(Some(owner_turn), &candidate.compaction_id);
        self.emit(
            Some(owner_turn.clone()),
            EventPayload::CompactionFailed {
                compaction_id: candidate.compaction_id,
                error: warning,
            },
        );
        Err(storage_error)
    }

    fn stop_automatic_after_reconciliation_failure(
        &mut self,
        owner_turn: &TurnId,
        compaction_id: CompactionId,
        message: impl Into<String>,
    ) -> Result<AutomaticCompactionOutcome, AgentError> {
        let error: AgentError = CompactionError::ReconciliationFailed {
            message: message.into(),
        }
        .into();
        self.machine.stop();
        self.emit(
            Some(owner_turn.clone()),
            EventPayload::CompactionFailed {
                compaction_id,
                error: error.clone(),
            },
        );
        Err(error)
    }

    fn finish_runtime_compaction(
        &mut self,
        owner_turn: Option<&TurnId>,
        compaction_id: &CompactionId,
    ) {
        if let Some(turn_id) = owner_turn {
            let _ = self.machine.finish_turn_compaction(turn_id, compaction_id);
        } else {
            let _ = self.machine.finish_compaction(compaction_id);
        }
    }

    async fn persist_compaction(&mut self, candidate: lato_core::CompactionCandidate) {
        let before_checkpoint = self.current_checkpoint_id.clone();
        match self
            .store
            .replace_history(
                &self.session_id,
                candidate.messages.clone(),
                HistoryReplacementReason::ContextCompaction,
            )
            .await
        {
            Ok(metadata) => {
                self.journal_sequence = metadata.last_journal_sequence.saturating_add(1);
                self.current_checkpoint_id = metadata.active_checkpoint_id.clone();
                let Some(checkpoint_id) = metadata.active_checkpoint_id else {
                    self.stop_after_reconciliation_failure(
                        candidate.compaction_id,
                        "replacement returned no active checkpoint",
                    );
                    return;
                };
                if let Err(error) = self
                    .driver
                    .install_history(candidate.messages.clone())
                    .await
                {
                    self.stop_after_reconciliation_failure(
                        candidate.compaction_id,
                        format!("install committed history: {error}"),
                    );
                    return;
                }
                let _ = self.machine.finish_compaction(&candidate.compaction_id);
                self.emit(
                    None,
                    EventPayload::CompactionCompleted {
                        compaction_id: candidate.compaction_id,
                        before: candidate.before,
                        after: candidate.after,
                        checkpoint_id,
                        warning: None,
                    },
                );
            }
            Err(error) => {
                self.reconcile_compaction_failure(candidate, before_checkpoint, error)
                    .await;
            }
        }
    }

    async fn reconcile_compaction_failure(
        &mut self,
        candidate: lato_core::CompactionCandidate,
        before_checkpoint: Option<String>,
        replacement_error: ProjectionError,
    ) {
        let warning = projection_error(replacement_error);
        let replay = match self.store.replay(&self.session_id).await {
            Ok(replay) => replay,
            Err(error) => {
                self.stop_after_reconciliation_failure(
                    candidate.compaction_id,
                    format!("replay after replacement error: {error}"),
                );
                return;
            }
        };
        self.journal_sequence = replay.projection.next_journal_sequence;
        let active_checkpoint = replay.projection.active_checkpoint_id.clone();
        if active_checkpoint != before_checkpoint {
            let Some(checkpoint_id) = active_checkpoint.clone() else {
                self.stop_after_reconciliation_failure(
                    candidate.compaction_id,
                    "replacement changed checkpoint state without an active checkpoint",
                );
                return;
            };
            if let Err(error) = self
                .driver
                .install_history(replay.projection.messages.clone())
                .await
            {
                self.stop_after_reconciliation_failure(
                    candidate.compaction_id,
                    format!("install reconciled history: {error}"),
                );
                return;
            }
            self.current_checkpoint_id = active_checkpoint;
            let _ = self.machine.finish_compaction(&candidate.compaction_id);
            self.emit(
                None,
                EventPayload::CompactionCompleted {
                    compaction_id: candidate.compaction_id,
                    before: candidate.before,
                    after: candidate.after,
                    checkpoint_id,
                    warning: Some(warning),
                },
            );
            return;
        }

        if self
            .commit(
                None,
                JournalRecord::CompactionFailed {
                    compaction_id: candidate.compaction_id.clone(),
                    error_code: warning.code.clone(),
                },
                JournalDurability::SyncData,
            )
            .await
            .is_err()
        {
            self.machine.stop();
            return;
        }
        let _ = self.machine.finish_compaction(&candidate.compaction_id);
        self.emit(
            None,
            EventPayload::CompactionFailed {
                compaction_id: candidate.compaction_id,
                error: warning,
            },
        );
    }

    fn stop_after_reconciliation_failure(
        &mut self,
        compaction_id: CompactionId,
        message: impl Into<String>,
    ) {
        let error: AgentError = CompactionError::ReconciliationFailed {
            message: message.into(),
        }
        .into();
        self.machine.stop();
        self.emit(
            None,
            EventPayload::CompactionFailed {
                compaction_id,
                error,
            },
        );
    }

    fn emit_session_started_once(&mut self) {
        if !self.started_emitted {
            self.started_emitted = true;
            self.emit(None, EventPayload::SessionStarted);
        }
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
            let mut task = active.task;
            if tokio::time::timeout(std::time::Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
            self.commit(
                active.owner_turn.clone(),
                JournalRecord::CompactionCancelled {
                    compaction_id: active.id.clone(),
                },
                JournalDurability::SyncData,
            )
            .await?;
            self.emit(
                active.owner_turn,
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

fn projection_error(error: ProjectionError) -> AgentError {
    AgentError::new(
        error.code(),
        ErrorCategory::Storage,
        error.to_string(),
        error.retryability(),
    )
}

fn automatic_compaction_must_stop(error: &AgentError) -> bool {
    let message = error.message.to_ascii_lowercase();
    error.code == "compaction.cancelled"
        || error.category == ErrorCategory::Storage
        || error.code == "model.auth"
        || message.contains("model.auth")
        || message.contains("http 401")
        || message.contains("http 403")
        || message.contains("oauth refresh failed")
}
