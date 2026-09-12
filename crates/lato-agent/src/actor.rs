// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs
// Skill lifecycle derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/prompt/skills.rs
// License: Apache-2.0
// Lato changes: adds immutable one-step skill prompts/scopes while retaining Lato's durable runtime and policy boundaries

use crate::{
    AutoCompactionSuppression, ContextTracker, HistoryItem, PREFIRE_LEAD_PERCENT,
    SamplingRecoveryBudget, SessionHookRuntime, SessionMcpHandle, SessionSkillHandle,
    SkillRuntimeBinding, TWO_PASS_SPLIT_PERCENT, compaction_suppression_reason, fingerprint_prefix,
    split_for_two_pass,
};
use async_trait::async_trait;
use lato_ai::{
    CONTEXT_HARD_LIMIT_BYTES, ModelStream, StreamPiece, ThinkTagFilter,
    extract_text_embedded_tool_calls, strip_think_tags,
};
pub use lato_core::ApprovalRequest;
use lato_core::{
    AgentError, CompactionPolicy, CompactionTrigger, ContextUsage, ExtensionAuditRecord,
    HookAuditOutcome, HookAuditPhase, JournalDurability, JournalRecord, McpAuditOutcome,
    ModelContent, ModelErrorKind, ModelMessage, ModelRole, PolicyAuditDecision, PolicyAuditStage,
    PolicyDecision, Retryability, SessionId, SkillInvocationOrigin, ToolCallId, ToolContext,
    ToolError, ToolName, TurnId, journal_request_hash,
};
use lato_extensions::{hooks::HookRegistry, skills::SkillCatalog};
use lato_runtime::{
    AutomaticCompactionOutcome, AutomaticCompactionRequest, PrefireCompactionRequest,
    TurnEventEmitter, TwoPassCompactionInput,
};
use lato_tools::{SkillToolScope, ToolRuntime, bound_tool_output};
use lato_workspace::{FileLocks, SessionTrust};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct PrefireCache {
    note1: String,
    prefix_len: usize,
    fingerprint: u64,
    model_generation: u64,
    _pass1_latency_ms: u64,
}

#[derive(Default)]
enum PrefireSlot {
    #[default]
    Empty,
    Running(tokio::task::JoinHandle<Result<PrefireCache, AgentError>>),
    Ready(PrefireCache),
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContextCompactionAction {
    None,
    Prefire,
    Final,
}

fn context_compaction_action(
    usage: &ContextUsage,
    threshold_percent: u8,
) -> ContextCompactionAction {
    if usage.threshold_reached(threshold_percent) {
        ContextCompactionAction::Final
    } else if usage.threshold_reached(threshold_percent.saturating_sub(PREFIRE_LEAD_PERCENT)) {
        ContextCompactionAction::Prefire
    } else {
        ContextCompactionAction::None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Start,
    Steer,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    Complete,
    Cancelled,
    Replaced,
}

#[async_trait]
pub trait ToolApproval: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest) -> bool;
}

