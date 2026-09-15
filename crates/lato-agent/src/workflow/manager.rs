// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/manager.rs
// License: Apache-2.0
// Lato changes: session-owned manager (Phase 7B4) — pause is a cancel plus
// pause_intent observed at the next host boundary. Phase 7B5 adds an optional
// `workflows_dir`: launch persists `run.json` + `script.rhai` + the journal
// under `<dir>/<runId>/`, every tracker status change rewrites `run.json`, and
// `new` restores paused runs from disk (active-at-exit becomes `interrupted`).
// `workflows_dir = None` keeps the 7B4 in-memory behavior (tests, CLI run).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use lato_ai::ModelStream;
use lato_extensions::PluginSnapshot;
use lato_workflow::{
    Journal, ScriptOutcome, WorkflowError, WorkflowRunParams, run_workflow_recovering,
};
use lato_workspace::{FileLocks, SessionTrust};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::host_service::DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS;
use super::persist::{self, PersistedRun, RUN_RECORD_VERSION, RestoredRun};
use super::{
    RunEvent, WorkflowHostParams, WorkflowRunState, WorkflowRunStatus, WorkflowTracker,
    spawn_workflow_host_service, workflow_max_concurrent_agents,
};
use crate::ToolApproval;

/// Default agent budget when a restored `run.json` has no recorded budget.
const RESTORED_FALLBACK_BUDGET: u64 = 128;

const STOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(25);

pub struct LaunchSpec {
    pub args: serde_json::Value,
    pub agent_budget: Option<u64>,
    pub resume_display_name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("workflow run '{0}' was not found")]
    UnknownRun(String),
    #[error("workflow run '{0}' cannot be resumed")]
    NotResumable(String),
    #[error("budget-limited run needs a higher agent budget: used {used}, current limit {limit}")]
    BudgetNotRaised { used: u64, limit: u64 },
    #[error("too many active workflow runs (maximum 4 per session)")]
    TooManyActiveRuns,
    #[error("workflow persistence failed: {0}")]
    Persist(String),
    #[error(transparent)]
    Resolve(#[from] WorkflowError),
}

