// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs
// Phase 6A derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/acp_session_impl/hooks_plugins.rs
// License: Apache-2.0
// Lato changes: ACP model selection and plugin snapshots publish only committed session changes

use crate::{
    ChildSessionRunner, PreparedModelSwitch, ProfileResultVerifier, RuntimeCompactionOutcome,
    RuntimePromptOutcome, RuntimeSession, SessionPluginSnapshots, SkillRuntimeBinding,
    ToolApproval, TranscriptStore,
    agentfield::{
        AgentFieldCatalog, AgentFieldManager, AgentFieldTool, SessionAgentFieldHandle,
        load_agentfield_config,
    },
    import_legacy_if_needed,
    workflow::{
        SessionWorkflowHandle, WorkflowManager, WorkflowTool, list_workflows, resolve_workflow,
    },
};
use lato_ai::{
    ActiveModelStream, CATALOG, CredentialStore, CustomHttpModelStream, CustomModel,
    FakeModelStream, HttpModelStream, ModelStream, StreamPiece, adapt_model_endpoint,
    api_key_login_allowed, custom_model_auth, dialect_implemented, get_auth_refreshing,
    load_models_json, lookup_model, oauth_allowed, phase0_supported, store_oauth,
};
use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, EventStore, JournalReplay, SessionId, SessionStore,
    TaskId, TaskOwner, Tool, ToolCapability, TurnId, VerificationPolicy, WorkspaceIntent,
};
use lato_extensions::{
    DiscoveryConfig, PluginConfig, PluginSnapshot, ReloadRequest, SharedPluginRegistryHandle,
    build_snapshot, discover_plugins,
};
use lato_protocol::{
    JsonRpcReq, METHODS_IMPLEMENTED, PROTOCOL_VERSION, PluginDiagnosticDto, PluginReloadRequest,
    PluginReloadResponse, err, err_with_data, is_implemented, ok,
};
use lato_runtime::{
    ChannelBackend, CoordinatorConfig, NoopTaskEventSink, TaskHandle, TaskRootRequest,
    spawn_subagent_coordinator_with_verifier,
};
use lato_store::{FileEventStore, derive_automatic_title};
use lato_workspace::{
    ApprovalMode, FileLocks, GitWorkspaceAllocator, MemoryWorkspaceAllocator, SessionTrust,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

pub struct AcpHost {
    sessions: HashMap<String, Arc<RuntimeSession>>,
    pub updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    next_id: usize,
    cwd: PathBuf,
    trust: SessionTrust,
    default_endpoint: ActiveModelStream,
    locks: Arc<FileLocks>,
    pub prompts_via_acp: usize,
    transcripts: Option<TranscriptStore>,
    events: Option<Arc<FileEventStore>>,
    credentials: Option<CredentialStore>,
    custom_models: Vec<CustomModel>,
    lato_home: PathBuf,
    plugin_registry: SharedPluginRegistryHandle,
    plugin_config: PluginConfig,
    cli_plugin_dirs: Vec<PathBuf>,
    session_plugins: SessionPluginSnapshots,
    tool_approval: Option<Arc<dyn ToolApproval>>,
    task_handle: TaskHandle,
    task_backends: HashMap<String, ChannelBackend>,
    next_task_root: u64,
    _task_actor: tokio::task::JoinHandle<()>,
    _task_events: tokio::task::JoinHandle<()>,
    _worktree_recovery: Option<tokio::task::JoinHandle<()>>,
}

impl AcpHost {
    pub fn new(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
    ) -> Self {
        Self::new_with_approval(cwd, trust, updates, stream, None)
    }

    pub fn new_with_home(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        lato_home: PathBuf,
    ) -> Self {
        Self::new_with_home_and_plugin_dirs(cwd, trust, updates, stream, lato_home, Vec::new())
    }

    pub fn new_with_home_and_plugin_dirs(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        lato_home: PathBuf,
        cli_plugin_dirs: Vec<PathBuf>,
    ) -> Self {
        Self::build(
            cwd,
            trust,
            updates,
            stream,
            None,
            Some(lato_home),
            cli_plugin_dirs,
        )
    }

    pub fn new_with_approval(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        tool_approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let lato_home = std::env::var_os("LATO_HOME").map(PathBuf::from);
        Self::build(
            cwd,
            trust,
            updates,
            stream,
            tool_approval,
            lato_home,
            Vec::new(),
        )
    }

    pub fn new_with_approval_and_home(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        tool_approval: Option<Arc<dyn ToolApproval>>,
        lato_home: PathBuf,
    ) -> Self {
        Self::new_with_approval_home_and_plugin_dirs(
            cwd,
            trust,
            updates,
            stream,
            tool_approval,
            lato_home,
            Vec::new(),
        )
    }

    pub fn new_with_approval_home_and_plugin_dirs(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        tool_approval: Option<Arc<dyn ToolApproval>>,
        lato_home: PathBuf,
        cli_plugin_dirs: Vec<PathBuf>,
    ) -> Self {
        Self::build(
            cwd,
            trust,
            updates,
            stream,
            tool_approval,
            Some(lato_home),
            cli_plugin_dirs,
        )
    }

    fn build(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        tool_approval: Option<Arc<dyn ToolApproval>>,
        lato_home: Option<PathBuf>,
        cli_plugin_dirs: Vec<PathBuf>,
    ) -> Self {
        let default_endpoint = if let Some(port) = stream.active_model_port() {
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
        };
        let transcripts = lato_home
            .as_deref()
            .and_then(|home| TranscriptStore::open(home).ok());
        let events = lato_home
            .as_deref()
            .and_then(|home| FileEventStore::open(home).ok())
            .map(Arc::new);
        let credentials = lato_home
            .as_deref()
            .and_then(|home| CredentialStore::open(home).ok());
        let custom_models = lato_home
            .as_deref()
            .and_then(|home| load_models_json(&home.join("models.json")).ok())
            .unwrap_or_default();
        let lato_home_path = lato_home.clone().unwrap_or_default();
        let plugin_config = load_plugin_config(lato_home.as_deref());
        let initial_discovery = discover_plugins(&DiscoveryConfig {
            cwd: cwd.clone(),
            lato_home: lato_home_path.clone(),
            cli_plugin_dirs: cli_plugin_dirs.clone(),
            project_trusted: trust.cwd_trusted(),
        });
        let initial_snapshot = build_snapshot(1, initial_discovery, &plugin_config)
            .unwrap_or_else(|_| PluginSnapshot::empty());
        let plugin_registry = SharedPluginRegistryHandle::new(Some(initial_snapshot));
        let session_plugins = SessionPluginSnapshots::default();
        let runner = Arc::new(
            ChildSessionRunner::new(
                default_endpoint.stream.clone(),
                Arc::new(FileLocks::new()),
                trust.clone(),
                updates.clone(),
                tool_approval.clone(),
            )
            .with_session_plugins(session_plugins.clone()),
        );
        let verifier = Arc::new(ProfileResultVerifier);
        let sink = Arc::new(NoopTaskEventSink);
        let mut worktree_recovery = None;
        let (task_handle, task_actor) =
            match GitWorkspaceAllocator::new(&cwd, cwd.join(".lato/worktrees")) {
                Ok(allocator) => {
                    let allocator = Arc::new(allocator);
                    let recovery_allocator = Arc::clone(&allocator);
                    let recovery_updates = updates.clone();
                    worktree_recovery = Some(tokio::spawn(async move {
                        match recovery_allocator.recover_stale().await {
                            Ok(0) => {}
                            Ok(recovered) => {
                                let _ = recovery_updates.send(serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "lato/task/worktree_recovery",
                                    "params": {"recovered": recovered},
                                }));
                            }
                            Err(error) => {
                                let _ = recovery_updates.send(serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "lato/task/worktree_recovery",
                                    "params": {"error": error},
                                }));
                            }
                        }
                    }));
                    spawn_subagent_coordinator_with_verifier(
                        CoordinatorConfig::default(),
                        runner,
                        allocator,
                        verifier,
                        sink,
                    )
                }
                Err(_) => spawn_subagent_coordinator_with_verifier(
                    CoordinatorConfig::default(),
                    runner,
                    Arc::new(
                        MemoryWorkspaceAllocator::new(&cwd)
                            .expect("an existing host cwd is a valid memory workspace root"),
                    ),
                    verifier,
                    sink,
                ),
            };
        let mut task_events = task_handle.subscribe();
        let task_updates = updates.clone();
        let task_event_relay = tokio::spawn(async move {
            loop {
                match task_events.recv().await {
                    Ok(event) => {
                        if event.parent_id.is_none() {
                            continue;
                        }
                        let _ = task_updates.send(serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "lato/task/event",
                            "params": event,
                        }));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self {
            sessions: HashMap::new(),
            updates,
            next_id: 1,
            cwd,
            trust,
            default_endpoint,
            locks: Arc::new(FileLocks::new()),
            prompts_via_acp: 0,
            transcripts,
            events,
            credentials,
            custom_models,
            lato_home: lato_home_path,
            plugin_registry,
            plugin_config,
            cli_plugin_dirs,
            session_plugins,
            tool_approval,
            task_handle,
            task_backends: HashMap::new(),
            next_task_root: 1,
            _task_actor: task_actor,
            _task_events: task_event_relay,
            _worktree_recovery: worktree_recovery,
        }
    }

    async fn ensure_task_root(&mut self, session_id: &str) -> Result<ChannelBackend, String> {
        if let Some(backend) = self.task_backends.get(session_id) {
            return Ok(backend.clone());
        }
        let root_sequence = self.next_task_root;
        self.next_task_root = self.next_task_root.saturating_add(1);
        let root_id = TaskId::from(format!("task-root-{session_id}-{root_sequence}"));
        let root = self
            .task_handle
            .register_root(TaskRootRequest {
                task_id: root_id,
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from(session_id),
                    turn_id: TurnId::from(format!("task-root-turn-{session_id}")),
                },
                profile: AgentProfile {
                    name: "coordinator".into(),
                    instructions: "Coordinate bounded child tasks.".into(),
                    capabilities: all_task_capabilities(),
                    workspace: WorkspaceIntent::IsolatedWorktree,
                    verification: VerificationPolicy::Accept,
                    definition_background: false,
                },
                permissions: all_task_capabilities(),
                budget: host_task_budget(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let backend = ChannelBackend::new(root);
        self.task_backends
            .insert(session_id.to_owned(), backend.clone());
        Ok(backend)
    }

    pub fn task_backend(&self, session_id: &str) -> Option<ChannelBackend> {
        self.task_backends.get(session_id).cloned()
    }

    async fn teardown_task_root(&mut self, session_id: &str) -> Result<(), String> {
        let Some(backend) = self.task_backends.remove(session_id) else {
            return Ok(());
        };
        backend
            .scoped_handle()
            .teardown_root_and_drain()
            .await
            .map_err(|error| error.to_string())?;
        self.task_handle
            .shutdown_root(backend.scoped_handle().root_id().clone())
            .await
            .map_err(|error| error.to_string())
    }

    async fn make_runtime_session(
        &mut self,
        sid: &str,
        replay: Option<JournalReplay>,
    ) -> Result<Arc<RuntimeSession>, String> {
        let backend = self.ensure_task_root(sid).await?;
        // Phase 7B7: the main-session catalog carries exactly one session-bound
        // `workflow` tool. Subagent and headless catalogs never receive it.
        let workflow_handle = SessionWorkflowHandle::new(
            self.cwd.clone(),
            self.effective_lato_home(),
            self.trust.clone(),
        );
        let mut session_extra: Vec<Arc<dyn Tool>> =
            vec![Arc::new(WorkflowTool::new(workflow_handle.clone()))];
        // Phase 7C2: the main-session `agentfield` tool is registered only
        // when the adapter is enabled AND the configuration validates.
        // Unconfigured / disabled / invalid → zero registration, zero
        // network (AC-01). The manager itself is installed after the session
        // is assembled and performs no network at registration time.
        let agentfield_handle = match self.prepare_agentfield(sid) {
            Some((handle, manager)) => {
                session_extra.push(Arc::new(AgentFieldTool::new(handle.clone())));
                Some((handle, manager))
            }
            None => None,
        };
        let skill_runtime = match SkillRuntimeBinding::build(|skill_resolver, mcp_backend| {
            lato_tools::builtin_tool_runtime_with_subagents_and_mcp_extra(
                lato_tools::BuiltinToolEnvironment {
                    cwd: self.cwd.clone(),
                    locks: self.locks.clone(),
                    trust: self.trust.clone(),
                    skill_resolver: Some(skill_resolver),
                },
                backend.into_resource(),
                mcp_backend,
                session_extra,
            )
        }) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.teardown_task_root(sid).await;
                return Err(error.to_string());
            }
        };
        if let Some(events) = self.events.clone() {
            let replay = match replay {
                Some(replay) => replay,
                None => events
                    .replay(&SessionId::from(sid))
                    .await
                    .map_err(|error| error.to_string())?,
            };
            let store: Arc<dyn SessionStore> = events;
            let endpoint = match replay.projection.model_selection.as_ref() {
                Some(selection) => self
                    .prepare_model_endpoint(&selection.provider, &selection.model)
                    .await
                    .map_err(|error| format!("model.unavailable_on_resume: {error}"))?,
                None => self.default_endpoint.clone(),
            };
            let endpoint_stream = endpoint.stream.clone();
            let session = RuntimeSession::new_with_store_endpoint_skill_runtime(
                sid.to_string(),
                endpoint,
                self.locks.clone(),
                self.trust.clone(),
                self.cwd.clone(),
                self.updates.clone(),
                self.tool_approval.clone(),
                store,
                replay,
                skill_runtime,
            )
            .await;
            return match session {
                Ok(session) => {
                    let session = Arc::new(session);
                    let manager = self.attach_workflow_manager(sid, &session, endpoint_stream);
                    // Phase 7B7: bind the session manager into the main-session
                    // `workflow` tool (construction-safe handle install).
                    workflow_handle.install(manager);
                    if let Some((handle, manager)) = agentfield_handle.as_ref() {
                        session.attach_agentfield_manager(manager.clone());
                        handle.install(manager.clone());
                    }
                    if let Err(error) = self.attach_session_plugins(sid, &session).await {
                        let _ = self.teardown_task_root(sid).await;
                        return Err(error);
                    }
                    Ok(session)
                }
                Err(error) => {
                    let _ = self.teardown_task_root(sid).await;
                    Err(error.to_string())
                }
            };
        }
        let session = Arc::new(RuntimeSession::new_with_endpoint_skill_runtime(
            sid.to_string(),
            self.default_endpoint.clone(),
            self.locks.clone(),
            self.trust.clone(),
            self.cwd.clone(),
            self.updates.clone(),
            self.tool_approval.clone(),
            skill_runtime,
        ));
        let manager =
            self.attach_workflow_manager(sid, &session, self.default_endpoint.stream.clone());
        workflow_handle.install(manager);
        if let Some((handle, manager)) = agentfield_handle.as_ref() {
            session.attach_agentfield_manager(manager.clone());
            handle.install(manager.clone());
        }
        if let Err(error) = self.attach_session_plugins(sid, &session).await {
            let _ = self.teardown_task_root(sid).await;
            return Err(error);
        }
        Ok(session)
    }

    /// Phase 7C2 registration gate: build the session `agentfield` handle
    /// and manager when the adapter is enabled, the config validates, and
    /// the credential resolves — otherwise `None` (zero tool registration,
    /// zero network). The manager resolves its client lazily on first use
    /// through the unique policy factory, so session start never touches
    /// the network here.
    fn prepare_agentfield(
        &self,
        sid: &str,
    ) -> Option<(SessionAgentFieldHandle, Arc<AgentFieldManager>)> {
        let config = load_agentfield_config(&self.effective_lato_home())?;
        // Credential resolution happens at the registration gate, BEFORE any
        // transport exists; an unresolvable reference means the adapter is
        // unconfigured and stays invisible to the model.
        let credential = crate::agentfield::resolve_agentfield_credential(
            self.credentials.as_ref(),
            &config.credential_reference,
        )?;
        let catalog = AgentFieldCatalog::from_config(&config);
        let manager = Arc::new(AgentFieldManager::new(sid, catalog, credential, config));
        Some((SessionAgentFieldHandle::new(), manager))
    }

    fn attach_workflow_manager(
        &self,
        sid: &str,
        session: &RuntimeSession,
        stream: std::sync::Arc<dyn ModelStream>,
    ) -> Arc<WorkflowManager> {
        // Phase 7B5: journals persist under the session directory so
        // `session/resume` can restore paused runs in a later process.
        let workflows_dir = Some(
            self.effective_lato_home()
                .join("sessions")
                .join(sid)
                .join("workflows"),
        );
        let manager = Arc::new(WorkflowManager::new(
            sid,
            self.cwd.clone(),
            self.trust.clone(),
            self.locks.clone(),
            stream,
            self.tool_approval.clone(),
            workflows_dir,
        ));
        session.attach_workflow_manager(manager.clone());
        manager
    }

    /// Same home fallback the CLI uses (env `LATO_HOME`, else `~/.lato`) so the
    /// ACP catalog matches `lato workflow list` (spec §3.1).
    fn effective_lato_home(&self) -> PathBuf {
        if self.lato_home.as_os_str().is_empty() {
            return std::env::var_os("LATO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".lato")
                });
        }
        self.lato_home.clone()
    }

    fn workflow_target(
        &self,
        req: &JsonRpcReq,
    ) -> Option<(std::sync::Arc<RuntimeSession>, serde_json::Value)> {
        let params = req.params.clone().unwrap_or_default();
        let sid = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("s1");
        let session = self.sessions.get(sid)?.clone();
        Some((session, params))
    }

    async fn resolve_session_workflow(
        &self,
        session: &RuntimeSession,
        name: &str,
    ) -> Result<crate::workflow::ResolvedWorkflow, String> {
        let snapshot = session.plugin_snapshot().await;
        resolve_workflow(
            &self.cwd,
            &self.effective_lato_home(),
            &snapshot,
            self.trust.cwd_trusted(),
            name,
        )
        .map_err(|error| error.to_string())
    }

    async fn attach_session_plugins(
        &self,
        sid: &str,
        session: &Arc<RuntimeSession>,
    ) -> Result<(), String> {
        let snapshot = self
            .plugin_registry
            .snapshot()
            .await
            .unwrap_or_else(PluginSnapshot::empty);
        session
            .stage_plugin_snapshot(Arc::clone(&snapshot))
            .await
            .map_err(|error| error.to_string())?;
        // Phase 7B7: mount the committed snapshot on the manager so the model
        // tool resolves the same catalog the ACP launch path sees.
        if let Some(manager) = session.workflow_manager() {
            manager.set_snapshot(Arc::clone(&snapshot));
        }
        self.session_plugins
            .register(SessionId::from(sid), snapshot)
            .await;
        Ok(())
    }

    pub async fn session_plugin_snapshot(&self, sid: &str) -> Option<Arc<PluginSnapshot>> {
        self.session_plugins.get(&SessionId::from(sid)).await
    }

    async fn make_new_runtime_session(&mut self, sid: &str) -> Result<Arc<RuntimeSession>, String> {
        // Single journal-ownership contract: the host never appends journal
        // records itself. The runtime session loop owns the first record
        // (`SessionStarted` at sequence 0) and every later sequence; this
        // method only constructs the loop with an empty replay.
        self.make_runtime_session(sid, None).await
    }

    async fn prepare_model_endpoint(
        &mut self,
        provider: &str,
        model: &str,
    ) -> Result<ActiveModelStream, String> {
        if self.default_endpoint.port.selection.provider == provider
            && self.default_endpoint.port.selection.model == model
        {
            return Ok(self.default_endpoint.clone());
        }
        if let Some(catalog_model) = lookup_model(provider, model) {
            if !phase0_supported(catalog_model.api) {
                return Err("dialect_unimplemented".into());
            }
            let store = self
                .credentials
                .as_mut()
                .ok_or_else(|| "model credentials unavailable".to_string())?;
            let auth = get_auth_refreshing(
                store,
                provider,
                &|name| std::env::var(name).ok(),
                None,
                &reqwest::Client::new(),
            )
            .await
            .map_err(|error| format!("oauth refresh failed: {error}"))?
            .ok_or_else(|| "model credentials unavailable".to_string())?;
            let metadata = catalog_model.metadata();
            let raw: Arc<dyn ModelStream> = Arc::new(HttpModelStream::new(catalog_model, auth));
            return adapt_model_endpoint(provider, model, metadata, raw)
                .map_err(|error| format!("invalid model selection: {error}"));
        }
        let custom = self
            .custom_models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model)
            .cloned()
            .ok_or_else(|| "unknown model".to_string())?;
        if !dialect_implemented(custom.api) {
            return Err("dialect_unimplemented".into());
        }
        let auth = custom_model_auth(&custom, &|name| std::env::var(name).ok())
            .ok_or_else(|| "model credentials unavailable".to_string())?;
        let metadata = lato_ai::ModelMetadata {
            context_window: custom.context_window,
            model_family: custom.model_family.clone(),
        };
        let raw: Arc<dyn ModelStream> = Arc::new(CustomHttpModelStream::new(custom, auth));
        adapt_model_endpoint(provider, model, metadata, raw)
            .map_err(|error| format!("invalid model selection: {error}"))
    }

    async fn session_exists(&self, sid: &str) -> Result<bool, String> {
        if self.sessions.contains_key(sid) {
            return Ok(true);
        }
        if let Some(store) = &self.transcripts
            && store
                .list()
                .map_err(|error| error.to_string())?
                .iter()
                .any(|id| id == sid)
        {
            return Ok(true);
        }
        if let Some(store) = &self.events
            && store
                .list_sessions()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|id| id.as_str() == sid)
        {
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn handle(&mut self, req: JsonRpcReq) -> Option<serde_json::Value> {
        let id = req.id.clone();
        if !is_implemented(&req.method) {
            return Some(err(id, -32601, "method not found"));
        }
        match req.method.as_str() {
            "initialize" => Some(ok(
                id,
                serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "agentCapabilities": {"methods": METHODS_IMPLEMENTED}}),
            )),
            "session/new" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let sid = format!("s{now}-{}", self.next_id);
                self.next_id += 1;
                let session = match self.make_new_runtime_session(&sid).await {
                    Ok(session) => session,
                    Err(error) => return Some(err(id, -32000, error)),
                };
                self.sessions.insert(sid.clone(), session);
                Some(ok(id, serde_json::json!({"sessionId": sid})))
            }
            "lato/session/skills" => {
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let Some(session) = self.sessions.get(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                Some(ok(id, session.list_skills().await))
            }
            "lato/session/workflows" => {
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let Some(session) = self.sessions.get(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                // Same keep-first registry the CLI uses: user → trusted project
                // → trusted plugins (spec §3.1).
                let snapshot = session.plugin_snapshot().await;
                let workflows = list_workflows(
                    &self.cwd,
                    &self.effective_lato_home(),
                    &snapshot,
                    self.trust.cwd_trusted(),
                );
                Some(ok(
                    id,
                    serde_json::json!({
                        "generation": snapshot.generation(),
                        "workflows": workflows
                            .iter()
                            .map(|workflow| {
                                serde_json::json!({
                                    "id": workflow.id,
                                    "name": workflow.display_name,
                                    "description": workflow.description,
                                    "source": workflow.source,
                                    "compiled": workflow.compiled,
                                    "agentBudget": workflow.agent_budget,
                                })
                            })
                            .collect::<Vec<_>>(),
                    }),
                ))
            }
            "lato/session/workflow" => {
                let Some((session, params)) = self.workflow_target(&req) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return Some(err(id, -32602, "workflow name is required"));
                };
                let resolved = match self.resolve_session_workflow(&session, name).await {
                    Ok(resolved) => resolved,
                    Err(error) => return Some(err(id, -32000, error)),
                };
                let args = params.get("args").cloned().unwrap_or(serde_json::json!({}));
                let agent_budget = match params
                    .get("agentBudget")
                    .and_then(serde_json::Value::as_u64)
                {
                    Some(raw) => match lato_workflow::clamp_agent_budget(Some(raw)) {
                        Ok(clamped) => Some(u64::from(clamped)),
                        Err(error) => return Some(err(id, -32602, error.to_string())),
                    },
                    None => None,
                };
                match session.workflow_launch(resolved, args, agent_budget).await {
                    Ok(state) => Some(ok(id, serde_json::to_value(&state).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/session/workflow/runs" => {
                let Some((session, _)) = self.workflow_target(&req) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let runs: Vec<_> = session
                    .workflow_runs()
                    .await
                    .iter()
                    .map(|state| serde_json::to_value(state).unwrap_or_default())
                    .collect();
                Some(ok(id, serde_json::json!({ "runs": runs })))
            }
            "lato/session/workflow/pause" => {
                let Some((session, params)) = self.workflow_target(&req) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return Some(err(id, -32602, "display name is required"));
                };
                match session.workflow_pause(name).await {
                    Ok(state) => Some(ok(id, serde_json::to_value(&state).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/session/workflow/resume" => {
                let Some((session, params)) = self.workflow_target(&req) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return Some(err(id, -32602, "display name is required"));
                };
                let agent_budget = params
                    .get("agentBudget")
                    .and_then(serde_json::Value::as_u64);
                match session.workflow_resume(name, agent_budget).await {
                    Ok(state) => Some(ok(id, serde_json::to_value(&state).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/session/workflow/stop" => {
                let Some((session, params)) = self.workflow_target(&req) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return Some(err(id, -32602, "display name is required"));
                };
                match session.workflow_stop(name).await {
                    Ok(state) => Some(ok(id, serde_json::to_value(&state).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "session/prompt" | "lato/session/skill" => {
                let is_skill = req.method == "lato/session/skill";
                self.prompts_via_acp += 1;
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let text = if is_skill {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = p.get("args").and_then(|v| v.as_str()).unwrap_or("");
                    format!("/skill {name} {args}").trim_end().to_owned()
                } else {
                    p.get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned()
                };
                let Some(session) = self.sessions.get(sid).cloned() else {
                    return Some(err(id, -32000, "unknown session"));
                };
                if self.trust.mode == ApprovalMode::Ask && text.contains("tool") {
                    let _ = self.updates.send(serde_json::json!({"jsonrpc":"2.0","id":format!("permission-{sid}"),"method":"session/request_permission","params":{"sessionId": sid,"options":["allow_once","allow_session","deny","cancel"]}}));
                }
                let outcome = if is_skill {
                    let Some(name) = p
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|name| !name.trim().is_empty())
                    else {
                        return Some(err(id, -32602, "skill name is required"));
                    };
                    let args = p.get("args").and_then(|v| v.as_str()).map(str::to_owned);
                    let context = p.get("context").and_then(|v| v.as_str()).map(str::to_owned);
                    session
                        .prompt_skill_with_context(name.to_owned(), args, context)
                        .await
                } else {
                    session.prompt(text.clone()).await
                };
                self.session_plugins
                    .adopt(SessionId::from(sid), session.plugin_snapshot().await)
                    .await;
                if let Some(events) = &self.events {
                    let _ = events
                        .ensure_automatic_title(&SessionId::from(sid), &text)
                        .await;
                }
                match outcome {
                    Ok(RuntimePromptOutcome::Complete { text }) => {
                        let _ = self.updates.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"text":text}}));
                        Some(ok(id, serde_json::json!({"status":"complete","text":text})))
                    }
                    Ok(RuntimePromptOutcome::Cancelled { .. }) => {
                        Some(ok(id, serde_json::json!({"status":"cancelled","text":""})))
                    }
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/plan/status" => {
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let Some(session) = self.sessions.get(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                Some(ok(id, session.plan_status().await))
            }
            "lato/plan/enter" | "lato/plan/submit" | "lato/plan/exit" => {
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let Some(session) = self.sessions.get(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                let outcome = match req.method.as_str() {
                    "lato/plan/enter" => session.plan_enter().await,
                    "lato/plan/submit" => session.plan_submit().await,
                    _ => session.plan_exit().await,
                };
                match outcome {
                    Ok(status) => Some(ok(id, status)),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/plan/approve" => {
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let Some(session) = self.sessions.get(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                // ACP approval mirrors the TUI confirmation: it arrives only
                // through this trusted human protocol channel, and it runs the
                // exact same state-machine checks. Model messages can never
                // forge it.
                let approver = p
                    .get("approver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("interactive");
                match session.plan_approve(approver).await {
                    Ok(status) => Some(ok(id, status)),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "session/cancel" => {
                if let Some(sid) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                {
                    if let Some(session) = self.sessions.get(sid).cloned() {
                        let _ = session.cancel().await;
                    }
                    if let Some(backend) = self.task_backends.get(sid).cloned() {
                        let _ = backend.scoped_handle().teardown_root_and_drain().await;
                        let _ = backend.scoped_handle().open_spawn_admission().await;
                    }
                }
                Some(ok(id, serde_json::json!({"status":"cancelled"})))
            }
            "lato/session/compact" => {
                let params = req.params.unwrap_or_default();
                let Some(sid) = params.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "sessionId is required"));
                };
                let user_context = params
                    .get("userContext")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
                let Some(session) = self.sessions.get(sid).cloned() else {
                    return Some(err(id, -32000, "unknown session"));
                };
                match session.compact(user_context).await {
                    Ok(RuntimeCompactionOutcome::Complete {
                        before,
                        after,
                        checkpoint_id,
                        warning,
                    }) => Some(ok(
                        id,
                        serde_json::json!({
                            "status": "complete",
                            "before": {
                                "messageCount": before.message_count,
                                "serializedBytes": before.serialized_bytes,
                            },
                            "after": {
                                "messageCount": after.message_count,
                                "serializedBytes": after.serialized_bytes,
                            },
                            "checkpointId": checkpoint_id,
                            "warning": warning,
                        }),
                    )),
                    Ok(RuntimeCompactionOutcome::Cancelled) => {
                        Some(ok(id, serde_json::json!({"status": "cancelled"})))
                    }
                    Err(error) => Some(err_with_data(
                        id,
                        -32000,
                        error.to_string(),
                        serde_json::to_value(&error).unwrap_or_default(),
                    )),
                }
            }
            "session/list" => {
                let mut sessions: Vec<String> = self.sessions.keys().cloned().collect();
                if let Some(store) = &self.transcripts
                    && let Ok(on_disk) = store.list()
                {
                    sessions.extend(on_disk);
                }
                if let Some(store) = &self.events
                    && let Ok(on_disk) = store.list_sessions().await
                {
                    sessions.extend(on_disk.into_iter().map(|session| session.to_string()));
                }
                sessions.sort();
                sessions.dedup();
                Some(ok(id, serde_json::json!({"sessions": sessions})))
            }
            "lato/session/list" => {
                let mut summaries = std::collections::BTreeMap::<String, serde_json::Value>::new();
                if let Some(store) = &self.events {
                    match store.list_session_summaries().await {
                        Ok(items) => {
                            for item in items {
                                summaries.insert(
                                    item.session_id.to_string(),
                                    serde_json::to_value(item).unwrap_or_default(),
                                );
                            }
                        }
                        Err(error) => return Some(err(id, -32000, error.to_string())),
                    }
                }
                if let Some(store) = &self.transcripts {
                    let legacy = match store.list() {
                        Ok(items) => items,
                        Err(error) => return Some(err(id, -32000, error)),
                    };
                    for sid in legacy {
                        if summaries.contains_key(&sid) {
                            continue;
                        }
                        let title = store
                            .load_optional(&sid)
                            .ok()
                            .flatten()
                            .and_then(|history| {
                                history.into_iter().find_map(|item| match item {
                                    crate::HistoryItem::User(text) => Some(text),
                                    _ => None,
                                })
                            })
                            .map(|text| derive_automatic_title(&text))
                            .unwrap_or_else(|| "New session".into());
                        summaries.insert(
                            sid.clone(),
                            serde_json::json!({
                                "sessionId": sid,
                                "title": title,
                                "titleSource": "automatic",
                                "createdAtMs": 0,
                                "updatedAtMs": 0,
                            }),
                        );
                    }
                }
                for sid in self.sessions.keys() {
                    summaries.entry(sid.clone()).or_insert_with(|| {
                        serde_json::json!({
                            "sessionId": sid,
                            "title": "New session",
                            "titleSource": "automatic",
                            "createdAtMs": 0,
                            "updatedAtMs": 0,
                        })
                    });
                }
                let mut summaries = summaries.into_values().collect::<Vec<_>>();
                summaries.sort_by(|left, right| {
                    right["updatedAtMs"]
                        .as_u64()
                        .cmp(&left["updatedAtMs"].as_u64())
                        .then_with(|| right["sessionId"].as_str().cmp(&left["sessionId"].as_str()))
                });
                Some(ok(id, serde_json::json!({"sessions": summaries})))
            }
            "lato/session/rename" => {
                let params = req.params.unwrap_or_default();
                let Some(sid) = params.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "sessionId is required"));
                };
                let Some(title) = params.get("title").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "title is required"));
                };
                let session_id = match SessionId::parse(sid) {
                    Ok(session_id) => session_id,
                    Err(error) => return Some(err(id, -32602, error.to_string())),
                };
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if let Some(session) = self.sessions.get(sid)
                    && session.is_active().await
                {
                    return Some(err(id, -32000, "session_busy"));
                }
                let Some(store) = &self.events else {
                    return Some(err(id, -32000, "session metadata unavailable"));
                };
                if let Err(error) =
                    import_legacy_if_needed(&session_id, self.transcripts.as_ref(), store.as_ref())
                        .await
                {
                    return Some(err(id, -32000, error.to_string()));
                }
                match store.rename_session(&session_id, title).await {
                    Ok(summary) => Some(ok(id, serde_json::to_value(summary).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/session/delete" => {
                let params = req.params.unwrap_or_default();
                let Some(sid) = params.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "sessionId is required"));
                };
                let session_id = match SessionId::parse(sid) {
                    Ok(session_id) => session_id,
                    Err(error) => return Some(err(id, -32602, error.to_string())),
                };
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if let Some(session) = self.sessions.get(sid)
                    && session.is_active().await
                {
                    return Some(err(id, -32000, "session_busy"));
                }
                if let Err(error) = self.teardown_task_root(sid).await {
                    return Some(err(id, -32000, error));
                }
                let shutdown_error = if let Some(session) = self.sessions.remove(sid) {
                    session.shutdown().await.err()
                } else {
                    None
                };
                self.session_plugins.remove(&session_id).await;
                if let Some(error) = shutdown_error {
                    return Some(err(id, -32000, error.to_string()));
                }
                if let Some(store) = &self.events
                    && let Err(error) = store.delete_session(&session_id).await
                {
                    return Some(err(id, -32000, error.to_string()));
                }
                if let Some(store) = &self.transcripts
                    && let Err(error) = store.delete(sid)
                {
                    return Some(err(id, -32000, error));
                }
                Some(ok(id, serde_json::json!({"deleted": true})))
            }
            "session/close" => {
                let sid = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str());
                if let Some(sid) = sid {
                    let _ = self.teardown_task_root(sid).await;
                    if let Some(session) = self.sessions.remove(sid) {
                        let _ = session.shutdown().await;
                    }
                    self.session_plugins.remove(&SessionId::from(sid)).await;
                }
                Some(ok(id, serde_json::json!({"closed": true})))
            }
            "session/resume" => {
                let sid = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("s1");
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if !self.sessions.contains_key(sid) {
                    let replay = if let Some(events) = &self.events {
                        match import_legacy_if_needed(
                            &SessionId::from(sid),
                            self.transcripts.as_ref(),
                            events.as_ref(),
                        )
                        .await
                        {
                            Ok(replay) => Some(replay),
                            Err(error) => {
                                return Some(err(id, -32000, error.to_string()));
                            }
                        }
                    } else {
                        None
                    };
                    let session = match self.make_runtime_session(sid, replay).await {
                        Ok(session) => session,
                        Err(error) => return Some(err(id, -32000, error)),
                    };
                    if self.events.is_none()
                        && let Some(store) = &self.transcripts
                        && let Ok(Some(history)) = store.load_optional(sid)
                    {
                        session.replace_history(history).await;
                    }
                    self.sessions.insert(sid.into(), session);
                }
                Some(ok(
                    id,
                    serde_json::json!({"sessionId": sid, "replayed": false}),
                ))
            }
            "session/set_model" => {
                let p = req.params.unwrap_or_default();
                let Some(sid) = p.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "missing sessionId"));
                };
                let Some(model) = p
                    .get("model")
                    .or_else(|| p.get("modelId"))
                    .and_then(|v| v.as_str())
                else {
                    return Some(err(id, -32602, "missing model"));
                };
                let provider = match p.get("provider").and_then(|value| value.as_str()) {
                    Some(provider) => provider.to_owned(),
                    None => {
                        let mut matches = CATALOG
                            .iter()
                            .filter(|entry| entry.id == model)
                            .map(|entry| entry.provider)
                            .chain(
                                self.custom_models
                                    .iter()
                                    .filter(|entry| entry.id == model)
                                    .map(|entry| entry.provider.as_str()),
                            )
                            .collect::<Vec<_>>();
                        matches.sort_unstable();
                        matches.dedup();
                        if matches.len() != 1 {
                            return Some(err(id, -32000, "ambiguous or unknown modelId"));
                        }
                        matches[0].to_owned()
                    }
                };
                let supported = lookup_model(&provider, model)
                    .map(|model| phase0_supported(model.api))
                    .or_else(|| {
                        self.custom_models
                            .iter()
                            .find(|entry| entry.provider == provider && entry.id == model)
                            .map(|entry| dialect_implemented(entry.api))
                    });
                match supported {
                    Some(true) => {}
                    Some(false) => return Some(err(id, -32000, "dialect_unimplemented")),
                    None => return Some(err(id, -32000, "unknown model")),
                }
                let endpoint = match self.prepare_model_endpoint(&provider, model).await {
                    Ok(endpoint) => endpoint,
                    Err(error) => return Some(err(id, -32000, error)),
                };
                let Some(session) = self.sessions.get(sid).cloned() else {
                    return Some(err(id, -32000, "unknown session"));
                };
                match session
                    .switch_model(PreparedModelSwitch { active: endpoint })
                    .await
                {
                    Ok(outcome) => {
                        let _ = self.updates.send(serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "lato/session/model_changed",
                            "params": {
                                "sessionId": sid,
                                "provider": outcome.provider,
                                "model": outcome.model,
                                "compactionWarning": outcome.compaction_warning,
                            }
                        }));
                        Some(ok(
                            id,
                            serde_json::json!({"supported": true, "provider": outcome.provider, "model": outcome.model, "compactionWarning": outcome.compaction_warning}),
                        ))
                    }
                    Err(error) => Some(err_with_data(
                        id,
                        -32000,
                        error.message.clone(),
                        serde_json::json!(error),
                    )),
                }
            }
            "lato/models/list" => {
                let mut models = CATALOG.iter().map(|m| serde_json::json!({"provider":m.provider,"id":m.id,"supported":phase0_supported(m.api),"reason": if phase0_supported(m.api) { serde_json::Value::Null } else { serde_json::json!("dialect_unimplemented") }})).collect::<Vec<_>>();
                models.extend(self.custom_models.iter().map(|m| serde_json::json!({
                    "provider":m.provider,"id":m.id,"api":m.api,"baseUrl":m.base_url,
                    "supported":dialect_implemented(m.api),
                    "reason":if dialect_implemented(m.api) { serde_json::Value::Null } else { serde_json::json!("dialect_unimplemented") }
                })));
                Some(ok(id, serde_json::json!({"models":models})))
            }
            "lato/session/info" => Some(ok(id, serde_json::json!({"cwd": self.cwd}))),
            "lato/auth/login" => {
                let p = req.params.unwrap_or_default();
                let provider = p.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let method = p
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("api_key");
                let Some(store) = self.credentials.as_mut() else {
                    return Some(err(id, -32000, "LATO_HOME credential store unavailable"));
                };
                match method {
                    "api_key" => {
                        if !api_key_login_allowed(provider) {
                            return Some(err(
                                id,
                                -32000,
                                "api key login not supported for provider",
                            ));
                        }
                        let Some(key) = p.get("key").and_then(|v| v.as_str()) else {
                            return Some(err(id, -32602, "missing secret key"));
                        };
                        if let Err(e) = store.modify(|entries| {
                            entries.insert(
                                provider.into(),
                                serde_json::json!({"type":"api_key","key":key}),
                            );
                        }) {
                            return Some(err(id, -32000, e.to_string()));
                        }
                    }
                    "oauth" => {
                        if !oauth_allowed(provider) {
                            return Some(err(id, -32000, "oauth not supported for provider"));
                        }
                        let access = p.get("access").and_then(|v| v.as_str()).unwrap_or("");
                        let refresh = p.get("refresh").and_then(|v| v.as_str()).unwrap_or("");
                        let expires = p.get("expires").and_then(|v| v.as_i64()).unwrap_or(0);
                        let account_id = p
                            .get("account_id")
                            .or_else(|| p.get("accountId"))
                            .and_then(|v| v.as_str());
                        if access.is_empty() || refresh.is_empty() {
                            return Some(err(id, -32602, "oauth interaction required"));
                        }
                        if let Err(e) =
                            store_oauth(store, provider, access, refresh, expires, account_id)
                        {
                            return Some(err(id, -32000, e.to_string()));
                        }
                    }
                    _ => return Some(err(id, -32602, "unknown login method")),
                }
                for session in self.sessions.values() {
                    session.auth_refreshed().await;
                }
                Some(ok(
                    id,
                    serde_json::json!({"ok": true,"provider":provider,"method":method}),
                ))
            }
            "lato/auth/logout" => {
                let provider = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let Some(store) = self.credentials.as_mut() else {
                    return Some(err(id, -32000, "LATO_HOME credential store unavailable"));
                };
                match store.modify(|entries| {
                    entries.remove(provider);
                }) {
                    Ok(()) => Some(ok(id, serde_json::json!({"ok":true}))),
                    Err(e) => Some(err(id, -32000, e.to_string())),
                }
            }
            "lato/plugins/reload" => {
                let params = match serde_json::from_value::<PluginReloadRequest>(
                    req.params.unwrap_or_else(|| serde_json::json!({})),
                ) {
                    Ok(params) => params,
                    Err(error) => return Some(err(id, -32602, error.to_string())),
                };
                let _requested_force = params.force;
                self.plugin_config = load_plugin_config(
                    (!self.lato_home.as_os_str().is_empty()).then_some(self.lato_home.as_path()),
                );
                let request = ReloadRequest {
                    discovery: DiscoveryConfig {
                        cwd: self.cwd.clone(),
                        lato_home: self.lato_home.clone(),
                        cli_plugin_dirs: self.cli_plugin_dirs.clone(),
                        project_trusted: self.trust.cwd_trusted(),
                    },
                    plugin_config: self.plugin_config.clone(),
                    force: true,
                };
                let outcome = match self.plugin_registry.reload(request).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return Some(err_with_data(
                            id,
                            -32000,
                            error.to_string(),
                            serde_json::json!({"code": error.code()}),
                        ));
                    }
                };
                let snapshot = self
                    .plugin_registry
                    .snapshot()
                    .await
                    .expect("a successful reload always publishes a snapshot");
                let mut failed_session_ids = Vec::new();
                for (sid, session) in &self.sessions {
                    if session
                        .stage_plugin_snapshot(Arc::clone(&snapshot))
                        .await
                        .is_err()
                    {
                        failed_session_ids.push(sid.clone());
                        continue;
                    }
                    self.session_plugins
                        .adopt(
                            SessionId::from(sid.as_str()),
                            session.plugin_snapshot().await,
                        )
                        .await;
                }
                if !failed_session_ids.is_empty() {
                    return Some(err_with_data(
                        id,
                        -32000,
                        "plugin reload published but one or more sessions rejected adoption",
                        serde_json::json!({
                            "generation": outcome.generation,
                            "failedSessionIds": failed_session_ids,
                        }),
                    ));
                }
                let response = PluginReloadResponse {
                    generation: outcome.generation,
                    discovered: outcome.discovered,
                    active: outcome.active,
                    diagnostics: snapshot
                        .diagnostics()
                        .iter()
                        .map(|diagnostic| PluginDiagnosticDto {
                            code: diagnostic.code.clone(),
                            plugin_id: None,
                            message: diagnostic.message.clone(),
                        })
                        .collect(),
                };
                Some(ok(id, serde_json::to_value(response).unwrap_or_default()))
            }
            "lato/auth/status" => {
                let provider = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let credential_type = self
                    .credentials
                    .as_ref()
                    .and_then(|store| store.get(provider))
                    .map(|credential| match credential {
                        lato_ai::Credential::ApiKey { .. } => "api_key",
                        lato_ai::Credential::Oauth { .. } => "oauth",
                    });
                Some(ok(
                    id,
                    serde_json::json!({"provider":provider,"configured":credential_type.is_some(),"type":credential_type}),
                ))
            }
            _ => Some(ok(id, serde_json::json!({"ok": true}))),
        }
    }
}

