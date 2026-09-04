use crate::{
    HistoryItem, PromptKind, SessionActor, ToolApproval, build_compacted_history,
    build_compaction_prompt, compaction_size, find_compaction_anchors, history_to_model_messages,
    model_messages_to_history, normalize_summary, prepare_compaction_messages, validate_reduction,
    validate_source_size, validate_summary,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use lato_ai::{ModelStream, SwitchableModelPort, adapt_model_port};
use lato_core::{
    AgentError, CompactionCandidate, CompactionError, ErrorCategory, ModelCallId, ModelContent,
    ModelError, ModelRequest, ModelStopReason, ModelStreamEvent, Retryability, SamplingParameters,
    ToolChoice, TurnOutput, UserInput,
};
use lato_runtime::{
    CompactionControl, CompactionRequest, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc};

/// Adapts the existing model/tool loop to the typed runtime turn contract.
pub struct LegacyTurnDriver {
    state: Mutex<LegacyState>,
    passthrough: mpsc::UnboundedSender<serde_json::Value>,
    model_port: Arc<SwitchableModelPort>,
    model_stream: Arc<dyn ModelStream>,
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
        let model_port = default_model_port(stream.clone());
        Self::new_with_model_port(
            session_id,
            stream,
            model_port,
            locks,
            trust,
            cwd,
            passthrough,
            approval,
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
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let model_stream = stream.clone();
        let actor = SessionActor::new(stream, locks, trust, cwd)
            .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough, model_port, model_stream)
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
        let model_port = default_model_port(stream.clone());
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let model_stream = stream.clone();
        let actor = SessionActor::new_with_tool_runtime(stream, locks, trust, cwd, tool_runtime)
            .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough, model_port, model_stream)
    }

    fn from_actor(
        actor: SessionActor,
        actor_events: mpsc::UnboundedReceiver<serde_json::Value>,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        model_port: Arc<SwitchableModelPort>,
        model_stream: Arc<dyn ModelStream>,
    ) -> Self {
        Self {
            state: Mutex::new(LegacyState {
                actor,
                actor_events,
            }),
            passthrough,
            model_port,
            model_stream,
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

    async fn history_snapshot(&self) -> Result<Vec<lato_core::ModelMessage>, AgentError> {
        history_to_model_messages(self.state.lock().await.actor.history()).map_err(journal_error)
    }

    async fn compact(
        &self,
        request: CompactionRequest,
        control: CompactionControl,
    ) -> Result<CompactionCandidate, AgentError> {
        let active = match self.model_stream.active_model_port() {
            Some(active) => active,
            None => self.model_port.snapshot().await,
        };
        run_compaction(active, request, control).await
    }

    async fn install_history(
        &self,
        messages: Vec<lato_core::ModelMessage>,
    ) -> Result<(), AgentError> {
        let history = model_messages_to_history(&messages).map_err(journal_error)?;
        *self.state.lock().await.actor.history_mut() = history;
        Ok(())
    }
}

async fn run_compaction(
    active: lato_ai::ActiveModelPort,
    request: CompactionRequest,
    control: CompactionControl,
) -> Result<CompactionCandidate, AgentError> {
    validate_source_size(&request.messages).map_err(AgentError::from)?;
    let prepared = prepare_compaction_messages(&request.messages).map_err(AgentError::from)?;
    let (system, latest_user) =
        find_compaction_anchors(&request.messages).map_err(AgentError::from)?;
    let mut model_messages = prepared;
    model_messages.push(lato_core::ModelMessage {
        role: lato_core::ModelRole::User,
        content: vec![ModelContent::Text {
            text: build_compaction_prompt(request.request.user_context.as_deref()),
        }],
    });
    let request_bytes = serde_json::to_vec(&model_messages)
        .map_err(|error| CompactionError::InvalidSummary {
            message: error.to_string(),
        })?
        .len() as u64;
    if let Some(window) = active.capabilities.context_window {
        let estimated_tokens = request_bytes.div_ceil(4);
        if estimated_tokens.saturating_add(request.policy.summary_reserve_tokens) > window {
            return Err(CompactionError::InputTooLarge.into());
        }
    }

    let max_attempts = request.policy.max_attempts.max(1);
    let mut last_error = None;
    for attempt in 1..=max_attempts {
        if control.cancellation.is_cancelled() {
            return Err(CompactionError::Cancelled.into());
        }
        let model_request = ModelRequest {
            call_id: ModelCallId::from(format!("{}-attempt-{attempt}", request.compaction_id)),
            selection: active.selection.clone(),
            messages: model_messages.clone(),
            tools: Vec::new(),
            parameters: SamplingParameters {
                temperature: Some(0.0),
                max_output_tokens: Some(request.policy.summary_reserve_tokens),
                tool_choice: Some(ToolChoice::None),
                response_schema: None,
            },
        };
        match sample_summary(&active.port, model_request, &control).await {
            Ok(raw) => {
                let summary = normalize_summary(&raw);
                if let Err(error) = validate_summary(&summary) {
                    last_error = Some(AgentError::from(error));
                } else {
                    let messages =
                        build_compacted_history(system.clone(), latest_user.clone(), &summary)
                            .map_err(AgentError::from)?;
                    let before = compaction_size(&request.messages).map_err(AgentError::from)?;
                    let after = compaction_size(&messages).map_err(AgentError::from)?;
                    if let Err(error) = validate_reduction(&before, &after) {
                        last_error = Some(AgentError::from(error));
                    } else {
                        return Ok(CompactionCandidate {
                            compaction_id: request.compaction_id,
                            messages,
                            before,
                            after,
                            summary_chars: summary.chars().count() as u64,
                        });
                    }
                }
            }
            Err(error) => {
                let retryable = error.retryability != Retryability::Never;
                last_error = Some(error);
                if !retryable {
                    break;
                }
            }
        }
        if attempt < max_attempts {
            tokio::select! {
                _ = control.cancellation.cancelled() => {
                    return Err(CompactionError::Cancelled.into());
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(25 * u64::from(attempt))) => {}
            }
        }
    }
    Err(last_error.unwrap_or_else(|| CompactionError::DegenerateSummary.into()))
}

async fn sample_summary(
    port: &Arc<dyn lato_core::ModelPort>,
    request: ModelRequest,
    control: &CompactionControl,
) -> Result<String, AgentError> {
    let cancellation = control.cancellation.clone();
    let mut stream = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(CompactionError::Cancelled.into()),
        result = port.stream(request, cancellation.clone()) => {
            result.map_err(model_error)?
        }
    };
    let mut text = String::new();
    let mut completed = false;
    loop {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CompactionError::Cancelled.into()),
            event = stream.next() => event,
        };
        match event {
            Some(Ok(ModelStreamEvent::TextDelta { text: delta })) => text.push_str(&delta),
            Some(Ok(ModelStreamEvent::ReasoningDelta { .. } | ModelStreamEvent::Usage(_))) => {}
            Some(Ok(ModelStreamEvent::ToolCallDelta(_))) => {
                return Err(CompactionError::InvalidSummary {
                    message: "compaction model attempted a tool call".into(),
                }
                .into());
            }
            Some(Ok(ModelStreamEvent::Completed {
                reason: ModelStopReason::Completed,
            })) => {
                completed = true;
                break;
            }
            Some(Ok(ModelStreamEvent::Completed { reason })) => {
                return Err(CompactionError::InvalidSummary {
                    message: format!("compaction stream stopped with {reason:?}"),
                }
                .into());
            }
            Some(Err(error)) => return Err(model_error(error)),
            None => break,
        }
    }
    if !completed {
        return Err(model_error(ModelError::new(
            "model.stream_interrupted",
            "compaction model stream ended before completion",
            Retryability::AfterBackoff,
        )));
    }
    Ok(text)
}

