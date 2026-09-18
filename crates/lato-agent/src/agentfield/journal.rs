// Phase 7C3: AgentField journal plumbing.
//
// The session journal is written by exactly one writer: the SessionLoop
// (single sequence owner). The manager, host helpers, and HTTP futures
// NEVER append directly — they submit semantic events through the
// [`AgentFieldJournalSink`] implemented here, which routes them into the
// SessionLoop as `Command::RecordAgentFieldEvent` and awaits the durable
// commit result. An append that fails is surfaced as
// [`JournalSinkError`] with a stable `journal.*` code: callers fail
// closed and never claim a persistence that did not happen.

use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};

use futures_util::future::BoxFuture;
use lato_core::AgentFieldJournalEvent;

/// Stable error for a failed journal append. `code` mirrors the frozen
/// `journal.*` family so callers map it onto
/// `agentfield.journal_unavailable` without re-deriving anything.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct JournalSinkError {
    pub code: &'static str,
    pub message: String,
}

impl JournalSinkError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "journal.unavailable",
            message: message.into(),
        }
    }
}

/// Durable AgentField journal sink. One implementation per process: the
/// production one wraps the live `SessionHandle`; tests use
/// [`MemoryAgentFieldJournal`]. The returned future resolves only after
/// the SessionLoop durably committed the event (SyncData durability).
pub trait AgentFieldJournalSink: Send + Sync {
    fn append(
        &self,
        event: AgentFieldJournalEvent,
    ) -> BoxFuture<'static, Result<(), JournalSinkError>>;
}

/// In-memory sink for tests and diagnostics. Records every event in order
/// and can be armed to fail (simulating journal I/O errors) — the failure
/// injection is what the crash-consistency tests assert against.
#[derive(Default)]
pub struct MemoryAgentFieldJournal {
    events: StdMutex<Vec<AgentFieldJournalEvent>>,
    fail: AtomicBool,
}

impl MemoryAgentFieldJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm/disarm deterministic append failures (fail-closed simulation).
    pub fn set_failing(&self, failing: bool) {
        self.fail.store(failing, Ordering::SeqCst);
    }

    pub fn events(&self) -> Vec<AgentFieldJournalEvent> {
        self.events.lock().expect("memory journal lock").clone()
    }

    pub fn event_count(&self) -> usize {
        self.events.lock().expect("memory journal lock").len()
    }
}

impl AgentFieldJournalSink for MemoryAgentFieldJournal {
    fn append(
        &self,
        event: AgentFieldJournalEvent,
    ) -> BoxFuture<'static, Result<(), JournalSinkError>> {
        if self.fail.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(JournalSinkError::unavailable("injected journal failure"))
            });
        }
        self.events.lock().expect("memory journal lock").push(event);
        Box::pin(async { Ok(()) })
    }
}

/// Convenience constructor for a shared memory sink.
pub fn memory_journal() -> Arc<MemoryAgentFieldJournal> {
    Arc::new(MemoryAgentFieldJournal::new())
}