fn all_task_capabilities() -> Vec<ToolCapability> {
    vec![
        ToolCapability::FileRead,
        ToolCapability::FileWrite,
        ToolCapability::ProcessSpawn,
        ToolCapability::NetworkRead,
        ToolCapability::NetworkWrite,
        ToolCapability::TaskControl,
        ToolCapability::ExtensionInvoke,
    ]
}

fn host_task_budget() -> BudgetLimits {
    BudgetLimits::limited(BudgetAmount {
        input_tokens: 10_000_000,
        output_tokens: 2_000_000,
        total_tokens: 12_000_000,
        tool_calls: 100_000,
        cost_micros: 100_000_000,
        wall_time_ms: 86_400_000,
        retries: 1_024,
        child_tasks: 1_024,
        worktrees: 128,
    })
}

fn load_plugin_config(lato_home: Option<&std::path::Path>) -> PluginConfig {
    #[derive(serde::Deserialize)]
    struct HostSettings {
        #[serde(default)]
        plugins: PluginConfig,
    }

    let Some(path) = lato_home
        .filter(|home| !home.as_os_str().is_empty())
        .map(|home| home.join("config.json"))
    else {
        return PluginConfig::default();
    };
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HostSettings>(&bytes).ok())
        .map(|settings| settings.plugins)
        .unwrap_or_default()
}

