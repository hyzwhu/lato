// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs
// License: Apache-2.0
// Lato changes: serialized per-session model switch with checkpoint-first immediate compaction

use crate::{
    HistoryItem, LegacyTurnDriver, SkillRuntimeBinding, SwitchCompaction, ToolApproval,
    decide_switch_compaction, estimate_history_tokens, model_messages_to_history,
};
use lato_ai::{ActiveModelStream, ModelStream, adapt_model_endpoint};
use lato_core::{
    AgentError, CancelReason, Command, CompactSession, CompactionId, CompactionPolicy,
    CompactionSize, CompactionTrigger, ErrorCategory, EventPayload, JournalError, JournalReplay,
    PluginSnapshotSummary, Retryability, SessionId, SessionStore, StartBehavior, StartTurn, TurnId,
    UserInput,
};
use lato_extensions::{
    PluginSnapshot,
    hooks::materialize_hooks,
    skills::{SkillCatalog, discover_skills},
};
use lato_runtime::{
    SessionBootstrap, SessionHandle, TurnDriver, spawn_session, spawn_session_with_store,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::Duration;

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

pub struct PreparedModelSwitch {
    pub active: ActiveModelStream,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchOutcome {
    pub provider: String,
    pub model: String,
    pub compaction_warning: Option<AgentError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActiveOperation {
    Turn(TurnId),
    Compaction(CompactionId),
}

#[derive(Clone, Copy, Debug)]
enum PromptObservationFailure {
    Lagged(u64),
    Closed,
    MissingTurnId,
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
    active_operation: Arc<Mutex<Option<ActiveOperation>>>,
    submission_gate: Mutex<()>,
    plugin_state: Arc<Mutex<SessionPluginState>>,
    failed_closed: Arc<AtomicBool>,
    hooks_started: AtomicBool,
    hooks_ended: AtomicBool,
}

struct PromptCleanupGuard {
    armed: bool,
    handle: SessionHandle,
    active_operation: Arc<Mutex<Option<ActiveOperation>>>,
    plugin_state: Arc<Mutex<SessionPluginState>>,
    failed_closed: Arc<AtomicBool>,
}

impl PromptCleanupGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PromptCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.failed_closed.store(true, Ordering::Release);
        let handle = self.handle.clone();
        let active_operation = Arc::clone(&self.active_operation);
        let plugin_state = Arc::clone(&self.plugin_state);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = handle.submit(Command::Shutdown).await;
                *active_operation.lock().await = None;
                let mut state = plugin_state.lock().await;
                state.active_turn = None;
                state.active_turn_skills = None;
            });
        } else {
            if let Ok(mut active) = active_operation.try_lock() {
                *active = None;
            }
            if let Ok(mut state) = plugin_state.try_lock() {
                state.active_turn = None;
                state.active_turn_skills = None;
            }
        }
    }
}

struct SessionPluginState {
    current: Arc<PluginSnapshot>,
    current_skills: Arc<SkillCatalog>,
    active_turn: Option<Arc<PluginSnapshot>>,
    active_turn_skills: Option<Arc<SkillCatalog>>,
    pending: Option<(Arc<PluginSnapshot>, Arc<SkillCatalog>)>,
}

enum DriverToolRuntime {
    Unbound(Arc<lato_tools::ToolRuntime>),
    Skills(SkillRuntimeBinding),
}

impl Default for SessionPluginState {
    fn default() -> Self {
        Self {
            current: PluginSnapshot::empty(),
            current_skills: SkillCatalog::from_discovery(Default::default()),
            active_turn: None,
            active_turn_skills: None,
            pending: None,
        }
    }
}

pub struct ChildSessionConfig {
    pub session_id: String,
    pub stream: Arc<dyn ModelStream>,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
    pub cwd: PathBuf,
    pub updates: mpsc::UnboundedSender<serde_json::Value>,
    pub approval: Option<Arc<dyn ToolApproval>>,
    pub tool_runtime: ChildToolRuntime,
    pub initial_history: Vec<HistoryItem>,
    pub plugin_snapshot: Arc<PluginSnapshot>,
}

pub struct ChildToolRuntime {
    runtime: Option<Arc<lato_tools::ToolRuntime>>,
    skills: Option<SkillRuntimeBinding>,
}

impl ChildToolRuntime {
    pub fn without_skills(runtime: Arc<lato_tools::ToolRuntime>) -> Self {
        Self {
            runtime: Some(runtime),
            skills: None,
        }
    }
}

impl From<SkillRuntimeBinding> for ChildToolRuntime {
    fn from(skills: SkillRuntimeBinding) -> Self {
        Self {
            runtime: None,
            skills: Some(skills),
        }
    }
}

impl RuntimeSession {
    fn from_driver(
        session_id: SessionId,
        driver: Arc<LegacyTurnDriver>,
        updates: mpsc::UnboundedSender<serde_json::Value>,
    ) -> Self {
        let runtime_driver: Arc<dyn TurnDriver> = driver.clone();
        let handle = spawn_session(session_id.clone(), runtime_driver);
        Self::from_spawned_driver(session_id, driver, updates, handle)
    }