struct ActiveRun {
    cancel: CancellationToken,
    pause_intent: Arc<AtomicBool>,
    settled: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Inner {
    seq: AtomicU64,
    tracker: WorkflowTracker,
    active: HashMap<String, ActiveRun>,
    journals: HashMap<String, Journal>,
    workflows: HashMap<String, super::ResolvedWorkflow>,
    args: HashMap<String, serde_json::Value>,
    starts: HashMap<String, Instant>,
}

struct ManagerCore {
    session_id: lato_core::SessionId,
    cwd: std::path::PathBuf,
    trust: SessionTrust,
    locks: Arc<FileLocks>,
    stream: Arc<dyn ModelStream>,
    approval: Option<Arc<dyn ToolApproval>>,
    max_concurrent_agents: usize,
    snapshot: std::sync::RwLock<Option<Arc<PluginSnapshot>>>,
    subs: Mutex<Vec<mpsc::UnboundedSender<WorkflowRunState>>>,
    inner: Mutex<Inner>,
    /// `Some` enables cross-process journal resume (Phase 7B5); `None` keeps
    /// the 7B4 purely in-memory behavior.
    workflows_dir: Option<PathBuf>,
}

pub struct WorkflowManager {
    core: Arc<ManagerCore>,
}

impl WorkflowManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: &str,
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        locks: Arc<FileLocks>,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        workflows_dir: Option<PathBuf>,
    ) -> Self {
        let core = Arc::new(ManagerCore {
            session_id: lato_core::SessionId::from(session_id.to_owned()),
            cwd,
            trust,
            locks,
            stream,
            approval,
            max_concurrent_agents: workflow_max_concurrent_agents(
                DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS,
            ),
            snapshot: std::sync::RwLock::new(None),
            subs: Mutex::new(Vec::new()),
            inner: Mutex::new(Inner::default()),
            workflows_dir,
        });
        restore_runs(&core);
        Self { core }
    }

    pub fn set_snapshot(&self, snapshot: Arc<PluginSnapshot>) {
        *self.core.snapshot.write().unwrap() = Some(snapshot);
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<WorkflowRunState> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.core.subs.lock().unwrap().push(tx);
        rx
    }

    /// Launch a new run (or resume when `spec.resume_display_name` is set).
    pub fn launch(
        &self,
        resolved: super::ResolvedWorkflow,
        spec: LaunchSpec,
    ) -> Result<WorkflowRunState, LaunchError> {
        if let Some(name) = spec.resume_display_name {
            return self.resume(&name, spec.agent_budget);
        }
        let budget = Some(
            spec.agent_budget
                .unwrap_or(u64::from(resolved.agent_budget)),
        );
        let mut inner = self.core.inner.lock().unwrap();
        if inner.tracker.active_count() >= super::WORKFLOW_MAX_ACTIVE_RUNS_PER_SESSION {
            return Err(LaunchError::TooManyActiveRuns);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let seq = inner.seq.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("wf_{now_ms:x}-{seq:x}");
        let state = inner
            .tracker
            .start_run(run_id.clone(), resolved.display_name.clone(), budget);
        inner.workflows.insert(run_id.clone(), resolved);
        inner.args.insert(run_id.clone(), spec.args);
        inner.starts.insert(run_id.clone(), Instant::now());
        inner.active.insert(
            run_id.clone(),
            ActiveRun {
                cancel: CancellationToken::new(),
                pause_intent: Arc::new(AtomicBool::new(false)),
                settled: Arc::new(tokio::sync::Notify::new()),
            },
        );
        if let Some(root) = self.core.workflows_dir.clone() {
            let dir = persist::run_dir(&root, &run_id);
            let script = inner
                .workflows
                .get(&run_id)
                .map(|resolved| resolved.script.clone())
                .unwrap_or_default();
            if let Err(error) = persist::write_script(&dir, &script)
                .and_then(|()| persist_run(&self.core, &inner, &state))
            {
                // No memory-only fallback: undo the launch and report the failure.
                inner.tracker.remove_run(&run_id);
                inner.workflows.remove(&run_id);
                inner.args.remove(&run_id);
                inner.starts.remove(&run_id);
                inner.active.remove(&run_id);
                return Err(LaunchError::Persist(error.to_string()));
            }
            inner.journals.insert(
                run_id.clone(),
                Journal::new(Some(dir.join(persist::JOURNAL_FILE))),
            );
        }
        stamp(&inner, state.clone());
        emit(&self.core, &state);
        drop(inner);
        spawn_run_task(Arc::clone(&self.core), run_id);
        Ok(state)
    }

    pub fn pause(&self, display_name: &str) -> Result<WorkflowRunState, LaunchError> {
        let inner = self.core.inner.lock().unwrap();
        let state = inner
            .tracker
            .by_display_name(display_name)
            .ok_or_else(|| LaunchError::UnknownRun(display_name.to_owned()))?;
        if state.status == WorkflowRunStatus::Active
            && let Some(active) = inner.active.get(&state.run_id)
        {
            active.pause_intent.store(true, Ordering::Release);
            active.cancel.cancel();
        }
        Ok(stamp(&inner, state))
    }

    pub async fn stop(&self, display_name: &str) -> Result<WorkflowRunState, LaunchError> {
        let trigger = {
            let mut inner = self.core.inner.lock().unwrap();
            let state = inner
                .tracker
                .by_display_name(display_name)
                .ok_or_else(|| LaunchError::UnknownRun(display_name.to_owned()))?;
            match state.status {
                WorkflowRunStatus::Active => {
                    let active = inner
                        .active
                        .get(&state.run_id)
                        .cloned()
                        .ok_or_else(|| LaunchError::UnknownRun(state.run_id.clone()))?;
                    Some((state.run_id.clone(), active))
                }
                WorkflowRunStatus::Complete
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::Interrupted => None,
                // Parked (paused family / blocked / budget_limited): stop in place.
                _ => {
                    if let Some(updated) =
                        inner
                            .tracker
                            .set_status(&state.run_id, WorkflowRunStatus::Cancelled, None)
                    {
                        let _ = persist_run(&self.core, &inner, &updated);
                        emit(&self.core, &stamp(&inner, updated));
                    }
                    None
                }
            }
        };
        if let Some((run_id, active)) = trigger {
            active.cancel.cancel();
            let _ = tokio::time::timeout(STOP_DRAIN_TIMEOUT, active.settled.notified()).await;
            let inner = self.core.inner.lock().unwrap();
            if let Some(state) = inner.tracker.get(&run_id) {
                return Ok(stamp(&inner, state));
            }
        }
        let inner = self.core.inner.lock().unwrap();
        let state = inner
            .tracker
            .by_display_name(display_name)
            .ok_or_else(|| LaunchError::UnknownRun(display_name.to_owned()))?;
        Ok(stamp(&inner, state))
    }

    pub fn resume(
        &self,
        display_name: &str,
        agent_budget: Option<u64>,
    ) -> Result<WorkflowRunState, LaunchError> {
        let mut inner = self.core.inner.lock().unwrap();
        let state = inner
            .tracker
            .by_display_name(display_name)
            .ok_or_else(|| LaunchError::UnknownRun(display_name.to_owned()))?;
        if state.status == WorkflowRunStatus::BudgetLimited {
            let raised = agent_budget.is_some_and(|budget| budget > state.agents_used);
            if !raised {
                return Err(LaunchError::BudgetNotRaised {
                    used: state.agents_used,
                    limit: state.agent_budget.unwrap_or(0),
                });
            }
        } else if !state.status.is_resumable() {
            return Err(LaunchError::NotResumable(display_name.to_owned()));
        }
        if !inner.workflows.contains_key(&state.run_id) {
            return Err(LaunchError::UnknownRun(state.run_id.clone()));
        }
        // The journal stays in `inner.journals`; the run task removes it when
        // it launches and stores the advanced copy back when the run settles.
        inner
            .tracker
            .resume_run(&state.run_id, agent_budget)
            .ok_or_else(|| LaunchError::NotResumable(display_name.to_owned()))?;
        inner.active.insert(
            state.run_id.clone(),
            ActiveRun {
                cancel: CancellationToken::new(),
                pause_intent: Arc::new(AtomicBool::new(false)),
                settled: Arc::new(tokio::sync::Notify::new()),
            },
        );
        let run_id = state.run_id.clone();
        let mut updated = inner.tracker.get(&run_id).unwrap_or(state);
        updated = stamp(&inner, updated);
        let _ = persist_run(&self.core, &inner, &updated);
        drop(inner);
        spawn_run_task(Arc::clone(&self.core), run_id);
        Ok(updated)
    }

    pub fn list(&self) -> Vec<WorkflowRunState> {
        let inner = self.core.inner.lock().unwrap();
        inner
            .tracker
            .list()
            .into_iter()
            .map(|state| stamp(&inner, state))
            .collect()
    }

    /// Cancel every active run; runs still active afterwards become
    /// `interrupted` (process exit / session close, not resumable this phase).
    pub async fn shutdown(&self) {
        let triggers: Vec<(String, ActiveRun)> = {
            let inner = self.core.inner.lock().unwrap();
            inner
                .active
                .iter()
                .map(|(run_id, active)| (run_id.clone(), active.clone()))
                .collect()
        };
        for (_, active) in &triggers {
            active.cancel.cancel();
        }
        for (_, active) in &triggers {
            let _ = tokio::time::timeout(STOP_DRAIN_TIMEOUT, active.settled.notified()).await;
        }
        let mut inner = self.core.inner.lock().unwrap();
        inner.active.clear();
        let still_active: Vec<String> = inner
            .tracker
            .list()
            .into_iter()
            .filter(|state| state.status == WorkflowRunStatus::Active)
            .map(|state| state.run_id)
            .collect();
        for run_id in still_active {
            if let Some(updated) = inner.tracker.interrupt(&run_id, "session closed") {
                let _ = persist_run(&self.core, &inner, &updated);
                emit(&self.core, &stamp(&inner, updated));
            }
        }
    }
}