pub fn default_fake_stream() -> Arc<dyn ModelStream> {
    Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
        "hi".into(),
    )]]))
}

/// Deterministic test stream: the turn issues a real, in-bounds `plan_draft`
/// tool call, so the session records a genuine draft-publication event.
pub fn fake_plan_draft_stream() -> Arc<dyn ModelStream> {
    Arc::new(FakeModelStream::new(vec![vec![
        StreamPiece::ToolCall {
            id: "plan-draft-1".into(),
            name: "plan_draft".into(),
            arguments: serde_json::json!({"contents": "# implementation plan\n\nstep one: real content\n"}),
        },
        StreamPiece::Text("drafted".into()),
    ]]))
}

/// Deterministic test stream: the turn calls `plan_draft` with an oversized
/// payload, so the tool call FAILS closed and nothing is published.
pub fn fake_plan_draft_failure_stream() -> Arc<dyn ModelStream> {
    let oversized = "a".repeat(lato_core::PLAN_DRAFT_MAX_BYTES + 1);
    Arc::new(FakeModelStream::new(vec![vec![
        StreamPiece::ToolCall {
            id: "plan-draft-fail-1".into(),
            name: "plan_draft".into(),
            arguments: serde_json::json!({ "contents": oversized }),
        },
        StreamPiece::Text("tried".into()),
    ]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AuthenticationFailureStream;

    #[async_trait::async_trait]
    impl ModelStream for AuthenticationFailureStream {
        async fn stream(
            &self,
            _prompt_bytes: usize,
            _context: serde_json::Value,
            _tx: tokio::sync::mpsc::Sender<StreamPiece>,
        ) -> Result<(), lato_core::ModelError> {
            Err(lato_core::ModelError::new(
                "model.auth",
                "invalid api key for deterministic compaction failure",
                lato_core::Retryability::Never,
            )
            .with_kind(lato_core::ModelErrorKind::Authentication))
        }
    }

    #[test]
    fn acp_model_constructors_cross_the_canonical_model_port_boundary() {
        let source = include_str!("host.rs");
        let boundary_call = ["adapt_model_endpoint", "(provider, model"].concat();
        assert_eq!(source.matches(&boundary_call).count(), 2);
    }

    pub fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
        JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(id)),
            method: method.into(),
            params: Some(params),
        }
    }
    fn host() -> AcpHost {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let cwd = std::env::current_dir().unwrap();
        AcpHost::new(
            cwd.clone(),
            SessionTrust::for_headless_prompt(cwd),
            tx,
            default_fake_stream(),
        )
    }

    #[tokio::test]
    async fn a1_2_prompt_text() {
        let mut h = host();
        h.handle(req(1, "initialize", serde_json::json!({})))
            .await
            .unwrap();
        let r = h
            .handle(req(2, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        let p = h
            .handle(req(
                3,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(p["result"]["status"], "complete");
    }
    #[tokio::test]
    async fn a1_3_cancel_then_reprompt() {
        let mut h = host();
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        h.handle(req(
            2,
            "session/cancel",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
        let p = h
            .handle(req(
                3,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":"again"}),
            ))
            .await
            .unwrap();
        assert!(p.get("result").is_some());
    }
    #[tokio::test]
    async fn a1_4_request_permission_for_ask() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cwd = std::env::current_dir().unwrap();
        let mut h = AcpHost::new(
            cwd.clone(),
            SessionTrust::for_interactive(cwd, true),
            tx,
            default_fake_stream(),
        );
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        h.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"please tool"}),
        ))
        .await
        .unwrap();
        let n = rx.try_recv().unwrap();
        assert_eq!(n["method"], "session/request_permission");
    }
    #[tokio::test]
    async fn a1_5_list_close_resume() {
        let directory = tempfile::tempdir().unwrap();
        let mut h = host();
        h.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        assert!(
            h.handle(req(2, "session/list", serde_json::json!({})))
                .await
                .unwrap()["result"]["sessions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(sid.clone()))
        );
        h.handle(req(
            3,
            "session/close",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
        let rr = h
            .handle(req(
                4,
                "session/resume",
                serde_json::json!({"sessionId":sid}),
            ))
            .await
            .unwrap();
        assert_eq!(rr["result"]["replayed"], false);
    }

    #[tokio::test]
    async fn resume_rejects_unknown_session_without_creating_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut h = host();
        h.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let response = h
            .handle(req(
                1,
                "session/resume",
                serde_json::json!({"sessionId": "s1700000000000-404"}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["message"], "unknown session");

        let listed = h
            .handle(req(2, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            !listed["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "s1700000000000-404")
        );
    }
    #[tokio::test]
    async fn a1_6_set_model_rejects_unsupported_catalog_model() {
        let mut h = host();
        let created = h
            .handle(req(0, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap();
        let e = h
            .handle(req(
                1,
                "session/set_model",
                serde_json::json!({"sessionId":sid,"provider":"radius","model":"radius-test"}),
            ))
            .await
            .unwrap();
        assert!(e.get("error").is_some());
        let ok = h
            .handle(req(
                2,
                "session/set_model",
                serde_json::json!({"sessionId":sid,"provider":"openai","model":"gpt-4.1"}),
            ))
            .await
            .unwrap();
        assert!(ok.get("result").is_some());
    }
    #[tokio::test]
    async fn e5_1_plugins_reload_preserves_project_trust_boundary() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".lato/plugins/demo/hooks")).unwrap();
        std::fs::write(
            d.path().join(".lato/plugins/demo/plugin.json"),
            r#"{"name":"demo","hooks":"hooks/hooks.json"}"#,
        )
        .unwrap();
        std::fs::write(d.path().join(".lato/plugins/demo/hooks/hooks.json"), "{}").unwrap();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut h = AcpHost::new(
            d.path().to_path_buf(),
            SessionTrust::for_interactive(d.path(), false),
            tx,
            default_fake_stream(),
        );
        let response = h
            .handle(req(
                1,
                "lato/plugins/reload",
                serde_json::json!({"force": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["discovered"], 1);
        assert_eq!(response["result"]["active"], 0);
        assert!(response["result"]["generation"].as_u64().unwrap() > 1);
        assert!(
            response["result"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "plugin.untrusted_project")
        );
    }

    #[tokio::test]
    async fn a4_6_acp_auth_status_uses_shared_store_without_secret() {
        let d = tempfile::tempdir().unwrap();
        let mut h = host();
        h.credentials = Some(CredentialStore::open(d.path()).unwrap());
        let login = h
            .handle(req(
                1,
                "lato/auth/login",
                serde_json::json!({"provider":"openai","method":"api_key","key":"sk-secret"}),
            ))
            .await
            .unwrap();
        assert_eq!(login["result"]["ok"], true);
        let status = h
            .handle(req(
                2,
                "lato/auth/status",
                serde_json::json!({"provider":"openai"}),
            ))
            .await
            .unwrap();
        assert_eq!(status["result"]["configured"], true);
        assert_eq!(status["result"]["type"], "api_key");
        assert!(!status.to_string().contains("sk-secret"));
        h.handle(req(
            3,
            "lato/auth/logout",
            serde_json::json!({"provider":"openai"}),
        ))
        .await
        .unwrap();
        assert!(h.credentials.as_ref().unwrap().get("openai").is_none());
    }

    #[tokio::test]
    async fn successful_login_clears_auth_suppression_for_all_current_sessions() {
        let home = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let (updates, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut h = AcpHost::new_with_home(
            cwd.clone(),
            SessionTrust::for_headless_prompt(&cwd),
            updates.clone(),
            default_fake_stream(),
            home.path().to_path_buf(),
        );
        let created = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let failing_endpoint = adapt_model_endpoint(
            "fixture",
            "auth-failure",
            lato_ai::ModelMetadata {
                context_window: Some(20_000),
                model_family: Some("auth-fixture".into()),
            },
            Arc::new(AuthenticationFailureStream),
        )
        .unwrap();
        let session = Arc::new(RuntimeSession::new_with_endpoint(
            sid.clone(),
            failing_endpoint,
            h.locks.clone(),
            h.trust.clone(),
            cwd,
            updates,
            None,
        ));
        session
            .replace_history(vec![
                crate::HistoryItem::System("system".into()),
                crate::HistoryItem::User("retain the objective".into()),
                crate::HistoryItem::AssistantText("prior-work-".repeat(6_800)),
            ])
            .await;
        h.sessions.insert(sid.clone(), session.clone());

        let failed = h
            .handle(req(
                2,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":"trigger auth suppression"}),
            ))
            .await
            .unwrap();
        assert!(failed.get("error").is_some(), "{failed}");
        assert!(
            std::iter::from_fn(|| updates_rx.try_recv().ok()).any(|update| {
                update["method"] == "lato/session/recovery"
                    && update["params"]["sessionId"] == sid
                    && update["params"]["automaticCompactionSuppression"] == "auth"
            }),
            "auth failure must publish auth suppression"
        );
        assert!(
            !session
                .automatic_compaction_allowed(lato_core::CompactionTrigger::Threshold)
                .await
        );

        let login = h
            .handle(req(
                3,
                "lato/auth/login",
                serde_json::json!({"provider":"openai","method":"api_key","key":"sk-refreshed"}),
            ))
            .await
            .unwrap();
        assert_eq!(login["result"]["ok"], true);
        assert!(
            session
                .automatic_compaction_allowed(lato_core::CompactionTrigger::Threshold)
                .await
        );
        assert!(
            std::iter::from_fn(|| updates_rx.try_recv().ok()).any(|update| {
                update["method"] == "lato/session/recovery"
                    && update["params"]["sessionId"] == sid
                    && update["params"]["automaticCompactionSuppression"] == "none"
            }),
            "successful login must publish cleared recovery state"
        );
    }

    #[tokio::test]
    async fn a1_8_load_errors() {
        let mut h = host();
        let e = h
            .handle(req(1, "session/load", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn runtime_resume_hydrates_transcript_history() {
        let directory = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(directory.path()).unwrap();
        store
            .append(
                "resume-1",
                &[
                    crate::HistoryItem::User("old".into()),
                    crate::HistoryItem::AssistantText("answer".into()),
                ],
            )
            .unwrap();
        let mut host = host();
        host.transcripts = Some(store);
        host.handle(req(
            1,
            "session/resume",
            serde_json::json!({"sessionId": "resume-1"}),
        ))
        .await
        .unwrap();
        let history = host.sessions["resume-1"].history_snapshot().await;
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn resumed_transcript_is_preserved_while_new_rows_go_to_the_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(directory.path()).unwrap();
        store
            .append(
                "resume-append",
                &[
                    crate::HistoryItem::User("old".into()),
                    crate::HistoryItem::AssistantText("answer".into()),
                ],
            )
            .unwrap();
        let mut host = host();
        host.transcripts = Some(store.clone());
        host.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        host.handle(req(
            1,
            "session/resume",
            serde_json::json!({"sessionId": "resume-append"}),
        ))
        .await
        .unwrap();
        host.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": "resume-append", "text": "new"}),
        ))
        .await
        .unwrap();
        let loaded = store.load("resume-append").unwrap();
        let old_user_rows = loaded
            .iter()
            .filter(|item| matches!(item, crate::HistoryItem::User(text) if text == "old"))
            .count();
        assert_eq!(old_user_rows, 1);
        assert_eq!(loaded.len(), 2);
        let replay = host
            .events
            .as_ref()
            .unwrap()
            .replay(&SessionId::from("resume-append"))
            .await
            .unwrap();
        assert!(replay.projection.messages.len() > 2);
    }

    #[tokio::test]
    async fn session_list_unions_legacy_and_journal_ids_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let transcripts = TranscriptStore::open(directory.path()).unwrap();
        transcripts
            .append("shared", &[crate::HistoryItem::User("legacy".into())])
            .unwrap();
        transcripts
            .append("legacy-only", &[crate::HistoryItem::User("legacy".into())])
            .unwrap();
        let events = Arc::new(FileEventStore::open(directory.path()).unwrap());
        for sid in ["shared", "journal-only"] {
            events
                .append(
                    lato_core::JournalEnvelope {
                        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
                        record_id: lato_core::JournalRecordId::from(format!("{sid}-0")),
                        session_id: SessionId::from(sid),
                        turn_id: None,
                        journal_sequence: 0,
                        timestamp_ms: 0,
                        record: lato_core::JournalRecord::SessionStarted,
                    },
                    lato_core::JournalDurability::SyncData,
                )
                .await
                .unwrap();
        }
        let mut host = host();
        host.transcripts = Some(transcripts);
        host.events = Some(events);
        let response = host
            .handle(req(1, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(
            response["result"]["sessions"],
            serde_json::json!(["journal-only", "legacy-only", "shared"])
        );
    }

    #[tokio::test]
    async fn session_admin_extensions_preserve_legacy_list_shape() {
        let directory = tempfile::tempdir().unwrap();
        let mut host = host();
        host.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let created = host
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        host.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "Implement session titles"}),
        ))
        .await
        .unwrap();

        let legacy = host
            .handle(req(3, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(legacy["result"]["sessions"], serde_json::json!([sid]));
        let listed = host
            .handle(req(4, "lato/session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(
            listed["result"]["sessions"][0]["title"],
            "Implement session titles"
        );
        assert_eq!(listed["result"]["sessions"][0]["titleSource"], "automatic");

        let renamed = host
            .handle(req(
                5,
                "lato/session/rename",
                serde_json::json!({"sessionId": sid, "title": "Manual title"}),
            ))
            .await
            .unwrap();
        assert_eq!(renamed["result"]["title"], "Manual title");
        assert_eq!(renamed["result"]["titleSource"], "manual");

        let deleted = host
            .handle(req(
                6,
                "lato/session/delete",
                serde_json::json!({"sessionId": sid}),
            ))
            .await
            .unwrap();
        assert_eq!(deleted["result"]["deleted"], true);
        let listed = host
            .handle(req(7, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(listed["result"]["sessions"], serde_json::json!([]));
    }

    /// Phase 7B5 (spec §3.6/§8): a paused in-session run survives
    /// `session/close` on disk and comes back resumable after
    /// `session/resume`, with one `lato/workflow` snapshot per restored run.
    #[tokio::test]
    async fn workflow_paused_run_survives_session_resume() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("workflows")).unwrap();
        std::fs::write(
            home.path().join("workflows").join("gated.rhai"),
            r#"
            let meta = #{ name: "gated", description: "d" };
            await_user("user", "need human");
            complete("ok");
            "#,
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut h = AcpHost::new_with_home(
            cwd.path().to_path_buf(),
            SessionTrust::for_headless_prompt(cwd.path()),
            tx,
            default_fake_stream(),
            home.path().to_path_buf(),
        );
        h.events = Some(Arc::new(FileEventStore::open(home.path()).unwrap()));

        let created = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let launch = h
            .handle(req(
                2,
                "lato/session/workflow",
                serde_json::json!({"sessionId": sid, "name": "gated"}),
            ))
            .await
            .unwrap();
        let run_id = launch["result"]["runId"].as_str().unwrap().to_string();

        let mut paused = false;
        for _ in 0..400 {
            let runs = h
                .handle(req(
                    3,
                    "lato/session/workflow/runs",
                    serde_json::json!({"sessionId": sid}),
                ))
                .await
                .unwrap();
            if runs["result"]["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|run| run["status"] == "user_paused")
            {
                paused = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(paused, "run never paused");

        h.handle(req(
            4,
            "session/close",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
        h.handle(req(
            5,
            "session/resume",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();

        let runs = h
            .handle(req(
                6,
                "lato/session/workflow/runs",
                serde_json::json!({"sessionId": sid}),
            ))
            .await
            .unwrap();
        let restored = runs["result"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["displayName"] == "gated")
            .expect("restored paused run after session/resume");
        assert_eq!(restored["runId"], run_id.as_str());
        assert_eq!(restored["status"], "user_paused");

        // One `lato/workflow` snapshot per restored run (current state only).
        let mut snapshot_restored = false;
        while let Ok(update) = rx.try_recv() {
            if update["params"]["sessionUpdate"] == "lato/workflow"
                && update["params"]["run"]["runId"] == run_id.as_str()
                && update["params"]["run"]["status"] == "user_paused"
            {
                snapshot_restored = true;
                break;
            }
        }
        assert!(snapshot_restored, "no restored workflow snapshot");

        let resumed = h
            .handle(req(
                7,
                "lato/session/workflow/resume",
                serde_json::json!({"sessionId": sid, "name": "gated"}),
            ))
            .await
            .unwrap();
        assert_eq!(resumed["result"]["status"], "active", "{resumed}");
        let mut completed = false;
        for _ in 0..400 {
            let runs = h
                .handle(req(
                    8,
                    "lato/session/workflow/runs",
                    serde_json::json!({"sessionId": sid}),
                ))
                .await
                .unwrap();
            if runs["result"]["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|run| run["status"] == "complete")
            {
                completed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(completed, "restored run never completed");
    }
}
