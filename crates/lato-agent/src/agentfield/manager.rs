// Phase 7C2: session-scoped AgentField run manager (spec §4.1, §7, §9.3).
//
// Single writer per session: every state transition goes through one
// mutex, terminal states are monotonic (never overwritten by late
// arrivals), and capacity is reserved atomically before the remote send
// (max 4 nonterminal runs, max 32 retained runs per session).
//
// Exactly-once send semantics (spec §7.2): `start_run` performs AT MOST
// one `execute/async` HTTP request. If the outcome of that request cannot
// be determined without an execution ID, the run enters the permanent
// terminal state `outcome_unknown` — never auto-retried, auto-queried, or
// reconciled (7C3 will not change this; only manual control-plane
// reconciliation is allowed).
//
// Session close (spec §9.2 item 4): `close()` marks the manager
// unavailable; late calls fail closed with `agentfield.unavailable` and
// the remote execution is NEVER implicitly cancelled.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::{Mutex, OnceCell};

use crate::agentfield::catalog::AgentFieldCatalog;
use crate::agentfield::client::{AgentFieldClient, AgentFieldError};
use crate::agentfield::config::AgentFieldConfig;
use crate::agentfield::probe::{AgentFieldHealthSnapshot, AgentFieldProbe, HEALTH_SNAPSHOT_TTL};
use crate::agentfield::types::{RemoteExecutionStatus, StatusEnvelope};

/// Maximum concurrent nonterminal runs per session (spec §9.3 / AC-06).
pub const MAX_ACTIVE_RUNS: usize = 4;
/// Maximum retained runs per session; only the oldest terminal run is
/// evicted, and only to admit a new run (spec §9.3).
pub const MAX_RETAINED_RUNS: usize = 32;
/// Maximum bounded result summary bytes per run (spec §9.3).
pub const MAX_SUMMARY_BYTES: usize = 8 * 1024;

/// Canonical Lato run status (spec §7.3). `paused` exists in the frozen
/// state set but the pinned `v0.1.138` contract cannot prove an
/// approval-wait state, so it is never assigned in 7C2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
    Unavailable,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Unavailable => "unavailable",
        }
    }

    /// Terminal states are monotonic; `unavailable` is explicitly NOT
    /// terminal (spec §7.3).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }
}

/// One owned run. Everything here is safe to project to the model: no
/// tokens, no base URLs, no raw inputs (only the input digest).
#[derive(Clone, Debug)]
pub struct RunState {
    pub run_id: String,
    pub alias: String,
    pub execute_target: String,
    /// Catalog revision carried by the `start` that created this run.
    pub revision: String,
    /// SHA-256 hex of the canonical input; the input itself is never kept.
    pub input_digest: String,
    pub execution_id: Option<String>,
    pub status: RunStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Bounded remote result summary (≤ 8 KiB, UTF-8-safe truncation).
    pub summary: Option<String>,
    /// Whether `summary` was truncated.
    pub summary_truncated: bool,
    /// Last stable error code observed for this run.
    pub last_error: Option<&'static str>,
}

/// Stable manager-level failures; `code()` yields the frozen `agentfield.*`
/// family (spec §11).
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// Session closed, control plane unreachable, health unknown.
    #[error("agentfield.unavailable: {0}")]
    Unavailable(String),
    /// Local capacity: 4 active / 32 retained.
    #[error("agentfield.limit_exceeded: {0}")]
    LimitExceeded(String),
    /// Unknown or foreign run id — indistinguishable by design (AC-07).
    #[error("agentfield.not_found")]
    NotFound,
    /// Pass-through client error (`agentfield.*` family).
    #[error("{0}")]
    Client(#[from] AgentFieldError),
}

impl ManagerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "agentfield.unavailable",
            Self::LimitExceeded(_) => "agentfield.limit_exceeded",
            Self::NotFound => "agentfield.not_found",
            Self::Client(error) => client_error_code(error),
        }
    }
}

/// Frozen §11 code for every client failure.
pub fn client_error_code(error: &AgentFieldError) -> &'static str {
    match error {
        AgentFieldError::Unavailable(_) => "agentfield.unavailable",
        AgentFieldError::Unauthorized => "agentfield.unauthorized",
        AgentFieldError::RemoteProtocol(_) => "agentfield.remote_protocol",
        AgentFieldError::RemoteDenied => "agentfield.remote_denied",
        AgentFieldError::BodyTooLarge(_) => "agentfield.output_too_large",
        AgentFieldError::CredentialMissing(_) => "agentfield.unconfigured",
    }
}

/// Cancel result discrimination (spec §7.4). A network timeout is never
/// reported as a successful cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    /// Remote confirmed the cancellation.
    Cancelled,
    /// The run was already terminal; no request was sent.
    AlreadyTerminal,
    /// The request may have reached the remote but no confirmation arrived.
    CancelRequested,
    /// The cancel could not be attempted or the manager is closed.
    Unavailable,
}

/// Lazy production client factory. The credential is resolved and the
/// transport connected on first use — session start performs zero network
/// requests; failures are not cached (the next call may retry once).
pub type ClientFactory = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<
            'static,
            Result<Arc<dyn AgentFieldClient>, AgentFieldError>,
        > + Send
        + Sync,
>;

pub fn production_client_factory(
    credential: crate::agentfield::client::RedactedToken,
    config: AgentFieldConfig,
) -> ClientFactory {
    Arc::new(move || {
        let credential = credential.clone();
        let config = config.clone();
        Box::pin(async move {
            let client =
                crate::agentfield::production_agentfield_client_from_token(credential, &config)
                    .await?;
            Ok(Arc::new(client) as Arc<dyn AgentFieldClient>)
        })
    })
}

struct Inner {
    runs: Vec<RunState>,
}

/// Drop guard for an in-flight `start_run` send. If the future is dropped
/// before the send resolved (tool timeout, task abort, mid-call cancel),
/// the reserved run — which may or may not have reached the control plane —
/// is marked permanently `outcome_unknown`: it can never stay a stranded,
/// unbound `queued` reservation (spec §7.2).
struct SendGuard {
    inner: Arc<StdMutex<Inner>>,
    run_id: String,
    resolved: bool,
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        if let Ok(mut inner) = self.inner.lock()
            && let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == self.run_id)
            && !run.status.is_terminal()
        {
            run.status = RunStatus::OutcomeUnknown;
            run.last_error = Some("agentfield.outcome_unknown");
            run.updated_at_ms = epoch_ms();
            run.summary = Some(
                "the start outcome could not be determined because the call was interrupted; the execution may have started. Verify manually in the AgentField control plane (by time, target, and audit records). Do NOT start again unless accepting duplicate-execution risk."
                    .to_owned(),
            );
        }
    }
}

/// Session-scoped run manager. All mutating transitions serialize on the
/// synchronous `inner` lock (short, await-free critical sections) so the
/// send-time drop guard can also run synchronously; the remote send happens
/// outside the lock so one slow request never blocks state reads, but
/// capacity is consumed inside the lock.
pub struct AgentFieldManager {
    session_id: String,
    catalog: AgentFieldCatalog,
    /// Real configuration sources backing the frozen catalog (user /
    /// project / plugin). Empty ⇒ the revision recheck trivially matches
    /// (no source to reload — programmatic/injected setups).
    catalog_sources: Vec<crate::agentfield::catalog::CatalogSource>,
    factory: Option<ClientFactory>,
    client: OnceCell<Arc<dyn AgentFieldClient>>,
    probe_cache: Mutex<Option<(Instant, AgentFieldHealthSnapshot)>>,
    inner: Arc<StdMutex<Inner>>,
    seq: AtomicU64,
    closed: AtomicBool,
}