impl Clone for ActiveRun {
    fn clone(&self) -> Self {
        Self {
            cancel: self.cancel.clone(),
            pause_intent: Arc::clone(&self.pause_intent),
            settled: Arc::clone(&self.settled),
        }
    }
}

fn stamp(inner: &Inner, mut state: WorkflowRunState) -> WorkflowRunState {
    if let Some(start) = inner.starts.get(&state.run_id) {
        state.elapsed_ms_floor = start.elapsed().as_millis() as u64;
    }
    state
}

/// Mirror a tracker status change into the run's `run.json`. A no-op when the
/// manager has no `workflows_dir` or the run never reached the tracker.
fn persist_run(core: &ManagerCore, inner: &Inner, state: &WorkflowRunState) -> std::io::Result<()> {
    let Some(root) = &core.workflows_dir else {
        return Ok(());
    };
    let Some(resolved) = inner.workflows.get(&state.run_id) else {
        return Ok(());
    };
    let record = PersistedRun {
        version: RUN_RECORD_VERSION,
        run_id: state.run_id.clone(),
        display_name: state.display_name.clone(),
        status: state.status,
        phase: state.current_phase.clone(),
        agent_budget: state.agent_budget,
        agents_used: state.agents_used,
        pause_message: state.pause_message.clone(),
        elapsed_ms_floor: state.elapsed_ms_floor,
        workflow_id: resolved.id.clone(),
        source: resolved.source.to_owned(),
        compiled: resolved.compiled,
        description: resolved.description.clone(),
        args: inner
            .args
            .get(&state.run_id)
            .cloned()
            .unwrap_or(serde_json::json!({})),
    };
    persist::write_run_record(&persist::run_dir(root, &state.run_id), &record)
}

