use crate::{HistoryItem, PromptKind, SessionActor, ToolApproval};
use async_trait::async_trait;
use lato_ai::ModelStream;
use lato_core::{AgentError, ErrorCategory, Retryability, TurnOutput, UserInput};
use lato_runtime::{TurnControl, TurnDriver, TurnEventEmitter, TurnRequest};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc};

/// Adapts the existing model/tool loop to the typed runtime turn contract.
pub struct LegacyTurnDriver {
    state: Mutex<LegacyState>,
    passthrough: mpsc::UnboundedSender<serde_json::Value>,
}

struct LegacyState {
    actor: SessionActor,
    actor_events: mpsc::UnboundedReceiver<serde_json::Value>,
}

impl LegacyTurnDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor = SessionActor::new(stream, locks, trust, cwd)
            .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_tool_runtime(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        tool_runtime: Arc<lato_tools::ToolRuntime>,
    ) -> Self {
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor = SessionActor::new_with_tool_runtime(stream, locks, trust, cwd, tool_runtime)
            .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough)
    }

    fn from_actor(
        actor: SessionActor,
        actor_events: mpsc::UnboundedReceiver<serde_json::Value>,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
    ) -> Self {
        Self {
            state: Mutex::new(LegacyState {
                actor,
                actor_events,
            }),
            passthrough,
        }
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.state.lock().await.actor.history().to_vec()
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        *self.state.lock().await.actor.history_mut() = history;
    }
}

#[async_trait]
impl TurnDriver for LegacyTurnDriver {
    async fn run(
        &self,
        request: TurnRequest,
        mut control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, AgentError> {
        let mut state = self.state.lock().await;
        let turn_id = request.turn_id.clone();
        let cancellation = control.cancellation.clone();
        let mut input = request.input;
        let mut kind = PromptKind::Start;
        let mut steering_open = true;

        loop {
            enum Next {
                Finished(Result<crate::TurnOutcome, String>),
                Cancelled,
                Steer(UserInput),
            }

            let next = {
                let LegacyState {
                    actor,
                    actor_events,
                } = &mut *state;
                actor.set_journal_events(Some(events.clone()));
                let mut prompt = Box::pin(actor.prompt_with_context(
                    kind,
                    input.text,
                    turn_id.clone(),
                    cancellation.clone(),
                ));
                loop {
                    tokio::select! {
                        biased;
                        _ = control.cancellation.cancelled() => break Next::Cancelled,
                        steer = control.steering.recv(), if steering_open => {
                            match steer {
                                Some(steer) => break Next::Steer(steer),
                                None => steering_open = false,
                            }
                        }
                        result = &mut prompt => break Next::Finished(result),
                        actor_event = actor_events.recv() => {
                            if let Some(actor_event) = actor_event {
                                forward_actor_event(&events, &self.passthrough, actor_event)?;
                            }
                        }
                    }
                }
            };

            match next {
                Next::Finished(Ok(_)) => {
                    drain_actor_events(&events, &self.passthrough, &mut state.actor_events)?;
                    return Ok(TurnOutput {
                        final_text: state.actor.latest_assistant_text(),
                    });
                }
                Next::Finished(Err(message)) => {
                    drain_actor_events(&events, &self.passthrough, &mut state.actor_events)?;
                    return Err(legacy_error(message));
                }
                Next::Cancelled => {
                    state.actor.cancel();
                    discard_actor_events(&mut state.actor_events);
                    return Ok(TurnOutput {
                        final_text: String::new(),
                    });
                }
                Next::Steer(steer) => {
                    state.actor.cancel();
                    discard_actor_events(&mut state.actor_events);
                    input = steer;
                    kind = PromptKind::Steer;
                }
            }
        }
    }
}

fn drain_actor_events(
    events: &TurnEventEmitter,
    passthrough: &mpsc::UnboundedSender<serde_json::Value>,
    actor_events: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) -> Result<(), AgentError> {
    loop {
        match actor_events.try_recv() {
            Ok(actor_event) => forward_actor_event(events, passthrough, actor_event)?,
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
        }
    }
}

fn discard_actor_events(actor_events: &mut mpsc::UnboundedReceiver<serde_json::Value>) {
    while actor_events.try_recv().is_ok() {}
}

fn forward_actor_event(
    events: &TurnEventEmitter,
    passthrough: &mpsc::UnboundedSender<serde_json::Value>,
    actor_event: serde_json::Value,
) -> Result<(), AgentError> {
    if actor_event.get("method").and_then(|value| value.as_str()) == Some("session/update")
        && let Some(delta) = actor_event
            .pointer("/params/delta")
            .and_then(|value| value.as_str())
    {
        return events.model_delta(delta);
    }
    let _ = passthrough.send(actor_event);
    Ok(())
}

fn legacy_error(message: String) -> AgentError {
    AgentError::new(
        "legacy.turn_failed",
        ErrorCategory::Task,
        message,
        Retryability::RequiresDecision,
    )
}
