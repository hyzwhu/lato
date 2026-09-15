// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/host_service.rs
// License: Apache-2.0
// Lato changes: SpawnAgent uses TaskCoordinator + ChildSessionRunner instead of
// SubagentRequest. Phase 7B6: write_scratch_file / read_scratch_file /
// render_template / git_diff_since are live here (scratch under the 7B5 run
// directory for ACP sessions, a host-owned temp dir for the CLI); fork_context
// stays Unsupported.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use lato_ai::ModelStream;
use lato_core::{
    AgentProfile, BudgetLimits, ResultContract, SessionId, TaskError, TaskErrorCode, TaskId,
    TaskOwner, TaskResult, TaskScope, ToolCapability, VerificationPolicy, WorkspaceIntent,
};
use lato_extensions::PluginSnapshot;
use lato_runtime::{
    CoordinatorConfig, NoopTaskEventSink, ScopedTaskHandle, SpawnMode, SpawnTaskRequest,
    TaskHandle, TaskRootRequest, spawn_subagent_coordinator_with_verifier,
};
use lato_workflow::{
    AgentOpts, AgentResult, BudgetState, HostError, MAX_AGENT_BUDGET, WorkflowHostRequest,
};
use lato_workspace::{FileLocks, GitWorkspaceAllocator, MemoryWorkspaceAllocator, SessionTrust};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::schema_contract::{
    SCHEMA_CONTRACT_RETRIES, compile_contract_schema, contract_prompt, validate_contract_output,
};
use super::scratch::{self};
use super::templates;
use crate::{
    BuiltinProfileName, ChildSessionRunner, ProfileResultVerifier, SessionPluginSnapshots,
    ToolApproval,
};

pub const DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS: usize = 32;
pub(crate) const WORKFLOW_MAX_AGENT_RUNS: u32 = MAX_AGENT_BUDGET * (SCHEMA_CONTRACT_RETRIES + 1);
const WORKFLOW_MAX_AGENT_PROMPT_BYTES: usize = 1024 * 1024;
const WORKFLOW_MAX_PHASE_BYTES: usize = 256;
const WORKFLOW_CHILD_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
const CONTRACT_OUTPUT_MAX_BYTES: usize = 2 * 1024 * 1024;
const WORKFLOW_MAX_GIT_DIFF_BYTES: u64 = 1024 * 1024;

/// Live run progress emitted by the host service so a session-level manager
/// can update the tracker without intercepting host traffic.
#[derive(Clone, Debug)]
pub enum RunEvent {
    Phase { title: String },
    AgentSpawned,
}

pub struct WorkflowHostParams {
    pub run_id: String,
    pub session_id: SessionId,
    pub max_concurrent_agents: usize,
    pub agent_budget: u64,
    pub cwd: PathBuf,
    pub stream: Arc<dyn ModelStream>,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
    pub snapshot: Arc<PluginSnapshot>,
    pub cancel: CancellationToken,
    pub approval: Option<Arc<dyn ToolApproval>>,
    pub notify: Option<mpsc::UnboundedSender<RunEvent>>,
    /// Phase 7B6: where `write_scratch_file` / `read_scratch_file` operate.
    /// `Some` — the ACP session's run directory (`<runDir>/scratch`); `None` —
    /// the host creates a temp dir that dies with the host service (CLI run).
    pub scratch_dir: Option<PathBuf>,
}

/// The configured cap clamped to the machine's parallelism, so small hosts run fewer agents at once.
pub fn workflow_max_concurrent_agents(configured: usize) -> usize {
    workflow_max_concurrent_agents_from(
        configured,
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS),
    )
}

fn workflow_max_concurrent_agents_from(configured: usize, parallelism: usize) -> usize {
    let clamp = parallelism.max(2);
    let requested = configured.max(1);
    requested.min(clamp)
}

pub fn spawn_workflow_host_service(
    params: WorkflowHostParams,
    mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (service, actor) = match setup_host(params).await {
            Ok(pair) => pair,
            Err(_) => {
                while let Some(req) = rx.recv().await {
                    reply_failed(req, "workflow host failed to start coordinator");
                }
                return;
            }
        };
        loop {
            let req = tokio::select! {
                req = rx.recv() => match req {
                    Some(req) => req,
                    None => break,
                },
                _ = service.params.cancel.cancelled() => {
                    while let Ok(req) = rx.try_recv() {
                        reply_cancelled(req);
                    }
                    break;
                }
            };
            service.clone().handle_request(req);
        }
        service.cancel_and_drain_children().await;
        let _ = service.coordinator.shutdown().await;
        let _ = actor.await;
    })
}