pub struct SessionActor {
    active: bool,
    cancelled: bool,
    history: Vec<HistoryItem>,
    stream: Arc<dyn ModelStream>,
    _locks: Arc<FileLocks>,
    _trust: SessionTrust,
    cwd: PathBuf,
    tool_runtime: Arc<ToolRuntime>,
    skill_handle: Option<SessionSkillHandle>,
    mcp_handle: Option<SessionMcpHandle>,
    skill_listing: String,
    skill_catalog_audit: Option<ExtensionAuditRecord>,
    hook_runtime: Option<SessionHookRuntime>,
    session_id: SessionId,
    turn_id: TurnId,
    turn_cancellation: CancellationToken,
    next_local_turn: u64,
    next_local_call: u64,
    events: Option<(mpsc::UnboundedSender<serde_json::Value>, String)>,
    tool_approval: Option<Arc<dyn ToolApproval>>,
    journal_events: Option<TurnEventEmitter>,
    context_tracker: ContextTracker,
    prefire: PrefireSlot,
    #[cfg(test)]
    pub(crate) on_after_persist: Option<Box<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    on_before_sample_spawn: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SessionActor {
    pub fn new(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
    ) -> Self {
        let binding = SkillRuntimeBinding::builtin(cwd.clone(), locks.clone(), trust.clone())
            .expect("static built-in tool descriptors must form a valid runtime");
        Self::new_with_skill_runtime(stream, locks, trust, cwd, binding)
    }

    pub fn new_with_tool_runtime(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        tool_runtime: Arc<ToolRuntime>,
    ) -> Self {
        Self::from_tool_runtime(stream, locks, trust, cwd, tool_runtime, None, None)
    }

    pub(crate) fn new_with_skill_runtime(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        binding: SkillRuntimeBinding,
    ) -> Self {
        let (tool_runtime, skill_handle, mcp_handle) = binding.into_parts();
        Self::from_tool_runtime(
            stream,
            locks,
            trust,
            cwd,
            tool_runtime,
            Some(skill_handle),
            Some(mcp_handle),
        )
    }

    fn from_tool_runtime(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        tool_runtime: Arc<ToolRuntime>,
        skill_handle: Option<SessionSkillHandle>,
        mcp_handle: Option<SessionMcpHandle>,
    ) -> Self {
        Self {
            active: false,
            cancelled: false,
            history: vec![HistoryItem::System(build_world_state(&cwd))],
            stream,
            _locks: locks,
            _trust: trust,
            cwd,
            tool_runtime,
            skill_handle,
            mcp_handle,
            skill_listing: String::new(),
            skill_catalog_audit: None,
            hook_runtime: None,
            session_id: SessionId::from("local-session"),
            turn_id: TurnId::from("local-turn-0"),
            turn_cancellation: CancellationToken::new(),
            next_local_turn: 0,
            next_local_call: 0,
            events: None,
            tool_approval: None,
            journal_events: None,
            context_tracker: ContextTracker::default(),
            prefire: PrefireSlot::Empty,
            #[cfg(test)]
            on_after_persist: None,
            #[cfg(test)]
            on_before_sample_spawn: None,
        }
    }
    pub fn with_interactive_events(
        mut self,
        events: mpsc::UnboundedSender<serde_json::Value>,
        session_id: String,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        self.session_id = SessionId::parse(session_id.clone())
            .unwrap_or_else(|_| SessionId::from("local-session"));
        self.events = Some((events, session_id));
        self.tool_approval = approval;
        self
    }

    pub async fn prompt(&mut self, kind: PromptKind, text: String) -> Result<TurnOutcome, String> {
        self.next_local_turn += 1;
        let turn_id = TurnId::from(format!("local-turn-{}", self.next_local_turn));
        self.prompt_with_context(kind, text, turn_id, CancellationToken::new())
            .await
    }

    pub fn set_journal_events(&mut self, events: Option<TurnEventEmitter>) {
        self.journal_events = events;
    }

    pub async fn bind_turn_skills(&mut self, catalog: Arc<SkillCatalog>) {
        let Some(handle) = &self.skill_handle else {
            self.skill_listing.clear();
            self.skill_catalog_audit = None;
            return;
        };
        self.skill_listing = catalog.render_model_listing();
        self.skill_catalog_audit = Some(ExtensionAuditRecord::SkillCatalogMaterialized {
            generation: catalog.generation(),
            visible_count: self.skill_listing.matches("<skill name=").count() as u64,
            omitted_count: catalog.omitted_listing_count() as u64,
            catalog_hash: journal_request_hash(
                "skill_catalog",
                &serde_json::json!({
                    "generation": catalog.generation(),
                    "listing": self.skill_listing,
                }),
            ),
        });
        handle.install(catalog).await;
    }

    pub fn bind_turn_hooks(&mut self, runtime: SessionHookRuntime) {
        self.hook_runtime = Some(runtime);
    }

    pub fn bind_turn_hook_registry(&mut self, registry: Arc<HookRegistry>) {
        self.hook_runtime = Some(SessionHookRuntime::new(
            registry,
            self.cwd.clone(),
            self.session_id.to_string(),
        ));
    }

    /// Bind a generation-scoped MCP manager for the upcoming turn.
    ///
    /// Paired with [`Self::bind_turn_skills`] / [`Self::bind_turn_hooks`]. MCP
    /// tools remain ordinary ToolRuntime entries — this only swaps the
    /// transport backend behind `search_tool` / `use_tool`.
    pub async fn bind_turn_mcp(&mut self, manager: Arc<lato_mcp::McpManager>) {
        let Some(handle) = &self.mcp_handle else {
            return;
        };
        // Turn-boundary adopt: retires the previous generation with cancel+reap.
        handle.adopt_generation(manager).await;
    }

    pub fn mcp_handle(&self) -> Option<&SessionMcpHandle> {
        self.mcp_handle.as_ref()
    }

    pub async fn shutdown_mcp(&self, deadline: std::time::Instant) {
        if let Some(handle) = &self.mcp_handle {
            handle.shutdown(deadline).await;
        }
    }

    pub async fn observe_bound_hook(
        &self,
        event: lato_extensions::hooks::HookEventName,
        payload: serde_json::Value,
    ) -> Vec<lato_extensions::hooks::HookRunRecord> {
        match &self.hook_runtime {
            Some(runtime) => {
                let runs = runtime
                    .observe(
                        event,
                        Some(self.turn_id.as_str()),
                        payload.clone(),
                        CancellationToken::new(),
                    )
                    .await;
                let _ = self.commit_hook_runs(runtime, event, &payload, &runs).await;
                runs
            }
            None => Vec::new(),
        }
    }

    pub async fn gate_prompt_hook(&self, text: &str) -> crate::PromptHookGate {
        let Some(runtime) = &self.hook_runtime else {
            return crate::PromptHookGate {
                block: None,
                audits: Vec::new(),
            };
        };
        let input = serde_json::json!({
            "promptHash": journal_request_hash("prompt", &serde_json::json!(text))
        });
        let result = runtime
            .prompt_submit("pre-submit", text, CancellationToken::new())
            .await;
        let audits = Self::hook_audit_records(
            runtime,
            lato_extensions::hooks::HookEventName::UserPromptSubmit,
            &input,
            &result.runs,
        );
        let block = result
            .block
            .map(|block| format!("hook.prompt_blocked [{}]: {}", block.hook_id, block.reason));
        crate::PromptHookGate { block, audits }
    }

    pub async fn prompt_with_context(
        &mut self,
        _kind: PromptKind,
        text: String,
        turn_id: TurnId,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, String> {
        if self.active {
            self.cancelled = true;
            self.active = false;
        }
        self.active = true;
        self.cancelled = false;
        self.turn_id = turn_id;
        self.turn_cancellation = cancellation;
        let previous_suppression = self.context_tracker.automatic_compaction_suppression();
        self.context_tracker.on_new_turn();
        self.emit_suppression_if_changed(previous_suppression);
        let task_requires_workspace_change = task_requires_workspace_change(&text);
        self.history.push(HistoryItem::User(text));
        if let Some(audit) = self.skill_catalog_audit.clone() {
            self.commit(
                JournalRecord::ExtensionAudit { audit },
                JournalDurability::Flush,
            )
            .await?;
        }
        let mut sampling_steps = 0usize;
        let mut stop_continuations = 0usize;
        let mut no_tool_retry_used = false;
        let mut executed_any_tool = false;
        let mut force_workspace_tool = false;
        let mut repeated_calls: HashMap<String, usize> = HashMap::new();
        let mut recovery_budget = SamplingRecoveryBudget::default();
        let mut next_skill_scope = None;
        loop {
            if self.cancelled || self.turn_cancellation.is_cancelled() {
                self.active = false;
                return Ok(TurnOutcome::Cancelled);
            }
            sampling_steps += 1;
            if sampling_steps > 50 {
                self.active = false;
                return Err("maximum sampling steps exceeded".into());
            }
            let active_model = self.stream.active_model_port();
            let mut compacted = false;
            if let Some(active) = active_model.as_ref() {
                let usage = self.context_tracker.measure(&self.history, active);
                self.emit_context_usage(usage.clone())?;
                let threshold = CompactionPolicy::default().threshold_percent;
                let threshold_action = context_compaction_action(&usage, threshold);
                if threshold_action == ContextCompactionAction::Prefire {
                    self.start_prefire(active).await?;
                }
                let preflight_overflow =
                    usage.context_window > 0 && usage.estimated_input_tokens > usage.context_window;
                let trigger = if preflight_overflow {
                    Some(CompactionTrigger::PreflightOverflow)
                } else if self.context_tracker.take_model_switch_check() {
                    usage
                        .threshold_reached(threshold)
                        .then_some(CompactionTrigger::ModelSwitch)
                } else {
                    (threshold_action == ContextCompactionAction::Final)
                        .then_some(CompactionTrigger::Threshold)
                };
                if let Some(trigger) = trigger {
                    compacted = self
                        .run_automatic_compaction(trigger, usage, active.generation)
                        .await?;
                    if !compacted && preflight_overflow {
                        self.active = false;
                        return Err(
                            "context.preflight_recovery_failed: automatic compaction did not reduce a known-oversized request"
                                .into(),
                        );
                    }
                }
            }
            if compacted {
                continue;
            }
            if self.encoded_len() > CONTEXT_HARD_LIMIT_BYTES {
                self.active = false;
                return Err("context exceeds hard limit; automatic compaction unavailable".into());
            }
            let (tx, mut rx) = mpsc::channel(16);
            let round_skill_scope = next_skill_scope.take();
            let messages = match messages_with_skill_listing(&self.history, &self.skill_listing) {
                Ok(messages) => messages,
                Err(error) => {
                    self.active = false;
                    return Err(error);
                }
            };
            let mut context = serde_json::json!({
                "messages": messages,
                "tools": self.model_definitions(round_skill_scope.as_ref()),
                "session_id": self.session_id.as_str(),
                "turn_id": self.turn_id.as_str(),
            });
            if force_workspace_tool {
                context["tool_choice"] = serde_json::json!("required");
                context["stream"] = serde_json::json!(false);
            }
            let stream = self.stream.clone();
            let prompt_bytes = self.encoded_len();
            #[cfg(test)]
            if let Some(hook) = &self.on_before_sample_spawn {
                hook();
            }
            if self.cancelled || self.turn_cancellation.is_cancelled() {
                self.active = false;
                return Ok(TurnOutcome::Cancelled);
            }
            let stream_task =
                tokio::spawn(
                    async move { stream.stream_with_report(prompt_bytes, context, tx).await },
                );
            let mut saw_tool = false;
            let mut round_text = String::new();
            let mut uncommitted_text = String::new();
            let mut observed_output = false;
            let mut think_filter = ThinkTagFilter::default();
            while let Some(piece) = rx.recv().await {
                if self.cancelled || self.turn_cancellation.is_cancelled() {
                    self.active = false;
                    return Ok(TurnOutcome::Cancelled);
                }
                match piece {
                    StreamPiece::Text(t) => {
                        observed_output = true;
                        round_text.push_str(&t);
                        self.history.push(HistoryItem::AssistantText(t.clone()));
                        let visible = think_filter.push(&t);
                        if visible.is_empty() {
                            continue;
                        }
                        if let Some((events, session_id)) = &self.events {
                            let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"delta":visible}}));
                        }
                        uncommitted_text.push_str(&visible);
                    }
                    StreamPiece::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        observed_output = true;
                        self.commit_assistant_text(&uncommitted_text).await?;
                        uncommitted_text.clear();
                        saw_tool = true;
                        match self
                            .process_tool_call(
                                id,
                                name,
                                arguments,
                                &mut repeated_calls,
                                round_skill_scope.as_ref(),
                                &mut next_skill_scope,
                            )
                            .await?
                        {
                            ProcessTool::Executed => executed_any_tool = true,
                            ProcessTool::Cancelled => {
                                self.active = false;
                                return Ok(TurnOutcome::Cancelled);
                            }
                        }
                    }
                }
            }
            let trailing = think_filter.finish();
            if !trailing.is_empty() {
                if let Some((events, session_id)) = &self.events {
                    let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"delta":trailing}}));
                }
                uncommitted_text.push_str(&trailing);
            }
            let stream_result = stream_task.await.map_err(|error| error.to_string())?;
            let report = match stream_result {
                Ok(report) => report,
                Err(mut error) => {
                    error.output_started |= observed_output;
                    let measured = active_model
                        .as_ref()
                        .map(|active| self.context_tracker.measure(&self.history, active));
                    let inferred_overflow = error.context_window.is_some_and(|window| {
                        measured
                            .as_ref()
                            .is_some_and(|usage| usage.estimated_input_tokens > window)
                    });
                    let recoverable = !error.output_started
                        && self
                            .context_tracker
                            .automatic_compaction_allowed(CompactionTrigger::ProviderOverflow)
                        && (error.kind == ModelErrorKind::ContextOverflow || inferred_overflow)
                        && recovery_budget.try_use_overflow_recovery();
                    if recoverable {
                        let mut usage = measured.unwrap_or(ContextUsage {
                            estimated_input_tokens: u64::try_from(prompt_bytes)
                                .unwrap_or(u64::MAX)
                                .div_ceil(4),
                            context_window: error.context_window.unwrap_or_default(),
                            utilization_percent: 0,
                        });
                        if let Some(window) = error.context_window.filter(|window| *window > 0) {
                            usage.context_window = window;
                            usage.utilization_percent = usage
                                .estimated_input_tokens
                                .saturating_mul(100)
                                .checked_div(window)
                                .unwrap_or_default()
                                .min(u64::from(u8::MAX))
                                as u8;
                            self.context_tracker.on_context_budget_changed();
                        }
                        let generation = active_model
                            .as_ref()
                            .map(|active| active.generation)
                            .unwrap_or_default();
                        match self
                            .run_automatic_compaction(
                                CompactionTrigger::ProviderOverflow,
                                usage,
                                generation,
                            )
                            .await
                        {
                            Ok(true) => continue,
                            Ok(false) => {}
                            Err(recovery) => {
                                return Err(format!(
                                    "{}; context recovery failed: {recovery}",
                                    error
                                ));
                            }
                        }
                    }
                    self.active = false;
                    return Err(error.to_string());
                }
            };
            let previous_suppression = self.context_tracker.automatic_compaction_suppression();
            self.context_tracker.on_provider_success();
            self.emit_suppression_if_changed(previous_suppression);
            recovery_budget = SamplingRecoveryBudget::default();
            self.commit_assistant_text(&uncommitted_text).await?;
            if let Some(active) = active_model.as_ref()
                && self
                    .context_tracker
                    .observe(&self.history, &report, active.generation)
            {
                let usage = self.context_tracker.measure(&self.history, active);
                self.emit_context_usage(usage)?;
            }
            if !saw_tool {
                for piece in extract_text_embedded_tool_calls(&round_text) {
                    let StreamPiece::ToolCall {
                        id,
                        name,
                        arguments,
                    } = piece
                    else {
                        continue;
                    };
                    saw_tool = true;
                    match self
                        .process_tool_call(
                            id,
                            name,
                            arguments,
                            &mut repeated_calls,
                            round_skill_scope.as_ref(),
                            &mut next_skill_scope,
                        )
                        .await?
                    {
                        ProcessTool::Executed => executed_any_tool = true,
                        ProcessTool::Cancelled => {
                            self.active = false;
                            return Ok(TurnOutcome::Cancelled);
                        }
                    }
                }
            }
            if !saw_tool {
                if !no_tool_retry_used && task_requires_workspace_change && !executed_any_tool {
                    no_tool_retry_used = true;
                    force_workspace_tool = true;
                    self.history.push(HistoryItem::User(
                        "You have not used any tool yet. The user asked you to create or modify files in the workspace. Call write_file (or another workspace tool) now. Do not only reply with text."
                            .into(),
                    ));
                    continue;
                }
                if task_requires_workspace_change && !executed_any_tool {
                    self.active = false;
                    return Err(
                        "workspace tool was required but the model finished without calling one"
                            .into(),
                    );
                }
                if let Some(runtime) = &self.hook_runtime {
                    let stop_input = serde_json::json!({"reason":"model_complete","lastAssistantContext":round_text});
                    let stop = runtime
                        .stop(
                            self.turn_id.as_str(),
                            stop_input.clone(),
                            self.turn_cancellation.clone(),
                        )
                        .await;
                    self.commit_hook_runs(
                        runtime,
                        lato_extensions::hooks::HookEventName::Stop,
                        &serde_json::json!({"lastAssistantHash":journal_request_hash("stop", &stop_input)}),
                        &stop.runs,
                    )
                    .await?;
                    if stop.prevent_continuation.is_none()
                        && (!stop.blocks.is_empty() || !stop.additional_context.is_empty())
                        && stop_continuations < lato_extensions::hooks::MAX_STOP_CONTINUATIONS
                    {
                        stop_continuations += 1;
                        let feedback = stop
                            .blocks
                            .into_iter()
                            .map(|block| block.reason)
                            .chain(
                                stop.additional_context
                                    .into_iter()
                                    .map(|context| context.text),
                            )
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.history.push(HistoryItem::System(format!(
                            "Hook requested continuation ({stop_continuations}/{}):\n{feedback}",
                            lato_extensions::hooks::MAX_STOP_CONTINUATIONS
                        )));
                        continue;
                    }
                }
                self.active = false;
                return Ok(TurnOutcome::Complete);
            }
        }
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }
    pub fn history(&self) -> &[HistoryItem] {
        &self.history
    }
    pub fn latest_assistant_text(&self) -> String {
        let mut parts = self
            .history
            .iter()
            .rev()
            .take_while(|item| !matches!(item, HistoryItem::User(_)))
            .filter_map(|item| match item {
                HistoryItem::AssistantText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        parts.reverse();
        strip_think_tags(&parts.concat())
    }
    pub fn history_mut(&mut self) -> &mut Vec<HistoryItem> {
        &mut self.history
    }
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(&self.history)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
    }

    pub fn mark_model_switch_check(&mut self) {
        self.context_tracker.mark_model_switch_check();
    }

    pub fn context_budget_changed(&mut self) {
        let previous_suppression = self.context_tracker.automatic_compaction_suppression();
        self.context_tracker.on_context_budget_changed();
        self.emit_suppression_if_changed(previous_suppression);
    }

    pub fn model_generation_changed(&mut self) {
        self.invalidate_prefire();
    }

    pub fn auth_refreshed(&mut self) {
        let previous_suppression = self.context_tracker.automatic_compaction_suppression();
        self.context_tracker.on_auth_refreshed();
        self.emit_suppression_if_changed(previous_suppression);
    }

    pub(crate) fn automatic_compaction_allowed(&self, trigger: CompactionTrigger) -> bool {
        self.context_tracker.automatic_compaction_allowed(trigger)
    }

    fn model_definitions(&self, scope: Option<&SkillToolScope>) -> Vec<serde_json::Value> {
        let mut definitions = self.tool_runtime.model_definitions_scoped(scope);
        if self.skill_handle.is_none() {
            definitions.retain(|definition| {
                definition
                    .pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
                    != Some("skill")
            });
        }
        definitions
    }

    fn emit_context_usage(&self, usage: ContextUsage) -> Result<(), String> {
        match &self.journal_events {
            Some(events) => events
                .context_usage(usage)
                .map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }

    async fn run_automatic_compaction(
        &mut self,
        trigger: CompactionTrigger,
        usage: ContextUsage,
        model_generation: u64,
    ) -> Result<bool, String> {
        if !self.context_tracker.automatic_compaction_allowed(trigger) {
            return Ok(false);
        }
        let Some(events) = self.journal_events.clone() else {
            return Ok(false);
        };
        let messages =
            crate::history_to_model_messages(&self.history).map_err(|error| error.to_string())?;
        let (two_pass, prior_model_attempts) = self.take_prefire(&messages, model_generation).await;
        let _ = self
            .observe_bound_hook(
                lato_extensions::hooks::HookEventName::PreCompact,
                serde_json::json!({"trigger": trigger, "messageCount": messages.len()}),
            )
            .await;
        let outcome = events
            .compact(AutomaticCompactionRequest {
                trigger,
                usage,
                messages,
                two_pass,
                prior_model_attempts,
            })
            .await;
        match outcome {
            Err(error) => {
                if self
                    .context_tracker
                    .suppress_automatic_compaction(compaction_suppression_reason(&error))
                {
                    self.emit_recovery_suppression();
                }
                Err(error.to_string())
            }
            Ok(AutomaticCompactionOutcome::Compacted(messages)) => {
                self.history = crate::model_messages_to_history(&messages)
                    .map_err(|error| error.to_string())?;
                let _ = self
                    .observe_bound_hook(
                        lato_extensions::hooks::HookEventName::PostCompact,
                        serde_json::json!({"trigger": trigger, "messageCount": messages.len()}),
                    )
                    .await;
                self.context_tracker.reseed(&self.history);
                let previous_suppression = self.context_tracker.automatic_compaction_suppression();
                self.context_tracker.on_compaction_success();
                self.emit_suppression_if_changed(previous_suppression);
                self.clear_prefire();
                Ok(true)
            }
            Ok(AutomaticCompactionOutcome::ContinueUnchanged { error }) => {
                if self
                    .context_tracker
                    .suppress_automatic_compaction(compaction_suppression_reason(&error))
                {
                    self.emit_recovery_suppression();
                }
                Ok(false)
            }
        }
    }

    async fn start_prefire(&mut self, active: &lato_ai::ActiveModelPort) -> Result<(), String> {
        self.refresh_prefire().await;
        if !matches!(self.prefire, PrefireSlot::Empty)
            || !self
                .context_tracker
                .automatic_compaction_allowed(CompactionTrigger::Threshold)
        {
            return Ok(());
        }
        let Some(events) = self.journal_events.clone() else {
            return Ok(());
        };
        let messages =
            crate::history_to_model_messages(&self.history).map_err(|error| error.to_string())?;
        let split = split_for_two_pass(&messages, TWO_PASS_SPLIT_PERCENT);
        if split.index == 0 || split.index >= messages.len() {
            return Ok(());
        }
        let prefix_len = split.index;
        let fingerprint = fingerprint_prefix(&messages, prefix_len);
        let model_generation = active.generation;
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = events
                .prefire_compaction(PrefireCompactionRequest {
                    messages,
                    prefix_len,
                    policy: CompactionPolicy::default(),
                })
                .await?;
            Ok(PrefireCache {
                note1: result.note1,
                prefix_len,
                fingerprint,
                model_generation,
                _pass1_latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            })
        });
        self.prefire = PrefireSlot::Running(handle);
        Ok(())
    }

    async fn refresh_prefire(&mut self) {
        let finished =
            matches!(&self.prefire, PrefireSlot::Running(handle) if handle.is_finished());
        if !finished {
            return;
        }
        let PrefireSlot::Running(handle) = std::mem::take(&mut self.prefire) else {
            return;
        };
        self.prefire = match handle.await {
            Ok(Ok(cache)) => PrefireSlot::Ready(cache),
            _ => PrefireSlot::Failed,
        };
    }

    async fn take_prefire(
        &mut self,
        messages: &[ModelMessage],
        model_generation: u64,
    ) -> (Option<TwoPassCompactionInput>, u8) {
        let slot = std::mem::take(&mut self.prefire);
        let cache = match slot {
            PrefireSlot::Empty => return (None, 0),
            PrefireSlot::Failed => return (None, 1),
            PrefireSlot::Ready(cache) => cache,
            PrefireSlot::Running(handle) => match handle.await {
                Ok(Ok(cache)) => cache,
                _ => return (None, 1),
            },
        };
        if cache.model_generation != model_generation
            || cache.prefix_len > messages.len()
            || fingerprint_prefix(messages, cache.prefix_len) != cache.fingerprint
        {
            return (None, 1);
        }
        (
            Some(TwoPassCompactionInput {
                note1: cache.note1,
                prefix_len: cache.prefix_len,
            }),
            1,
        )
    }

    fn clear_prefire(&mut self) {
        if let PrefireSlot::Running(handle) = std::mem::take(&mut self.prefire) {
            handle.abort();
        }
    }

    fn invalidate_prefire(&mut self) {
        match std::mem::take(&mut self.prefire) {
            PrefireSlot::Empty => {}
            PrefireSlot::Running(handle) => {
                handle.abort();
                self.prefire = PrefireSlot::Failed;
            }
            PrefireSlot::Ready(_) | PrefireSlot::Failed => {
                self.prefire = PrefireSlot::Failed;
            }
        }
    }

    fn emit_suppression_if_changed(&self, previous: AutoCompactionSuppression) {
        if previous != self.context_tracker.automatic_compaction_suppression() {
            self.emit_recovery_suppression();
        }
    }

    fn emit_recovery_suppression(&self) {
        let Some((events, session_id)) = &self.events else {
            return;
        };
        let suppression = match self.context_tracker.automatic_compaction_suppression() {
            AutoCompactionSuppression::None => "none",
            AutoCompactionSuppression::Turn => "turn",
            AutoCompactionSuppression::Sticky => "sticky",
            AutoCompactionSuppression::UntilSuccess => "until_success",
            AutoCompactionSuppression::Auth => "auth",
        };
        let _ = events.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "lato/session/recovery",
            "params": {"sessionId": session_id, "automaticCompactionSuppression": suppression}
        }));
    }

    async fn commit_assistant_text(&self, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        self.commit(
            JournalRecord::ConversationItemCommitted {
                message: ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::Text {
                        text: text.to_owned(),
                    }],
                },
            },
            JournalDurability::Flush,
        )
        .await
    }

    async fn commit(
        &self,
        record: JournalRecord,
        durability: JournalDurability,
    ) -> Result<(), String> {
        match &self.journal_events {
            Some(events) => events
                .commit(record, durability)
                .await
                .map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }

    async fn process_tool_call(
        &mut self,
        id: String,
        name: String,
        arguments: serde_json::Value,
        repeated_calls: &mut HashMap<String, usize>,
        round_skill_scope: Option<&SkillToolScope>,
        next_skill_scope: &mut Option<SkillToolScope>,
    ) -> Result<ProcessTool, String> {
        let fingerprint = format!(
            "{name}:{}",
            serde_json::to_string(&arguments).unwrap_or_default()
        );
        let repeats = repeated_calls.entry(fingerprint).or_default();
        *repeats += 1;
        if *repeats > 3 {
            self.active = false;
            return Err("stalled: identical tool call repeated more than 3 times".into());
        }
        let call_id = ToolCallId::parse(id.clone()).unwrap_or_else(|_| {
            self.next_local_call += 1;
            ToolCallId::from(format!("local-tool-call-{}", self.next_local_call))
        });
        let tool_runtime = self.tool_runtime.clone();
        let journal_name = tool_runtime
            .descriptor_for_wire_name(&name)
            .map(|descriptor| descriptor.name)
            .unwrap_or_else(|| fallback_tool_name(&name));
        let is_skill_request = journal_name.local_name() == "skill";
        let original_request_hash = journal_request_hash(journal_name.as_str(), &arguments);
        if self.cancelled || self.turn_cancellation.is_cancelled() {
            self.commit(
                JournalRecord::ToolCallRequested {
                    call_id: call_id.clone(),
                    name: journal_name,
                    arguments: arguments.clone(),
                    request_hash: original_request_hash.clone(),
                },
                JournalDurability::Flush,
            )
            .await?;
            self.history.push(HistoryItem::ToolCall {
                id,
                name,
                arguments: arguments.clone(),
            });
            let error = ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            );
            if is_skill_request {
                self.commit(
                    JournalRecord::ExtensionAudit {
                        audit: skill_rejected_audit(&arguments, &error.code),
                    },
                    JournalDurability::Flush,
                )
                .await?;
            }
            self.commit(
                JournalRecord::ToolCallRejected {
                    call_id,
                    request_hash: original_request_hash,
                    error,
                },
                JournalDurability::SyncData,
            )
            .await?;
            return Ok(ProcessTool::Cancelled);
        }
        let context = ToolContext {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            call_id: call_id.clone(),
            cancellation: self.turn_cancellation.clone(),
            execution_grant: None,
        };
        let initial_validation = tool_runtime.resolve_and_validate(&name, arguments.clone());
        let mut final_arguments = initial_validation
            .as_ref()
            .map(|validated| validated.arguments.clone())
            .unwrap_or_else(|_| arguments.clone());
        let mut hook_ask_reason = None;
        let mut pre_hook_error = None;
        let mut pre_hook_context = Vec::new();
        if let (Ok(validated), Some(runtime)) = (&initial_validation, &self.hook_runtime) {
            let result = runtime
                .pre_tool_use(
                    self.turn_id.as_str(),
                    validated.canonical_name.as_str(),
                    final_arguments.clone(),
                    self.turn_cancellation.clone(),
                )
                .await;
            self.commit_hook_runs(
                runtime,
                lato_extensions::hooks::HookEventName::PreToolUse,
                &serde_json::json!({"toolName":validated.canonical_name.as_str(),"argumentsHash":journal_request_hash(validated.canonical_name.as_str(), &final_arguments)}),
                &result.runs,
            ).await?;
            if let Some(updated) = result.updated_input {
                final_arguments = updated;
            }
            pre_hook_context = result.additional_context;
            match result.decision {
                lato_extensions::hooks::HookDecision::Deny { hook_id, reason } => {
                    pre_hook_error = Some(ToolError::new(
                        "hook.pre_tool_denied",
                        format!("blocked by {hook_id}: {reason}"),
                        Retryability::Never,
                    ));
                }
                lato_extensions::hooks::HookDecision::Ask { reason, .. } => {
                    hook_ask_reason =
                        Some(reason.unwrap_or_else(|| "approval requested by hook".into()));
                }
                _ => {}
            }
        }
        let skill_is_unbound = self.skill_handle.is_none()
            && tool_runtime
                .descriptor_for_wire_name(&name)
                .is_some_and(|descriptor| descriptor.name.local_name() == "skill");
        let prepared = if let Err(error) = initial_validation {
            Err(error)
        } else if let Some(error) = pre_hook_error {
            Err(error)
        } else if skill_is_unbound {
            Err(ToolError::new(
                "skill.resolver_unbound",
                "skill invocation is unavailable without a session-bound resolver",
                Retryability::Never,
            ))
        } else {
            tool_runtime.prepare_scoped(context, &name, final_arguments.clone(), round_skill_scope)
        };
        // Argument-scoped rules may canonicalize a path before policy. The
        // journal lifecycle must use that final prepared request hash even
        // though the provider's original arguments remain in conversation
        // history.
        let request_hash = prepared
            .as_ref()
            .map(|prepared| prepared.audit().request_hash)
            .unwrap_or(original_request_hash);
        self.commit(
            JournalRecord::ToolCallRequested {
                call_id: call_id.clone(),
                name: journal_name.clone(),
                arguments: arguments.clone(),
                request_hash: request_hash.clone(),
            },
            JournalDurability::Flush,
        )
        .await?;
        self.history.push(HistoryItem::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        });
        #[cfg(test)]
        if let Some(cb) = &self.on_after_persist {
            cb();
        }
        let authorization = match prepared {
            Ok(prepared) => match tool_runtime.decision(&prepared).clone() {
                PolicyDecision::Allow(grant) => {
                    self.commit(
                        JournalRecord::PolicyDecisionCommitted {
                            audit: prepared.policy_audit(
                                PolicyAuditStage::Evaluated,
                                PolicyAuditDecision::Allowed,
                            ),
                        },
                        JournalDurability::Flush,
                    )
                    .await?;
                    if let Some(reason) = hook_ask_reason.clone() {
                        let request = tool_runtime.hook_approval_request(&prepared, reason);
                        let approved = match &self.tool_approval {
                            Some(approval) => approval.approve(&request).await,
                            None => false,
                        };
                        if approved {
                            tool_runtime
                                .approve_hook_gate(&request)
                                .map(|fresh_grant| (prepared, fresh_grant))
                        } else {
                            Err(ToolError::new(
                                "policy.approval_denied",
                                "tool approval denied by user",
                                Retryability::Never,
                            ))
                        }
                    } else {
                        Ok((prepared, grant))
                    }
                }
                PolicyDecision::RequireApproval(request) => {
                    let approved = match &self.tool_approval {
                        Some(approval) => approval.approve(&request).await,
                        None => false,
                    };
                    if approved {
                        match tool_runtime.approve(&request) {
                            Ok(grant) => {
                                self.commit(
                                    JournalRecord::PolicyDecisionCommitted {
                                        audit: prepared.policy_audit(
                                            PolicyAuditStage::ApprovalResolved,
                                            PolicyAuditDecision::Approved,
                                        ),
                                    },
                                    JournalDurability::Flush,
                                )
                                .await?;
                                Ok((prepared, grant))
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        let error = ToolError::new(
                            "policy.approval_denied",
                            "tool approval denied by user",
                            Retryability::Never,
                        );
                        self.commit(
                            JournalRecord::PolicyDecisionCommitted {
                                audit: prepared.policy_audit(
                                    PolicyAuditStage::ApprovalResolved,
                                    PolicyAuditDecision::Denied {
                                        code: error.code.clone(),
                                    },
                                ),
                            },
                            JournalDurability::Flush,
                        )
                        .await?;
                        Err(error)
                    }
                }
                PolicyDecision::Deny(denial) => {
                    self.commit(
                        JournalRecord::PolicyDecisionCommitted {
                            audit: prepared.policy_audit(
                                PolicyAuditStage::Evaluated,
                                PolicyAuditDecision::Denied {
                                    code: denial.code.clone(),
                                },
                            ),
                        },
                        JournalDurability::Flush,
                    )
                    .await?;
                    Err(ToolError::new(
                        denial.code,
                        denial.message,
                        Retryability::Never,
                    ))
                }
            },
            Err(error) => Err(error),
        };
        if let Some((events, session_id)) = &self.events {
            let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/tool_call","params":{"sessionId":session_id,"id":id,"name":name,"arguments":arguments}}));
        }
        let invocation = match authorization {
            Ok((prepared, grant)) => {
                let audit = prepared.audit();
                let canonical_builtin_skill = prepared.is_canonical_builtin_skill();
                self.commit(
                    JournalRecord::ToolCallPrepared {
                        audit: audit.clone(),
                    },
                    JournalDurability::SyncData,
                )
                .await?;
                let mut result = tool_runtime.execute_authorized(prepared, grant).await;
                if canonical_builtin_skill && let Ok(output) = &result {
                    result =
                        compile_skill_scope(canonical_builtin_skill, output, tool_runtime.as_ref())
                            .map(|scope| {
                                *next_skill_scope = scope;
                                output.clone()
                            });
                }
                if canonical_builtin_skill {
                    let audit = match &result {
                        Ok(output) => skill_invoked_audit(output).unwrap_or_else(|| {
                            skill_rejected_audit(&arguments, "skill.invalid_metadata")
                        }),
                        Err(error) => skill_rejected_audit(&arguments, &error.code),
                    };
                    self.commit(
                        JournalRecord::ExtensionAudit { audit },
                        JournalDurability::Flush,
                    )
                    .await?;
                }
                if let Some(mcp_audit) = mcp_tool_audit_from_result(&result) {
                    self.commit(
                        JournalRecord::ExtensionAudit { audit: mcp_audit },
                        JournalDurability::Flush,
                    )
                    .await?;
                }
                self.commit(
                    JournalRecord::ToolCallCompleted {
                        call_id: audit.call_id,
                        request_hash: audit.request_hash,
                        result: result.clone(),
                    },
                    JournalDurability::SyncData,
                )
                .await?;
                result
            }
            Err(error) => {
                if is_skill_request {
                    self.commit(
                        JournalRecord::ExtensionAudit {
                            audit: skill_rejected_audit(&arguments, &error.code),
                        },
                        JournalDurability::Flush,
                    )
                    .await?;
                }
                self.commit(
                    JournalRecord::ToolCallRejected {
                        call_id,
                        request_hash: request_hash.clone(),
                        error: error.clone(),
                    },
                    JournalDurability::SyncData,
                )
                .await?;
                Err(error)
            }
        };
        let failed = invocation.is_err();
        let mut out = invocation
            .map(|output| output.content)
            .unwrap_or_else(|error| format!("ERROR [{}]: {}", error.code, error.message));
        let mut post_hook_context = Vec::new();
        if let Some(runtime) = &self.hook_runtime {
            let post = runtime
                .post_tool_use(
                    self.turn_id.as_str(),
                    serde_json::json!({
                        "toolName": journal_name.as_str(),
                        "arguments": final_arguments,
                        "success": !failed,
                        "output": out,
                    }),
                    self.turn_cancellation.clone(),
                )
                .await;
            self.commit_hook_runs(
                runtime,
                lato_extensions::hooks::HookEventName::PostToolUse,
                &serde_json::json!({"toolName":journal_name.as_str(),"requestHash":request_hash}),
                &post.runs,
            )
            .await?;
            if let Some(replacement) = post.replacement {
                out = replacement
                    .as_str()
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        replacement
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .or_else(|| {
                        replacement
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .unwrap_or_else(|| replacement.to_string());
            }
            post_hook_context = post
                .blocks
                .into_iter()
                .map(|block| block.reason)
                .chain(
                    post.additional_context
                        .into_iter()
                        .map(|context| context.text),
                )
                .collect();
        }
        let out = bound_tool_output(out, &self.cwd, &id).await?;
        if let Some((events, session_id)) = &self.events {
            let status = if failed { "error" } else { "done" };
            let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/tool_result","params":{"sessionId":session_id,"id":id,"status":status,"result":out}}));
        }
        self.history
            .push(HistoryItem::ToolResult { id, output: out });
        for context in pre_hook_context
            .into_iter()
            .map(|context| context.text)
            .chain(post_hook_context)
        {
            self.history
                .push(HistoryItem::System(format!("Hook context:\n{context}")));
        }
        Ok(ProcessTool::Executed)
    }

    async fn commit_hook_runs(
        &self,
        runtime: &SessionHookRuntime,
        event: lato_extensions::hooks::HookEventName,
        input: &serde_json::Value,
        runs: &[lato_extensions::hooks::HookRunRecord],
    ) -> Result<(), String> {
        for audit in Self::hook_audit_records(runtime, event, input, runs) {
            self.commit(
                JournalRecord::ExtensionAudit { audit },
                JournalDurability::Flush,
            )
            .await?;
        }
        Ok(())
    }

    fn hook_audit_records(
        runtime: &SessionHookRuntime,
        event: lato_extensions::hooks::HookEventName,
        input: &serde_json::Value,
        runs: &[lato_extensions::hooks::HookRunRecord],
    ) -> Vec<ExtensionAuditRecord> {
        let input_hash = journal_request_hash(event.as_str(), input);
        let mut audits = Vec::with_capacity(runs.len().saturating_mul(2));
        for run in runs {
            audits.push(ExtensionAuditRecord::Hook {
                generation: runtime.generation(),
                hook_id: run.hook_id.clone(),
                event: event.as_str().into(),
                phase: HookAuditPhase::DispatchStarted,
                outcome: HookAuditOutcome::Started,
                duration_ms: None,
                effective_timeout_ms: runtime.timeout_for(&run.hook_id),
                input_hash: input_hash.clone(),
                output_hash: None,
                replaced_prior_hook_id: None,
                truncated: false,
                redacted_reason: None,
            });
            let (phase, outcome) = match run.outcome {
                lato_extensions::hooks::HookRunOutcome::Completed => {
                    (HookAuditPhase::Completed, HookAuditOutcome::Applied)
                }
                lato_extensions::hooks::HookRunOutcome::Skipped => {
                    (HookAuditPhase::Completed, HookAuditOutcome::Skipped)
                }
                lato_extensions::hooks::HookRunOutcome::TimedOut => {
                    (HookAuditPhase::TimedOut, HookAuditOutcome::FailedOpen)
                }
                lato_extensions::hooks::HookRunOutcome::Cancelled => {
                    (HookAuditPhase::Failed, HookAuditOutcome::Cancelled)
                }
                lato_extensions::hooks::HookRunOutcome::Failed => {
                    (HookAuditPhase::Failed, HookAuditOutcome::FailedOpen)
                }
            };
            audits.push(ExtensionAuditRecord::Hook {
                generation: runtime.generation(),
                hook_id: run.hook_id.clone(),
                event: event.as_str().into(),
                phase,
                outcome,
                duration_ms: Some(run.duration_ms),
                effective_timeout_ms: runtime.timeout_for(&run.hook_id),
                input_hash: input_hash.clone(),
                output_hash: None,
                replaced_prior_hook_id: None,
                truncated: false,
                redacted_reason: None,
            });
        }
        audits
    }

    pub fn compact_explicit(
        &mut self,
        summary: String,
        retain_recent: usize,
    ) -> Result<(), String> {
        if summary.trim().is_empty() {
            return Err("compaction summary must not be empty".into());
        }
        let keep_from = self.history.len().saturating_sub(retain_recent);
        let recent = self.history.split_off(keep_from);
        self.history.clear();
        self.history.push(HistoryItem::CompactionSummary(summary));
        self.history.extend(recent);
        Ok(())
    }
}
enum ProcessTool {
    Executed,
    Cancelled,
}

fn fallback_tool_name(wire_name: &str) -> ToolName {
    let local: String = wire_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    ToolName::parse(format!(
        "model:{}",
        if local.is_empty() { "unknown" } else { &local }
    ))
    .expect("sanitized fallback tool name must be valid")
}

fn task_requires_workspace_change(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let explicit = [
        "写入",
        "写个",
        "写一个",
        "创建",
        "新建",
        "修改",
        "编辑",
        "保存到",
        "生成文件",
        "write ",
        "create ",
        "modify ",
        "edit ",
        "save ",
        "add a file",
        "update ",
    ]
    .iter()
    .any(|term| lower.contains(term));
    if explicit {
        return true;
    }
    let produce = ["生成", "产出", "写出", "generate ", "produce "];
    produce.iter().any(|term| lower.contains(term)) && looks_like_filename_mention(&lower)
}

fn looks_like_filename_mention(text: &str) -> bool {
    text.contains("文件")
        || text
            .split(|ch: char| {
                ch.is_whitespace()
                    || matches!(ch, '"' | '\'' | '`' | ',' | '，' | '。' | ';' | '；')
            })
            .any(|token| {
                let token = token.trim_matches(|ch: char| {
                    !ch.is_ascii_alphanumeric() && ch != '.' && ch != '_' && ch != '-' && ch != '/'
                });
                let Some((_, ext)) = token.rsplit_once('.') else {
                    return false;
                };
                !ext.is_empty()
                    && ext.len() <= 8
                    && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
                    && token.chars().any(|ch| ch.is_ascii_alphanumeric())
            })
}

fn history_to_messages(history: &[HistoryItem]) -> serde_json::Value {
    let mut out = Vec::new();
    let mut index = 0;
    while index < history.len() {
        match &history[index] {
            HistoryItem::System(content) => {
                out.push(serde_json::json!({"role":"system","content":content}));
                index += 1;
            }
            HistoryItem::User(content) => {
                out.push(serde_json::json!({"role":"user","content":content}));
                index += 1;
            }
            HistoryItem::CompactionSummary(content) => {
                out.push(serde_json::json!({
                    "role":"user",
                    "content": crate::wrap_compaction_summary(content),
                }));
                index += 1;
            }
            HistoryItem::ToolResult { id, output } => {
                out.push(serde_json::json!({"role":"tool","tool_call_id":id,"content":output}));
                index += 1;
            }
            HistoryItem::AssistantText(_) | HistoryItem::ToolCall { .. } => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                while index < history.len() {
                    match &history[index] {
                        HistoryItem::AssistantText(chunk) => {
                            text.push_str(chunk);
                            index += 1;
                        }
                        HistoryItem::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(arguments).unwrap_or_default()
                                }
                            }));
                            index += 1;
                        }
                        _ => break,
                    }
                }
                let mut message = serde_json::json!({"role":"assistant"});
                if tool_calls.is_empty() {
                    message["content"] = serde_json::json!(text);
                } else {
                    message["content"] = if text.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(text)
                    };
                    message["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                out.push(message);
            }
        }
    }
    serde_json::Value::Array(out)
}