impl AgentFieldManager {
    /// Production constructor: lazy client creation through the unique
    /// policy factory (`production_client_factory`). The credential is
    /// resolved by the caller at the registration gate — an unresolvable
    /// reference means zero tool registration and zero network. Client
    /// creation performs no network at session start.
    pub fn new(
        session_id: &str,
        catalog: AgentFieldCatalog,
        credential: crate::agentfield::client::RedactedToken,
        config: AgentFieldConfig,
        catalog_sources: Vec<crate::agentfield::catalog::CatalogSource>,
    ) -> Self {
        Self::with_factory_and_sources(
            session_id,
            catalog,
            Some(production_client_factory(credential, config)),
            catalog_sources,
        )
    }

    /// Test/injection constructor with no configuration sources: the
    /// revision recheck trivially matches. `None` factory means every
    /// client use fails closed with `agentfield.unavailable`.
    pub fn with_factory(
        session_id: &str,
        catalog: AgentFieldCatalog,
        factory: Option<ClientFactory>,
    ) -> Self {
        Self::with_factory_and_sources(session_id, catalog, factory, Vec::new())
    }

    pub fn with_factory_and_sources(
        session_id: &str,
        catalog: AgentFieldCatalog,
        factory: Option<ClientFactory>,
        catalog_sources: Vec<crate::agentfield::catalog::CatalogSource>,
    ) -> Self {
        Self {
            session_id: session_id.to_owned(),
            catalog,
            catalog_sources,
            factory,
            client: OnceCell::new(),
            probe_cache: Mutex::new(None),
            inner: Arc::new(StdMutex::new(Inner { runs: Vec::new() })),
            seq: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    pub fn catalog(&self) -> &AgentFieldCatalog {
        &self.catalog
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Mark the manager unavailable (session close). Idempotent; performs
    /// NO remote cancel — remote executions keep running (spec §9.2).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn ensure_open(&self) -> Result<(), ManagerError> {
        if self.is_closed() {
            return Err(ManagerError::Unavailable("this session has closed".into()));
        }
        Ok(())
    }

    async fn client(&self) -> Result<Arc<dyn AgentFieldClient>, ManagerError> {
        self.ensure_open()?;
        let Some(factory) = self.factory.as_ref() else {
            return Err(ManagerError::Unavailable("no control-plane client".into()));
        };
        let factory = factory.clone();
        // get_or_try_init does NOT cache a failed initialization, so a
        // transient connect failure at first use never disables the
        // session permanently.
        let client = self
            .client
            .get_or_try_init(|| async move { factory().await })
            .await?;
        Ok(client.clone())
    }

    /// Fresh (≤ 30 s) health snapshot. A fresh snapshot is reused with zero
    /// network; an expired one re-probes once; a failed re-probe surfaces
    /// `agentfield.unavailable` and leaves the stale snapshot un-refreshed
    /// (spec §6.2: probing is never a per-turn hard dependency, and `list`
    /// degrades to `available: false`).
    pub async fn health_snapshot(&self) -> Result<AgentFieldHealthSnapshot, ManagerError> {
        self.ensure_open()?;
        {
            let cached = self.probe_cache.lock().await;
            if let Some((observed_at, snapshot)) = cached.as_ref()
                && observed_at.elapsed() < HEALTH_SNAPSHOT_TTL
            {
                return Ok(snapshot.clone());
            }
        }
        let client = self.client().await?;
        let fresh = AgentFieldProbe::new(client).probe().await?;
        *self.probe_cache.lock().await = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// Whether `target` was healthy in a fresh (≤ 30 s) snapshot. `None`
    /// means health is unknown right now (no fresh snapshot and the
    /// re-probe failed) — callers fail `start` closed.
    pub async fn target_health(&self, target: &str) -> Result<Option<bool>, ManagerError> {
        match self.health_snapshot().await {
            Ok(snapshot) => Ok(Some(
                snapshot.healthy_execute_targets.iter().any(|t| t == target),
            )),
            Err(error) => {
                // A stale snapshot exists but is not fresh: health unknown.
                let stale = self.probe_cache.lock().await.clone();
                if stale.is_some() {
                    let _ = error;
                    return Ok(None);
                }
                Err(error)
            }
        }
    }

    /// Healthy execute targets from a fresh (≤ 30 s) snapshot, or `None`
    /// when health is unknown right now (`list` degrades every capability
    /// to `available: false`; spec §6.2).
    pub async fn healthy_targets(&self) -> Option<Vec<String>> {
        if self.is_closed() {
            return None;
        }
        match self.health_snapshot().await {
            Ok(snapshot) => Some(snapshot.healthy_execute_targets.clone()),
            Err(_) => self.probe_cache.lock().await.clone().map(|_| Vec::new()),
        }
    }

    /// The last VERIFIED discovery snapshot without probing (fresh or
    /// stale). `list` intersects the local allowlist against its execute
    /// targets; `None` means no verified discovery exists at all.
    pub async fn verified_discovery_snapshot(&self) -> Option<AgentFieldHealthSnapshot> {
        self.probe_cache
            .lock()
            .await
            .clone()
            .map(|(_, snapshot)| snapshot)
    }

    /// Post-approval TOCTOU recheck (Round-1 acceptance P1-3): recompute
    /// the merged catalog from the real configuration sources and compare
    /// against the session-frozen revision. `true` only when the sources
    /// still yield the frozen revision; any change — capability edits,
    /// enabled flips, sources disappearing, or disagreeing origins — fails
    /// closed to `false` (the caller returns `agentfield.catalog_changed`).
    /// With no sources wired (programmatic/injected setups) the frozen
    /// revision trivially matches.
    pub fn recheck_revision_matches(&self) -> bool {
        if self.catalog_sources.is_empty() {
            return true;
        }
        match crate::agentfield::catalog::assemble_catalog_config(&self.catalog_sources) {
            Some(config) => {
                let current = AgentFieldCatalog::from_config(&config);
                current.revision() == self.catalog.revision()
            }
            None => false,
        }
    }

    /// Reserve capacity, perform the exactly-one async execute request, and
    /// bind the remote execution ID. Called only after schema, allowlist,
    /// policy/approval, and revision recheck have all passed.
    ///
    /// Send-outcome mapping (spec §7.2):
    /// - 202 envelope with a non-empty execution ID → bound, status `queued`;
    /// - definitive remote rejection (401/403, structured deny) → run kept
    ///   as `failed`, stable error returned;
    /// - anything else where the request may have arrived (transport
    ///   failure, timeout, undecodable 202 body, empty execution ID) →
    ///   permanent `outcome_unknown`.
    pub async fn start_run(
        &self,
        alias: &str,
        execute_target: &str,
        revision: &str,
        input: &Value,
    ) -> Result<RunState, ManagerError> {
        self.ensure_open()?;
        // Obtain the client BEFORE the reservation: a client failure means
        // NO request was attempted and nothing needs rolling back.
        let client = self.client().await?;
        let input_digest = digest_hex(&canonical_input(input));
        let run_id = {
            let mut inner = self.inner.lock().expect("agentfield run state lock");
            let nonterminal = inner
                .runs
                .iter()
                .filter(|run| !run.status.is_terminal())
                .count();
            if nonterminal >= MAX_ACTIVE_RUNS {
                return Err(ManagerError::LimitExceeded(format!(
                    "at most {MAX_ACTIVE_RUNS} nonterminal runs per session"
                )));
            }
            if inner.runs.len() >= MAX_RETAINED_RUNS {
                // Evict the oldest terminal run only (spec §9.3).
                let evict = inner.runs.iter().position(|run| run.status.is_terminal());
                match evict {
                    Some(index) => {
                        inner.runs.remove(index);
                    }
                    None => {
                        return Err(ManagerError::LimitExceeded(format!(
                            "at most {MAX_RETAINED_RUNS} retained runs per session"
                        )));
                    }
                }
            }
            let now = epoch_ms();
            let run_id = format!(
                "afrun_{:x}-{:x}",
                now,
                self.seq.fetch_add(1, Ordering::Relaxed)
            );
            inner.runs.push(RunState {
                run_id: run_id.clone(),
                alias: alias.to_owned(),
                execute_target: execute_target.to_owned(),
                revision: revision.to_owned(),
                input_digest,
                execution_id: None,
                status: RunStatus::Queued,
                created_at_ms: now,
                updated_at_ms: now,
                summary: None,
                summary_truncated: false,
                last_error: None,
            });
            run_id
        };
        // Disarmed on every deliberate resolution path; fires only when the
        // future is dropped mid-send (cancel/timeout/abort).
        let mut guard = SendGuard {
            inner: self.inner.clone(),
            run_id: run_id.clone(),
            resolved: false,
        };

        // Exactly one send attempt. Everything below is post-decision.
        let outcome = client.start_async(execute_target, input).await;
        match outcome {
            Ok(envelope) => {
                if envelope.execution_id.is_empty() {
                    let run = self.mark_outcome_unknown(&run_id).await;
                    guard.resolved = true;
                    return Ok(run);
                }
                let mut inner = self.inner.lock().expect("agentfield run state lock");
                if let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) {
                    run.execution_id = Some(envelope.execution_id);
                    run.status = RunStatus::Queued;
                    run.updated_at_ms = epoch_ms();
                    guard.resolved = true;
                    Ok(run.clone())
                } else {
                    // The reservation cannot vanish while nonterminal.
                    Err(ManagerError::NotFound)
                }
            }
            Err(error) => {
                let result = self.finish_failed_send(&run_id, error).await;
                guard.resolved = true;
                result
            }
        }
    }

    /// Classify a failed send. Definitive rejections (the control plane
    /// answered and refused before creating an execution) keep the run as
    /// `failed`; uncertain outcomes become permanent `outcome_unknown`.
    async fn finish_failed_send(
        &self,
        run_id: &str,
        error: AgentFieldError,
    ) -> Result<RunState, ManagerError> {
        let definitive = matches!(
            error,
            AgentFieldError::Unauthorized | AgentFieldError::RemoteDenied
        );
        let mut inner = self.inner.lock().expect("agentfield run state lock");
        let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
            return Err(ManagerError::NotFound);
        };
        run.updated_at_ms = epoch_ms();
        if definitive {
            run.status = RunStatus::Failed;
            run.last_error = Some(client_error_code(&error));
        } else {
            // Transport failure, timeout, undecodable body: the request may
            // have reached the control plane. Permanent local terminal —
            // no auto retry, no auto query, no reconcile (spec §7.2).
            run.status = RunStatus::OutcomeUnknown;
            run.last_error = Some("agentfield.outcome_unknown");
            run.summary = Some(
                "the remote start outcome could not be determined; the execution may have \
                 started. Verify manually in the AgentField control plane (by time, target, \
                 and audit records). Do NOT start again unless accepting duplicate-execution \
                 risk."
                    .to_owned(),
            );
        }
        Ok(run.clone())
    }

    async fn mark_outcome_unknown(&self, run_id: &str) -> RunState {
        let mut inner = self.inner.lock().expect("agentfield run state lock");
        let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
            unreachable!("reservation for {run_id} cannot vanish before binding");
        };
        run.status = RunStatus::OutcomeUnknown;
        run.last_error = Some("agentfield.outcome_unknown");
        run.updated_at_ms = epoch_ms();
        run.summary = Some(
            "the remote accepted the start but no execution id was bound; the execution may \
             have started. Verify manually in the AgentField control plane (by time, target, \
             and audit records). Do NOT start again unless accepting duplicate-execution risk."
                .to_owned(),
        );
        run.clone()
    }

