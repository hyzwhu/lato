//! Plan-mode session runtime (Phase 8B, frozen spec v1.0).
//!
//! Owns the per-session [`PlanApproval`] lifecycle: creation, unique
//! generation, revocation, and every phase transition. All edges are driven by
//! trusted human command paths or the mutation guard — there is no model-
//! reachable API that can drive a transition (spec §5).
//!
//! Locking contract (correction #2): every method acquires the plan-state
//! mutex exactly once and never holds it across an `await` on external work.
//! The two TOCTOU checks each perform "read state → bounded read + hash →
//! compare → revoke if stale" atomically under that single acquisition.

use lato_core::{
    PLAN_APPROVAL_STALE_CODE, PLAN_DRAFT_MAX_BYTES, PLAN_FILE_NAME, PlanCommand, PlanPhase,
    plan_transition,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Mutex;

/// Session-local record that a human approved a specific plan content hash.
/// It is NOT a policy `ExecutionGrant` and never authorizes any tool call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanApproval {
    pub activation: u64,
    pub generation: u64,
    pub content_hash: String,
    pub approver: String,
    pub approved_at_ms: u64,
}

/// Snapshot for `/plan status` and ACP `lato/plan/status`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanStatus {
    pub phase: PlanPhase,
    pub activation: u64,
    pub plan_path: PathBuf,
    pub last_draft_hash: Option<String>,
    pub approval: Option<PlanApproval>,
    /// True when at least one `plan_draft` tool call completed successfully
    /// during THIS activation. Headless exit semantics depend on this event,
    /// never on whether an old `plan.md` happens to exist on disk.
    pub draft_published: bool,
}

/// Errors surfaced to the human command path. The plan state is always left
/// unchanged on `Err`.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanModeError {
    #[error("plan mode is already active in this session")]
    AlreadyActive,
    #[error("this command is not allowed while a turn is in flight")]
    TurnInFlight,
    #[error("this command is not allowed in plan phase {0:?}")]
    IllegalEdge(PlanPhase),
    #[error("the plan draft is unreadable or oversized")]
    DraftUnreadable,
}

/// Identifies the approval generation revoked by a failed TOCTOU check, for
/// the durable `PlanApprovalRevoked` journal trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanApprovalRevocation {
    pub activation: u64,
    pub generation: u64,
}

/// Outcome of the first (pre-prepare) TOCTOU check for a mutation call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationPreflight {
    /// No active approval: the call proceeds through ordinary policy only.
    NotGuarded,
    /// Approval verified at this instant; the captured triple must be
    /// presented again to [`PlanModeRuntime::complete_mutation_preflight`]
    /// immediately before the grant is consumed.
    Guarded {
        activation: u64,
        generation: u64,
        content_hash: String,
    },
    /// The approval is stale or the draft is unreadable: the call is denied,
    /// the generation is revoked, and the session transitions to `Revising`.
    Denied {
        code: &'static str,
        revocation: PlanApprovalRevocation,
    },
}

struct PlanInner {
    phase: PlanPhase,
    activation: u64,
    approval: Option<PlanApproval>,
    last_draft_hash: Option<String>,
    draft_published: bool,
}

/// Per-session Plan-mode state machine, plan file path, and overlay flag.
pub struct PlanModeRuntime {
    inner: Mutex<PlanInner>,
    plan_path: PathBuf,
    plan_flag: Arc<AtomicBool>,
    next_generation: AtomicU64,
    next_activation: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

/// Bounded read of the plan draft. `Ok(None)` means the file does not exist.
/// Oversized files fail closed with `Err(())` — they are never truncated.
fn bounded_read(path: &Path) -> Result<Option<String>, ()> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(());
            }
            if metadata.len() as usize > PLAN_DRAFT_MAX_BYTES {
                return Err(());
            }
        }
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > PLAN_DRAFT_MAX_BYTES {
                return Err(());
            }
            String::from_utf8(bytes)
                .map(|text| Some(sha256_hex(text.as_bytes())))
                .map_err(|_| ())
        }
        Err(_) => Err(()),
    }
}

