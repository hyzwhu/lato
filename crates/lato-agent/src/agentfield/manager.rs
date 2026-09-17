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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Session-scoped run manager. All mutating transitions serialize on
/// `inner`; the remote send happens outside the lock so one slow request
/// never blocks state reads, but capacity is consumed inside the lock.
pub struct AgentFieldManager {
    session_id: String,
    catalog: AgentFieldCatalog,
    factory: Option<ClientFactory>,
    client: OnceCell<Arc<dyn AgentFieldClient>>,
    probe_cache: Mutex<Option<(Instant, AgentFieldHealthSnapshot)>>,
    inner: Mutex<Inner>,
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
    ) -> Self {
        Self::with_factory(
            session_id,
            catalog,
            Some(production_client_factory(credential, config)),
        )
    }

    /// Test/injection constructor: `None` factory means every client use
    /// fails closed with `agentfield.unavailable`.
    pub fn with_factory(
        session_id: &str,
        catalog: AgentFieldCatalog,
        factory: Option<ClientFactory>,
    ) -> Self {
        Self {
            session_id: session_id.to_owned(),
            catalog,
            factory,
            client: OnceCell::new(),
            probe_cache: Mutex::new(None),
            inner: Mutex::new(Inner { runs: Vec::new() }),
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
        let input_digest = digest_hex(&canonical_input(input));
        let run_id = {
            let mut inner = self.inner.lock().await;
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

        // Obtain the client BEFORE the send decision: a client failure means
        // NO request was attempted, so the reservation rolls back and the
        // stable error surfaces with zero run residue.
        let client = match self.client().await {
            Ok(client) => client,
            Err(error) => {
                self.inner
                    .lock()
                    .await
                    .runs
                    .retain(|run| run.run_id != run_id);
                return Err(error);
            }
        };

        // Exactly one send attempt. Everything below is post-decision.
        match client.start_async(execute_target, input).await {
            Ok(envelope) => {
                if envelope.execution_id.is_empty() {
                    return Ok(self.mark_outcome_unknown(&run_id).await);
                }
                let mut inner = self.inner.lock().await;
                if let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) {
                    run.execution_id = Some(envelope.execution_id);
                    run.status = RunStatus::Queued;
                    run.updated_at_ms = epoch_ms();
                    Ok(run.clone())
                } else {
                    // The reservation cannot vanish while nonterminal.
                    Err(ManagerError::NotFound)
                }
            }
            Err(error) => self.finish_failed_send(&run_id, error).await,
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
        let mut inner = self.inner.lock().await;
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
        let mut inner = self.inner.lock().await;
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
            let inner = self.inner.lock().await;
            let runs: Vec<RunState> = inner.runs.iter().rev().take(20).cloned().collect();
            return Ok(runs);
        };
        let execution_id = {
            let inner = self.inner.lock().await;
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
            let inner = self.inner.lock().await;
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
                let mut inner = self.inner.lock().await;
                let Some(run) = inner.runs.iter_mut().find(|run| run.run_id == run_id) else {
                    return Err(ManagerError::NotFound);
                };
                apply_remote_status(run, status, &envelope);
                Ok(vec![run.clone()])
            }
            Err(AgentFieldError::Unavailable(_)) => {
                // Control plane temporarily unreachable: not terminal, the
                // last known state is preserved (spec §7.3).
                let mut inner = self.inner.lock().await;
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
            let inner = self.inner.lock().await;
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
            let inner = self.inner.lock().await;
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
                let mut inner = self.inner.lock().await;
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
                let inner = self.inner.lock().await;
                let run = inner
                    .runs
                    .iter()
                    .find(|run| run.run_id == run_id)
                    .ok_or(ManagerError::NotFound)?;
                Ok((run.clone(), CancelOutcome::AlreadyTerminal))
            }
            Err(error) => {
                let mut inner = self.inner.lock().await;
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
        let inner = self.inner.lock().await;
        inner.runs.iter().find(|run| run.run_id == run_id).cloned()
    }

    pub async fn run_count(&self) -> usize {
        self.inner.lock().await.runs.len()
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
                let (summary, truncated) = truncate_utf8(result, MAX_SUMMARY_BYTES);
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
                let (summary, truncated) = truncate_utf8(detail, MAX_SUMMARY_BYTES);
                run.summary = Some(summary);
                run.summary_truncated = truncated;
            }
        }
        _ => {}
    }
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
            *manager.inner.lock().await.runs.last_mut().unwrap() = {
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
            let mut inner = manager.inner.lock().await;
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
}