    /// Query one owned run: `Some(run_id)` → possibly a remote status
    /// refresh; `None` → the 20 most recent owned runs (local only, no
    /// network).
    pub async fn run_status(&self, run_id: Option<&str>) -> Result<Vec<RunState>, ManagerError> {
        self.ensure_open()?;
        let Some(run_id) = run_id else {
            let inner = self.inner.lock().expect("agentfield run state lock");
            let runs: Vec<RunState> = inner.runs.iter().rev().take(20).cloned().collect();
            return Ok(runs);
        };
        let execution_id = {
            let inner = self.inner.lock().expect("agentfield run state lock");
            let run = inner
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                // Unknown and foreign ids are indistinguishable (AC-07).
                .ok_or(ManagerError::NotFound)?;
            match run.status {
                // Permanent local terminal: never queried, never reconciled.
                RunStatus::OutcomeUnknown
                | RunStatus::Completed
                | RunStatus::Failed
                | RunStatus::Cancelled => return Ok(vec![run.clone()]),
                _ => run.execution_id.clone(),
            }
        };
        let Some(execution_id) = execution_id else {
            // Nonterminal without a bound execution id cannot exist (every
            // admitted run binds or turns outcome_unknown before returning).
            let inner = self.inner.lock().expect("agentfield run state lock");
            let run = inner
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .ok_or(ManagerError::NotFound)?;
            return Ok(vec![run.clone()]);
        };
        let client = self.client().await?;
        let envelope = client.status(&execution_id).await;
        match envelope {
            Ok(envelope) => {
                let status = map_remote_status(&envelope)?;
                let mut inner = self.inner.lock().expect("agentfield run state lock");
                let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
                    return Err(ManagerError::NotFound);
                };
                apply_remote_status(run, status, &envelope);
                Ok(vec![run.clone()])
            }
            Err(AgentFieldError::Unavailable(_)) => {
                // Control plane temporarily unreachable: not terminal, the
                // last known state is preserved (spec §7.3).
                let mut inner = self.inner.lock().expect("agentfield run state lock");
                let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
                    return Err(ManagerError::NotFound);
                };
                if !run.status.is_terminal() {
                    run.status = RunStatus::Unavailable;
                    run.updated_at_ms = epoch_ms();
                }
                run.last_error = Some("agentfield.unavailable");
                Ok(vec![run.clone()])
            }
            Err(error) => {
                // Protocol violations fail closed and keep the last known
                // state; other codes surface as-is (spec §7.3).
                Err(error.into())
            }
        }
    }

    /// Cancel an owned run (spec §7.4). Terminal runs answer
    /// `already_terminal` with zero network. An unconfirmed request is
    /// NEVER reported as a successful cancellation.
    pub async fn cancel_run(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<(RunState, CancelOutcome), ManagerError> {
        self.ensure_open()?;
        let execution_id = {
            let inner = self.inner.lock().expect("agentfield run state lock");
            let run = inner
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .ok_or(ManagerError::NotFound)?;
            if run.status.is_terminal() {
                return Ok((run.clone(), CancelOutcome::AlreadyTerminal));
            }
            run.execution_id.clone()
        };
        let Some(execution_id) = execution_id else {
            let inner = self.inner.lock().expect("agentfield run state lock");
            let run = inner
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .ok_or(ManagerError::NotFound)?;
            return Ok((run.clone(), CancelOutcome::Unavailable));
        };
        let client = self.client().await?;
        let result = client.cancel(&execution_id, reason).await;
        match result {
            Ok(Some(_envelope)) => {
                let mut inner = self.inner.lock().expect("agentfield run state lock");
                let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
                    return Err(ManagerError::NotFound);
                };
                if !run.status.is_terminal() {
                    run.status = RunStatus::Cancelled;
                    run.updated_at_ms = epoch_ms();
                }
                Ok((run.clone(), CancelOutcome::Cancelled))
            }
            Ok(None) => {
                // 409 invalid_state: the remote says the execution is already
                // terminal. The local state is left for the next `status` to
                // reconcile; the outcome is still truthful.
                let inner = self.inner.lock().expect("agentfield run state lock");
                let run = inner
                    .runs
                    .iter()
                    .find(|run| run.run_id == run_id)
                    .ok_or(ManagerError::NotFound)?;
                Ok((run.clone(), CancelOutcome::AlreadyTerminal))
            }
            Err(error) => {
                let mut inner = self.inner.lock().expect("agentfield run state lock");
                let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
                    return Err(ManagerError::NotFound);
                };
                run.last_error = Some(client_error_code(&error));
                Ok((run.clone(), CancelOutcome::CancelRequested))
            }
        }
    }

    /// Local run lookup (no network). Unknown and foreign ids return
    /// `None` — the caller renders the same `agentfield.not_found`.
    pub async fn run(&self, run_id: &str) -> Option<RunState> {
        let inner = self.inner.lock().expect("agentfield run state lock");
        inner.runs.iter().find(|run| run.run_id == run_id).cloned()
    }

    pub async fn run_count(&self) -> usize {
        self.inner
            .lock()
            .expect("agentfield run state lock")
            .runs
            .len()
    }
}

