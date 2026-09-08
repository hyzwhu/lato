// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs
// License: Apache-2.0
// Lato changes: added current-model, typed-stream compaction sampling to the legacy turn adapter

use crate::{
    CompactionInputStage, HistoryItem, PromptKind, SessionActor, ToolApproval,
    build_compacted_history, build_compaction_prompt, compaction_size, find_compaction_anchors,
    history_to_model_messages, model_messages_to_history, normalize_summary,
    prepare_compaction_input, validate_reduction, validate_source_size, validate_summary,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use lato_ai::{
    ActiveModelPort, ActiveModelStream, ModelStream, SwitchableModelPort, SwitchableModelStream,
    adapt_model_endpoint,
};
use lato_core::{
    AgentError, CompactionCandidate, CompactionError, ErrorCategory, ModelCallId, ModelContent,
    ModelError, ModelErrorKind, ModelRequest, ModelStopReason, ModelStreamEvent, Retryability,
    SamplingParameters, ToolChoice, TurnOutput, UserInput,
};
use lato_extensions::{hooks::HookRegistry, skills::SkillCatalog};
use lato_runtime::{
    CompactionControl, CompactionRequest, PrefireCompactionRequest, PrefireCompactionResult,
    TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};

/// Adapts the existing model/tool loop to the typed runtime turn contract.
pub struct LegacyTurnDriver {
    state: Mutex<LegacyState>,
    passthrough: mpsc::UnboundedSender<serde_json::Value>,
    model_port: Arc<SwitchableModelPort>,
    model_stream: Arc<SwitchableModelStream>,
    prefire_model_attempts: AtomicU8,
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
        let endpoint = endpoint_from_stream(stream);
        Self::new_with_endpoint(
            session_id,
            endpoint,
            locks,
            trust,
            cwd,
            passthrough,
            approval,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_endpoint(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let model_port = Arc::new(SwitchableModelPort::from_active(endpoint.port.clone()));
        let model_stream = Arc::new(SwitchableModelStream::new(endpoint));
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor = SessionActor::new(model_stream.clone(), locks, trust, cwd)
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
        let endpoint = endpoint_from_stream(stream);
        Self::new_with_endpoint_and_tool_runtime(
            session_id,
            endpoint,
            locks,
            trust,
            cwd,
            passthrough,
            approval,
            tool_runtime,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_endpoint_and_tool_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        tool_runtime: Arc<lato_tools::ToolRuntime>,
    ) -> Self {
        Self::from_endpoint_tool_runtime(
            session_id,
            endpoint,
            locks,
            trust,
            cwd,
            passthrough,
            approval,
            tool_runtime,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_endpoint_skill_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        binding: crate::SkillRuntimeBinding,
    ) -> Self {
        let model_port = Arc::new(SwitchableModelPort::from_active(endpoint.port.clone()));
        let model_stream = Arc::new(SwitchableModelStream::new(endpoint));
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor =
            SessionActor::new_with_skill_runtime(model_stream.clone(), locks, trust, cwd, binding)
                .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough, model_port, model_stream)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_endpoint_tool_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        tool_runtime: Arc<lato_tools::ToolRuntime>,
    ) -> Self {
        let model_port = Arc::new(SwitchableModelPort::from_active(endpoint.port.clone()));
        let model_stream = Arc::new(SwitchableModelStream::new(endpoint));
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor = SessionActor::new_with_tool_runtime(
            model_stream.clone(),
            locks,
            trust,
            cwd,
            tool_runtime,
        )
        .with_interactive_events(actor_tx, session_id, approval);
        Self::from_actor(actor, actor_events, passthrough, model_port, model_stream)
    }

    fn from_actor(
        actor: SessionActor,
        actor_events: mpsc::UnboundedReceiver<serde_json::Value>,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        model_port: Arc<SwitchableModelPort>,
        model_stream: Arc<SwitchableModelStream>,
    ) -> Self {
        Self {
            state: Mutex::new(LegacyState {
                actor,
                actor_events,
            }),
            passthrough,
            model_port,
            model_stream,
            prefire_model_attempts: AtomicU8::new(0),
        }
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.state.lock().await.actor.history().to_vec()
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        *self.state.lock().await.actor.history_mut() = history;
    }

    pub async fn bind_turn_skills(&self, catalog: Arc<SkillCatalog>) {
        self.state
            .lock()
            .await
            .actor
            .bind_turn_skills(catalog)
            .await;
    }

    pub async fn bind_turn_hooks(&self, registry: Arc<HookRegistry>) {
        self.state
            .lock()
            .await
            .actor
            .bind_turn_hook_registry(registry);
    }

    pub async fn observe_hook(
        &self,
        event: lato_extensions::hooks::HookEventName,
        payload: serde_json::Value,
    ) -> Vec<lato_extensions::hooks::HookRunRecord> {
        self.state
            .lock()
            .await
            .actor
            .observe_bound_hook(event, payload)
            .await
    }

    pub async fn gate_prompt_hook(&self, text: &str) -> crate::PromptHookGate {
        self.state.lock().await.actor.gate_prompt_hook(text).await
    }

    pub async fn active_model(&self) -> ActiveModelPort {
        self.model_stream
            .active_model_port()
            .expect("switchable model stream always has an active model")
    }

    pub fn active_endpoint(&self) -> ActiveModelStream {
        self.model_stream.snapshot()
    }

    pub async fn activate_model(&self, endpoint: ActiveModelStream) {
        self.model_stream.set_active(endpoint).await;
        let active = self
            .model_stream
            .active_model_port()
            .expect("switchable model stream always has an active model");
        self.model_port.set_active(active).await;
    }

    pub async fn has_model_authored_history(&self) -> bool {
        crate::has_model_authored_history(self.state.lock().await.actor.history())
    }

    pub async fn mark_model_switch_check(&self) {
        self.state.lock().await.actor.mark_model_switch_check();
    }

    pub async fn context_budget_changed(&self) {
        self.state.lock().await.actor.context_budget_changed();
    }

    pub async fn model_generation_changed(&self) {
        self.state.lock().await.actor.model_generation_changed();
    }

    pub async fn auth_refreshed(&self) {
        let mut state = self.state.lock().await;
        state.actor.auth_refreshed();
        while let Ok(actor_event) = state.actor_events.try_recv() {
            let _ = self.passthrough.send(actor_event);
        }
    }

    pub(crate) async fn automatic_compaction_allowed(
        &self,
        trigger: lato_core::CompactionTrigger,
    ) -> bool {
        self.state
            .lock()
            .await
            .actor
            .automatic_compaction_allowed(trigger)
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
        mut request: CompactionRequest,
        control: CompactionControl,
    ) -> Result<CompactionCandidate, AgentError> {
        request.prior_model_attempts = request
            .prior_model_attempts
            .max(self.prefire_model_attempts.swap(0, Ordering::AcqRel));
        let active = match self.model_stream.active_model_port() {
            Some(active) => active,
            None => self.model_port.snapshot().await,
        };
        run_compaction(active, request, control).await
    }

    async fn prefire_compaction(
        &self,
        request: PrefireCompactionRequest,
        control: CompactionControl,
    ) -> Result<PrefireCompactionResult, AgentError> {
        if request.prefix_len == 0 || request.prefix_len > request.messages.len() {
            return Err(AgentError::new(
                "compaction.invalid_prefire_prefix",
                ErrorCategory::InvalidInput,
                "prefire prefix is outside the history snapshot",
                Retryability::Never,
            ));
        }
        let max_attempts = request.policy.max_attempts.max(1);
        let prefire_limit = max_attempts.saturating_sub(1);
        if self
            .prefire_model_attempts
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < prefire_limit).then_some(current.saturating_add(1))
            })
            .is_err()
        {
            return Err(AgentError::new(
                "compaction.prefire_attempt_budget_exhausted",
                ErrorCategory::Task,
                "prefire compaction reserved the remaining model submission for final compaction",
                Retryability::Never,
            ));
        }
        let active = match self.model_stream.active_model_port() {
            Some(active) => active,
            None => self.model_port.snapshot().await,
        };
        let messages = crate::build_pass_one_history(
            &request.messages[..request.prefix_len],
            &build_compaction_prompt(None),
        );
        let model_request = ModelRequest {
            call_id: ModelCallId::from("compaction-prefire-pass-one"),
            selection: active.selection,
            messages,
            tools: Vec::new(),
            parameters: SamplingParameters {
                temperature: Some(0.0),
                max_output_tokens: Some(request.policy.summary_reserve_tokens),
                tool_choice: Some(ToolChoice::None),
                response_schema: None,
            },
        };
        let raw = sample_summary(&active.port, model_request, &control)
            .await
            .map_err(compaction_sample_error)?;
        let note1 = crate::note_for_pass_two(&raw);
        if note1.is_empty() {
            return Err(AgentError::new(
                "compaction.empty_prefire_summary",
                ErrorCategory::Task,
                "prefire compaction returned an empty first-pass note",
                Retryability::Never,
            ));
        }
        Ok(PrefireCompactionResult { note1 })
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
    let (system, latest_user) =
        find_compaction_anchors(&request.messages).map_err(AgentError::from)?;
    let max_attempts = request.policy.max_attempts.max(1);
    let remaining_attempts = max_attempts.saturating_sub(request.prior_model_attempts);
    if remaining_attempts == 0 {
        return Err(AgentError::new(
            "compaction.attempt_budget_exhausted",
            ErrorCategory::Task,
            "compaction model submission budget was exhausted before final compaction",
            Retryability::Never,
        ));
    }
    let mut last_error = None;
    let mut stage = CompactionInputStage::Prepared;
    let mut context_window = active
        .metadata
        .context_window
        .or(active.capabilities.context_window);
    let compaction_prompt = build_compaction_prompt(request.request.user_context.as_deref());
    let protocol_overhead_tokens = u64::try_from(compaction_prompt.len())
        .unwrap_or(u64::MAX)
        .div_ceil(4)
        .saturating_add(256);
    let mut two_pass = request.two_pass.clone();
    for attempt in 1..=remaining_attempts {
        if control.cancellation.is_cancelled() {
            return Err(CompactionError::Cancelled.into());
        }
        let input_budget = match (stage, context_window) {
            (CompactionInputStage::Prepared, _) | (_, None) => u64::MAX / 4,
            (CompactionInputStage::Fitted, Some(window)) => window
                .saturating_sub(request.policy.summary_reserve_tokens)
                .saturating_sub(protocol_overhead_tokens),
            (CompactionInputStage::Lossy, Some(window)) => window
                .saturating_mul(7)
                .checked_div(10)
                .unwrap_or_default()
                .saturating_sub(request.policy.summary_reserve_tokens)
                .saturating_sub(protocol_overhead_tokens),
        };
        let pass = two_pass.take();
        let used_two_pass = pass.is_some();
        let model_messages = if let Some(pass) = pass {
            if pass.prefix_len == 0 || pass.prefix_len > request.messages.len() {
                return Err(AgentError::new(
                    "compaction.invalid_two_pass_prefix",
                    ErrorCategory::InvalidInput,
                    "two-pass prefix is outside the compaction history",
                    Retryability::Never,
                ));
            }
            crate::build_pass_two_history(
                &request.messages[..pass.prefix_len],
                &request.messages[pass.prefix_len..],
                &pass.note1,
                &compaction_prompt,
            )
        } else {
            let mut messages = prepare_compaction_input(&request.messages, stage, input_budget)
                .map_err(AgentError::from)?;
            messages.push(lato_core::ModelMessage {
                role: lato_core::ModelRole::User,
                content: vec![ModelContent::Text {
                    text: compaction_prompt.clone(),
                }],
            });
            messages
        };
        if let Some(window) = context_window {
            let estimated_tokens = serde_json::to_vec(&model_messages)
                .map_err(|error| CompactionError::InvalidSummary {
                    message: error.to_string(),
                })?
                .len()
                .div_ceil(4) as u64;
            if estimated_tokens.saturating_add(request.policy.summary_reserve_tokens) > window {
                if !used_two_pass {
                    let Some(next) = stage.next() else {
                        return Err(CompactionError::InputTooLarge.into());
                    };
                    stage = next;
                }
                continue;
            }
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
            Err(CompactionSampleError::Model(error))
                if error.kind == ModelErrorKind::ContextOverflow =>
            {
                if let Some(window) = error.context_window {
                    context_window = Some(context_window.map_or(window, |old| old.min(window)));
                }
                if !used_two_pass {
                    let Some(next) = stage.next() else {
                        return Err(CompactionError::InputTooLarge.into());
                    };
                    stage = next;
                }
                last_error = Some(model_error(error));
            }
            Err(CompactionSampleError::Model(error)) => {
                let retryable = error.retryability != Retryability::Never;
                last_error = Some(model_error(error));
                if !retryable {
                    break;
                }
            }
            Err(CompactionSampleError::Compaction(error)) => {
                last_error = Some(AgentError::from(error));
                break;
            }
        }
        if attempt < remaining_attempts {
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
) -> Result<String, CompactionSampleError> {
    let cancellation = control.cancellation.clone();
    let mut stream = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(CompactionSampleError::Compaction(CompactionError::Cancelled)),
        result = port.stream(request, cancellation.clone()) => {
            result.map_err(CompactionSampleError::Model)?
        }
    };
    let mut text = String::new();
    let mut completed = false;
    loop {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CompactionSampleError::Compaction(CompactionError::Cancelled)),
            event = stream.next() => event,
        };
        match event {
            Some(Ok(ModelStreamEvent::TextDelta { text: delta })) => text.push_str(&delta),
            Some(Ok(ModelStreamEvent::ReasoningDelta { .. } | ModelStreamEvent::Usage(_))) => {}
            Some(Ok(ModelStreamEvent::ToolCallDelta(_))) => {
                return Err(CompactionSampleError::Compaction(
                    CompactionError::InvalidSummary {
                        message: "compaction model attempted a tool call".into(),
                    },
                ));
            }
            Some(Ok(ModelStreamEvent::Completed {
                reason: ModelStopReason::Completed,
            })) => {
                completed = true;
                break;
            }
            Some(Ok(ModelStreamEvent::Completed { reason })) => {
                return Err(CompactionSampleError::Compaction(
                    CompactionError::InvalidSummary {
                        message: format!("compaction stream stopped with {reason:?}"),
                    },
                ));
            }
            Some(Err(error)) => return Err(CompactionSampleError::Model(error)),
            None => break,
        }
    }
    if !completed {
        return Err(CompactionSampleError::Model(ModelError::new(
            "model.stream_interrupted",
            "compaction model stream ended before completion",
            Retryability::AfterBackoff,
        )));
    }
    Ok(text)
}

