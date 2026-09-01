// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values

use crate::driver::{
    DriverEvent, DriverMessage, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
};
use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventEnvelope, EventId, EventPayload,
    Retryability, SessionId, SessionMachine, SessionPhase, StartDecision, StartTurn,
    TransitionError, TurnId, UserInput,
};
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
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
    let session = SessionLoop::new(session_id, driver, command_rx, event_tx.clone());
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

struct PendingStart {
    id: TurnId,
    start: StartTurn,
}

struct SessionLoop {
    session_id: SessionId,
    driver: Arc<dyn TurnDriver>,
    machine: SessionMachine,
    command_rx: mpsc::Receiver<SubmittedCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
    driver_tx: mpsc::UnboundedSender<DriverMessage>,
    driver_rx: mpsc::UnboundedReceiver<DriverMessage>,
    active: Option<ActiveRuntimeTurn>,
    pending_start: Option<PendingStart>,
    sequence: u64,
    started_emitted: bool,
}

impl SessionLoop {
    fn new(
        session_id: SessionId,
        driver: Arc<dyn TurnDriver>,
        command_rx: mpsc::Receiver<SubmittedCommand>,
        event_tx: broadcast::Sender<EventEnvelope>,
    ) -> Self {
        let (driver_tx, driver_rx) = mpsc::unbounded_channel();
        Self {
            session_id,
            driver,
            machine: SessionMachine::new(),
            command_rx,
            event_tx,
            driver_tx,
            driver_rx,
            active: None,
            pending_start: None,
            sequence: 0,
            started_emitted: false,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        self.shutdown();
                        break;
                    };
                    self.emit_session_started_once();
                    let result = self.handle_command(command.command);
                    let _ = command.reply_tx.send(result);
                    if matches!(self.machine.phase(), SessionPhase::Stopped) {
                        break;
                    }
                }
                message = self.driver_rx.recv() => {
                    let Some(message) = message else {
                        self.shutdown();
                        break;
                    };
                    self.handle_driver_message(message);
                    if matches!(self.machine.phase(), SessionPhase::Stopped) {
                        break;
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: Command) -> Result<(), AgentError> {
        match command {
            Command::StartTurn(start) => self.request_start(start),
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
            Command::Shutdown => {
                self.shutdown();
                Ok(())
            }
        }
    }

    fn request_start(&mut self, start: StartTurn) -> Result<(), AgentError> {
        let requested_id = next_turn_id();
        match self
            .machine
            .request_start(requested_id.clone(), start.behavior)
            .map_err(transition_error)?
        {
            StartDecision::StartNow => self.launch(requested_id, start),
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

    fn launch(&mut self, turn_id: TurnId, start: StartTurn) -> Result<(), AgentError> {
        if self.active.is_some() {
            return Err(invalid_state(
                "runtime.active_turn_exists",
                "cannot launch a second foreground turn",
            ));
        }
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

    fn handle_driver_message(&mut self, message: DriverMessage) {
        match message {
            DriverMessage::Event { turn_id, event } => {
                if self.active.as_ref().map(|active| &active.id) != Some(&turn_id) {
                    return;
                }
                let payload = match event {
                    DriverEvent::ModelDelta(text) => EventPayload::ModelDelta { text },
                    DriverEvent::ReasoningDelta(text) => EventPayload::ReasoningDelta { text },
                };
                self.emit(Some(turn_id), payload);
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
                    self.emit(
                        Some(turn_id),
                        EventPayload::TurnFailed {
                            error: invalid_state(
                                "runtime.invalid_transition",
                                "finished turn does not match the state machine",
                            ),
                        },
                    );
                    self.emit(None, EventPayload::SessionStopped);
                    return;
                }
                if let Some(reason) = active.cancel_reason {
                    self.emit(Some(turn_id), EventPayload::TurnCancelled { reason });
                } else {
                    match result {
                        Ok(output) => self.emit(Some(turn_id), EventPayload::TurnCompleted(output)),
                        Err(error) => self.emit(Some(turn_id), EventPayload::TurnFailed { error }),
                    }
                }
                if let Some(pending) = self.pending_start.take() {
                    match self
                        .machine
                        .request_start(pending.id.clone(), pending.start.behavior)
                    {
                        Ok(StartDecision::StartNow) => {
                            if let Err(error) = self.launch(pending.id, pending.start) {
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
        }
    }

    fn emit_session_started_once(&mut self) {
        if !self.started_emitted {
            self.started_emitted = true;
            self.emit(None, EventPayload::SessionStarted);
        }
    }

    fn shutdown(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            active.task.abort();
            self.emit(
                Some(active.id),
                EventPayload::TurnCancelled {
                    reason: CancelReason::Shutdown,
                },
            );
        }
        self.machine.stop();
        self.emit(None, EventPayload::SessionStopped);
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn transition_error(error: TransitionError) -> AgentError {
    invalid_state("runtime.invalid_transition", error.to_string())
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