    fn from_spawned_driver(
        session_id: SessionId,
        driver: Arc<LegacyTurnDriver>,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        handle: SessionHandle,
    ) -> Self {
        Self {
            session_id,
            handle,
            driver,
            updates,
            active_operation: Arc::new(Mutex::new(None)),
            submission_gate: Mutex::new(()),
            plugin_state: Arc::new(Mutex::new(SessionPluginState::default())),
            failed_closed: Arc::new(AtomicBool::new(false)),
            hooks_started: AtomicBool::new(false),
            hooks_ended: AtomicBool::new(false),
        }
    }

    pub async fn new_child(config: ChildSessionConfig) -> Result<Self, AgentError> {
        let session_id = SessionId::parse(config.session_id.clone()).map_err(|error| {
            AgentError::new(
                "task.child.invalid_session_id",
                ErrorCategory::Task,
                error.to_string(),
                Retryability::Never,
            )
        })?;
        let driver = Arc::new(match config.tool_runtime.skills {
            Some(binding) => LegacyTurnDriver::new_with_endpoint_skill_runtime(
                config.session_id,
                endpoint_from_stream(config.stream),
                config.locks,
                config.trust,
                config.cwd,
                config.updates.clone(),
                config.approval,
                binding,
            ),
            None => LegacyTurnDriver::new_with_endpoint_and_tool_runtime(
                config.session_id,
                endpoint_from_stream(config.stream),
                config.locks,
                config.trust,
                config.cwd,
                config.updates.clone(),
                config.approval,
                config
                    .tool_runtime
                    .runtime
                    .expect("unbound child runtime is present"),
            ),
        });
        if !config.initial_history.is_empty() {
            driver.replace_history(config.initial_history).await;
        }
        let runtime_driver: Arc<dyn TurnDriver> = driver.clone();
        let handle = spawn_session(session_id.clone(), runtime_driver);
        let session = Self {
            session_id,
            handle,
            driver,
            updates: config.updates,
            active_operation: Arc::new(Mutex::new(None)),
            submission_gate: Mutex::new(()),
            plugin_state: Arc::new(Mutex::new(SessionPluginState::default())),
            failed_closed: Arc::new(AtomicBool::new(false)),
            hooks_started: AtomicBool::new(false),
            hooks_ended: AtomicBool::new(false),
        };
        session
            .stage_plugin_snapshot(config.plugin_snapshot)
            .await?;
        Ok(session)
    }

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
        Self::new_with_endpoint(
            session_id,
            endpoint_from_stream(stream),
            locks,
            trust,
            cwd,
            updates,
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
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new_with_endpoint(
            session_id.to_string(),
            endpoint,
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
            active_operation: Arc::new(Mutex::new(None)),
            submission_gate: Mutex::new(()),
            plugin_state: Arc::new(Mutex::new(SessionPluginState::default())),
            failed_closed: Arc::new(AtomicBool::new(false)),
            hooks_started: AtomicBool::new(false),
            hooks_ended: AtomicBool::new(false),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_endpoint_and_tool_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        tool_runtime: Arc<lato_tools::ToolRuntime>,
    ) -> Self {
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new_with_endpoint_and_tool_runtime(
            session_id.to_string(),
            endpoint,
            locks,
            trust,
            cwd,
            updates.clone(),
            approval,
            tool_runtime,
        ));
        Self::from_driver(session_id, driver, updates)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_endpoint_skill_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        binding: SkillRuntimeBinding,
    ) -> Self {
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(LegacyTurnDriver::new_with_endpoint_skill_runtime(
            session_id.to_string(),
            endpoint,
            locks,
            trust,
            cwd,
            updates.clone(),
            approval,
            binding,
        ));
        let runtime_driver: Arc<dyn TurnDriver> = driver.clone();
        let handle = spawn_session(session_id.clone(), runtime_driver);
        Self::from_spawned_driver(session_id, driver, updates, handle)
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
        Self::new_with_store_and_endpoint(
            session_id,
            endpoint_from_stream(stream),
            locks,
            trust,
            cwd,
            updates,
            approval,
            store,
            replay,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_store_and_endpoint(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
    ) -> Result<Self, AgentError> {
        let binding = SkillRuntimeBinding::builtin(cwd.clone(), locks.clone(), trust.clone())
            .map_err(|error| {
                AgentError::new(
                    "tool.runtime_initialization",
                    ErrorCategory::Tool,
                    error.to_string(),
                    Retryability::Never,
                )
            })?;
        Self::new_with_store_endpoint_skill_runtime(
            session_id, endpoint, locks, trust, cwd, updates, approval, store, replay, binding,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_store_endpoint_and_tool_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
        tool_runtime: Arc<lato_tools::ToolRuntime>,
    ) -> Result<Self, AgentError> {
        Self::new_with_store_endpoint_runtime(
            session_id,
            endpoint,
            locks,
            trust,
            cwd,
            updates,
            approval,
            store,
            replay,
            DriverToolRuntime::Unbound(tool_runtime),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_store_endpoint_skill_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
        binding: SkillRuntimeBinding,
    ) -> Result<Self, AgentError> {
        Self::new_with_store_endpoint_runtime(
            session_id,
            endpoint,
            locks,
            trust,
            cwd,
            updates,
            approval,
            store,
            replay,
            DriverToolRuntime::Skills(binding),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_with_store_endpoint_runtime(
        session_id: String,
        endpoint: ActiveModelStream,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
        store: Arc<dyn SessionStore>,
        replay: JournalReplay,
        tool_runtime: DriverToolRuntime,
    ) -> Result<Self, AgentError> {
        if let Some(unresolved) = replay.projection.unresolved_tools.first() {
            return Err(journal_error(JournalError::IncompleteSideEffect {
                call_id: unresolved.call_id.clone(),
            }));
        }
        let session_id = SessionId::from(session_id);
        let driver = Arc::new(match tool_runtime {
            DriverToolRuntime::Skills(binding) => {
                LegacyTurnDriver::new_with_endpoint_skill_runtime(
                    session_id.to_string(),
                    endpoint,
                    locks,
                    trust,
                    cwd,
                    updates.clone(),
                    approval,
                    binding,
                )
            }
            DriverToolRuntime::Unbound(tool_runtime) => {
                LegacyTurnDriver::new_with_endpoint_and_tool_runtime(
                    session_id.to_string(),
                    endpoint,
                    locks,
                    trust,
                    cwd,
                    updates.clone(),
                    approval,
                    tool_runtime,
                )
            }
        });
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
            active_operation: Arc::new(Mutex::new(None)),
            submission_gate: Mutex::new(()),
            plugin_state: Arc::new(Mutex::new(SessionPluginState::default())),
            failed_closed: Arc::new(AtomicBool::new(false)),
            hooks_started: AtomicBool::new(false),
            hooks_ended: AtomicBool::new(false),
        })
    }

    pub async fn prompt(&self, input: String) -> Result<RuntimePromptOutcome, AgentError> {
        self.ensure_observation_open()?;
        self.prompt_after_outer_observation_check(input).await
    }

    #[cfg(test)]
    async fn prompt_after_outer_check_signal(
        &self,
        input: String,
        passed_outer_check: tokio::sync::oneshot::Sender<()>,
    ) -> Result<RuntimePromptOutcome, AgentError> {
        self.ensure_observation_open()?;
        let _ = passed_outer_check.send(());
        self.prompt_after_outer_observation_check(input).await
    }

    async fn prompt_after_outer_observation_check(
        &self,
        input: String,
    ) -> Result<RuntimePromptOutcome, AgentError> {
        // Locking before subscribing prevents a waiting prompt from consuming
        // another prompt's start event while preserving subscribe-before-submit.
        let gate = self.submission_gate.lock().await;
        // A queued prompt may have passed the fast check before the prompt
        // holding the gate lost the authoritative event stream. Recheck under
        // the gate before binding a plugin generation or touching the command
        // bus.
        self.ensure_observation_open()?;
        let mut cleanup = PromptCleanupGuard {
            armed: true,
            handle: self.handle.clone(),
            active_operation: Arc::clone(&self.active_operation),
            plugin_state: Arc::clone(&self.plugin_state),
            failed_closed: Arc::clone(&self.failed_closed),
        };
        if let Err(error) = self.begin_plugin_turn().await {
            cleanup.disarm();
            return Err(error);
        }
        let mut events = self.handle.subscribe();
        if let Err(error) = self
            .handle
            .submit(Command::StartTurn(StartTurn {
                input: UserInput::text(input),
                behavior: StartBehavior::Reject,
            }))
            .await
        {
            self.abort_plugin_turn().await;
            cleanup.disarm();
            return Err(error);
        }

        let mut observed_turn = None;
        let mut gate = Some(gate);
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let error = self
                        .fail_prompt_observation(PromptObservationFailure::Lagged(skipped))
                        .await;
                    cleanup.disarm();
                    return Err(error);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    let error = self
                        .fail_prompt_observation(PromptObservationFailure::Closed)
                        .await;
                    cleanup.disarm();
                    return Err(error);
                }
            };

            if event.session_id != self.session_id {
                continue;
            }

            match event.payload {
                EventPayload::TurnStarted => {
                    let Some(turn_id) = event.turn_id else {
                        let error = self
                            .fail_prompt_observation(PromptObservationFailure::MissingTurnId)
                            .await;
                        cleanup.disarm();
                        return Err(error);
                    };
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
                EventPayload::ContextUsageUpdated { usage }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    let _ = self.updates.send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "lato/session/context",
                        "params": {
                            "sessionId": self.session_id.as_str(),
                            "estimatedInputTokens": usage.estimated_input_tokens,
                            "contextWindow": usage.context_window,
                            "utilizationPercent": usage.utilization_percent,
                        },
                    }));
                }
                EventPayload::CompactionStarted {
                    compaction_id,
                    trigger,
                } if event.turn_id.as_ref() == observed_turn.as_ref() => {
                    self.send_compaction_update(
                        "started",
                        serde_json::json!({"compactionId": compaction_id, "trigger": trigger}),
                    );
                }
                EventPayload::CompactionCompleted {
                    compaction_id,
                    before,
                    after,
                    checkpoint_id,
                    warning,
                } if event.turn_id.as_ref() == observed_turn.as_ref() => {
                    self.send_compaction_update(
                        "completed",
                        serde_json::json!({"compactionId": compaction_id, "before": before, "after": after, "checkpointId": checkpoint_id, "warning": warning}),
                    );
                }
                EventPayload::CompactionFailed {
                    compaction_id,
                    error,
                } if event.turn_id.as_ref() == observed_turn.as_ref() => {
                    self.send_compaction_update(
                        "failed",
                        serde_json::json!({"compactionId": compaction_id, "error": error}),
                    );
                }
                EventPayload::CompactionCancelled { compaction_id }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.send_compaction_update(
                        "cancelled",
                        serde_json::json!({"compactionId": compaction_id}),
                    );
                }
                EventPayload::TurnCompleted(output)
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.finish_turn(observed_turn.as_ref()).await?;
                    cleanup.disarm();
                    return Ok(RuntimePromptOutcome::Complete {
                        text: output.final_text,
                    });
                }
                EventPayload::TurnCancelled { reason }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.finish_turn(observed_turn.as_ref()).await?;
                    cleanup.disarm();
                    return Ok(RuntimePromptOutcome::Cancelled { reason });
                }
                EventPayload::TurnFailed { error }
                    if event.turn_id.as_ref() == observed_turn.as_ref() =>
                {
                    self.finish_turn(observed_turn.as_ref()).await?;
                    cleanup.disarm();
                    return Err(error);
                }
                EventPayload::SessionStopped => {
                    self.clear_active(observed_turn.as_ref()).await;
                    self.abort_plugin_turn().await;
                    cleanup.disarm();
                    return Err(runtime_stopped());
                }
                EventPayload::SessionStarted
                | EventPayload::ModelDelta { .. }
                | EventPayload::ReasoningDelta { .. }
                | EventPayload::ContextUsageUpdated { .. }
                | EventPayload::TurnCompleted(_)
                | EventPayload::TurnCancelled { .. }
                | EventPayload::TurnFailed { .. }
                | EventPayload::CompactionStarted { .. }
                | EventPayload::CompactionCompleted { .. }
                | EventPayload::CompactionFailed { .. }
                | EventPayload::CompactionCancelled { .. }
                | EventPayload::PluginSnapshotAdopted { .. } => {}
            }
        }
    }

    pub async fn stage_plugin_snapshot(
        &self,
        snapshot: Arc<PluginSnapshot>,
    ) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        let skills = SkillCatalog::from_discovery(discover_skills(&snapshot));
        let mut state = self.plugin_state.lock().await;
        let newest = state
            .pending
            .as_ref()
            .map_or(state.current.generation(), |(pending, _)| {
                pending.generation()
            });
        if snapshot.generation() < newest {
            return Err(plugin_generation_rollback(newest, snapshot.generation()));
        }
        if snapshot.generation() == newest {
            return Ok(());
        }
        if state.active_turn.is_some() || self.active_operation.lock().await.is_some() {
            state.pending = Some((snapshot, skills));
            return Ok(());
        }
        let previous = Arc::clone(&state.current);
        drop(state);
        if let Err(error) = self.adopt_plugin_snapshot(&snapshot).await {
            self.plugin_state.lock().await.current = previous;
            return Err(error);
        }
        let mut state = self.plugin_state.lock().await;
        state.current = snapshot;
        state.current_skills = skills;
        Ok(())
    }

    pub async fn plugin_snapshot(&self) -> Arc<PluginSnapshot> {
        Arc::clone(&self.plugin_state.lock().await.current)
    }

    pub async fn active_turn_plugin_snapshot(&self) -> Option<Arc<PluginSnapshot>> {
        self.plugin_state.lock().await.active_turn.clone()
    }

    pub async fn cancel(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        if self.failed_closed.load(Ordering::Acquire) {
            let _ = self.handle.submit(Command::Shutdown).await;
            self.clear_active(None).await;
            self.abort_plugin_turn().await;
            return Ok(());
        }
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

    pub async fn steer(&self, input: String) -> Result<(), AgentError> {
        self.handle
            .submit(Command::SteerTurn(UserInput::text(input)))
            .await
    }

    pub async fn shutdown(&self) -> Result<(), AgentError> {
        let _gate = self.submission_gate.lock().await;
        if self.hooks_started.load(Ordering::Acquire)
            && !self.hooks_ended.swap(true, Ordering::AcqRel)
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.driver.observe_hook(
                    lato_extensions::hooks::HookEventName::SessionEnd,
                    serde_json::json!({"reason":"shutdown","status":"stopped"}),
                ),
            )
            .await;
        }
        let result = self.handle.submit(Command::Shutdown).await;
        *self.active_operation.lock().await = None;
        self.abort_plugin_turn().await;
        match result {
            Err(error)
                if self.failed_closed.load(Ordering::Acquire)
                    && (error.code == "runtime.command_bus_closed"
                        || error.code == "runtime.reply_bus_closed") =>
            {
                Ok(())
            }
            other => other,
        }
    }

    pub async fn cancel_and_join(&self, deadline: Duration) -> Result<(), AgentError> {
        self.cancel().await?;
        tokio::time::timeout(deadline, self.shutdown())
            .await
            .map_err(|_| {
                AgentError::new(
                    "task.child.shutdown_timeout",
                    ErrorCategory::Task,
                    "child runtime session did not shut down before its deadline",
                    Retryability::Safe,
                )
            })??;
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<lato_core::EventEnvelope> {
        self.handle.subscribe()
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.driver.history_snapshot().await
    }

    pub async fn active_model(&self) -> lato_ai::ActiveModelPort {
        self.driver.active_model().await
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        self.driver.replace_history(history).await;
    }

    pub async fn is_active(&self) -> bool {
        self.active_operation.lock().await.is_some()
    }

    pub(crate) async fn auth_refreshed(&self) {
        self.driver.auth_refreshed().await;
    }

    #[cfg(test)]
    pub(crate) async fn automatic_compaction_allowed(&self, trigger: CompactionTrigger) -> bool {
        self.driver.automatic_compaction_allowed(trigger).await
    }

    pub async fn switch_model(
        &self,
        prepared: PreparedModelSwitch,
    ) -> Result<ModelSwitchOutcome, AgentError> {
        let gate = self.submission_gate.lock().await;
        if self.active_operation.lock().await.is_some() {
            return Err(session_busy());
        }

        let previous = self.driver.active_endpoint();
        let history = self.driver.history_snapshot().await;
        let estimated_tokens = estimate_history_tokens(&history);
        let switch = decide_switch_compaction(
            &previous.port.metadata,
            &prepared.active.port.metadata,
            estimated_tokens,
            crate::has_model_authored_history(&history),
            CompactionPolicy::default().threshold_percent,
        );
        let selection = prepared.active.port.selection.clone();
        let metadata = prepared.active.port.metadata.clone();
        let context_budget_changed =
            previous.port.metadata.context_window != metadata.context_window;
        self.driver.activate_model(prepared.active).await;

        if let Err(error) = self
            .handle
            .submit(Command::SelectModel {
                selection: selection.clone(),
                model_family: metadata.model_family,
                context_window: metadata.context_window,
            })
            .await
        {
            self.driver.activate_model(previous).await;
            return Err(error);
        }
        self.driver.model_generation_changed().await;
        if context_budget_changed {
            self.driver.context_budget_changed().await;
        }

        let mut compaction_warning = None;
        match switch {
            SwitchCompaction::Immediate => {
                if self
                    .driver
                    .automatic_compaction_allowed(CompactionTrigger::ModelSwitch)
                    .await
                {
                    match self
                        .compact_with_gate_held(CompactionTrigger::ModelSwitch, None, false, gate)
                        .await
                    {
                        Ok(RuntimeCompactionOutcome::Complete { warning, .. }) => {
                            compaction_warning = warning;
                        }
                        Ok(RuntimeCompactionOutcome::Cancelled) => {}
                        Err(error) if is_auth_failure(&error) => return Err(error),
                        Err(error) => compaction_warning = Some(error),
                    }
                }
            }
            SwitchCompaction::BeforeNextSample => {
                self.driver.mark_model_switch_check().await;
            }
            SwitchCompaction::None => {}
        }

        Ok(ModelSwitchOutcome {
            provider: selection.provider,
            model: selection.model,
            compaction_warning,
        })
    }

    async fn clear_active(&self, turn_id: Option<&TurnId>) {
        let expected = turn_id.cloned().map(ActiveOperation::Turn);
        self.clear_operation(expected.as_ref()).await;
    }

    fn ensure_observation_open(&self) -> Result<(), AgentError> {
        if self.failed_closed.load(Ordering::Acquire) {
            Err(runtime_observation_failed_closed())
        } else {
            Ok(())
        }
    }

    /// Losing the authoritative event stream means the facade can no longer
    /// prove which generation owns the live turn. Fail the session closed and
    /// wait for the runtime shutdown acknowledgement before clearing facade
    /// state. Pending plugin generations are deliberately left unapplied.
    async fn fail_prompt_observation(&self, failure: PromptObservationFailure) -> AgentError {
        self.failed_closed.store(true, Ordering::Release);
        let _ = self.handle.submit(Command::Shutdown).await;
        self.clear_active(None).await;
        self.abort_plugin_turn().await;
        match failure {
            PromptObservationFailure::Lagged(skipped) => event_lagged(skipped),
            PromptObservationFailure::Closed => event_bus_closed(),
            PromptObservationFailure::MissingTurnId => missing_turn_id(),
        }
    }

    async fn finish_turn(&self, turn_id: Option<&TurnId>) -> Result<(), AgentError> {
        self.clear_active(turn_id).await;
        let _gate = self.submission_gate.lock().await;
        let pending = {
            let mut state = self.plugin_state.lock().await;
            state.active_turn = None;
            state.active_turn_skills = None;
            state.pending.take()
        };
        let Some((pending, pending_skills)) = pending else {
            return Ok(());
        };
        if let Err(error) = self.adopt_plugin_snapshot(&pending).await {
            self.plugin_state.lock().await.pending = Some((pending, pending_skills));
            return Err(error);
        }
        let mut state = self.plugin_state.lock().await;
        state.current = pending;
        state.current_skills = pending_skills;
        Ok(())
    }

    async fn begin_plugin_turn(&self) -> Result<(), AgentError> {
        let mut state = self.plugin_state.lock().await;
        if state.active_turn.is_some() {
            return Err(session_busy());
        }
        let current = Arc::clone(&state.current);
        let catalog = Arc::clone(&state.current_skills);
        self.driver.bind_turn_skills(catalog).await;
        self.driver
            .bind_turn_hooks(materialize_hooks(&current))
            .await;
        if !self.hooks_started.swap(true, Ordering::AcqRel) {
            let _ = self
                .driver
                .observe_hook(
                    lato_extensions::hooks::HookEventName::SessionStart,
                    serde_json::json!({"source":"runtime","generation":current.generation()}),
                )
                .await;
        }
        state.active_turn = Some(current);
        state.active_turn_skills = Some(Arc::clone(&state.current_skills));
        Ok(())
    }

    async fn abort_plugin_turn(&self) {
        let mut state = self.plugin_state.lock().await;
        state.active_turn = None;
        state.active_turn_skills = None;
    }

    async fn adopt_plugin_snapshot(&self, snapshot: &PluginSnapshot) -> Result<(), AgentError> {
        self.handle
            .submit(Command::AdoptPluginSnapshot {
                summary: snapshot_summary(snapshot),
            })
            .await
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
        self.compact_with_gate_held(CompactionTrigger::Manual, user_context, true, gate)
            .await
    }

    async fn compact_with_gate_held<'a>(
        &self,
        trigger: CompactionTrigger,
        user_context: Option<String>,
        release_gate_after_start: bool,
        gate: tokio::sync::MutexGuard<'a, ()>,
    ) -> Result<RuntimeCompactionOutcome, AgentError> {
        let snapshot = self.plugin_snapshot().await;
        self.driver
            .bind_turn_hooks(materialize_hooks(&snapshot))
            .await;
        if !self.hooks_started.swap(true, Ordering::AcqRel) {
            let _ = self
                .driver
                .observe_hook(
                    lato_extensions::hooks::HookEventName::SessionStart,
                    serde_json::json!({"source":"compaction","generation":snapshot.generation()}),
                )
                .await;
        }
        let _ = self
            .driver
            .observe_hook(
                lato_extensions::hooks::HookEventName::PreCompact,
                serde_json::json!({"trigger":trigger}),
            )
            .await;
        let mut events = self.handle.subscribe();
        self.handle
            .submit(Command::CompactSession(CompactSession {
                user_context: user_context.and_then(|value| {
                    let value = value.trim().to_owned();
                    (!value.is_empty()).then_some(value)
                }),
                trigger,
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
                    if release_gate_after_start {
                        drop(gate.take());
                    }
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
                    let _ = self
                        .driver
                        .observe_hook(
                            lato_extensions::hooks::HookEventName::PostCompact,
                            serde_json::json!({"trigger":trigger,"before":before,"after":after}),
                        )
                        .await;
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

fn runtime_observation_failed_closed() -> AgentError {
    AgentError::new(
        "runtime.observation_failed_closed",
        ErrorCategory::InternalInvariant,
        "session is unavailable after losing its authoritative runtime event stream",
        Retryability::Never,
    )
}

fn session_busy() -> AgentError {
    AgentError::new(
        "runtime.session_busy",
        ErrorCategory::Task,
        "cannot switch models while the session is active",
        Retryability::Never,
    )
}

fn plugin_generation_rollback(current: u64, requested: u64) -> AgentError {
    AgentError::new(
        "plugin.snapshot_generation_rollback",
        ErrorCategory::InvalidInput,
        format!(
            "cannot replace plugin snapshot generation {current} with older generation {requested}"
        ),
        Retryability::Never,
    )
}

fn snapshot_summary(snapshot: &PluginSnapshot) -> PluginSnapshotSummary {
    PluginSnapshotSummary {
        generation: snapshot.generation(),
        discovered: snapshot.plugins().len(),
        active: snapshot.active_plugins().count(),
        project_trusted: snapshot.project_trusted(),
    }
}

fn is_auth_failure(error: &AgentError) -> bool {
    let message = error.message.to_ascii_lowercase();
    error.code == "model.auth"
        || message.contains("model.auth")
        || message.contains("http 401")
        || message.contains("http 403")
        || message.contains("oauth refresh failed")
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
    use async_trait::async_trait;
    use lato_ai::{ModelMetadata, StreamPiece};
    use lato_core::{
        ModelContent, ModelError, ModelMessage, ModelRole, ToolCallId, UnresolvedToolCall,
    };
    use lato_extensions::{DiscoveryResult, PluginConfig, build_snapshot};
    use lato_runtime::{CompactionControl, PrefireCompactionRequest};
    use lato_store::MemoryEventStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct InvalidThenSummaryStream {
        calls: AtomicUsize,
    }

    struct TextStream {
        text: String,
        calls: AtomicUsize,
    }

    struct BlockingTurnStream;

    #[async_trait]
    impl ModelStream for BlockingTurnStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            _tx: mpsc::Sender<StreamPiece>,
        ) -> Result<(), ModelError> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ModelStream for TextStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            tx: mpsc::Sender<StreamPiece>,
        ) -> Result<(), ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tx.send(StreamPiece::Text(self.text.clone()))
                .await
                .map_err(|_| ModelError::cancelled())
        }
    }

    #[tokio::test]
    async fn observation_failures_stop_the_turn_and_never_adopt_pending_generation() {
        for (failure, expected_code) in [
            (PromptObservationFailure::Lagged(7), "runtime.event_lagged"),
            (PromptObservationFailure::Closed, "runtime.event_bus_closed"),
            (
                PromptObservationFailure::MissingTurnId,
                "runtime.missing_turn_id",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (updates, _) = mpsc::unbounded_channel();
            let session = RuntimeSession::new(
                format!("observation-failure-{expected_code}"),
                Arc::new(BlockingTurnStream),
                Arc::new(FileLocks::new()),
                SessionTrust::for_headless_prompt(directory.path()),
                directory.path().to_path_buf(),
                updates,
                None,
            );
            session.begin_plugin_turn().await.unwrap();
            let mut events = session.handle.subscribe();
            session
                .handle
                .submit(Command::StartTurn(StartTurn {
                    input: UserInput::text("block"),
                    behavior: StartBehavior::Reject,
                }))
                .await
                .unwrap();
            let turn_id = loop {
                let event = events.recv().await.unwrap();
                if matches!(event.payload, EventPayload::TurnStarted) {
                    break event.turn_id.unwrap();
                }
            };
            *session.active_operation.lock().await = Some(ActiveOperation::Turn(turn_id));
            let pending =
                build_snapshot(2, DiscoveryResult::default(), &PluginConfig::default()).unwrap();
            let pending_skills = SkillCatalog::from_discovery(discover_skills(&pending));
            session.plugin_state.lock().await.pending = Some((pending, pending_skills));

            let error = session.fail_prompt_observation(failure).await;
            assert_eq!(error.code, expected_code);
            assert!(session.failed_closed.load(Ordering::Acquire));
            assert!(!session.is_active().await);
            let state = session.plugin_state.lock().await;
            assert!(state.active_turn.is_none());
            assert!(state.active_turn_skills.is_none());
            assert_eq!(state.current.generation(), 0);
            assert_eq!(state.pending.as_ref().unwrap().0.generation(), 2);
            drop(state);
            let prompt_error = session.prompt("again".into()).await.unwrap_err();
            assert_eq!(prompt_error.code, "runtime.observation_failed_closed");
        }
    }

    #[tokio::test]
    async fn queued_prompt_rechecks_failed_closed_after_acquiring_submission_gate() {
        let directory = tempfile::tempdir().unwrap();
        let (updates, _) = mpsc::unbounded_channel();
        let session = Arc::new(RuntimeSession::new(
            "queued-observation-failure".into(),
            Arc::new(BlockingTurnStream),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(directory.path()),
            directory.path().to_path_buf(),
            updates,
            None,
        ));
        let gate = session.submission_gate.lock().await;
        let (passed_outer_check, outer_check_passed) = tokio::sync::oneshot::channel();
        let queued = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .prompt_after_outer_check_signal("queued".into(), passed_outer_check)
                    .await
            }
        });
        outer_check_passed.await.unwrap();

        let pending =
            build_snapshot(2, DiscoveryResult::default(), &PluginConfig::default()).unwrap();
        let pending_skills = SkillCatalog::from_discovery(discover_skills(&pending));
        session.plugin_state.lock().await.pending = Some((pending, pending_skills));
        let failure = session
            .fail_prompt_observation(PromptObservationFailure::Closed)
            .await;
        assert_eq!(failure.code, "runtime.event_bus_closed");
        drop(gate);

        let error = queued.await.unwrap().unwrap_err();
        assert_eq!(error.code, "runtime.observation_failed_closed");
        assert!(!session.is_active().await);
        let state = session.plugin_state.lock().await;
        assert!(state.active_turn.is_none());
        assert!(state.active_turn_skills.is_none());
        assert_eq!(state.current.generation(), 0);
        assert_eq!(state.pending.as_ref().unwrap().0.generation(), 2);
    }

    #[tokio::test]
    async fn aborted_prompt_owns_shutdown_and_clears_facade_without_adopting_pending() {
        let directory = tempfile::tempdir().unwrap();
        let (updates, _) = mpsc::unbounded_channel();
        let session = Arc::new(RuntimeSession::new(
            "aborted-prompt-cleanup".into(),
            Arc::new(BlockingTurnStream),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(directory.path()),
            directory.path().to_path_buf(),
            updates,
            None,
        ));
        let mut events = session.subscribe();
        let prompt = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.prompt("block".into()).await }
        });
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.payload, EventPayload::TurnStarted) {
                break;
            }
        }
        let pending =
            build_snapshot(2, DiscoveryResult::default(), &PluginConfig::default()).unwrap();
        let pending_skills = SkillCatalog::from_discovery(discover_skills(&pending));
        session.plugin_state.lock().await.pending = Some((pending, pending_skills));

        prompt.abort();
        assert!(prompt.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let inactive = !session.is_active().await;
                let state = session.plugin_state.lock().await;
                let cleaned =
                    inactive && state.active_turn.is_none() && state.active_turn_skills.is_none();
                drop(state);
                if cleaned {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(session.failed_closed.load(Ordering::Acquire));
        let state = session.plugin_state.lock().await;
        assert_eq!(state.current.generation(), 0);
        assert_eq!(state.pending.as_ref().unwrap().0.generation(), 2);
        drop(state);
        session.cancel().await.unwrap();
        session.shutdown().await.unwrap();
        assert!(!session.is_active().await);
        assert_eq!(Arc::strong_count(&session), 1);
    }

    fn prefire_request() -> PrefireCompactionRequest {
        PrefireCompactionRequest {
            messages: vec![
                ModelMessage {
                    role: ModelRole::System,
                    content: vec![ModelContent::Text {
                        text: "system".into(),
                    }],
                },
                ModelMessage {
                    role: ModelRole::User,
                    content: vec![ModelContent::Text {
                        text: "retain objective".into(),
                    }],
                },
            ],
            prefix_len: 2,
            policy: CompactionPolicy::default(),
        }
    }

    #[async_trait]
    impl ModelStream for InvalidThenSummaryStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            tx: mpsc::Sender<StreamPiece>,
        ) -> Result<(), ModelError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = if call < 2 {
                "invalid summary".to_string()
            } else {
                let detail = "preserve verified state and pending work ".repeat(3);
                crate::REQUIRED_SECTIONS
                    .iter()
                    .enumerate()
                    .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            tx.send(StreamPiece::Text(text))
                .await
                .map_err(|_| ModelError::cancelled())
        }
    }

    #[tokio::test]
    async fn repeated_same_window_model_switches_keep_prefire_and_final_within_budget() {
        let directory = tempfile::tempdir().unwrap();
        let (updates, _updates_rx) = mpsc::unbounded_channel();
        let first_prefire = Arc::new(TextStream {
            text: "first speculative note".into(),
            calls: AtomicUsize::new(0),
        });
        let initial = adapt_model_endpoint(
            "fixture",
            "old",
            ModelMetadata {
                context_window: Some(120_000),
                model_family: Some("family-a".into()),
            },
            first_prefire.clone(),
        )
        .unwrap();
        let session = RuntimeSession::new_with_endpoint(
            "same-window-prefire-budget".into(),
            initial,
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(directory.path()),
            directory.path().to_path_buf(),
            updates,
            None,
        );
        session
            .replace_history(vec![
                HistoryItem::System("system".into()),
                HistoryItem::User("retain objective".into()),
                HistoryItem::AssistantText("prior work ".repeat(30_000)),
            ])
            .await;
        TurnDriver::prefire_compaction(
            session.driver.as_ref(),
            prefire_request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        let second_prefire = Arc::new(TextStream {
            text: "second speculative note".into(),
            calls: AtomicUsize::new(0),
        });
        let active = adapt_model_endpoint(
            "fixture",
            "middle",
            ModelMetadata {
                context_window: Some(120_000),
                model_family: Some("family-a".into()),
            },
            second_prefire.clone(),
        )
        .unwrap();
        session
            .switch_model(PreparedModelSwitch { active })
            .await
            .unwrap();
        TurnDriver::prefire_compaction(
            session.driver.as_ref(),
            prefire_request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        let rejected_prefire = Arc::new(TextStream {
            text: "must not be submitted".into(),
            calls: AtomicUsize::new(0),
        });
        let active = adapt_model_endpoint(
            "fixture",
            "third",
            ModelMetadata {
                context_window: Some(120_000),
                model_family: Some("family-a".into()),
            },
            rejected_prefire.clone(),
        )
        .unwrap();
        session
            .switch_model(PreparedModelSwitch { active })
            .await
            .unwrap();
        let error = TurnDriver::prefire_compaction(
            session.driver.as_ref(),
            prefire_request(),
            CompactionControl {
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "compaction.prefire_attempt_budget_exhausted");
        assert_eq!(rejected_prefire.calls.load(Ordering::SeqCst), 0);

        let final_stream = Arc::new(InvalidThenSummaryStream {
            calls: AtomicUsize::new(0),
        });
        let active = adapt_model_endpoint(
            "fixture",
            "new",
            ModelMetadata {
                context_window: Some(120_000),
                model_family: Some("family-b".into()),
            },
            final_stream.clone(),
        )
        .unwrap();

        let switched = session
            .switch_model(PreparedModelSwitch { active })
            .await
            .unwrap();
        assert!(switched.compaction_warning.is_some());
        assert_eq!(final_stream.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first_prefire.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_prefire.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first_prefire.calls.load(Ordering::SeqCst)
                + second_prefire.calls.load(Ordering::SeqCst)
                + final_stream.calls.load(Ordering::SeqCst),
            usize::from(CompactionPolicy::default().max_attempts)
        );

        let manual = session.compact(None).await.unwrap();
        assert!(matches!(manual, RuntimeCompactionOutcome::Complete { .. }));
        assert_eq!(final_stream.calls.load(Ordering::SeqCst), 3);
    }

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