fn map_remote_status(envelope: &StatusEnvelope) -> Result<RunStatus, ManagerError> {
    // The pinned decoder has already failed closed on unknown enums; this
    // mapping covers only the five known remote statuses.
    Ok(match envelope.status {
        RemoteExecutionStatus::Queued => RunStatus::Queued,
        RemoteExecutionStatus::Running => RunStatus::Running,
        RemoteExecutionStatus::Completed => RunStatus::Completed,
        RemoteExecutionStatus::Failed => RunStatus::Failed,
        RemoteExecutionStatus::Cancelled => RunStatus::Cancelled,
    })
}

/// Monotonic state application (AC-08): terminal states are never
/// overwritten by late arrivals; `unavailable` never masks a known state
/// (the previous status is kept as `last_known` in the projection via
/// `summary`/`last_error`).
fn apply_remote_status(run: &mut RunState, status: RunStatus, envelope: &StatusEnvelope) {
    if run.status.is_terminal() {
        // Late arrival: keep the terminal state.
        return;
    }
    run.status = status;
    run.updated_at_ms = epoch_ms();
    match status {
        RunStatus::Completed => {
            if let Some(result) = envelope.optional_ignored.result.as_deref() {
                // Remote-controlled content is redacted BEFORE it can reach
                // any summary/projection surface (Round-1 acceptance P1-2).
                let (summary, truncated) =
                    truncate_utf8(&redact_secrets(result), MAX_SUMMARY_BYTES);
                run.summary = Some(summary);
                run.summary_truncated = truncated;
            }
        }
        RunStatus::Failed => {
            let detail = envelope
                .optional_ignored
                .error
                .as_deref()
                .or(envelope.optional_ignored.error_details.as_deref())
                .or(envelope.optional_ignored.status_reason.as_deref());
            if let Some(detail) = detail {
                let (summary, truncated) =
                    truncate_utf8(&redact_secrets(detail), MAX_SUMMARY_BYTES);
                run.summary = Some(summary);
                run.summary_truncated = truncated;
            }
        }
        _ => {}
    }
}

/// Redact credential-shaped material from remote-controlled text before it
/// reaches a summary, log, or model projection: any `*authorization` header
/// value (any case, any scheme — `Basic`/`Digest`/`Bearer`/`Token`/
/// `Negotiate`/`AWS4-HMAC-SHA256`/unknown/multi-parameter), `Bearer`
/// credentials, and `token`/`secret`/`password`/`api[-_]key` assignments.
/// The redaction is value-agnostic and fails safe: after an
/// `authorization`-shaped header the ENTIRE remainder of the line is
/// redacted, so no scheme spelling or parameter layout can leave credential
/// material behind.
pub fn redact_secrets(text: &str) -> String {
    const TRIGGERS: [&str; 8] = [
        "authorization",
        "bearer",
        "token",
        "secret",
        "password",
        "api-key",
        "api_key",
        "apikey",
    ];
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < lower.len() {
        // Find the next trigger occurrence with a word boundary before it.
        // A preceding `-` is allowed so compound header names
        // (`proxy-authorization`, `x-api-key`, …) trigger too — the
        // redaction is value-agnostic, so the extra breadth fails safe.
        let mut hit: Option<(usize, &str)> = None;
        for trigger in TRIGGERS {
            let mut from = cursor;
            while let Some(rel) = lower[from..].find(trigger) {
                let start = from + rel;
                let end = start + trigger.len();
                let before_ok = start == 0
                    || !matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_');
                let after_ok = end == bytes.len()
                    || !matches!(bytes[end], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_');
                if before_ok && after_ok && hit.is_none_or(|(offset, _)| start < offset) {
                    hit = Some((start, trigger));
                    break;
                }
                from = end;
            }
        }
        let Some((start, trigger)) = hit else {
            out.push_str(&text[cursor..]);
            break;
        };
        // Emit everything up to the trigger, then the trigger itself.
        out.push_str(&text[cursor..start]);
        let mut pos = start + trigger.len();
        out.push_str(&text[start..pos]);
        // Consume separators (`:`, `=`, quotes, whitespace) and remember
        // whether an assignment shape (`:` or `=`) is present and whether
        // the value is quote-delimited (a quote is the LAST separator).
        let mut saw_assignment = false;
        let mut value_quoted = false;
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b':' | b'=' | b'"' | b'\'') {
            if matches!(bytes[pos], b':' | b'=') {
                saw_assignment = true;
            }
            value_quoted = matches!(bytes[pos], b'"' | b'\'');
            out.push(bytes[pos] as char);
            pos += 1;
        }
        if trigger == "authorization" && !saw_assignment {
            // Prose mentioning "authorization" without a header/assignment
            // shape: not credential material, keep scanning after the word.
            cursor = pos;
            continue;
        }
        if value_quoted {
            // Quoted value: everything up to the matching closing quote is
            // credential material — whitespace inside must not split it
            // (`token="a b"` redacts BOTH tokens). A missing closer fails
            // safe by redacting to the end of the text.
            let closing = bytes[pos..]
                .iter()
                .position(|byte| *byte == b'"' || *byte == b'\'')
                .map_or(bytes.len(), |rel| pos + rel);
            out.push_str("[redacted]");
            if closing < bytes.len() {
                out.push_str(&text[closing..closing + 1]);
            }
            pos = closing + usize::from(closing < bytes.len());
            cursor = pos;
            continue;
        }
        // Unquoted values. `authorization`-shaped headers redact the whole
        // remainder of the line PLUS any folded continuation lines (remote
        // text may use folded header layout and must not be trusted to keep
        // a header on one line); every other trigger redacts the single
        // value token (everything until whitespace or a JSON-ish closer).
        let line_rest = trigger == "authorization";
        loop {
            let value_start = pos;
            while pos < bytes.len()
                && !(if line_rest {
                    matches!(bytes[pos], b'\n' | b'\r')
                } else {
                    matches!(
                        bytes[pos],
                        b' ' | b'\t' | b'\n' | b'\r' | b',' | b';' | b'}' | b']' | b'"'
                    )
                })
            {
                pos += 1;
            }
            if pos > value_start {
                out.push_str("[redacted]");
            }
            if !line_rest || pos >= bytes.len() {
                break;
            }
            // Consume the line terminator verbatim.
            let term_start = pos;
            if bytes[pos] == b'\r' {
                pos += 1;
            }
            if pos < bytes.len() && bytes[pos] == b'\n' {
                pos += 1;
            }
            out.push_str(&text[term_start..pos]);
            // A folded continuation line keeps the credential going; a
            // `key:`-shaped line is the next field and survives.
            if line_is_header_shaped(bytes, pos) {
                break;
            }
        }
        cursor = pos;
    }
    out
}