/// Phase 7B5 restore (spec §7): scan the session workflows directory and
/// rebuild tracker / workflows / args / journals from disk. Damaged runs are
/// skipped; `active` on disk comes back as terminal `interrupted`.
fn restore_runs(core: &ManagerCore) {
    let Some(root) = &core.workflows_dir else {
        return;
    };
    let candidates: Vec<RestoredRun> = persist::scan_restore_candidates(root);
    let mut inner = core.inner.lock().unwrap();
    for restored in candidates {
        let record = restored.record;
        let Some(script) = persist::read_script(&restored.dir) else {
            continue;
        };
        let journal = match Journal::load(restored.dir.join(persist::JOURNAL_FILE)) {
            Ok(journal) => journal,
            Err(_) => continue,
        };
        let mut state = WorkflowRunState {
            run_id: record.run_id.clone(),
            display_name: record.display_name.clone(),
            status: record.status,
            current_phase: record.phase.clone(),
            agent_budget: record.agent_budget,
            agents_used: record.agents_used,
            pause_message: record.pause_message.clone(),
            elapsed_ms_floor: record.elapsed_ms_floor,
        };
        if state.status == WorkflowRunStatus::Active {
            state.status = WorkflowRunStatus::Interrupted;
            state.pause_message = Some("process exited while active".to_owned());
            state.current_phase = None;
            let rewritten = PersistedRun {
                status: state.status,
                phase: None,
                pause_message: state.pause_message.clone(),
                ..record.clone()
            };
            let _ = persist::write_run_record(&restored.dir, &rewritten);
        }
        if !inner.tracker.insert_restored(state.clone()) {
            continue;
        }
        inner.workflows.insert(
            state.run_id.clone(),
            super::ResolvedWorkflow {
                id: record.workflow_id.clone(),
                display_name: record.display_name.clone(),
                description: record.description.clone(),
                script,
                agent_budget: u32::try_from(
                    record.agent_budget.unwrap_or(RESTORED_FALLBACK_BUDGET),
                )
                .unwrap_or(u32::MAX),
                source: restored_source(&record.source),
                compiled: record.compiled,
            },
        );
        inner.args.insert(state.run_id.clone(), record.args.clone());
        inner.journals.insert(state.run_id.clone(), journal);
        // Keep new `wf_<ms>-<seq>` ids from colliding with restored ones.
        if let Some((_, seq)) = state.run_id.rsplit_once('-')
            && let Ok(seq) = u64::from_str_radix(seq, 16)
        {
            let current = inner.seq.load(Ordering::Relaxed);
            inner
                .seq
                .fetch_max(seq.saturating_add(1).max(current), Ordering::Relaxed);
        }
    }
}

/// Map a persisted source label back onto the static catalog labels.
fn restored_source(source: &str) -> &'static str {
    match source {
        "project" => "project",
        "plugin" => "plugin",
        _ => "user",
    }
}

fn emit(core: &ManagerCore, state: &WorkflowRunState) {
    let mut subs = core.subs.lock().unwrap();
    subs.retain(|tx| tx.send(state.clone()).is_ok());
}

