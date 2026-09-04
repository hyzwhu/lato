use crate::{HistoryItem, LegacyTurnDriver, ToolApproval, model_messages_to_history};
use lato_ai::{ModelStream, SwitchableModelPort, adapt_model_port};
use lato_core::{
    AgentError, CancelReason, Command, CompactSession, CompactionId, CompactionSize,
    CompactionTrigger, ErrorCategory, EventPayload, JournalError, JournalReplay, Retryability,
    SessionId, SessionStore, StartBehavior, StartTurn, TurnId, UserInput,
};
use lato_runtime::{
    SessionBootstrap, SessionHandle, TurnDriver, spawn_session, spawn_session_with_store,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, broadcast, mpsc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePromptOutcome {
    Complete { text: String },
    Cancelled { reason: CancelReason },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCompactionOutcome {
    Complete {
        before: CompactionSize,
        after: CompactionSize,
        checkpoint_id: String,
        warning: Option<AgentError>,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActiveOperation {
    Turn(TurnId),
    Compaction(CompactionId),
}

/// Typed session facade used by protocol adapters.
///
/// The submission gate closes the small window between accepting a start
/// command and observing its `TurnStarted` event. A concurrent cancel or
/// shutdown therefore cannot mistake an accepted turn for an idle session.
pub struct RuntimeSession {
    session_id: SessionId,
    handle: SessionHandle,
    driver: Arc<LegacyTurnDriver>,
    updates: mpsc::UnboundedSender<serde_json::Value>,
    active_operation: Mutex<Option<ActiveOperation>>,
    submission_gate: Mutex<()>,
}

impl RuntimeSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let model_port = switchable_model_port(&stream);
        Self::new_with_model_port(
            session_id, stream, model_port, locks, trust, cwd, updates, approval,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_model_port(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        model_port: Arc<SwitchableModelPort>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new_with_model_port(
            session_id.to_string(),
            stream,
            model_port,
            locks,
            trust,
            cwd,
            updates.clone(),
            approval,
        ));
        let runtime_driver: Arc<dyn TurnDriver> = driver.clone();
        let handle = spawn_session(session_id.clone(), runtime_driver);
        Self {
            session_id,
            handle,
            driver,
            updates,
            active_operation: Mutex::new(None),
            submission_gate: Mutex::new(()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_store(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
    ) -> Result<Self, AgentError> {
        let model_port = switchable_model_port(&stream);
        Self::new_with_store_and_model_port(
            session_id, stream, model_port, locks, trust, cwd, updates, approval, store, replay,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_store_and_model_port(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        model_port: Arc<SwitchableModelPort>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
    ) -> Result<Self, AgentError> {
        if let Some(unresolved) = replay.projection.unresolved_tools.first() {
            return Err(journal_error(JournalError::IncompleteSideEffect {
                call_id: unresolved.call_id.clone(),
            }));
        }
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new_with_model_port(
            session_id.to_string(),
            stream,
            model_port,
            locks,
            trust,
            cwd,
            updates.clone(),
            approval,
        ));
        let mut history =
            model_messages_to_history(&replay.projection.messages).map_err(journal_error)?;
        if !history.is_empty() && !matches!(history.first(), Some(HistoryItem::System(_))) {
            let initial = driver.history_snapshot().await;
            if let Some(system) = initial.into_iter().next() {
                history.insert(0, system);
            }
        }
        if !history.is_empty() {
            driver.replace_history(history).await;
        }
        let runtime_driver: Arc<dyn TurnDriver> = driver.clone();
        let handle = spawn_session_with_store(
            session_id.clone(),
            runtime_driver,
            store,
            SessionBootstrap { replay },
        );
        Ok(Self {
            session_id,
            handle,
            driver,
            updates,
            active_operation: Mutex::new(None),
            submission_gate: Mutex::new(()),
        })
    }

    pub async fn prompt(&self, input: String) -> Result<RuntimePromptOutcome, AgentError> {
        // Locking before subscribing prevents a waiting prompt from consuming
        // another prompt's start event while preserving subscribe-before-submit.
        let gate = self.submission_gate.lock().await;
        let mut events = self.handle.subscribe();
        self.handle
            .submit(Command::StartTurn(StartTurn {
                input: UserInput::text(input),
                behavior: StartBehavior::Reject,
            }))
            .await?;

        let mut observed_turn = None;
        let mut gate = Some(gate);
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Err(event_lagged(skipped));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Err(event_bus_closed());
                }
            };

            if event.session_id != self.session_id {
                continue;
            }

            match event.payload {
                EventPayload::TurnStarted => {
                    let turn_id = event.turn_id.ok_or_else(missing_turn_id)?;
                    observed_turn = Some(turn_id.clone());
                    *self.active_operation.lock().await = Some(ActiveOperation::Turn(turn_id));
                    drop(gate.take());
                }
                EventPayload::ModelDelta { text }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    let _ = self.updates.send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {"sessionId": self.session_id.as_str(), "delta": text},
                    }));
                }
                EventPayload::ReasoningDelta { text }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    let _ = self.updates.send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/reasoning",
                        "params": {"sessionId": self.session_id.as_str(), "delta": text},
                    }));
                }
                EventPayload::TurnCompleted(output)
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Ok(RuntimePromptOutcome::Complete {
                        text: output.final_text,
                    });
                }
                EventPayload::TurnCancelled { reason }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Ok(RuntimePromptOutcome::Cancelled { reason });
                }
                EventPayload::TurnFailed { error }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Err(error);
                }
                EventPayload::SessionStopped => {
                    self.clear_active(observed_turn.as_ref()).await;
                    return Err(runtime_stopped());
                }
                EventPayload::SessionStarted
                | EventPayload::ModelDelta { .. }
                | EventPayload::ReasoningDelta { .. }
                | EventPayload::TurnCompleted(_)
                | EventPayload::TurnCancelled { .. }
                | EventPayload::TurnFailed { .. }
                | EventPayload::CompactionStarted { .. }
                | EventPayload::CompactionCompleted { .. }
                | EventPayload::CompactionFailed { .. }
                | EventPayload::CompactionCancelled { .. } => {}
            }
        }
    }

    pub async fn cancel(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        let operation = self.active_operation.lock().await.clone();
        let Some(operation) = operation else {
            return Ok(());
        };
        let command = match &operation {
            ActiveOperation::Turn(turn_id) => Command::CancelTurn {
                turn_id: turn_id.clone(),
            },
            ActiveOperation::Compaction(compaction_id) => Command::CancelCompaction {
                compaction_id: compaction_id.clone(),
            },
        };
        match self.handle.submit(command).await {
            Ok(()) => Ok(()),
            Err(error)
                if error.code == "runtime.invalid_transition"
                    || error.code == "runtime.no_active_turn"
                    || error.code == "compaction.not_active" =>
            {
                self.clear_operation(Some(&operation)).await;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn shutdown(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        let result = self.handle.submit(Command::Shutdown).await;
        *self.active_operation.lock().await = None;
        result
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.driver.history_snapshot().await
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        self.driver.replace_history(history).await;
    }

    pub async fn is_active(&self) -> bool {
        self.active_operation.lock().await.is_some()
    }

    async fn clear_active(&self, turn_id: Option<&TurnId>) {
        let expected = turn_id.cloned().map(ActiveOperation::Turn);
        self.clear_operation(expected.as_ref()).await;
    }

    async fn clear_operation(&self, operation: Option<&ActiveOperation>) {
        let mut active = self.active_operation.lock().await;
        if operation.is_none() || active.as_ref() == operation {
            *active = None;
        }
    }

    pub async fn compact(
        &self,
        user_context: Option<String>,
    ) -> Result<RuntimeCompactionOutcome, AgentError> {
        let gate = self.submission_gate.lock().await;
        let mut events = self.handle.subscribe();
        self.handle
            .submit(Command::CompactSession(CompactSession {
                user_context: user_context.and_then(|value| {
                    let value = value.trim().to_owned();
                    (!value.is_empty()).then_some(value)
                }),
                trigger: CompactionTrigger::Manual,
            }))
            .await?;
        let mut observed = None;
        let mut gate = Some(gate);
        loop {
            let event = events.recv().await.map_err(|error| match error {
                broadcast::error::RecvError::Lagged(skipped) => event_lagged(skipped),
                broadcast::error::RecvError::Closed => event_bus_closed(),
            })?;
            if event.session_id != self.session_id {
                continue;
            }
            match event.payload {
                EventPayload::CompactionStarted {
                    compaction_id,
                    trigger,
                } if observed.is_none() => {
                    observed = Some(compaction_id.clone());
                    *self.active_operation.lock().await =
                        Some(ActiveOperation::Compaction(compaction_id.clone()));
                    drop(gate.take());
                    self.send_compaction_update(
                        "started",
                        serde_json::json!({
                            "compactionId": compaction_id,
                            "trigger": trigger,
                        }),
                    );
                }
                EventPayload::CompactionCompleted {
                    compaction_id,
                    before,
                    after,
                    checkpoint_id,
                    warning,
                } if observed.as_ref() == Some(&compaction_id) => {
                    self.clear_operation(Some(&ActiveOperation::Compaction(compaction_id.clone())))
                        .await;
                    self.send_compaction_update(
                        "completed",
                        serde_json::json!({
                            "compactionId": compaction_id,
                            "before": before,
                            "after": after,
                            "checkpointId": checkpoint_id,
                            "warning": warning,
                        }),
                    );
                    return Ok(RuntimeCompactionOutcome::Complete {
                        before,
                        after,
                        checkpoint_id,
                        warning,
                    });
                }
                EventPayload::CompactionFailed {
                    compaction_id,
                    error,
                } if observed.as_ref() == Some(&compaction_id) => {
                    self.clear_operation(Some(&ActiveOperation::Compaction(compaction_id.clone())))
                        .await;
                    self.send_compaction_update(
                        "failed",
                        serde_json::json!({
                            "compactionId": compaction_id,
                            "error": error,
                        }),
                    );
                    return Err(error);
                }
                EventPayload::CompactionCancelled { compaction_id }
                    if observed.as_ref() == Some(&compaction_id) =>
                {
                    self.clear_operation(Some(&ActiveOperation::Compaction(compaction_id.clone())))
                        .await;
                    self.send_compaction_update(
                        "cancelled",
                        serde_json::json!({
                            "compactionId": compaction_id,
                        }),
                    );
                    return Ok(RuntimeCompactionOutcome::Cancelled);
                }
                EventPayload::SessionStopped => return Err(runtime_stopped()),
                _ => {}
            }
        }
    }

    fn send_compaction_update(&self, event: &str, fields: serde_json::Value) {
        let mut params = serde_json::json!({
            "sessionId": self.session_id.as_str(),
            "event": event,
        });
        if let (Some(target), Some(source)) = (params.as_object_mut(), fields.as_object()) {
            target.extend(source.clone());
        }
        let _ = self.updates.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "lato/session/compaction",
            "params": params,
        }));
    }
}