/// True when the line starting at `start` begins with a `key:`-shaped
/// field (token of `[A-Za-z0-9_-]+`, optional whitespace, then `:`).
/// Continuation lines of a folded header do not have this shape.
fn line_is_header_shaped(bytes: &[u8], start: usize) -> bool {
    let mut peek = start;
    while peek < bytes.len() && matches!(bytes[peek], b' ' | b'\t') {
        peek += 1;
    }
    let token_start = peek;
    while peek < bytes.len()
        && matches!(bytes[peek], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_')
    {
        peek += 1;
    }
    if peek == token_start {
        return false;
    }
    while peek < bytes.len() && matches!(bytes[peek], b' ' | b'\t') {
        peek += 1;
    }
    peek < bytes.len() && bytes[peek] == b':'
}

/// UTF-8-safe truncation to a byte budget: the cut only happens on char
/// boundaries, so the summary is always valid UTF-8 (AC-10).
pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_owned(), true)
}

fn canonical_input(input: &Value) -> Vec<u8> {
    lato_policy::canonical_arguments(input)
        .expect("tool inputs are JSON-serializable by construction")
}

pub(crate) fn digest_hex(canonical: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canonical);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentfield::types::{AsyncStartEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    fn catalog() -> AgentFieldCatalog {
        let raw = json!({
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object"},
                    "risk": "remote_read",
                }
            }
        });
        let config = AgentFieldConfig::parse(&raw).unwrap().unwrap();
        AgentFieldCatalog::from_config(&config)
    }

    fn start_envelope(execution_id: &str) -> AsyncStartEnvelope {
        AsyncStartEnvelope::decode(&json!({
            "execution_id": execution_id,
            "status": "queued",
            "target": "legal.review_contract",
            "type": "reasoner",
            "run_id": "run-1",
            "workflow_id": "run-1",
            "created_at": "2026-09-17T00:00:00Z",
            "enqueued_at": "2026-09-17T00:00:00Z",
            "webhook_registered": false,
        }))
        .unwrap()
    }

    fn status_envelope(execution_id: &str, status: &str) -> StatusEnvelope {
        StatusEnvelope::decode(&json!({
            "execution_id": execution_id,
            "status": status,
            "run_id": "run-1",
            "started_at": "2026-09-17T00:00:01Z",
            "webhook_registered": false,
            "result": "all good",
        }))
        .unwrap()
    }

    /// Fake client with programmable per-call behavior and counters.
    struct FakeClient {
        discovery_calls: AtomicUsize,
        start_calls: AtomicUsize,
        status_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
        start_behavior: Mutex<StartBehavior>,
        status_behavior: Mutex<StatusBehavior>,
        cancel_behavior: Mutex<CancelBehavior>,
    }

    #[derive(Clone)]
    enum StartBehavior {
        Ok(String),
        EmptyId,
        Fail(AgentFieldError),
        /// Sleep before answering, so the send future can be aborted
        /// mid-flight (Round-1 acceptance P1-1 regression).
        DelayThenOk(String, u64),
    }

    #[derive(Clone)]
    enum StatusBehavior {
        Ok(String),
        Fail(AgentFieldError),
    }

    #[derive(Clone)]
    enum CancelBehavior {
        Confirmed,
        Conflict,
        Fail(AgentFieldError),
    }

    impl FakeClient {
        fn new(start: StartBehavior) -> Self {
            Self {
                discovery_calls: AtomicUsize::new(0),
                start_calls: AtomicUsize::new(0),
                status_calls: AtomicUsize::new(0),
                cancel_calls: AtomicUsize::new(0),
                start_behavior: Mutex::new(start),
                status_behavior: Mutex::new(StatusBehavior::Ok("running".into())),
                cancel_behavior: Mutex::new(CancelBehavior::Confirmed),
            }
        }

        fn discovery_value() -> Value {
            json!({
                "discovered_at": "2026-09-17T00:00:00Z",
                "total_agents": 1,
                "total_reasoners": 1,
                "total_skills": 0,
                "pagination": {"limit": 100, "offset": 0, "has_more": false},
                "capabilities": [{
                    "agent_id": "legal",
                    "group_id": "",
                    "base_url": "https://agentfield.invalid",
                    "version": "v0.1.138",
                    "health_status": "healthy",
                    "deployment_type": "service",
                    "last_heartbeat": "2026-09-17T00:00:00Z",
                    "reasoners": [{
                        "id": "review_contract",
                        "invocation_target": "legal:review_contract"
                    }],
                    "skills": []
                }]
            })
        }
    }

    #[async_trait]
    impl AgentFieldClient for FakeClient {
        async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
            self.discovery_calls.fetch_add(1, Ordering::SeqCst);
            DiscoveryEnvelope::decode(&Self::discovery_value())
                .map_err(AgentFieldError::RemoteProtocol)
        }

        async fn start_async(
            &self,
            _execute_target: &str,
            _input: &Value,
        ) -> Result<AsyncStartEnvelope, AgentFieldError> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self.start_behavior.lock().await.clone();
            match behavior {
                StartBehavior::Ok(id) => Ok(start_envelope(&id)),
                StartBehavior::EmptyId => Ok(start_envelope("")),
                StartBehavior::Fail(error) => Err(error),
                StartBehavior::DelayThenOk(id, ms) => {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    Ok(start_envelope(&id))
                }
            }
        }

        async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self.status_behavior.lock().await.clone();
            match behavior {
                StatusBehavior::Ok(status) => Ok(status_envelope(execution_id, &status)),
                StatusBehavior::Fail(error) => Err(error),
            }
        }

        async fn cancel(
            &self,
            _execution_id: &str,
            _reason: &str,
        ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self.cancel_behavior.lock().await.clone();
            match behavior {
                CancelBehavior::Confirmed => Ok(Some(
                    CancelSuccessEnvelope::decode(&json!({
                        "execution_id": "exec-1",
                        "status": "cancelled",
                        "previous_status": "running",
                        "cancelled_at": "2026-09-17T00:00:02Z",
                    }))
                    .unwrap(),
                )),
                CancelBehavior::Conflict => Ok(None),
                CancelBehavior::Fail(error) => Err(error),
            }
        }
    }

    fn manager_with(start: StartBehavior) -> (AgentFieldManager, Arc<FakeClient>) {
        let client = Arc::new(FakeClient::new(start));
        let shared: Arc<dyn AgentFieldClient> = client.clone();
        let manager = AgentFieldManager::with_factory(
            "session-test",
            catalog(),
            Some(Arc::new(move || {
                let shared = shared.clone();
                Box::pin(async move { Ok(shared.clone()) })
            })),
        );
        (manager, client)
    }

    fn input() -> Value {
        json!({"contract": "acme.pdf"})
    }

    #[tokio::test]
    async fn start_binds_the_remote_execution_id() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                manager.catalog().revision(),
                &input(),
            )
            .await
            .unwrap();
        assert_eq!(run.execution_id.as_deref(), Some("exec-1"));
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.alias, "contract-review");
        assert_eq!(run.input_digest.len(), 64);
        assert_eq!(client.start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_execution_id_is_permanent_outcome_unknown() {
        let (manager, client) = manager_with(StartBehavior::EmptyId);
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::OutcomeUnknown);
        assert!(run.status.is_terminal());
        assert!(run.execution_id.is_none());
        assert_eq!(run.last_error, Some("agentfield.outcome_unknown"));
        assert!(run.summary.as_deref().unwrap().contains("control plane"));
        // Terminal local state: no remote query ever happens.
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::OutcomeUnknown);
        assert_eq!(client.status_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn uncertain_send_failure_is_permanent_outcome_unknown() {
        for error in [
            AgentFieldError::Unavailable("connection reset".into()),
            AgentFieldError::RemoteProtocol("bad 202 body".into()),
            AgentFieldError::BodyTooLarge(1024),
        ] {
            let (manager, client) = manager_with(StartBehavior::Fail(error));
            let run = manager
                .start_run(
                    "contract-review",
                    "legal.review_contract",
                    "sha256:x",
                    &input(),
                )
                .await
                .unwrap();
            assert_eq!(run.status, RunStatus::OutcomeUnknown, "{run:?}");
            assert!(run.execution_id.is_none());
            assert_eq!(
                client.start_calls.load(Ordering::SeqCst),
                1,
                "exactly one send"
            );
        }
    }

    #[tokio::test]
    async fn definitive_rejection_keeps_the_run_failed() {
        for error in [AgentFieldError::Unauthorized, AgentFieldError::RemoteDenied] {
            let (manager, _) = manager_with(StartBehavior::Fail(error));
            let run = manager
                .start_run(
                    "contract-review",
                    "legal.review_contract",
                    "sha256:x",
                    &input(),
                )
                .await
                .unwrap();
            assert_eq!(run.status, RunStatus::Failed);
            assert!(run.last_error.is_some());
        }
    }

    #[tokio::test]
    async fn concurrent_starts_respect_the_atomic_four_active_cap() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let manager = Arc::new(manager);
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                manager
                    .start_run(
                        "contract-review",
                        "legal.review_contract",
                        "sha256:x",
                        &input(),
                    )
                    .await
            }));
        }
        let mut ok = 0usize;
        let mut limited = 0usize;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => ok += 1,
                Err(error) => {
                    assert_eq!(error.code(), "agentfield.limit_exceeded");
                    limited += 1;
                }
            }
        }
        assert_eq!(ok, MAX_ACTIVE_RUNS, "exactly 4 admitted");
        assert_eq!(limited, 12 - MAX_ACTIVE_RUNS);
        assert_eq!(client.start_calls.load(Ordering::SeqCst), MAX_ACTIVE_RUNS);
        assert_eq!(manager.run_count().await, MAX_ACTIVE_RUNS);
    }

    #[tokio::test]
    async fn retained_cap_evicts_the_oldest_terminal_run_only() {
        let (manager, _) = manager_with(StartBehavior::Ok("exec-1".into()));
        // Fill with terminals then one nonterminal; eviction must skip the
        // nonterminal even when it is oldest.
        for _ in 0..(MAX_RETAINED_RUNS - 1) {
            let run = manager
                .start_run(
                    "contract-review",
                    "legal.review_contract",
                    "sha256:x",
                    &input(),
                )
                .await
                .unwrap();
            // Force each to a terminal state through a completed status.
            *manager.inner.lock().expect("lock").runs.last_mut().unwrap() = {
                let mut completed = run.clone();
                completed.status = RunStatus::Completed;
                completed
            };
        }
        let nonterminal = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        // 31 terminals + 1 nonterminal = 32. One more start must evict the
        // OLDEST TERMINAL, never the nonterminal.
        let evicted = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await;
        assert!(evicted.is_ok());
        let runs = manager.run_status(None).await.unwrap();
        assert!(runs.iter().any(|run| run.run_id == nonterminal.run_id));
        assert_eq!(manager.run_count().await, MAX_RETAINED_RUNS);
    }

    #[tokio::test]
    async fn retained_cap_fails_closed_without_evictable_terminals() {
        let (manager, _) = manager_with(StartBehavior::Ok("exec-1".into()));
        // A single nonterminal run occupies the only slot we can create here;
        // simulate a full nonterminal set by patching inner directly.
        {
            let mut inner = manager.inner.lock().expect("lock");
            for seq in 0..MAX_RETAINED_RUNS {
                inner.runs.push(RunState {
                    run_id: format!("afrun_fill-{seq}"),
                    alias: "contract-review".into(),
                    execute_target: "legal.review_contract".into(),
                    revision: "sha256:x".into(),
                    input_digest: "0".repeat(64),
                    execution_id: Some(format!("exec-{seq}")),
                    status: RunStatus::Running,
                    created_at_ms: seq as u64,
                    updated_at_ms: seq as u64,
                    summary: None,
                    summary_truncated: false,
                    last_error: None,
                });
            }
        }
        let error = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "agentfield.limit_exceeded");
    }

    #[tokio::test]
    async fn status_refreshes_nonterminal_runs_and_is_monotonic() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();

        // running → running.
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(client.status_calls.load(Ordering::SeqCst), 1);

        // completed carries the bounded result summary.
        *client.status_behavior.lock().await = StatusBehavior::Ok("completed".into());
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Completed);
        assert_eq!(runs[0].summary.as_deref(), Some("all good"));

        // Late `running` from the remote must NOT overwrite the terminal.
        let late = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(late[0].status, RunStatus::Completed);
        assert_eq!(client.status_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_remote_status_fails_closed_and_keeps_last_known() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Running);
        // The decoder rejects unknown enums; the manager fails closed and
        // the last known state is preserved (spec §7.3).
        *client.status_behavior.lock().await = StatusBehavior::Fail(
            AgentFieldError::RemoteProtocol("unknown status enum".into()),
        );
        let error = manager.run_status(Some(&run.run_id)).await.unwrap_err();
        assert_eq!(error.code(), "agentfield.remote_protocol");
        let last_known = manager.run(&run.run_id).await.unwrap();
        assert_eq!(
            last_known.status,
            RunStatus::Running,
            "last known state kept"
        );
    }

    #[tokio::test]
    async fn status_unavailable_preserves_the_last_known_state() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Running);
        *client.status_behavior.lock().await =
            StatusBehavior::Fail(AgentFieldError::Unavailable("control plane down".into()));
        let runs = manager.run_status(Some(&run.run_id)).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Unavailable);
        assert!(!runs[0].status.is_terminal());
        assert_eq!(runs[0].last_error, Some("agentfield.unavailable"));
    }

    #[tokio::test]
    async fn unknown_and_foreign_runs_share_one_not_found_result() {
        let (manager, _client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let missing = manager
            .run_status(Some("afrun_unknown-1"))
            .await
            .unwrap_err();
        assert_eq!(missing.code(), "agentfield.not_found");
        let cancel = manager
            .cancel_run("afrun_unknown-1", "reason")
            .await
            .unwrap_err();
        assert_eq!(cancel.code(), "agentfield.not_found");
    }

    #[tokio::test]
    async fn cancel_confirmed_then_locally_terminal_is_idempotent() {
        // cancelled.
        let (manager, _client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        let (run, outcome) = manager.cancel_run(&run.run_id, "user").await.unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert_eq!(run.status, RunStatus::Cancelled);

        // already_terminal (local check, zero network).
        let (run, outcome) = manager.cancel_run(&run.run_id, "user").await.unwrap();
        assert_eq!(outcome, CancelOutcome::AlreadyTerminal);
        assert_eq!(run.status, RunStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_timeout_is_never_reported_cancelled() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        *client.cancel_behavior.lock().await =
            CancelBehavior::Fail(AgentFieldError::Unavailable("timeout".into()));
        let (run, outcome) = manager.cancel_run(&run.run_id, "user").await.unwrap();
        assert_eq!(outcome, CancelOutcome::CancelRequested);
        assert_ne!(run.status, RunStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_conflict_maps_to_already_terminal() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        *client.cancel_behavior.lock().await = CancelBehavior::Conflict;
        let (_run, outcome) = manager.cancel_run(&run.run_id, "user").await.unwrap();
        assert_eq!(outcome, CancelOutcome::AlreadyTerminal);
    }

    #[tokio::test]
    async fn close_fails_all_late_calls_without_remote_cancel() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        manager.close();
        assert!(manager.is_closed());
        let start = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap_err();
        assert_eq!(start.code(), "agentfield.unavailable");
        let status = manager.run_status(Some(&run.run_id)).await.unwrap_err();
        assert_eq!(status.code(), "agentfield.unavailable");
        let cancel = manager.cancel_run(&run.run_id, "user").await.unwrap_err();
        assert_eq!(cancel.code(), "agentfield.unavailable");
        // Session close never cancels the remote execution.
        assert_eq!(client.cancel_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_factory_fails_closed() {
        let manager = AgentFieldManager::with_factory("s", catalog(), None);
        let error = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "agentfield.unavailable");
    }

    #[test]
    fn utf8_truncation_is_byte_bounded_and_char_safe() {
        let text = "é".repeat(10_000); // 2 bytes per char
        let (summary, truncated) = truncate_utf8(&text, MAX_SUMMARY_BYTES);
        assert!(truncated);
        assert_eq!(summary.len(), MAX_SUMMARY_BYTES);
        assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
        let (short, not_truncated) = truncate_utf8("short", MAX_SUMMARY_BYTES);
        assert_eq!(short, "short");
        assert!(!not_truncated);
        // A 3-byte char straddling the boundary is cut, not corrupted.
        let mixed = format!("{}{}", "a".repeat(MAX_SUMMARY_BYTES - 1), "日");
        let (summary, truncated) = truncate_utf8(&mixed, MAX_SUMMARY_BYTES);
        assert!(truncated);
        assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn aborted_send_future_leaves_no_stranded_queued_run() {
        let (manager, client) = manager_with(StartBehavior::DelayThenOk("exec-1".into(), 5_000));
        let manager = Arc::new(manager);
        let task = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .start_run(
                        "contract-review",
                        "legal.review_contract",
                        "sha256:x",
                        &input(),
                    )
                    .await
            }
        });
        // Wait until the request is in flight, then drop the future.
        while client.start_calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        task.abort();
        let _ = task.await;
        // The synchronous drop guard must have flipped the unbound run to
        // the permanent local terminal — never a stranded `queued`.
        let runs = manager.run_status(None).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::OutcomeUnknown);
        assert!(runs[0].execution_id.is_none());
        assert_eq!(runs[0].last_error, Some("agentfield.outcome_unknown"));
        assert!(runs[0].summary.as_deref().unwrap().contains("interrupted"));
        // The stranded run is terminal, so it consumes no active capacity.
        let inner = manager.inner.lock().expect("lock");
        let nonterminal = inner
            .runs
            .iter()
            .filter(|run| !run.status.is_terminal())
            .count();
        assert_eq!(nonterminal, 0);
    }

    #[test]
    fn redaction_removes_credential_values_from_remote_text() {
        let cases = [
            // Round-3 (AC-10): any `*authorization` header, any scheme, any
            // parameter layout — the whole header value is redacted.
            (
                "Authorization: Bearer QA_SUPER_SECRET",
                "Authorization: [redacted]",
            ),
            (
                "Authorization: Basic QA_SUPER_SECRET",
                "Authorization: [redacted]",
            ),
            (
                "authorization: digest realm=x, nonce=QA_SECRET, ok",
                "authorization: [redacted]",
            ),
            (
                "AUTHORIZATION: Token QA_SUPER_SECRET",
                "AUTHORIZATION: [redacted]",
            ),
            (
                "Authorization: AWS4-HMAC-SHA256 Credential=QA_SECRET",
                "Authorization: [redacted]",
            ),
            // Unrecognized scheme or bare credential: fail safe.
            (
                "Authorization: WeirdScheme QA_SUPER_SECRET tail",
                "Authorization: [redacted]",
            ),
            (
                "authorization: QA_SUPER_SECRET",
                "authorization: [redacted]",
            ),
            // Compound header names trigger too.
            (
                "Proxy-Authorization: Basic QA_SUPER_SECRET",
                "Proxy-Authorization: [redacted]",
            ),
            ("x-api-key: QA_SUPER_SECRET", "x-api-key: [redacted]"),
            // Content after the line survives.
            (
                "Authorization: Basic QA_SUPER_SECRET\nstatus: ok",
                "Authorization: [redacted]\nstatus: ok",
            ),
            // JSON-embedded shape: quoted values redact to the closing
            // quote, which survives.
            (
                "\"authorization\": \"Bearer QA_SUPER_SECRET\"",
                "\"authorization\": \"[redacted]\"",
            ),
            // Round-4 (acceptance P1-2): folded header layout.
            (
                "Authorization:\r\n Basic QA_FOLDED_SECRET\r\nstatus: failed",
                "Authorization:\r\n[redacted]\r\nstatus: failed",
            ),
            // Round-4 (acceptance P1-2): quoted value containing whitespace.
            (
                "token=\"QA_FIRST_SECRET QA_SECOND_SECRET\"",
                "token=\"[redacted]\"",
            ),
            // Quoted value without a closer fails safe to end-of-text.
            ("token=\"QA_OPEN_SECRET", "token=\"[redacted]"),
            // Prose mentioning authorization without an assignment shape is
            // not credential material and survives.
            (
                "the authorization is handled by the gateway",
                "the authorization is handled by the gateway",
            ),
            ("Bearer sk-123", "Bearer [redacted]"),
            ("token=abc123", "token=[redacted]"),
            ("secret: hunter2", "secret: [redacted]"),
            ("password=\"p@ss\"", "password=\"[redacted]\""),
            ("api_key=KEY-1; ok", "api_key=[redacted]; ok"),
            ("no credentials in this text", "no credentials in this text"),
            ("the tokens are many", "the tokens are many"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), expected, "input: {input}");
        }
        assert!(
            !redact_secrets("Authorization: Bearer QA_SUPER_SECRET").contains("QA_SUPER_SECRET")
        );
        // UTF-8 safety: multibyte content survives.
        assert!(redact_secrets("结果: token=abc 密钥值保留").contains("密钥值保留"));
    }

    #[tokio::test]
    async fn completed_result_is_redacted_before_entering_the_summary() {
        let (manager, client) = manager_with(StartBehavior::Ok("exec-1".into()));
        let run = manager
            .start_run(
                "contract-review",
                "legal.review_contract",
                "sha256:x",
                &input(),
            )
            .await
            .unwrap();
        // Malicious remote result carrying a credential.
        let malicious = StatusEnvelope::decode(&json!({
            "execution_id": "exec-1",
            "status": "completed",
            "run_id": "run-1",
            "started_at": "2026-09-17T00:00:01Z",
            "webhook_registered": false,
            "result": "done. Authorization: Bearer QA_SUPER_SECRET",
        }))
        .unwrap();
        client.status_calls.fetch_add(1, Ordering::SeqCst);
        let mut inner = manager.inner.lock().expect("lock");
        let run = inner
            .runs
            .iter_mut()
            .find(|r| r.run_id == run.run_id)
            .unwrap();
        apply_remote_status(run, RunStatus::Completed, &malicious);
        assert_eq!(run.status, RunStatus::Completed);
        let summary = run.summary.as_deref().unwrap();
        assert!(summary.contains("done."));
        assert!(!summary.contains("QA_SUPER_SECRET"), "{summary}");
        assert!(summary.contains("Authorization: [redacted]"), "{summary}");
    }

    #[tokio::test]
    async fn recheck_detects_user_and_project_and_plugin_source_changes() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let temp_cwd = tempfile::TempDir::new().unwrap();
        let user_config = temp_home.path().join("config.json");
        let project_config = temp_cwd.path().join(".lato").join("config.json");
        std::fs::create_dir_all(project_config.parent().unwrap()).unwrap();
        let capability = |alias: &str, target: &str| {
            json!({
                "enabled": true,
                "baseUrl": "https://agents.example.internal",
                "credential": "agentfield:primary",
                "capabilities": {
                    alias: {
                        "target": target,
                        "description": "Review one contract",
                        "inputSchema": {"type":"object"},
                        "risk": "remote_read",
                    }
                }
            })
        };
        std::fs::write(
            &user_config,
            json!({"agentfield": capability("contract-review", "legal.review_contract")})
                .to_string(),
        )
        .unwrap();
        let sources =
            crate::agentfield::catalog::catalog_sources(temp_home.path(), temp_cwd.path());
        let assembled = crate::agentfield::catalog::assemble_catalog_config(&sources).unwrap();
        let catalog = AgentFieldCatalog::from_config(&assembled);
        let fake: Arc<FakeClient> = Arc::new(FakeClient::new(StartBehavior::Ok("exec-1".into())));
        let shared: Arc<dyn AgentFieldClient> = fake.clone();
        let manager = AgentFieldManager::with_factory_and_sources(
            "session-src",
            catalog,
            Some(Arc::new(move || {
                let shared = shared.clone();
                Box::pin(async move { Ok(shared.clone()) })
            })),
            sources,
        );
        assert!(
            manager.recheck_revision_matches(),
            "unchanged sources match"
        );

        // user source: target change ⇒ revision change.
        std::fs::write(
            &user_config,
            json!({"agentfield": capability("contract-review", "legal.review_other")}).to_string(),
        )
        .unwrap();
        assert!(!manager.recheck_revision_matches());
        std::fs::write(
            &user_config,
            json!({"agentfield": capability("contract-review", "legal.review_contract")})
                .to_string(),
        )
        .unwrap();
        assert!(manager.recheck_revision_matches());

        // user source: capability removed (config.json deleted) ⇒ changed.
        std::fs::remove_file(&user_config).unwrap();
        assert!(!manager.recheck_revision_matches());

        // project source: adding a project capability ⇒ changed.
        std::fs::write(
            &user_config,
            json!({"agentfield": capability("contract-review", "legal.review_contract")})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            &project_config,
            json!({"agentfield": capability("alpha-task", "alpha.task")}).to_string(),
        )
        .unwrap();
        assert!(!manager.recheck_revision_matches());
        std::fs::remove_file(&project_config).unwrap();
        assert!(manager.recheck_revision_matches());

        // plugin source (production wiring): the REAL filesystem-backed
        // plugin source observes manifest additions, edits, and removals.
        let plugin_dir = temp_home.path().join("plugins").join("alpha");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let plugin_manifest = plugin_dir.join("plugin.json");
        let plugin_config = || {
            json!({
                "name": "alpha",
                "agentfield": {
                    "enabled": true,
                    "baseUrl": "https://agents.example.internal",
                    "credential": "agentfield:primary",
                    "capabilities": {
                        "plugin-task": {
                            "target": "plugin.task",
                            "description": "Plugin task",
                            "inputSchema": {"type":"object"},
                            "risk": "remote_read",
                        }
                    }
                }
            })
        };
        std::fs::write(&plugin_manifest, plugin_config().to_string()).unwrap();
        assert!(
            !manager.recheck_revision_matches(),
            "plugin contribution changes the revision"
        );
        std::fs::write(&plugin_manifest, json!({"name": "alpha"}).to_string()).unwrap();
        assert!(
            manager.recheck_revision_matches(),
            "manifest without an agentfield stanza contributes nothing again"
        );
        std::fs::write(&plugin_manifest, "{not json").unwrap();
        assert!(
            !manager.recheck_revision_matches(),
            "malformed plugin manifest fails closed"
        );
        // Round-5 (acceptance P1): a directory-shaped manifest is
        // present-but-unusable and fails closed on the recheck path too.
        std::fs::remove_file(&plugin_manifest).unwrap();
        std::fs::create_dir_all(plugin_dir.join("plugin.json")).unwrap();
        assert!(
            !manager.recheck_revision_matches(),
            "directory-shaped plugin manifest fails closed"
        );
        std::fs::remove_dir(plugin_dir.join("plugin.json")).unwrap();
        assert!(
            manager.recheck_revision_matches(),
            "removing the unusable manifest restores the frozen revision"
        );
        std::fs::write(&plugin_manifest, plugin_config().to_string()).unwrap();
        assert!(!manager.recheck_revision_matches());
        std::fs::remove_file(&plugin_manifest).unwrap();
        assert!(
            manager.recheck_revision_matches(),
            "plugin removal restores the frozen revision"
        );

        // Two plugins disagreeing on origin fail closed.
        std::fs::write(&plugin_manifest, plugin_config().to_string()).unwrap();
        let plugin_dir_b = temp_home.path().join("plugins").join("beta");
        std::fs::create_dir_all(&plugin_dir_b).unwrap();
        std::fs::write(
            plugin_dir_b.join("plugin.json"),
            json!({
                "name": "beta",
                "agentfield": {
                    "enabled": true,
                    "baseUrl": "https://other.example.internal",
                    "credential": "agentfield:primary",
                    "capabilities": {}
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            !manager.recheck_revision_matches(),
            "disagreeing plugin origins fail closed"
        );
        std::fs::remove_file(plugin_dir_b.join("plugin.json")).unwrap();
        std::fs::remove_file(&plugin_manifest).unwrap();
        assert!(
            manager.recheck_revision_matches(),
            "all plugin contributions removed restores the frozen revision"
        );

        // Disagreeing origins fail closed.
        let disagree = json!({
            "enabled": true,
            "baseUrl": "https://other.example.internal",
            "credential": "agentfield:primary",
            "capabilities": {}
        });
        std::fs::write(&project_config, json!({"agentfield": disagree}).to_string()).unwrap();
        let sources =
            crate::agentfield::catalog::catalog_sources(temp_home.path(), temp_cwd.path());
        assert!(crate::agentfield::catalog::assemble_catalog_config(&sources).is_none());
    }
}