fn spawn_run_task(core: Arc<ManagerCore>, run_id: String) {
    let (resolved, args, journal, budget, active, session_id) = {
        let mut inner = core.inner.lock().unwrap();
        let resolved = match inner.workflows.get(&run_id) {
            Some(resolved) => resolved.clone(),
            None => return,
        };
        let args = inner
            .args
            .get(&run_id)
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let journal = inner
            .journals
            .remove(&run_id)
            .unwrap_or_else(|| Journal::new(None));
        let budget = inner
            .tracker
            .get(&run_id)
            .and_then(|state| state.agent_budget)
            .unwrap_or(u64::from(resolved.agent_budget));
        let active = match inner.active.get(&run_id) {
            Some(active) => active.clone(),
            None => return,
        };
        (
            resolved,
            args,
            journal,
            budget,
            active,
            core.session_id.clone(),
        )
    };

    tokio::spawn(async move {
        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<RunEvent>();
        let snapshot = core
            .snapshot
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(PluginSnapshot::empty);
        let host = spawn_workflow_host_service(
            WorkflowHostParams {
                run_id: run_id.clone(),
                session_id: session_id.clone(),
                max_concurrent_agents: core.max_concurrent_agents,
                agent_budget: budget,
                cwd: core.cwd.clone(),
                stream: Arc::clone(&core.stream),
                locks: Arc::clone(&core.locks),
                trust: core.trust.clone(),
                snapshot,
                cancel: active.cancel.clone(),
                approval: core.approval.clone(),
                notify: Some(notify_tx),
                // Phase 7B6: scratch files live under this run's directory so
                // they survive cross-process resume; memory-only runs (CLI)
                // get None and the host uses a temp dir.
                scratch_dir: core
                    .workflows_dir
                    .as_ref()
                    .map(|root| persist::run_dir(root, &run_id).join("scratch")),
            },
            host_rx,
        );
        // Forward live phase / agent-spawn progress into the tracker until the
        // host service task ends (it owns the notify sender).
        let listener = tokio::spawn({
            let core = Arc::clone(&core);
            let run_id = run_id.clone();
            async move {
                while let Some(event) = notify_rx.recv().await {
                    let mut inner = core.inner.lock().unwrap();
                    let Some(state) = inner.tracker.get(&run_id) else {
                        continue;
                    };
                    if state.status != WorkflowRunStatus::Active {
                        continue;
                    }
                    match event {
                        RunEvent::Phase { title } => inner.tracker.set_phase(&run_id, title),
                        RunEvent::AgentSpawned => inner.tracker.agent_spawned(&run_id),
                    }
                    if let Some(state) = inner.tracker.get(&run_id) {
                        emit(&core, &stamp(&inner, state));
                    }
                }
            }
        });
        let cancel = active.cancel.clone();
        let joined = tokio::task::spawn_blocking(move || {
            run_workflow_recovering(WorkflowRunParams {
                script: resolved.script,
                args,
                journal,
                host_tx,
                cancel,
                max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
            })
        })
        .await;
        let _ = host.await;
        let _ = listener.await;
        let (outcome, journal) = joined.unwrap_or_else(|error| {
            (
                ScriptOutcome::Failed {
                    error: format!("workflow run task failed: {error}"),
                },
                Journal::new(None),
            )
        });
        let settled = {
            let mut inner = core.inner.lock().unwrap();
            inner.journals.insert(run_id.clone(), journal);
            // A user pause surfaces as an engine cancellation; keep it resumable.
            let pause_requested = active.pause_intent.load(Ordering::Acquire);
            if pause_requested && matches!(outcome, ScriptOutcome::Cancelled) {
                let _ = inner.tracker.set_status(
                    &run_id,
                    WorkflowRunStatus::UserPaused,
                    Some("paused by user".to_owned()),
                );
            } else {
                inner.tracker.apply_outcome(&run_id, &outcome);
            }
            if let Some(state) = inner.tracker.get(&run_id) {
                let _ = persist_run(&core, &inner, &state);
            }
            inner.active.remove(&run_id).map(|active| active.settled)
        };
        {
            let inner = core.inner.lock().unwrap();
            if let Some(state) = inner.tracker.get(&run_id) {
                emit(&core, &stamp(&inner, state));
            }
        }
        if let Some(settled) = settled {
            settled.notify_one();
        }
    });
}

// Keep the run task's outcome mapping readable.
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_starts_and_shuts_down() {
        let manager = WorkflowManager::new(
            "s-test",
            std::env::temp_dir(),
            SessionTrust::for_headless_prompt(std::env::temp_dir()),
            Arc::new(FileLocks::new()),
            crate::default_fake_stream(),
            None,
            None,
        );
        let mut rx = manager.subscribe();
        let resolved = super::super::ResolvedWorkflow {
            id: "demo/hold".into(),
            display_name: "hold".into(),
            description: "d".into(),
            script: r#"
                let meta = #{ name: "hold", description: "d" };
                await_user("user", "need human");
                complete("ok");
            "#
            .into(),
            agent_budget: 8,
            source: "plugin",
            compiled: true,
        };
        let state = manager
            .launch(
                resolved,
                LaunchSpec {
                    args: serde_json::json!({}),
                    agent_budget: None,
                    resume_display_name: None,
                },
            )
            .unwrap();
        assert_eq!(state.display_name, "hold");
        assert_eq!(state.status, WorkflowRunStatus::Active);
        // The await_user run parks quickly; wait for the paused snapshot.
        let mut paused = false;
        for _ in 0..200 {
            if manager.list()[0].status == WorkflowRunStatus::UserPaused {
                paused = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(paused, "run should pause: {:?}", manager.list());
        assert!(rx.try_recv().is_ok(), "subscriber saw snapshots");
        manager.shutdown().await;
    }
}