fn switchable_model_port(stream: &Arc<dyn ModelStream>) -> Arc<SwitchableModelPort> {
    let active = stream.active_model_port().unwrap_or_else(|| {
        adapt_model_port("openai", "gpt-4.1", stream.clone())
            .expect("fallback model selection is statically valid")
    });
    Arc::new(SwitchableModelPort::from_active(active))
}

fn event_lagged(skipped: u64) -> AgentError {
    AgentError::new(
        "runtime.event_lagged",
        ErrorCategory::InternalInvariant,
        format!("runtime event receiver lagged by {skipped} events"),
        Retryability::Never,
    )
}

fn event_bus_closed() -> AgentError {
    AgentError::new(
        "runtime.event_bus_closed",
        ErrorCategory::InternalInvariant,
        "runtime event bus closed",
        Retryability::Never,
    )
}

fn runtime_stopped() -> AgentError {
    AgentError::new(
        "runtime.session_stopped",
        ErrorCategory::InvalidInput,
        "runtime session stopped before the turn completed",
        Retryability::Never,
    )
}

fn missing_turn_id() -> AgentError {
    AgentError::new(
        "runtime.missing_turn_id",
        ErrorCategory::InternalInvariant,
        "turn event did not contain a turn ID",
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

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{ToolCallId, UnresolvedToolCall};
    use lato_store::MemoryEventStore;

    #[test]
    fn lagged_event_error_is_structured_and_reports_the_count() {
        let error = event_lagged(17);
        assert_eq!(error.code, "runtime.event_lagged");
        assert_eq!(error.category, ErrorCategory::InternalInvariant);
        assert!(error.message.contains("17"));
    }

    #[test]
    fn closed_event_error_is_structured() {
        let error = event_bus_closed();
        assert_eq!(error.code, "runtime.event_bus_closed");
        assert_eq!(error.category, ErrorCategory::InternalInvariant);
    }

    #[tokio::test]
    async fn unresolved_prepared_tool_prevents_driver_start() {
        let directory = tempfile::tempdir().unwrap();
        let sid = SessionId::from("unknown-outcome");
        let mut replay = JournalReplay::empty(sid.clone());
        replay.exists = true;
        replay.projection.unresolved_tools.push(UnresolvedToolCall {
            call_id: ToolCallId::from("call-1"),
            request_hash: "sha256:v1:test".into(),
        });
        let (updates, _updates_rx) = mpsc::unbounded_channel();
        let result = RuntimeSession::new_with_store(
            sid.to_string(),
            crate::default_fake_stream(),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(directory.path()),
            directory.path().to_path_buf(),
            updates,
            None,
            Arc::new(MemoryEventStore::new()),
            replay,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("unknown side-effect outcome must block resume"),
            Err(error) => error,
        };
        assert_eq!(error.code, "journal.incomplete_side_effect");
    }
}