enum CompactionSampleError {
    Model(ModelError),
    Compaction(CompactionError),
}

fn compaction_sample_error(error: CompactionSampleError) -> AgentError {
    match error {
        CompactionSampleError::Model(error) => model_error(error),
        CompactionSampleError::Compaction(error) => error.into(),
    }
}

fn endpoint_from_stream(stream: Arc<dyn ModelStream>) -> ActiveModelStream {
    if let Some(port) = stream.active_model_port() {
        ActiveModelStream { stream, port }
    } else {
        let port = adapt_model_endpoint(
            "openai",
            "gpt-4.1",
            lato_ai::ModelMetadata::default(),
            stream.clone(),
        )
        .expect("fallback model selection is statically valid")
        .port;
        ActiveModelStream { stream, port }
    }
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

    type ModelScript = Result<Vec<Result<ModelStreamEvent, ModelError>>, ModelError>;

    struct ScriptedPort {
        scripts: StdMutex<Vec<ModelScript>>,
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
            two_pass: None,
            prior_model_attempts: 0,
        }
    }

    fn active(port: Arc<dyn ModelPort>) -> lato_ai::ActiveModelPort {
        lato_ai::ActiveModelPort {
            selection: ModelSelection::new("openai", "test").unwrap(),
            metadata: lato_ai::ModelMetadata::default(),
            capabilities: port.capabilities(),
            generation: 0,
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

    fn overflow(window: u64) -> ModelError {
        ModelError::new(
            "model.context_overflow",
            "context window exceeded",
            Retryability::Never,
        )
        .with_kind(ModelErrorKind::ContextOverflow)
        .with_context_window(window)
    }

    fn ladder_request() -> CompactionRequest {
        let mut value = request();
        value.messages = vec![ModelMessage {
            role: ModelRole::System,
            content: vec![ModelContent::Text {
                text: "system".into(),
            }],
        }];
        for index in 0..40 {
            value.messages.push(ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: format!("objective-{index} {}", "x".repeat(2_000)),
                }],
            });
        }
        value.messages.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "latest objective".into(),
            }],
        });
        value
    }

    #[tokio::test]
    async fn overflow_advances_through_three_decreasing_input_stages() {
        let port = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![
                Err(overflow(20_000)),
                Err(overflow(20_000)),
                Ok(completed(summary())),
            ]),
            requests: StdMutex::new(Vec::new()),
        });
        let candidate = run_compaction(
            active(port.clone()),
            ladder_request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
        assert!(candidate.after.serialized_bytes < candidate.before.serialized_bytes);
        let requests = port.requests.lock().unwrap();
        let sizes = requests
            .iter()
            .map(|request| serde_json::to_vec(&request.messages).unwrap().len())
            .collect::<Vec<_>>();
        assert_eq!(sizes.len(), 3);
        assert!(sizes[0] > sizes[1] && sizes[1] > sizes[2], "{sizes:?}");
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| crate::message_text(message).contains("latest objective"))
        );
    }

    #[tokio::test]
    async fn third_overflow_is_terminal_without_a_fourth_submission() {
        let port = Arc::new(ScriptedPort {
            scripts: StdMutex::new(vec![
                Err(overflow(20_000)),
                Err(overflow(20_000)),
                Err(overflow(20_000)),
            ]),
            requests: StdMutex::new(Vec::new()),
        });
        let error = run_compaction(
            active(port.clone()),
            ladder_request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "compaction.input_too_large");
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