async fn setup_host(
    params: WorkflowHostParams,
) -> Result<(Arc<HostService>, tokio::task::JoinHandle<()>), HostError> {
    let session_plugins = SessionPluginSnapshots::default();
    session_plugins
        .register(params.session_id.clone(), Arc::clone(&params.snapshot))
        .await;
    let (updates, _) = mpsc::unbounded_channel();
    let runner = Arc::new(
        ChildSessionRunner::new(
            Arc::clone(&params.stream),
            Arc::clone(&params.locks),
            params.trust.clone(),
            updates,
            params.approval.clone(),
        )
        .with_session_plugins(session_plugins),
    );
    let verifier = Arc::new(ProfileResultVerifier);
    let sink = Arc::new(NoopTaskEventSink);
    let max_concurrent = params.max_concurrent_agents.max(1);
    let mut config = CoordinatorConfig::default();
    config.max_global_running = config.max_global_running.max(max_concurrent);
    config.max_running_per_root = config.max_running_per_root.max(max_concurrent);
    config.max_children_per_parent = (MAX_AGENT_BUDGET as usize).saturating_mul(2).max(16);
    config.max_total_tasks = config.max_children_per_parent.saturating_add(8);
    config.max_completed = config.max_total_tasks;
    let (coordinator, actor) =
        match GitWorkspaceAllocator::new(&params.cwd, params.cwd.join(".lato/worktrees")) {
            Ok(allocator) => spawn_subagent_coordinator_with_verifier(
                config,
                runner,
                Arc::new(allocator),
                verifier,
                sink,
            ),
            Err(_) => {
                let allocator = MemoryWorkspaceAllocator::new(&params.cwd).map_err(|error| {
                    HostError::Failed(format!("workflow workspace allocator: {error}"))
                })?;
                spawn_subagent_coordinator_with_verifier(
                    config,
                    runner,
                    Arc::new(allocator),
                    verifier,
                    sink,
                )
            }
        };
    let root_id = TaskId::from(format!("wf-root-{}", params.run_id));
    let scratch = ScratchArea::open(params.scratch_dir.clone())?;
    let mut budget = BudgetLimits::unlimited();
    // Schema correction re-spawns one child under the same logical agent_budget.
    budget.child_tasks = Some(
        params
            .agent_budget
            .saturating_mul(u64::from(SCHEMA_CONTRACT_RETRIES.saturating_add(1))),
    );
    let scoped = coordinator
        .register_root(TaskRootRequest {
            task_id: root_id.clone(),
            owner: TaskOwner::Workflow {
                run_id: params.run_id.clone(),
                session_id: params.session_id.clone(),
            },
            profile: workflow_root_profile(),
            permissions: AgentProfile::worker().capabilities,
            budget,
        })
        .await
        .map_err(|error| HostError::Failed(format!("workflow root registration: {error}")))?;
    Ok((
        Arc::new(HostService {
            agent_runs: AtomicU32::new(0),
            agent_seq: AtomicU64::new(1),
            spent: AtomicU64::new(0),
            agent_slots: tokio::sync::Semaphore::new(max_concurrent),
            coordinator,
            scoped: tokio::sync::Mutex::new(Some(scoped)),
            root_id,
            params,
            scratch,
        }),
        actor,
    ))
}

/// Where scratch files live for one host service. `Temp` dies with the
/// service (CLI one-shot runs); `Dir` persists under the 7B5 run directory.
enum ScratchArea {
    Dir(PathBuf),
    Temp(tempfile::TempDir),
}

impl ScratchArea {
    fn open(scratch_dir: Option<PathBuf>) -> Result<Self, HostError> {
        match scratch_dir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)
                    .map_err(|error| HostError::Failed(format!("scratch dir: {error}")))?;
                Ok(Self::Dir(dir))
            }
            None => Ok(Self::Temp(
                tempfile::TempDir::new()
                    .map_err(|error| HostError::Failed(format!("scratch temp dir: {error}")))?,
            )),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Dir(dir) => dir.as_path(),
            Self::Temp(temp) => temp.path(),
        }
    }
}