fn default_model_port(stream: Arc<dyn ModelStream>) -> Arc<SwitchableModelPort> {
    let active = stream.active_model_port().unwrap_or_else(|| {
        adapt_model_port("openai", "gpt-4.1", stream)
            .expect("fallback model selection is statically valid")
    });
    Arc::new(SwitchableModelPort::from_active(active))
}

fn model_error(error: ModelError) -> AgentError {
    CompactionError::ModelFailed {
        source: AgentError::from(error),
    }
    .into()
}

fn journal_error(error: lato_core::JournalError) -> AgentError {
    AgentError::new(
        error.code(),
        ErrorCategory::Storage,
        error.to_string(),
        error.retryability(),
    )
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

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use crate::REQUIRED_SECTIONS;
    use lato_core::{
        CompactSession, CompactionId, CompactionPolicy, CompactionTrigger, ModelCapabilities,
        ModelEventStream, ModelMessage, ModelPort, ModelRole, ModelSelection, ToolCallDelta,
        ToolName,
    };
    use std::sync::{Arc, Mutex as StdMutex};

    struct ScriptedPort {
        scripts: StdMutex<Vec<Result<Vec<Result<ModelStreamEvent, ModelError>>, ModelError>>>,
        requests: StdMutex<Vec<ModelRequest>>,
    }

    struct CancellationPort {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ModelPort for CancellationPort {
        async fn stream(
            &self,
            _request: ModelRequest,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            self.started.notify_one();
            cancellation.cancelled().await;
            Err(ModelError::cancelled())
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                context_window: Some(1_000_000),
                ..ModelCapabilities::default()
            }
        }
    }

    #[async_trait]
    impl ModelPort for ScriptedPort {
        async fn stream(
            &self,
            request: ModelRequest,
            _cancellation: tokio_util::sync::CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            self.requests.lock().unwrap().push(request);
            let script = self.scripts.lock().unwrap().remove(0)?;
            Ok(Box::pin(futures_util::stream::iter(script)))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                context_window: Some(1_000_000),
                ..ModelCapabilities::default()
            }
        }
    }

    fn summary() -> String {
        let detail =
            "retain verified facts, decisions, errors, commands, and pending work ".repeat(2);
        REQUIRED_SECTIONS
            .iter()
            .enumerate()
            .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn completed(text: String) -> Vec<Result<ModelStreamEvent, ModelError>> {
        vec![
            Ok(ModelStreamEvent::TextDelta { text }),
            Ok(ModelStreamEvent::Completed {
                reason: ModelStopReason::Completed,
            }),
        ]
    }

    fn source() -> Vec<ModelMessage> {
        vec![
            ModelMessage {
                role: ModelRole::System,
                content: vec![ModelContent::Text {
                    text: "system".into(),
                }],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "finish the parser".into(),
                }],
            },
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text {
                    text: "large prior work ".repeat(2_000),
                }],
            },
        ]
    }

    fn request() -> CompactionRequest {
        CompactionRequest {
            compaction_id: CompactionId::from("compact-test"),
            request: CompactSession {
                user_context: Some("preserve parser diagnosis".into()),
                trigger: CompactionTrigger::Manual,
            },
            messages: source(),
            policy: CompactionPolicy::default(),
        }
    }

    fn active(port: Arc<dyn ModelPort>) -> lato_ai::ActiveModelPort {
        lato_ai::ActiveModelPort {
            selection: ModelSelection::new("openai", "test").unwrap(),
            capabilities: port.capabilities(),
            port,
        }
    }

    #[tokio::test]
    async fn compaction_is_tool_free_and_uses_the_current_model() {
        let port = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![Ok(completed(summary()))]),
            requests: StdMutex::new(Vec::new()),
        });
        let candidate = run_compaction(
            active(port.clone()),
            request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
        assert!(candidate.after.serialized_bytes < candidate.before.serialized_bytes);
        let requests = port.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty());
        assert_eq!(requests[0].parameters.tool_choice, Some(ToolChoice::None));
        assert_eq!(requests[0].selection.model, "test");
    }

    #[tokio::test]
    async fn degenerate_outputs_retry_but_never_more_than_three_times() {
        let port = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![
                Ok(completed("short".into())),
                Ok(completed("still short".into())),
                Ok(completed(summary())),
            ]),
            requests: StdMutex::new(Vec::new()),
        });
        run_compaction(
            active(port.clone()),
            request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(port.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn deterministic_model_failure_and_tool_calls_do_not_retry() {
        let deterministic = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![Err(ModelError::new(
                "model.auth",
                "denied",
                Retryability::Never,
            ))]),
            requests: StdMutex::new(Vec::new()),
        });
        assert!(
            run_compaction(
                active(deterministic.clone()),
                request(),
                CompactionControl {
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .is_err()
        );
        assert_eq!(deterministic.requests.lock().unwrap().len(), 1);

        let tool_call = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![Ok(vec![Ok(ModelStreamEvent::ToolCallDelta(
                ToolCallDelta {
                    index: 0,
                    call_id: None,
                    name: Some(ToolName::parse("legacy:read_file").unwrap()),
                    arguments_delta: "{}".into(),
                },
            ))])]),
            requests: StdMutex::new(Vec::new()),
        });
        assert!(
            run_compaction(
                active(tool_call.clone()),
                request(),
                CompactionControl {
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .is_err()
        );
        assert_eq!(tool_call.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn transient_failures_stop_at_the_attempt_budget() {
        let transient = || {
            Err(ModelError::new(
                "model.busy",
                "retry",
                Retryability::AfterBackoff,
            ))
        };
        let port = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![transient(), transient(), transient()]),
            requests: StdMutex::new(Vec::new()),
        });
        assert!(
            run_compaction(
                active(port.clone()),
                request(),
                CompactionControl {
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .is_err()
        );
        assert_eq!(port.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn cancellation_reaches_a_blocked_compaction_provider() {
        let started = Arc::new(tokio::sync::Notify::new());
        let port = Arc::new(CancellationPort {
            started: started.clone(),
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            run_compaction(
                active(port),
                request(),
                CompactionControl {
                    cancellation: task_cancellation,
                },
            )
            .await
        });
        started.notified().await;
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_millis(250), task)
            .await
            .expect("compaction did not stop after cancellation")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "compaction.cancelled");
    }
}