impl PlanModeRuntime {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            inner: Mutex::new(PlanInner {
                phase: PlanPhase::Inactive,
                activation: 0,
                approval: None,
                last_draft_hash: None,
                draft_published: false,
            }),
            plan_path: workspace_root.join(PLAN_FILE_NAME),
            plan_flag: Arc::new(AtomicBool::new(false)),
            next_generation: AtomicU64::new(0),
            next_activation: AtomicU64::new(0),
        }
    }

    /// Shared overlay flag consumed by the policy engine and catalog filter.
    pub fn plan_flag(&self) -> Arc<AtomicBool> {
        self.plan_flag.clone()
    }

    pub fn plan_path(&self) -> &Path {
        &self.plan_path
    }

    fn set_flag(&self, active: bool) {
        self.plan_flag.store(active, Ordering::Release);
    }

    /// Applies a human command (or integrity edge) and returns the transition.
    /// Journal persistence is the caller's responsibility.
    async fn apply(
        &self,
        command: PlanCommand,
    ) -> Result<(u64, PlanPhase, PlanPhase), PlanModeError> {
        let mut inner = self.inner.lock().await;
        let from = inner.phase;
        let Some(to) = plan_transition(from, command) else {
            return Err(PlanModeError::IllegalEdge(from));
        };
        inner.phase = to;
        match command {
            PlanCommand::Enter => {
                inner.activation = self.next_activation.fetch_add(1, Ordering::SeqCst) + 1;
                // A new activation never reuses an old approval.
                inner.approval = None;
                inner.last_draft_hash = None;
                // A new activation starts with no successful draft
                // publication of its own.
                inner.draft_published = false;
            }
            // A revision request withdraws any recorded approval.
            PlanCommand::Revise => inner.approval = None,
            _ => {}
        }
        // Entering (or re-entering) Plan mode loads an existing, readable
        // plan.md as the starting draft of this activation: the draft hash is
        // recorded for `/plan status` and the file itself stays on disk for
        // the model to continue from. Unreadable or oversized leftovers are
        // ignored here (they fail closed later, at submit/approve).
        if matches!(command, PlanCommand::Enter) && to.expects_draft() {
            inner.last_draft_hash = bounded_read(&self.plan_path).ok().flatten();
        }
        let activation = inner.activation;
        drop(inner);
        self.set_flag(to.plan_mode_active());
        Ok((activation, from, to))
    }

    /// `/plan` — start a new activation. Refused while a turn is in flight.
    pub async fn enter(
        &self,
        turn_in_flight: bool,
    ) -> Result<(u64, PlanPhase, PlanPhase), PlanModeError> {
        if turn_in_flight {
            return Err(PlanModeError::TurnInFlight);
        }
        self.apply(PlanCommand::Enter).await
    }

    /// `/plan exit` — leave Plan mode; the plan file stays on disk.
    pub async fn exit(&self) -> Result<(u64, PlanPhase, PlanPhase), PlanModeError> {
        self.apply(PlanCommand::Exit).await
    }

    /// `/plan submit` — the sole `Drafting|Revising → AwaitingApproval` edge.
    /// Fails closed when the draft is unreadable or oversized.
    pub async fn submit(&self) -> Result<(u64, PlanPhase, PlanPhase), PlanModeError> {
        let mut inner = self.inner.lock().await;
        let from = inner.phase;
        let Some(to) = plan_transition(from, PlanCommand::Submit) else {
            return Err(PlanModeError::IllegalEdge(from));
        };
        match bounded_read(&self.plan_path) {
            Ok(hash) => inner.last_draft_hash = hash,
            Err(()) => return Err(PlanModeError::DraftUnreadable),
        }
        inner.phase = to;
        let activation = inner.activation;
        drop(inner);
        self.set_flag(true);
        Ok((activation, from, to))
    }

    /// `/plan approve` — human-only approval; records a fresh `PlanApproval`
    /// with a unique generation bound to the current file hash.
    pub async fn approve(&self, approver: &str) -> Result<PlanApproval, PlanModeError> {
        let mut inner = self.inner.lock().await;
        if plan_transition(inner.phase, PlanCommand::Approve).is_none() {
            return Err(PlanModeError::IllegalEdge(inner.phase));
        }
        let content_hash = match bounded_read(&self.plan_path) {
            Ok(Some(hash)) => hash,
            _ => return Err(PlanModeError::DraftUnreadable),
        };
        let approval = PlanApproval {
            activation: inner.activation,
            generation: self.next_generation.fetch_add(1, Ordering::SeqCst) + 1,
            content_hash: content_hash.clone(),
            approver: approver.to_owned(),
            approved_at_ms: now_ms(),
        };
        inner.approval = Some(approval.clone());
        inner.phase = PlanPhase::Approved;
        drop(inner);
        // Approval is the transition out of the read-only overlay (spec §5):
        // the session continues in its normal trust mode; the TOCTOU guard —
        // not the overlay — watches the plan file from here on.
        self.set_flag(false);
        Ok(approval)
    }

    /// User requests edits instead of approving (`AwaitingApproval/Approved →
    /// Revising`). Drafting/Revising stay unchanged as Revising.
    pub async fn request_revision(&self) -> Result<(u64, PlanPhase, PlanPhase), PlanModeError> {
        self.apply(PlanCommand::Revise).await
    }

    /// `/plan status` snapshot.
    pub async fn status(&self) -> PlanStatus {
        let inner = self.inner.lock().await;
        PlanStatus {
            phase: inner.phase,
            activation: inner.activation,
            plan_path: self.plan_path.clone(),
            last_draft_hash: inner.last_draft_hash.clone(),
            approval: inner.approval.clone(),
            draft_published: inner.draft_published,
        }
    }

    /// Records that a `plan_draft` tool call completed successfully in this
    /// session. Called by the session actor on the real tool completion
    /// event; the headless exit code is derived from this, never from a
    /// stale `plan.md` on disk.
    pub async fn record_draft_published(&self) {
        let mut inner = self.inner.lock().await;
        inner.draft_published = true;
    }

    /// First TOCTOU check, run before ordinary policy preparation of a
    /// mutation-capable call. One lock acquisition; no awaits inside.
    pub async fn begin_mutation_preflight(&self) -> MutationPreflight {
        let mut inner = self.inner.lock().await;
        if inner.phase != PlanPhase::Approved {
            return MutationPreflight::NotGuarded;
        }
        let Some(approval) = inner.approval.as_ref() else {
            return MutationPreflight::NotGuarded;
        };
        let activation = approval.activation;
        let generation = approval.generation;
        let approved_hash = approval.content_hash.clone();
        match bounded_read(&self.plan_path) {
            Ok(Some(hash)) if hash == approved_hash => MutationPreflight::Guarded {
                activation,
                generation,
                content_hash: approved_hash,
            },
            // Changed, unreadable, oversized, or deleted draft: revoke.
            _ => {
                inner.phase = PlanPhase::Revising;
                inner.approval = None;
                drop(inner);
                self.set_flag(true); // Revising re-engages the read-only overlay.
                MutationPreflight::Denied {
                    code: PLAN_APPROVAL_STALE_CODE,
                    revocation: PlanApprovalRevocation {
                        activation,
                        generation,
                    },
                }
            }
        }
    }

    /// Second TOCTOU check, run immediately before the call's execution grant
    /// is consumed. Verifies the session is still `Approved`, the activation
    /// and generation are unchanged (even if the bytes were restored), and the
    /// hash matches. On failure the approval is revoked and the revocation is
    /// returned for the durable journal trace.
    pub async fn complete_mutation_preflight(
        &self,
        activation: u64,
        generation: u64,
        content_hash: &str,
    ) -> Result<(), PlanApprovalRevocation> {
        let mut inner = self.inner.lock().await;
        let still_valid = inner.phase == PlanPhase::Approved
            && inner.approval.as_ref().is_some_and(|approval| {
                approval.activation == activation && approval.generation == generation
            })
            && match bounded_read(&self.plan_path) {
                Ok(Some(hash)) => hash == content_hash,
                _ => false,
            };
        if still_valid {
            return Ok(());
        }
        if inner.phase == PlanPhase::Approved {
            inner.phase = PlanPhase::Revising;
            inner.approval = None;
            drop(inner);
            self.set_flag(true);
            return Err(PlanApprovalRevocation {
                activation,
                generation,
            });
        }
        Err(PlanApprovalRevocation {
            activation,
            generation,
        })
    }

    /// Stable denial code for a failed second check.
    pub const STALE_CODE: &'static str = PLAN_APPROVAL_STALE_CODE;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lato-plan-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn write_plan(dir: &Path, text: &str) {
        fs::write(dir.join(PLAN_FILE_NAME), text).unwrap();
    }

    #[tokio::test]
    async fn full_lifecycle_produces_unique_generation() {
        let dir = temp_workspace("lifecycle");
        let runtime = PlanModeRuntime::new(&dir);
        assert_eq!(runtime.status().await.phase, PlanPhase::Inactive);

        // Submit/approve/revision are illegal before entering.
        assert!(matches!(
            runtime.submit().await,
            Err(PlanModeError::IllegalEdge(PlanPhase::Inactive))
        ));
        assert!(matches!(
            runtime.approve("user").await,
            Err(PlanModeError::IllegalEdge(PlanPhase::Inactive))
        ));

        runtime.enter(false).await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Drafting);
        assert!(runtime.plan_flag().load(Ordering::SeqCst));

        // Approve before submit is an illegal edge.
        assert!(matches!(
            runtime.approve("user").await,
            Err(PlanModeError::IllegalEdge(PlanPhase::Drafting))
        ));

        write_plan(&dir, "# plan").await;
        runtime.submit().await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::AwaitingApproval);

        // Approve is idempotent-free: second approve in Approved is illegal.
        let approval = runtime.approve("user").await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Approved);
        assert_eq!(approval.content_hash, sha256_hex(b"# plan"));
        let status = runtime.status().await;
        assert_eq!(
            status.approval.as_ref().unwrap().generation,
            approval.generation
        );
        runtime.exit().await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Exited);
        assert!(!runtime.plan_flag().load(Ordering::SeqCst));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn re_entering_creates_new_activation_without_reusing_approval() {
        let dir = temp_workspace("reactivation");
        let runtime = PlanModeRuntime::new(&dir);
        write_plan(&dir, "v1").await;
        runtime.enter(false).await.unwrap();
        runtime.submit().await.unwrap();
        let first = runtime.approve("user").await.unwrap();
        runtime.exit().await.unwrap();

        runtime.enter(false).await.unwrap();
        let status = runtime.status().await;
        assert_eq!(status.phase, PlanPhase::Drafting);
        assert_eq!(status.activation, first.activation + 1);
        assert!(
            status.approval.is_none(),
            "old approval must not survive re-entry"
        );

        // Old generation can never be completed again.
        assert!(
            runtime
                .complete_mutation_preflight(
                    first.activation,
                    first.generation,
                    &first.content_hash
                )
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_detection_revokes_and_prevents_replay() {
        let dir = temp_workspace("stale");
        let runtime = PlanModeRuntime::new(&dir);
        write_plan(&dir, "approved contents").await;
        runtime.enter(false).await.unwrap();
        runtime.submit().await.unwrap();
        let approval = runtime.approve("user").await.unwrap();

        // Unchanged file: guarded.
        match runtime.begin_mutation_preflight().await {
            MutationPreflight::Guarded {
                activation,
                generation,
                content_hash,
            } => {
                assert_eq!(generation, approval.generation);
                // Second check passes while unchanged.
                runtime
                    .complete_mutation_preflight(activation, generation, &content_hash)
                    .await
                    .unwrap();
            }
            other => panic!("expected guarded, got {other:?}"),
        }

        // Mutate between checks: the second check must deny and revoke.
        let guarded = match runtime.begin_mutation_preflight().await {
            MutationPreflight::Guarded {
                activation,
                generation,
                content_hash,
            } => (activation, generation, content_hash),
            other => panic!("expected guarded, got {other:?}"),
        };
        write_plan(&dir, "mutated contents").await;
        assert!(
            runtime
                .complete_mutation_preflight(guarded.0, guarded.1, &guarded.2)
                .await
                .is_err()
        );
        assert_eq!(runtime.status().await.phase, PlanPhase::Revising);

        // Restoring identical bytes never revives the old approval.
        write_plan(&dir, "approved contents").await;
        match runtime.begin_mutation_preflight().await {
            MutationPreflight::NotGuarded => {}
            other => panic!("expected not guarded after revocation, got {other:?}"),
        }
        assert!(
            runtime
                .complete_mutation_preflight(
                    approval.activation,
                    approval.generation,
                    &approval.content_hash
                )
                .await
                .is_err()
        );

        // Recovery requires a fresh submit + approve.
        runtime.submit().await.unwrap();
        let second = runtime.approve("user").await.unwrap();
        assert_ne!(second.generation, approval.generation);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unreadable_or_oversized_drafts_fail_closed() {
        let dir = temp_workspace("failclosed");
        let runtime = PlanModeRuntime::new(&dir);
        runtime.enter(false).await.unwrap();

        // A missing draft may be submitted (a human choice), but approval
        // still fails closed until a readable draft exists.
        runtime.submit().await.unwrap();
        assert_eq!(
            runtime.approve("user").await,
            Err(PlanModeError::DraftUnreadable)
        );
        runtime.request_revision().await.unwrap();

        // Oversized draft (131,073 bytes) is rejected, not truncated.
        fs::write(
            dir.join(PLAN_FILE_NAME),
            vec![b'a'; PLAN_DRAFT_MAX_BYTES + 1],
        )
        .unwrap();
        assert_eq!(runtime.submit().await, Err(PlanModeError::DraftUnreadable));

        // Exactly 131,072 bytes submits fine.
        fs::write(dir.join(PLAN_FILE_NAME), vec![b'a'; PLAN_DRAFT_MAX_BYTES]).unwrap();
        runtime.submit().await.unwrap();

        // A directory in place of the plan file is unreadable: approve fails
        // closed and stays in AwaitingApproval.
        fs::remove_file(dir.join(PLAN_FILE_NAME)).unwrap();
        fs::create_dir(dir.join(PLAN_FILE_NAME)).unwrap();
        assert_eq!(
            runtime.approve("user").await,
            Err(PlanModeError::DraftUnreadable)
        );
        assert_eq!(runtime.status().await.phase, PlanPhase::AwaitingApproval);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preflight_denies_unreadable_plan_after_approval() {
        let dir = temp_workspace("preflight-unreadable");
        let runtime = PlanModeRuntime::new(&dir);
        write_plan(&dir, "hello").await;
        runtime.enter(false).await.unwrap();
        runtime.submit().await.unwrap();
        runtime.approve("user").await.unwrap();

        // Replace the file with a symlink: metadata is non-regular → stale.
        fs::remove_file(dir.join(PLAN_FILE_NAME)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", dir.join(PLAN_FILE_NAME)).unwrap();
        #[cfg(not(unix))]
        fs::write(dir.join(PLAN_FILE_NAME), "other").unwrap();

        match runtime.begin_mutation_preflight().await {
            MutationPreflight::Denied { code, .. } => {
                assert_eq!(code, PlanModeRuntime::STALE_CODE)
            }
            other => panic!("expected denied, got {other:?}"),
        }
        assert_eq!(runtime.status().await.phase, PlanPhase::Revising);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn entering_plan_mode_loads_an_existing_draft_as_the_starting_point() {
        let dir = temp_workspace("starting-draft");
        let runtime = PlanModeRuntime::new(&dir);

        // With an existing readable draft: entering records its hash as the
        // starting point and the file stays on disk untouched.
        write_plan(&dir, "# carried-over plan").await;
        runtime.enter(false).await.unwrap();
        let status = runtime.status().await;
        assert_eq!(status.phase, PlanPhase::Drafting);
        assert_eq!(
            status.last_draft_hash.as_deref(),
            Some(sha256_hex(b"# carried-over plan").as_str())
        );
        assert_eq!(
            fs::read_to_string(dir.join(PLAN_FILE_NAME)).unwrap(),
            "# carried-over plan"
        );

        // Re-entering after an edit picks up the new content hash.
        runtime.exit().await.unwrap();
        write_plan(&dir, "# carried-over plan v2").await;
        runtime.enter(false).await.unwrap();
        assert_eq!(
            runtime.status().await.last_draft_hash.as_deref(),
            Some(sha256_hex(b"# carried-over plan v2").as_str())
        );

        // Without any draft on disk there is no starting hash, and that does
        // not block entering Plan mode.
        runtime.exit().await.unwrap();
        fs::remove_file(dir.join(PLAN_FILE_NAME)).unwrap();
        runtime.enter(false).await.unwrap();
        assert!(runtime.status().await.last_draft_hash.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plan_state_mutex_is_never_held_across_await_points() {
        let dir = temp_workspace("lock-scope");
        let runtime = Arc::new(PlanModeRuntime::new(&dir));
        write_plan(&dir, "approved contents").await;
        runtime.enter(false).await.unwrap();
        runtime.submit().await.unwrap();
        runtime.approve("user").await.unwrap();

        // Capture a mutation guard; the plan-state mutex must be released by
        // the time begin_mutation_preflight returns, so a concurrent reader
        // (status/preflight from another task) makes progress immediately.
        let guarded = match runtime.begin_mutation_preflight().await {
            MutationPreflight::Guarded {
                activation,
                generation,
                content_hash,
            } => (activation, generation, content_hash),
            other => panic!("expected guarded, got {other:?}"),
        };
        let reader = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                // Both take the same mutex; they must complete well within
                // the timeout because no lock is held across awaits.
                runtime.status().await;
                runtime.begin_mutation_preflight().await
            })
        };
        let concurrent = tokio::time::timeout(std::time::Duration::from_secs(2), reader)
            .await
            .expect("plan mutex must not stay held across awaits")
            .unwrap();
        assert!(matches!(concurrent, MutationPreflight::Guarded { .. }));

        // The original guard is still completable: concurrent readers never
        // invalidated it.
        runtime
            .complete_mutation_preflight(guarded.0, guarded.1, &guarded.2)
            .await
            .unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enter_is_refused_while_turn_in_flight() {
        let dir = temp_workspace("turn-in-flight");
        let runtime = PlanModeRuntime::new(&dir);
        assert_eq!(runtime.enter(true).await, Err(PlanModeError::TurnInFlight));
        assert_eq!(runtime.status().await.phase, PlanPhase::Inactive);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn revision_requests_keep_session_read_only() {
        let dir = temp_workspace("revision");
        let runtime = PlanModeRuntime::new(&dir);
        write_plan(&dir, "draft").await;
        runtime.enter(false).await.unwrap();
        runtime.request_revision().await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Revising);
        runtime.submit().await.unwrap();
        runtime.request_revision().await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Revising);
        runtime.submit().await.unwrap();
        runtime.approve("user").await.unwrap();
        runtime.request_revision().await.unwrap();
        assert_eq!(runtime.status().await.phase, PlanPhase::Revising);
        assert!(
            runtime.status().await.approval.is_none(),
            "revision revokes the approval record"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