/// `git diff <commit> --` on the host cwd. The commit is a single argument
/// (no leading `-`, no whitespace/NUL) and stdout+stderr is capped at 1 MiB.
fn git_diff_since(cwd: &Path, commit: &str) -> Result<String, HostError> {
    if commit.is_empty()
        || commit.starts_with('-')
        || commit.contains(|c: char| c.is_whitespace() || c == '\0')
    {
        return Err(HostError::Failed("invalid git diff commit".into()));
    }
    let output = std::process::Command::new("git")
        .arg("diff")
        .arg(commit)
        .arg("--")
        .current_dir(cwd)
        .output()
        .map_err(|error| HostError::Failed(format!("git diff failed: {error}")))?;
    if output.stdout.len() as u64 + output.stderr.len() as u64 > WORKFLOW_MAX_GIT_DIFF_BYTES {
        return Err(HostError::Failed(
            "git diff output exceeded the 1 MiB cap".into(),
        ));
    }
    if !output.status.success() {
        let brief: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("git exited with an error")
            .chars()
            .take(200)
            .collect();
        return Err(HostError::Failed(format!("git diff failed: {brief}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn workflow_root_profile() -> AgentProfile {
    let worker = AgentProfile::worker();
    AgentProfile {
        name: "workflow-root".into(),
        instructions: "Own a workflow run without widening child authority.".into(),
        capabilities: worker.capabilities,
        workspace: WorkspaceIntent::IsolatedWorktree,
        verification: VerificationPolicy::Accept,
        definition_background: false,
    }
}

fn reply_cancelled(req: WorkflowHostRequest) {
    use WorkflowHostRequest as R;
    match req {
        R::ReserveAgentCalls { reply, .. } | R::ReleaseAgentCalls { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::SpawnAgent { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::BudgetQuery { reply } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::RenderTemplate { reply, .. }
        | R::WriteScratchFile { reply, .. }
        | R::ReadScratchFile { reply, .. }
        | R::GitDiffSince { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::Phase { .. } | R::Log { .. } | R::Telemetry { .. } => {}
    }
}

fn reply_failed(req: WorkflowHostRequest, message: &str) {
    use WorkflowHostRequest as R;
    match req {
        R::ReserveAgentCalls { reply, .. } | R::ReleaseAgentCalls { reply, .. } => {
            let _ = reply.send(Err(HostError::Failed(message.into())));
        }
        R::SpawnAgent { reply, .. } => {
            let _ = reply.send(Err(HostError::Failed(message.into())));
        }
        R::BudgetQuery { reply } => {
            let _ = reply.send(Err(HostError::Failed(message.into())));
        }
        R::RenderTemplate { reply, .. }
        | R::WriteScratchFile { reply, .. }
        | R::ReadScratchFile { reply, .. }
        | R::GitDiffSince { reply, .. } => {
            let _ = reply.send(Err(HostError::Failed(message.into())));
        }
        R::Phase { .. } | R::Log { .. } | R::Telemetry { .. } => {}
    }
}

struct HostService {
    agent_runs: AtomicU32,
    agent_seq: AtomicU64,
    spent: AtomicU64,
    agent_slots: tokio::sync::Semaphore,
    coordinator: TaskHandle,
    scoped: tokio::sync::Mutex<Option<ScopedTaskHandle>>,
    root_id: TaskId,
    params: WorkflowHostParams,
    scratch: ScratchArea,
}

impl HostService {
    fn handle_request(self: Arc<Self>, req: WorkflowHostRequest) {
        match req {
            WorkflowHostRequest::ReserveAgentCalls { count, reply } => {
                let _ = reply.send(self.reserve_agent_calls(count));
            }
            WorkflowHostRequest::ReleaseAgentCalls { count, reply } => {
                let _ = reply.send(self.release_agent_calls(count));
            }
            WorkflowHostRequest::SpawnAgent { opts, reply } => {
                if let Some(notify) = &self.params.notify {
                    let _ = notify.send(RunEvent::AgentSpawned);
                }
                tokio::spawn(async move {
                    let result = self.spawn_agent(opts).await;
                    let _ = reply.send(result);
                });
            }
            WorkflowHostRequest::Phase { title, .. } => {
                if let Some(notify) = &self.params.notify {
                    let _ = notify.send(RunEvent::Phase { title });
                }
            }
            WorkflowHostRequest::Log { .. } => {}
            WorkflowHostRequest::Telemetry { .. } => {}
            WorkflowHostRequest::BudgetQuery { reply } => {
                let _ = reply.send(Ok(self.budget_state()));
            }
            WorkflowHostRequest::RenderTemplate { name, vars, reply } => {
                let _ = reply.send(templates::render_template(&name, &vars));
            }
            WorkflowHostRequest::WriteScratchFile { name, content, reply } => {
                let _ = reply.send(scratch::write_scratch(self.scratch.path(), &name, &content));
            }
            WorkflowHostRequest::ReadScratchFile { name, reply } => {
                let _ = reply.send(scratch::read_scratch(self.scratch.path(), &name));
            }
            WorkflowHostRequest::GitDiffSince { commit, reply } => {
                let _ = reply.send(git_diff_since(&self.params.cwd, &commit));
            }
        }
    }

    fn budget_state(&self) -> BudgetState {
        let total = Some(self.params.agent_budget);
        let spent = self.spent.load(Ordering::SeqCst);
        BudgetState {
            total,
            spent,
            reserved: 0,
            remaining: total.map(|total| total.saturating_sub(spent)),
        }
    }

    fn reserve_agent_calls(&self, count: u64) -> Result<(), HostError> {
        if self.params.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        if count == 0 {
            return Ok(());
        }
        loop {
            let spent = self.spent.load(Ordering::SeqCst);
            let requested = spent.saturating_add(count);
            if requested > self.params.agent_budget {
                return Err(HostError::AgentCallQuotaExceeded {
                    requested,
                    maximum: self.params.agent_budget,
                });
            }
            match self
                .spent
                .compare_exchange(spent, requested, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }

    fn release_agent_calls(&self, count: u64) -> Result<(), HostError> {
        if self.params.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        let _ = self
            .spent
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |spent| {
                Some(spent.saturating_sub(count))
            });
        Ok(())
    }

    async fn acquire_agent_slot(&self) -> Result<tokio::sync::SemaphorePermit<'_>, HostError> {
        match self.agent_slots.try_acquire() {
            Ok(permit) if self.params.cancel.is_cancelled() => {
                drop(permit);
                Err(HostError::Cancelled)
            }
            Ok(permit) => Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => Err(HostError::Cancelled),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                if self.params.cancel.is_cancelled() {
                    return Err(HostError::Cancelled);
                }
                let permit = tokio::select! {
                    biased;
                    _ = self.params.cancel.cancelled() => return Err(HostError::Cancelled),
                    permit = self.agent_slots.acquire() => {
                        match permit {
                            Ok(permit) => permit,
                            Err(_) => return Err(HostError::Cancelled),
                        }
                    }
                };
                Ok(permit)
            }
        }
    }

    async fn spawn_agent(&self, opts: AgentOpts) -> Result<AgentResult, HostError> {
        if self.params.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        if opts.prompt.is_empty() || opts.prompt.len() > WORKFLOW_MAX_AGENT_PROMPT_BYTES {
            return Err(HostError::Failed(format!(
                "agent prompt must be non-empty and at most {WORKFLOW_MAX_AGENT_PROMPT_BYTES} bytes"
            )));
        }
        if opts.fork_context {
            return Err(HostError::Unsupported(
                "fork_context is restricted to built-in workflows".into(),
            ));
        }
        if opts
            .label
            .as_ref()
            .is_some_and(|label| label.len() > WORKFLOW_MAX_PHASE_BYTES)
            || opts
                .phase
                .as_ref()
                .is_some_and(|phase| phase.len() > WORKFLOW_MAX_PHASE_BYTES)
        {
            return Err(HostError::Failed(
                "agent label and phase must each be at most 256 bytes".into(),
            ));
        }

        let mut profile =
            resolve_child_profile(opts.agent_type.as_deref(), opts.capability_mode.as_deref())?;
        if opts.isolation_worktree {
            profile.workspace = WorkspaceIntent::IsolatedWorktree;
        }
        let requested_capabilities =
            requested_capabilities_for_mode(&profile, opts.capability_mode.as_deref())?;

        let schema_validator = match &opts.output_schema {
            None => None,
            Some(schema) => Some(compile_contract_schema(schema).map_err(HostError::Failed)?),
        };
        let prompt = match &opts.output_schema {
            None => opts.prompt.clone(),
            Some(schema) => contract_prompt(&opts.prompt, schema),
        };

        let _agent_slot = self.acquire_agent_slot().await?;
        let scoped = {
            let guard = self.scoped.lock().await;
            guard.clone().ok_or_else(|| {
                HostError::Failed("workflow coordinator is not ready to spawn agents".into())
            })?
        };

        let id = format!(
            "wf-agent-{}",
            self.agent_seq.fetch_add(1, Ordering::Relaxed)
        );
        let mut attempts: u32 = 0;
        let mut total_tokens: u64 = 0;
        let mut total_duration: u64 = 0;
        let mut next_prompt = prompt;

        let (success, output, cancelled) = loop {
            attempts += 1;
            let run = self.agent_runs.fetch_add(1, Ordering::Relaxed);
            if run >= WORKFLOW_MAX_AGENT_RUNS {
                return Err(HostError::Failed(format!(
                    "workflow agent-run quota exceeded (maximum {WORKFLOW_MAX_AGENT_RUNS})"
                )));
            }
            let child_id = if attempts == 1 {
                id.clone()
            } else {
                format!(
                    "wf-agent-{}",
                    self.agent_seq.fetch_add(1, Ordering::Relaxed)
                )
            };
            let child_cancel = CancellationToken::new();
            let mut child_budget = BudgetLimits::unlimited();
            // Reserve only the coordinator's fixed child_tasks=1, not the parent remainder.
            child_budget.child_tasks = Some(0);
            let request = SpawnTaskRequest {
                task_id: TaskId::from(child_id.clone()),
                scope: TaskScope {
                    objective: next_prompt.clone(),
                    context_refs: Vec::new(),
                },
                profile: profile.clone(),
                requested_capabilities: requested_capabilities.clone(),
                budget: child_budget,
                result_contract: ResultContract {
                    schema: None,
                    max_output_bytes: CONTRACT_OUTPUT_MAX_BYTES,
                },
                mode: SpawnMode::AwaitCompletion,
                cancellation: child_cancel.clone(),
            };

            let wait = tokio::select! {
                result = scoped.spawn_and_wait(request) => result,
                _ = self.params.cancel.cancelled() => {
                    child_cancel.cancel();
                    return Err(HostError::Cancelled);
                }
            };

            if self.params.cancel.is_cancelled() {
                return Err(HostError::Cancelled);
            }

            let disposition = match wait {
                Ok(disposition) => disposition,
                Err(error) => return Err(map_task_error(error)),
            };
            if disposition.backgrounded {
                return Err(HostError::Failed(format!(
                    "subagent {child_id} was auto-backgrounded by the await budget; its result \
                     is not available to this run (engine bug — workflow spawns must await to \
                     completion)"
                )));
            }

            let snapshot = scoped
                .inspect(TaskId::from(child_id.clone()))
                .await
                .map_err(map_task_error)?;
            let result = snapshot.result.clone().unwrap_or_else(|| TaskResult {
                success: false,
                output: String::new(),
                error: None,
                usage: Default::default(),
                duration_ms: snapshot.elapsed_ms,
                output_ref: None,
            });
            total_tokens = total_tokens.saturating_add(result.usage.total_tokens);
            total_duration = total_duration.saturating_add(result.duration_ms);
            if result
                .error
                .as_ref()
                .is_some_and(|error| error.code == TaskErrorCode::Cancelled)
                || snapshot.node.status.is_cancelled()
                || disposition.explicitly_killed
            {
                return Err(HostError::Cancelled);
            }

            let Some(validator) = schema_validator.as_ref() else {
                let output = if result.success {
                    serde_json::Value::String(result.output.clone())
                } else {
                    serde_json::Value::String(
                        result
                            .error
                            .as_ref()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| result.output.clone()),
                    )
                };
                break (result.success, output, false);
            };
            if !result.success {
                let output = serde_json::Value::String(
                    result
                        .error
                        .as_ref()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| result.output.clone()),
                );
                break (false, output, false);
            }
            match validate_contract_output(validator, &result.output) {
                Ok(value) => break (true, value, false),
                Err(err) if attempts <= SCHEMA_CONTRACT_RETRIES => {
                    next_prompt = format!(
                        "Your final message did not satisfy the output contract: {err}\n\
                         Reply with a single ```json fenced block containing one JSON \
                         value conforming to the schema from <output-contract>, and \
                         nothing else."
                    );
                    continue;
                }
                Err(err) => {
                    let output = serde_json::Value::String(format!(
                        "structured output validation failed: {err}"
                    ));
                    break (false, output, false);
                }
            }
        };

        Ok(AgentResult {
            agent_id: id,
            success,
            output,
            cancelled,
            tokens_used: total_tokens,
            duration_ms: total_duration,
        })
    }

    async fn cancel_and_drain_children(&self) {
        let cancel = self
            .coordinator
            .cancel_workflow(self.params.run_id.clone(), Some(self.root_id.clone()));
        if tokio::time::timeout(WORKFLOW_CHILD_DRAIN_TIMEOUT, cancel)
            .await
            .is_err()
        {
            // Drain timed out; shutdown still proceeds so the actor cannot leak.
        }
    }
}

fn resolve_child_profile(
    agent_type: Option<&str>,
    capability_mode: Option<&str>,
) -> Result<AgentProfile, HostError> {
    let name = match agent_type {
        None | Some("general-purpose") | Some("worker") => BuiltinProfileName::Worker,
        Some("explorer") => BuiltinProfileName::Explorer,
        Some("reviewer") => BuiltinProfileName::Reviewer,
        Some(other) => {
            return Err(HostError::Failed(format!(
                "unknown workflow agent_type '{other}' (expected explorer, reviewer, worker, or general-purpose)"
            )));
        }
    };
    if capability_mode == Some("read-only") && name == BuiltinProfileName::Worker {
        return Ok(BuiltinProfileName::Explorer.resolve());
    }
    Ok(name.resolve())
}

fn requested_capabilities_for_mode(
    profile: &AgentProfile,
    mode: Option<&str>,
) -> Result<Option<Vec<ToolCapability>>, HostError> {
    match mode {
        None => Ok(None),
        Some("read-only") => Ok(Some(narrow_read_only(profile))),
        Some("read-write") => Ok(Some(narrow_read_write(profile))),
        Some("execute") | Some("all") => Ok(None),
        Some(other) => Err(HostError::Failed(format!(
            "invalid capability_mode '{other}' (expected read-only, read-write, execute, or all)"
        ))),
    }
}

fn profile_has(profile: &AgentProfile, capability: ToolCapability) -> bool {
    profile.capabilities.contains(&capability)
}

fn narrow_read_only(profile: &AgentProfile) -> Vec<ToolCapability> {
    let mut caps = Vec::new();
    if profile_has(profile, ToolCapability::FileRead) {
        caps.push(ToolCapability::FileRead);
    }
    if profile_has(profile, ToolCapability::NetworkRead) {
        caps.push(ToolCapability::NetworkRead);
    }
    caps
}

fn narrow_read_write(profile: &AgentProfile) -> Vec<ToolCapability> {
    let mut caps = narrow_read_only(profile);
    if profile_has(profile, ToolCapability::FileWrite) {
        caps.push(ToolCapability::FileWrite);
    }
    caps
}

fn map_task_error(error: TaskError) -> HostError {
    match error.code {
        TaskErrorCode::Cancelled => HostError::Cancelled,
        TaskErrorCode::BudgetExceeded
        | TaskErrorCode::BudgetExceededInputTokens
        | TaskErrorCode::BudgetExceededOutputTokens
        | TaskErrorCode::BudgetExceededTotalTokens
        | TaskErrorCode::BudgetExceededToolCalls
        | TaskErrorCode::BudgetExceededCostMicros
        | TaskErrorCode::BudgetExceededRetries
        | TaskErrorCode::BudgetExceededWallTimeMs
        | TaskErrorCode::BudgetReservation => HostError::BudgetExceeded,
        TaskErrorCode::CoordinatorClosed => {
            HostError::Failed("subagent coordinator channel closed before completion".into())
        }
        _ => HostError::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::workflow_max_concurrent_agents_from;

    #[test]
    fn concurrency_clamps_to_parallelism_with_floor_two() {
        assert_eq!(workflow_max_concurrent_agents_from(32, 1), 2);
        assert_eq!(workflow_max_concurrent_agents_from(32, 8), 8);
        assert_eq!(workflow_max_concurrent_agents_from(4, 16), 4);
    }
}
