use lato_core::{ApprovalFingerprint, ExecutionGrant, GrantId, SandboxObligation};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval grant has expired")]
    Expired,
    #[error("approval grant does not match the prepared call")]
    Mismatch,
    #[error("approval grant has already been consumed or does not exist")]
    ConsumedOrMissing,
    #[error("approval ledger is unavailable")]
    Unavailable,
}

impl ApprovalError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Expired => "policy.grant_expired",
            Self::Mismatch => "policy.grant_mismatch",
            Self::ConsumedOrMissing => "policy.grant_consumed",
            Self::Unavailable => "policy.unavailable",
        }
    }
}

#[derive(Debug)]
struct GrantRecord {
    fingerprint: ApprovalFingerprint,
    sandbox: SandboxObligation,
    expires_at: Instant,
}

#[derive(Debug)]
pub struct ApprovalLedger {
    next_id: AtomicU64,
    grants: Mutex<HashMap<GrantId, GrantRecord>>,
    ttl: Duration,
}

impl ApprovalLedger {
    pub fn new(ttl: Duration) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            grants: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn issue(
        &self,
        fingerprint: ApprovalFingerprint,
        sandbox: SandboxObligation,
    ) -> Result<ExecutionGrant, ApprovalError> {
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map(GrantId)
            .map_err(|_| ApprovalError::Unavailable)?;
        let expires_at = Instant::now()
            .checked_add(self.ttl)
            .ok_or(ApprovalError::Unavailable)?;
        let record = GrantRecord {
            fingerprint: fingerprint.clone(),
            sandbox: sandbox.clone(),
            expires_at,
        };
        self.grants
            .lock()
            .map_err(|_| ApprovalError::Unavailable)?
            .insert(id, record);
        Ok(ExecutionGrant {
            id,
            fingerprint,
            sandbox,
        })
    }

    pub fn consume(
        &self,
        grant: &ExecutionGrant,
        expected: &ApprovalFingerprint,
    ) -> Result<(), ApprovalError> {
        let mut grants = self.grants.lock().map_err(|_| ApprovalError::Unavailable)?;
        let record = grants
            .remove(&grant.id)
            .ok_or(ApprovalError::ConsumedOrMissing)?;

        if Instant::now() >= record.expires_at {
            return Err(ApprovalError::Expired);
        }
        if &record.fingerprint != expected
            || grant.fingerprint != record.fingerprint
            || grant.sandbox != record.sandbox
        {
            return Err(ApprovalError::Mismatch);
        }
        Ok(())
    }
}