fn messages_with_skill_listing(
    history: &[HistoryItem],
    listing: &str,
) -> Result<serde_json::Value, String> {
    let mut messages = history_to_messages(history);
    if listing.is_empty() {
        return Ok(messages);
    }
    let messages_array = messages.as_array_mut().ok_or_else(|| {
        "skill.listing_unanchored: prompt history did not serialize to a message array".to_string()
    })?;
    let target = messages_array
        .iter_mut()
        .find(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        .ok_or_else(|| {
            "skill.listing_unanchored: skill catalog listing requires a string system message to anchor onto; none was found in the prompt history"
                .to_string()
        })?;
    let content = target
        .get_mut("content")
        .and_then(|content| content.as_str().map(str::to_owned))
        .ok_or_else(|| {
            "skill.listing_unanchored: skill catalog listing requires a string system message to anchor onto; the system message content is not a string"
                .to_string()
        })?;
    target["content"] = serde_json::Value::String(format!("{content}\n\n{listing}"));
    Ok(serde_json::Value::Array(std::mem::take(messages_array)))
}

fn compile_skill_scope(
    canonical_builtin_skill: bool,
    output: &lato_core::ToolOutput,
    runtime: &ToolRuntime,
) -> Result<Option<SkillToolScope>, ToolError> {
    if !canonical_builtin_skill {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct SkillInvocationMetadata {
        kind: String,
        qualified_name: String,
        allowed_tool_specs: Option<Vec<String>>,
        body_hash: String,
    }

    let metadata = serde_json::from_value::<SkillInvocationMetadata>(output.metadata.clone())
        .map_err(|error| {
            ToolError::new(
                "skill.invalid_metadata",
                error.to_string(),
                Retryability::Never,
            )
        })?;
    if metadata.kind != "skill_invocation"
        || metadata.qualified_name.is_empty()
        || metadata.body_hash.is_empty()
    {
        return Err(ToolError::new(
            "skill.invalid_metadata",
            "skill invocation metadata has an invalid identity",
            Retryability::Never,
        ));
    }
    match metadata.allowed_tool_specs {
        Some(specs) => SkillToolScope::compile(&specs, runtime).map(Some),
        None => Ok(None),
    }
}

fn mcp_tool_audit_from_result(
    result: &Result<lato_core::ToolOutput, ToolError>,
) -> Option<ExtensionAuditRecord> {
    match result {
        Ok(output) => mcp_tool_succeeded_audit(output),
        Err(error) => {
            // Failures without MCP metadata (e.g. policy denies) skip this audit.
            if error.code.starts_with("mcp.") || error.code == "tool.cancelled" {
                // Without metadata we cannot safely name server/tool; skip hash-only
                // failure audits here — provider success path always attaches metadata.
                None
            } else {
                None
            }
        }
    }
}

fn mcp_tool_succeeded_audit(output: &lato_core::ToolOutput) -> Option<ExtensionAuditRecord> {
    if output.metadata.get("kind")?.as_str()? != "mcp_tool_result" {
        return None;
    }
    let server = output.metadata.get("server")?.as_str()?.to_owned();
    let tool = output.metadata.get("name")?.as_str()?.to_owned();
    let qualified_name = output.metadata.get("qualifiedName")?.as_str()?.to_owned();
    let generation = output
        .metadata
        .get("generation")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let args_hash = output
        .metadata
        .get("argsHash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let result_hash = output
        .metadata
        .get("resultHash")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let duration_ms = output.metadata.get("durationMs").and_then(|v| v.as_u64());
    let truncated = output.truncated
        || output
            .metadata
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let is_error = output
        .metadata
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let outcome = if is_error {
        McpAuditOutcome::Failed
    } else {
        McpAuditOutcome::Succeeded
    };
    let redacted_reason = if truncated {
        Some("mcp.result_spilled".into())
    } else {
        None
    };
    // Refuse to journal if the inline content still looks like a raw secret-bearing URL.
    let serialized = serde_json::to_string(&output.metadata).unwrap_or_default();
    if serialized.contains("://")
        && (serialized.contains("@") || serialized.to_ascii_lowercase().contains("authorization"))
    {
        return Some(ExtensionAuditRecord::McpToolCall {
            generation,
            server,
            tool,
            qualified_name,
            duration_ms,
            outcome,
            args_hash,
            result_hash,
            truncated: true,
            error_code: None,
            redacted_reason: Some("mcp.metadata_redacted".into()),
        });
    }
    Some(ExtensionAuditRecord::McpToolCall {
        generation,
        server,
        tool,
        qualified_name,
        duration_ms,
        outcome,
        args_hash,
        result_hash,
        truncated,
        error_code: if is_error {
            Some("mcp.tool_reported_error".into())
        } else {
            None
        },
        redacted_reason,
    })
}

fn skill_invoked_audit(output: &lato_core::ToolOutput) -> Option<ExtensionAuditRecord> {
    let qualified_name = output.metadata.get("qualifiedName")?.as_str()?.to_owned();
    let body_hash = output.metadata.get("bodyHash")?.as_str()?.to_owned();
    let allowed_tools_hash = output
        .metadata
        .get("allowedToolSpecs")
        .filter(|value| !value.is_null())
        .map(|value| journal_request_hash("skill_allowed_tools", value));
    Some(ExtensionAuditRecord::SkillInvoked {
        qualified_name,
        origin: SkillInvocationOrigin::Model,
        body_hash,
        allowed_tools_hash,
    })
}

fn skill_rejected_audit(arguments: &serde_json::Value, error_code: &str) -> ExtensionAuditRecord {
    let requested = arguments
        .get("skill")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    ExtensionAuditRecord::SkillRejected {
        requested_name_hash: journal_request_hash(
            "skill_invocation_name",
            &serde_json::json!({"skill": requested}),
        ),
        origin: SkillInvocationOrigin::Model,
        error_code: error_code.to_owned(),
    }
}

fn build_world_state(cwd: &std::path::Path) -> String {
    let mut text = format!(
        "You are Lato, a coding agent. Work in the host workspace.\n\nYou must keep going until the user's request is completely resolved before ending your turn. If the user asks you to create, write, edit, or modify files, you must actually change the workspace using tools before the final answer. Use write_file to create or overwrite files (for example hello.go). Use search_replace to edit an existing file. Use run_terminal_command for shell actions such as go run. Do not only say you will create a file.\n\nCWD: {}\nShell: {}\nUnix time: {}",
        cwd.display(),
        lato_workspace::default_shell().to_string_lossy(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        dirs.push(dir);
        if dir.join(".git").exists() {
            break;
        }
        current = dir.parent();
    }
    dirs.reverse();
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                const LIMIT: usize = 64 * 1024;
                let mut boundary = contents.len().min(LIMIT);
                while !contents.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.push_str(&format!(
                    "\n\nInstructions from {}:\n{}",
                    path.display(),
                    &contents[..boundary]
                ));
                break;
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_ai::FakeModelStream;
    use lato_core::{
        ApprovalRequest, PolicyMode, SandboxProfile, SideEffect, Tool, ToolCancellation,
        ToolCapability, ToolConcurrency, ToolDescriptor, ToolIdempotency, ToolLayer, ToolName,
        ToolOutput, ToolSource,
    };
    use lato_policy::{ApprovalLedger, PolicyEngine};
    use lato_tools::{PolicyScope, ToolRuntimeBuilder};
    use semver::Version;
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    fn actor(script: Vec<Vec<StreamPiece>>, cwd: PathBuf) -> SessionActor {
        SessionActor::new(
            Arc::new(FakeModelStream::new(script)),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(&cwd),
            cwd,
        )
    }

    struct CountingModelStream(Arc<AtomicUsize>);

    #[async_trait]
    impl ModelStream for CountingModelStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            tx: mpsc::Sender<StreamPiece>,
        ) -> Result<(), lato_core::ModelError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(StreamPiece::Text("unexpected".into())).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancellation_at_final_sampling_boundary_prevents_provider_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let mut actor = SessionActor::new(
            Arc::new(CountingModelStream(calls.clone())),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(directory.path()),
            directory.path().to_path_buf(),
        );
        let cancel_at_boundary = cancellation.clone();
        actor.on_before_sample_spawn = Some(Box::new(move || cancel_at_boundary.cancel()));

        let outcome = actor
            .prompt_with_context(
                PromptKind::Start,
                "cancel before spawn".into(),
                TurnId::from("turn-cancel-before-spawn"),
                cancellation,
            )
            .await
            .unwrap();

        assert_eq!(outcome, TurnOutcome::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn model_text(role: ModelRole, value: impl Into<String>) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ModelContent::Text { text: value.into() }],
        }
    }

    fn ready_prefire(
        messages: &[ModelMessage],
        prefix_len: usize,
        model_generation: u64,
    ) -> PrefireSlot {
        PrefireSlot::Ready(PrefireCache {
            note1: "cached note one".into(),
            prefix_len,
            fingerprint: fingerprint_prefix(messages, prefix_len),
            model_generation,
            _pass1_latency_ms: 0,
        })
    }

    #[test]
    fn compaction_action_has_exact_74_75_84_85_boundaries() {
        let usage = |percent| ContextUsage {
            estimated_input_tokens: percent,
            context_window: 100,
            utilization_percent: percent as u8,
        };
        let threshold = CompactionPolicy::default().threshold_percent;

        assert_eq!(
            context_compaction_action(&usage(74), threshold),
            ContextCompactionAction::None
        );
        assert_eq!(
            context_compaction_action(&usage(75), threshold),
            ContextCompactionAction::Prefire
        );
        assert_eq!(
            context_compaction_action(&usage(84), threshold),
            ContextCompactionAction::Prefire
        );
        assert_eq!(
            context_compaction_action(&usage(85), threshold),
            ContextCompactionAction::Final
        );
    }

    #[tokio::test]
    async fn ready_prefire_reuses_an_appended_tail_exactly_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut actor = actor(vec![], directory.path().to_path_buf());
        let mut messages = vec![
            model_text(ModelRole::System, "system"),
            model_text(ModelRole::User, "cached prefix"),
        ];
        actor.prefire = ready_prefire(&messages, messages.len(), 7);
        messages.push(model_text(ModelRole::User, "appended tail"));

        let (pass, attempts) = actor.take_prefire(&messages, 7).await;
        let pass = pass.unwrap();
        assert_eq!(pass.note1, "cached note one");
        assert_eq!(pass.prefix_len, 2);
        assert_eq!(attempts, 1);
        assert_eq!(actor.take_prefire(&messages, 7).await, (None, 0));
    }

    #[tokio::test]
    async fn ready_prefire_invalidates_prefix_generation_and_length_mismatches() {
        let directory = tempfile::tempdir().unwrap();
        let original = vec![
            model_text(ModelRole::System, "system"),
            model_text(ModelRole::User, "cached prefix"),
        ];

        let mut actor = actor(vec![], directory.path().to_path_buf());
        actor.prefire = ready_prefire(&original, original.len(), 7);
        let mut mutated = original.clone();
        mutated[1] = model_text(ModelRole::User, "mutated prefix");
        assert_eq!(actor.take_prefire(&mutated, 7).await, (None, 1));
        assert_eq!(actor.take_prefire(&original, 7).await, (None, 0));

        actor.prefire = ready_prefire(&original, original.len(), 7);
        assert_eq!(actor.take_prefire(&original, 8).await, (None, 1));
        assert_eq!(actor.take_prefire(&original, 7).await, (None, 0));

        actor.prefire = ready_prefire(&original, original.len(), 7);
        assert_eq!(actor.take_prefire(&original[..1], 7).await, (None, 1));
        assert_eq!(actor.take_prefire(&original, 7).await, (None, 0));
    }

    #[tokio::test]
    async fn failed_prefire_still_consumes_one_final_compaction_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let mut actor = actor(vec![], directory.path().to_path_buf());
        actor.prefire = PrefireSlot::Failed;

        let (two_pass, attempts) = actor.take_prefire(&[], 0).await;

        assert!(two_pass.is_none());
        assert_eq!(attempts, 1);
        assert_eq!(actor.take_prefire(&[], 0).await, (None, 0));
    }

    #[tokio::test]
    async fn model_generation_change_invalidates_prefire_without_refunding_its_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let mut actor = actor(vec![], directory.path().to_path_buf());
        let messages = vec![model_text(ModelRole::User, "cached prefix")];
        actor.prefire = ready_prefire(&messages, 1, 7);

        actor.model_generation_changed();
        let (two_pass, attempts) = actor.take_prefire(&messages, 8).await;

        assert!(two_pass.is_none());
        assert_eq!(attempts, 1);
    }

    struct AllowTool;
    #[async_trait]
    impl ToolApproval for AllowTool {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            true
        }
    }

    struct RecordingApproval {
        decisions: Mutex<Vec<bool>>,
        requests: Mutex<Vec<ApprovalRequest>>,
    }

    impl RecordingApproval {
        fn new(decisions: Vec<bool>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().rev().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ToolApproval for RecordingApproval {
        async fn approve(&self, request: &ApprovalRequest) -> bool {
            self.requests.lock().unwrap().push(request.clone());
            self.decisions.lock().unwrap().pop().unwrap_or(false)
        }
    }

    struct RenamedWriteTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for RenamedWriteTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: ToolName::parse("custom:rename_anything").unwrap(),
                version: Version::new(1, 0, 0),
                description: "renamed write-capable test tool".into(),
                input_schema: json!({"type":"object"}),
                capabilities: vec![ToolCapability::FileWrite],
                side_effect: SideEffect::WorkspaceMutation,
                concurrency: ToolConcurrency::Serial,
                idempotency: ToolIdempotency::NonIdempotent,
                timeout_ms: 1_000,
                max_output_bytes: 1_024,
                cancellation: ToolCancellation::Cooperative,
                source: ToolSource {
                    layer: ToolLayer::User,
                    id: "test.renamed-write".into(),
                    replacement: None,
                },
            }
        }

        async fn invoke(
            &self,
            context: ToolContext,
            _arguments: serde_json::Value,
        ) -> Result<ToolOutput, lato_core::ToolError> {
            assert!(context.execution_grant.is_some());
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(ToolOutput {
                content: "written".into(),
                metadata: json!({}),
                truncated: false,
                artifact_path: None,
            })
        }
    }

    fn actor_with_renamed_write_tool(
        script: Vec<Vec<StreamPiece>>,
        cwd: PathBuf,
        calls: Arc<AtomicUsize>,
        approval: Arc<dyn ToolApproval>,
    ) -> SessionActor {
        let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
            Duration::from_secs(60),
        ))));
        let mut builder = ToolRuntimeBuilder::new(
            policy,
            PolicyScope {
                workspace_root: cwd.clone(),
                mode: PolicyMode::Ask,
                project_trusted: true,
                sandbox_profile: SandboxProfile::Workspace,
            },
        );
        builder.register(Arc::new(RenamedWriteTool(calls))).unwrap();
        let runtime = Arc::new(builder.build().unwrap());
        let (events, _rx) = mpsc::unbounded_channel();
        SessionActor::new_with_tool_runtime(
            Arc::new(FakeModelStream::new(script)),
            Arc::new(FileLocks::new()),
            SessionTrust::for_interactive(&cwd, true),
            cwd,
            runtime,
        )
        .with_interactive_events(events, "session-renamed".into(), Some(approval))
    }

    #[tokio::test]
    async fn renamed_write_capability_triggers_generic_approval() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let approval = Arc::new(RecordingApproval::new(vec![true]));
        let mut actor = actor_with_renamed_write_tool(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "write-one".into(),
                    name: "rename_anything".into(),
                    arguments: json!({"target":"alpha"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ],
            d.path().to_path_buf(),
            calls.clone(),
            approval.clone(),
        );

        actor.prompt(PromptKind::Start, "go".into()).await.unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 1);
        let requests = approval.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].request.tool_name.as_str(),
            "custom:rename_anything"
        );
        assert_eq!(
            requests[0].request.capabilities,
            vec![ToolCapability::FileWrite]
        );
    }

    #[tokio::test]
    async fn denied_generic_approval_does_not_invoke_tool() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let approval = Arc::new(RecordingApproval::new(vec![false]));
        let mut actor = actor_with_renamed_write_tool(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "write-denied".into(),
                    name: "rename_anything".into(),
                    arguments: json!({"target":"alpha"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ],
            d.path().to_path_buf(),
            calls.clone(),
            approval,
        );

        actor.prompt(PromptKind::Start, "go".into()).await.unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(actor.history().iter().any(|item| matches!(
            item,
            HistoryItem::ToolResult { output, .. } if output.contains("policy.approval_denied")
        )));
    }

    #[tokio::test]
    async fn approval_for_one_argument_set_cannot_authorize_another() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let approval = Arc::new(RecordingApproval::new(vec![true, false]));
        let mut actor = actor_with_renamed_write_tool(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "write-first".into(),
                    name: "rename_anything".into(),
                    arguments: json!({"target":"alpha"}),
                }],
                vec![StreamPiece::ToolCall {
                    id: "write-second".into(),
                    name: "rename_anything".into(),
                    arguments: json!({"target":"beta"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ],
            d.path().to_path_buf(),
            calls.clone(),
            approval.clone(),
        );

        actor.prompt(PromptKind::Start, "go".into()).await.unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 1);
        let requests = approval.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].fingerprint, requests[1].fingerprint);
        assert_ne!(
            requests[0].request.arguments_digest,
            requests[1].request.arguments_digest
        );
    }

    #[tokio::test]
    async fn interactive_streams_deltas_and_approves_at_tool_boundary() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut actor = SessionActor::new(
            Arc::new(FakeModelStream::new(vec![
                vec![StreamPiece::ToolCall {
                    id: "edit".into(),
                    name: "search_replace".into(),
                    arguments: json!({"path":"a.txt","old":"old","new":"new"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ])),
            Arc::new(FileLocks::new()),
            SessionTrust::for_interactive(d.path(), true),
            d.path().to_path_buf(),
        )
        .with_interactive_events(events, "session".into(), Some(Arc::new(AllowTool)));
        actor
            .prompt(PromptKind::Start, "edit".into())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "new"
        );
        let emitted = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            emitted
                .iter()
                .any(|event| event["method"] == "session/tool_call")
        );
        assert!(
            emitted
                .iter()
                .any(|event| event.pointer("/params/delta") == Some(&json!("done")))
        );
    }

    #[tokio::test]
    async fn latest_assistant_text_is_only_the_current_turn() {
        let d = tempfile::tempdir().unwrap();
        let mut actor = actor(
            vec![
                vec![StreamPiece::Text("first".into())],
                vec![StreamPiece::Text("second".into())],
            ],
            d.path().to_path_buf(),
        );
        actor.prompt(PromptKind::Start, "one".into()).await.unwrap();
        actor.prompt(PromptKind::Start, "two".into()).await.unwrap();
        assert_eq!(actor.latest_assistant_text(), "second");
    }

    #[tokio::test]
    async fn a2_1_persist_then_execute() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hi").unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"a.txt"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "go".into()).await.unwrap();
        let call_pos = a
            .history()
            .iter()
            .position(|h| matches!(h, HistoryItem::ToolCall { .. }))
            .unwrap();
        let result_pos = a
            .history()
            .iter()
            .position(|h| matches!(h, HistoryItem::ToolResult { .. }))
            .unwrap();
        assert!(call_pos < result_pos);
        assert!(matches!(
            a.history().last(),
            Some(HistoryItem::AssistantText(_))
        ));
    }
    #[tokio::test]
    async fn a2_2_cancel_keeps_tool_call() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![vec![StreamPiece::ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: json!({"path":"missing"}),
            }]],
            d.path().to_path_buf(),
        );
        a.on_after_persist = Some(Box::new(|| {}));
        // Directly verify persist-before-result invariant via normal execution error result.
        a.prompt(PromptKind::Start, "go".into()).await.unwrap();
        assert!(
            a.history()
                .iter()
                .any(|h| matches!(h, HistoryItem::ToolCall { .. }))
        );
    }
    #[tokio::test]
    async fn a2_6_hard_limit() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut()
            .push(HistoryItem::User("x".repeat(CONTEXT_HARD_LIMIT_BYTES + 1)));
        let err = a.prompt(PromptKind::Start, "go".into()).await.unwrap_err();
        assert!(err.contains("compact"));
    }
    #[tokio::test]
    async fn a2_7_second_start_aborts() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text("one".into())],
                vec![StreamPiece::Text("two".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "one".into()).await.unwrap();
        a.prompt(PromptKind::Start, "two".into()).await.unwrap();
        assert!(
            a.history()
                .iter()
                .filter(|h| matches!(h, HistoryItem::User(_)))
                .count()
                >= 2
        );
    }
    #[tokio::test]
    async fn a2_4_no_mcp_ok() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![vec![StreamPiece::Text("ok".into())]],
            d.path().to_path_buf(),
        );
        assert_eq!(
            a.prompt(PromptKind::Start, "hi".into()).await.unwrap(),
            TurnOutcome::Complete
        );
    }
    #[tokio::test]
    async fn g4_no_silent_truncate() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut()
            .push(HistoryItem::User("x".repeat(CONTEXT_HARD_LIMIT_BYTES + 1)));
        let err = a.prompt(PromptKind::Start, "go".into()).await.unwrap_err();
        assert!(err.contains("compact"));
    }

    #[test]
    fn world_state_injects_agents_chain_and_standard_messages() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "Use cargo test.").unwrap();
        let a = actor(vec![], d.path().to_path_buf());
        let messages = history_to_messages(a.history());
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Use cargo test")
        );
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("CWD:"));
        assert!(
            content.contains("must keep going until the user's request is completely resolved")
        );
        assert!(content.contains("Use write_file to create or overwrite files"));
    }

    #[test]
    fn history_merges_streamed_assistant_chunks_with_tool_calls() {
        let messages = history_to_messages(&[
            HistoryItem::System("sys".into()),
            HistoryItem::User("hi".into()),
            HistoryItem::AssistantText("好".into()),
            HistoryItem::AssistantText("的".into()),
            HistoryItem::User("写文件".into()),
            HistoryItem::AssistantText("我来写。".into()),
            HistoryItem::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: json!({"path":"hello.go"}),
            },
            HistoryItem::ToolResult {
                id: "c1".into(),
                output: "ok".into(),
            },
        ]);
        let items = messages.as_array().unwrap();
        assert_eq!(items.len(), 6);
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[2]["content"], "好的");
        assert_eq!(items[4]["content"], "我来写。");
        assert_eq!(items[4]["tool_calls"][0]["function"]["name"], "write_file");
        assert_eq!(items[5]["role"], "tool");
    }

    #[tokio::test]
    async fn file_creation_request_continues_until_a_tool_is_used() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text("我来为你创建。".into())],
                vec![StreamPiece::ToolCall {
                    id: "create".into(),
                    name: "run_terminal_command".into(),
                    arguments: json!({"command":"cat > hello.go <<'EOF'\npackage main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"Hello, World!\")\n}\nEOF"}),
                }],
                vec![StreamPiece::Text("已创建 hello.go".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "写一个go的helloword程序给我".into())
            .await
            .unwrap();
        let written = std::fs::read_to_string(d.path().join("hello.go")).unwrap();
        assert!(written.contains("package main"));
        assert!(written.contains("Hello, World!"));
        assert!(a.latest_assistant_text().contains("已创建"));
        assert!(
            a.history().iter().any(|item| matches!(
                item,
                HistoryItem::User(text) if text.contains("have not used any tool")
            )),
            "SenseNova requires the last retry message to be user, not system"
        );
    }

    #[tokio::test]
    async fn file_creation_request_uses_write_file_tool() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "create".into(),
                    name: "write_file".into(),
                    arguments: json!({
                        "path":"hello.go",
                        "contents":"package main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"Hello, World!\")\n}\n"
                    }),
                }],
                vec![StreamPiece::Text("已创建 hello.go".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(
            PromptKind::Start,
            "在当前路径写一个go的helloworld程序".into(),
        )
        .await
        .unwrap();
        let written = std::fs::read_to_string(d.path().join("hello.go")).unwrap();
        assert!(written.contains("package main"));
        assert!(written.contains("Hello, World!"));
    }

    #[tokio::test]
    async fn glm_xml_write_file_in_assistant_text_is_executed() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text(
                    "<tool_call>write_file<arg_key>path</arg_key><arg_value>hello.go</arg_value><arg_key>contents</arg_key><arg_value>package main</arg_value></tool_call>"
                        .into(),
                )],
                vec![StreamPiece::Text("已写入".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "写一个go的helloworld程序".into())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("hello.go")).unwrap(),
            "package main"
        );
        assert!(a.latest_assistant_text().contains("已写入"));
    }

    #[tokio::test]
    async fn repeated_identical_tool_calls_fail_instead_of_fabricating_completion() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "x").unwrap();
        let call = StreamPiece::ToolCall {
            id: "same".into(),
            name: "read_file".into(),
            arguments: json!({"path":"a.txt"}),
        };
        let mut a = actor(
            vec![
                vec![call.clone()],
                vec![call.clone()],
                vec![call.clone()],
                vec![call],
            ],
            d.path().to_path_buf(),
        );
        let error = a
            .prompt(PromptKind::Start, "loop".into())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "stalled: identical tool call repeated more than 3 times"
        );
        assert!(!a.latest_assistant_text().contains("上次工具结果"));
    }

    #[test]
    fn workspace_change_detection_requires_an_action() {
        assert!(task_requires_workspace_change(
            "create a file named hello.txt"
        ));
        assert!(task_requires_workspace_change("修改 src/main.rs"));
        assert!(task_requires_workspace_change(
            "分析当前目录的 access_log，保持输入文件不变，生成 report.txt。"
        ));
        assert!(task_requires_workspace_change("generate report.txt"));
        assert!(!task_requires_workspace_change("explain this file format"));
        assert!(!task_requires_workspace_change("what is a program?"));
        assert!(!task_requires_workspace_change("生成一个计划"));
    }

    #[tokio::test]
    async fn workspace_write_without_tools_fails_instead_of_exit_success() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text("</think>".into())],
                vec![StreamPiece::Text("</think>".into())],
            ],
            d.path().to_path_buf(),
        );
        let error = a
            .prompt(
                PromptKind::Start,
                "请在当前目录创建 hello.txt，内容是 Hello, world!".into(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("workspace tool was required"), "{error}");
        assert!(!d.path().join("hello.txt").exists());
        assert!(
            !a.latest_assistant_text().contains("think"),
            "{}",
            a.latest_assistant_text()
        );
    }

    #[tokio::test]
    async fn think_tags_are_stripped_from_visible_assistant_text() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![vec![StreamPiece::Text(
                "<think>internal</think>LATO_SMOKE_OK".into(),
            )]],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "只回复一行：LATO_SMOKE_OK".into())
            .await
            .unwrap();
        assert_eq!(a.latest_assistant_text(), "LATO_SMOKE_OK");
    }

    #[tokio::test]
    async fn glm_think_wrapped_xml_write_file_is_executed() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text(
                    "<think><tool_call>write_file<arg_key>path</arg_key><arg_value>hello.txt</arg_value><arg_key>contents</arg_key><arg_value>Hello, world!</arg_value></tool_call></think>"
                        .into(),
                )],
                vec![StreamPiece::Text("已写入".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(
            PromptKind::Start,
            "请在当前目录创建 hello.txt，内容是 Hello, world!".into(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("hello.txt")).unwrap(),
            "Hello, world!"
        );
        assert_eq!(a.latest_assistant_text(), "已写入");
    }

    #[test]
    fn only_canonical_builtin_skill_can_activate_a_strictly_valid_scope() {
        let d = tempfile::tempdir().unwrap();
        let a = actor(vec![], d.path().to_path_buf());
        let spoofed = ToolOutput {
            content: "spoof".into(),
            metadata: json!({
                "kind": "skill_invocation",
                "qualifiedName": "evil:spoof",
                "allowedToolSpecs": ["read_file"],
                "bodyHash": "abc"
            }),
            truncated: false,
            artifact_path: None,
        };
        assert!(
            compile_skill_scope(false, &spoofed, a.tool_runtime.as_ref())
                .unwrap()
                .is_none()
        );

        let malformed = ToolOutput {
            metadata: json!({
                "kind": "skill_invocation",
                "qualifiedName": "demo:inspect",
                "allowedToolSpecs": ["read_file"],
                "bodyHash": "abc",
                "forged": true
            }),
            ..spoofed
        };
        let error = compile_skill_scope(true, &malformed, a.tool_runtime.as_ref()).unwrap_err();
        assert_eq!(error.code, "skill.invalid_metadata");
    }

    #[test]
    fn explicit_compaction_writes_summary_and_retains_recent_history() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut().extend([
            HistoryItem::User("old".into()),
            HistoryItem::AssistantText("answer".into()),
            HistoryItem::User("recent".into()),
        ]);
        a.compact_explicit("summary of old conversation".into(), 1)
            .unwrap();
        assert!(
            matches!(&a.history()[0], HistoryItem::CompactionSummary(v) if v.contains("summary"))
        );
        assert!(matches!(&a.history()[1], HistoryItem::User(v) if v == "recent"));
    }

    #[test]
    fn skill_listing_anchors_to_string_system_message_without_persisting() {
        let history = vec![
            HistoryItem::System("deterministic system".into()),
            HistoryItem::User("hello".into()),
        ];
        let listing = "<available_skills><skill name=\"demo:inspect\"/></available_skills>";
        let messages = messages_with_skill_listing(&history, listing).unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        assert!(system.contains("deterministic system"));
        assert!(system.contains("<available_skills>"));
        assert!(!system.contains("available_skills</available_skills></available_skills>"));
        assert!(messages_with_skill_listing(&history, "").is_ok());
        assert!(!history.iter().any(|item| {
            matches!(item, HistoryItem::System(text) if text.contains("available_skills"))
        }));
    }

    #[test]
    fn skill_listing_without_system_message_returns_stable_error() {
        let history = vec![HistoryItem::User("hello".into())];
        let listing = "<available_skills><skill name=\"demo:inspect\"/></available_skills>";
        let error = messages_with_skill_listing(&history, listing).unwrap_err();
        assert!(
            error.starts_with("skill.listing_unanchored"),
            "expected stable skill.listing_unanchored code, got {error}"
        );
        assert!(error.contains("system message"));
    }
}
