use crate::{HistoryItem, LegacyTurnDriver, ToolApproval};
use lato_ai::ModelStream;
use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventPayload, Retryability, SessionId,
    StartBehavior, StartTurn, TurnId, UserInput,
};
use lato_runtime::{SessionHandle, TurnDriver, spawn_session};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, broadcast, mpsc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePromptOutcome {
    Complete { text: String },
    Cancelled { reason: CancelReason },
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
    active_turn: Mutex<Option<TurnId>>,
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
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new(
            session_id.to_string(),
            stream,
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
            active_turn: Mutex::new(None),
            submission_gate: Mutex::new(()),
        }
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
                    *self.active_turn.lock().await = Some(turn_id);
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
                | EventPayload::TurnFailed { .. } => {}
            }
        }
    }

    pub async fn cancel(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        let turn_id = self.active_turn.lock().await.clone();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        match self
            .handle
            .submit(Command::CancelTurn {
                turn_id: turn_id.clone(),
            })
            .await
        {
            Ok(()) => Ok(()),
            Err(error)
                if error.code == "runtime.invalid_transition"
                    || error.code == "runtime.no_active_turn" =>
            {
                self.clear_active(Some(&turn_id)).await;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn shutdown(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        let result = self.handle.submit(Command::Shutdown).await;
        *self.active_turn.lock().await = None;
        result
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.driver.history_snapshot().await
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        self.driver.replace_history(history).await;
    }

    async fn clear_active(&self, turn_id: Option<&TurnId>) {
        let mut active = self.active_turn.lock().await;
        if turn_id.is_none() || active.as_ref() == turn_id {
            *active = None;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
