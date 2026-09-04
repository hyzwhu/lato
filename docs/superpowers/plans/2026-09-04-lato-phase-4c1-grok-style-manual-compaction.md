# Lato Phase 4C1 Grok-Style Manual Compaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Grok Build-style, manually triggered `/compact [context]` operation that summarizes the active conversation with the current model and installs the validated replacement through Lato's durable checkpoint-first session store.

**Architecture:** The typed runtime owns a compaction state distinct from foreground turns, persists lifecycle boundaries, invokes an agent-layer compactor on an immutable history snapshot, and reconciles the journal after Phase 4B history replacement. The compactor is tool-free and side-effect-free; the existing session writer remains the only component allowed to write journal, checkpoint, and derived-history files.

**Tech Stack:** Rust 2024, Tokio, async-trait, serde/serde_json, lato-core typed contracts, lato-runtime session actor, lato-agent legacy adapter, lato-store JSONL journal/projection writer, Ratatui TUI, fake model streams and tempfile fixtures.

## Global Constraints

- Follow Grok Build at commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` semantically; do not import memory, telemetry, hooks, transcript segments, todos, or multi-agent state.
- Phase 4C1 enables only manual compaction; threshold, model-switch, preflight-overflow, resubmission, two-pass, and prefire behavior remain disabled.
- Compaction uses the current session model and advertises zero tools with `tool_choice = "none"`.
- A source shorter than 2,000 Unicode scalar values, excluding system and separately re-injected project instructions, returns `compaction.nothing_to_compact` without sampling.
- A cleaned summary shorter than 500 Unicode scalar values is degenerate; a summary larger than 32 KiB is invalid.
- A replacement candidate must serialize at least 20 percent smaller than its source.
- One operation performs at most three cancellation-aware attempts.
- `events.jsonl` remains canonical and append-only; no implementation may truncate, rotate, archive, or delete its valid records.
- Checkpoint, marker, derived history, and metadata writes remain serialized by the existing Phase 4B writer.
- A committed replacement marker makes the checkpoint history authoritative even when derived publication fails.
- Unresolved tool side effects always fail closed and are never hidden or retried by compaction.
- Preserve the user's existing dirty and untracked workspace files.
- Run relevant tests before the repository-required `cargo install --path .` deployment.

---

## File structure

New focused files:

- `crates/lato-core/src/compaction.rs`: IDs, request/result values, trigger/policy/usage contracts, error codes, and size summaries.
- `crates/lato-core/tests/compaction_contract.rs`: stable serde and error-code contracts.
- `crates/lato-agent/src/compaction.rs`: pure history preparation, prompt construction, output normalization/validation, retry loop, and `LegacyTurnDriver` compaction implementation.
- `crates/lato-agent/tests/compaction_runtime.rs`: model-driven compaction and runtime/store integration fixtures.
- `tests/session_compaction_cli.rs`: ACP extension and restart/recovery end-to-end coverage without requiring a terminal.

Existing files with bounded changes:

- `crates/lato-core/src/id.rs`, `command.rs`, `event.rs`, `journal.rs`, `state.rs`, `lib.rs`: expose compaction contracts and state transitions.
- `crates/lato-core/src/projection.rs`: add the combined `SessionStore` trait used by runtime.
- `crates/lato-runtime/src/driver.rs`, `session.rs`, `lib.rs`: add the compaction driver boundary and serialize the operation with turns.
- `crates/lato-agent/src/legacy_driver.rs`, `runtime_session.rs`, `host.rs`, `lib.rs`: bridge history/model access, expose `lato/session/compact`, and map lifecycle updates.
- `crates/lato-protocol/src/methods.rs`: advertise the Lato extension method.
- `src/client.rs`: add interactive-client compact and cancellation routing.
- `src/tui/commands.rs`, `backend.rs`, `state.rs`, `mod.rs`, `render.rs`: parse, execute, cancel, and render manual compaction.
- `crates/lato-store/src/file.rs`, `memory.rs`, `projection.rs`, `writer.rs`: only the minimal lifecycle-projection and test-support changes required by new journal variants.
- `README.md` and `docs/superpowers/reference/lato-upstream-sources.md`: document behavior and provenance.

---

### Task 1: Define typed compaction contracts and state transitions

**Files:**
- Create: `crates/lato-core/src/compaction.rs`
- Create: `crates/lato-core/tests/compaction_contract.rs`
- Modify: `crates/lato-core/src/id.rs`
- Modify: `crates/lato-core/src/command.rs`
- Modify: `crates/lato-core/src/event.rs`
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/src/state.rs`
- Modify: `crates/lato-core/src/lib.rs`

**Interfaces:**
- Produces: `CompactionId`, `CompactSession`, `CompactionTrigger`, `CompactionPolicy`, `ContextUsage`, `CompactionSize`, `CompactionCandidate`, and `CompactionError`.
- Produces: `SessionPhase::Compacting(ActiveCompaction)`, `request_compaction`, `request_compaction_cancel`, and `finish_compaction`.
- Produces: compaction command, event, and journal variants consumed by every later task.

- [ ] **Step 1: Write failing serde and state-machine tests**

Create `crates/lato-core/tests/compaction_contract.rs` with tests shaped exactly around the public contract:

```rust
use lato_core::*;

#[test]
fn compact_command_has_stable_wire_shape() {
    let command = Command::CompactSession(CompactSession {
        user_context: Some("preserve the parser diagnosis".into()),
        trigger: CompactionTrigger::Manual,
    });
    assert_eq!(serde_json::to_value(command).unwrap(), serde_json::json!({
        "type": "compact_session",
        "user_context": "preserve the parser diagnosis",
        "trigger": "manual"
    }));
}

#[test]
fn compaction_ids_reject_empty_values() {
    assert!(CompactionId::parse(" ").is_err());
}

#[test]
fn compaction_error_codes_are_stable() {
    assert_eq!(CompactionError::NothingToCompact.code(), "compaction.nothing_to_compact");
    assert_eq!(CompactionError::AlreadyActive.code(), "compaction.already_active");
    assert_eq!(CompactionError::Cancelled.code(), "compaction.cancelled");
}

#[test]
fn turn_and_compaction_are_mutually_exclusive() {
    let mut machine = SessionMachine::new();
    let cid = CompactionId::from("compact-1");
    machine.request_compaction(cid.clone()).unwrap();
    assert_eq!(
        machine.request_start(TurnId::from("turn-1"), StartBehavior::Reject),
        Err(TransitionError::CompactionAlreadyActive)
    );
    machine.request_compaction_cancel(&cid).unwrap();
    machine.finish_compaction(&cid).unwrap();
    assert!(matches!(machine.phase(), SessionPhase::Idle));
}
```

- [ ] **Step 2: Run the focused tests and verify the contracts are missing**

Run: `cargo test -p lato-core --test compaction_contract`

Expected: compilation fails because the compaction types and state transitions do not exist.

- [ ] **Step 3: Add the core compaction values**

Add `string_id!(CompactionId);` to `id.rs`. Create `compaction.rs` with these exact public shapes:

```rust
use crate::{AgentError, CompactionId, ModelMessage, Retryability};

pub const DEFAULT_COMPACTION_MAX_ATTEMPTS: u8 = 3;
pub const MIN_COMPACTION_SOURCE_CHARS: usize = 2_000;
pub const MIN_COMPACTION_SUMMARY_CHARS: usize = 500;
pub const MAX_COMPACTION_SUMMARY_BYTES: usize = 32 * 1024;
pub const MIN_COMPACTION_REDUCTION_PERCENT: u8 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Threshold,
    PreflightOverflow,
    ModelSwitch,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactSession {
    pub user_context: Option<String>,
    pub trigger: CompactionTrigger,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactionPolicy {
    pub threshold_percent: u8,
    pub max_attempts: u8,
    pub summary_reserve_tokens: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self { threshold_percent: 80, max_attempts: 3, summary_reserve_tokens: 8_192 }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ContextUsage {
    pub estimated_input_tokens: u64,
    pub context_window: u64,
    pub utilization_percent: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactionSize {
    pub message_count: u64,
    pub serialized_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionCandidate {
    pub compaction_id: CompactionId,
    pub messages: Vec<ModelMessage>,
    pub before: CompactionSize,
    pub after: CompactionSize,
    pub summary_chars: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CompactionError {
    #[error("nothing to compact")]
    NothingToCompact,
    #[error("a foreground turn is active")]
    ActiveTurn,
    #[error("compaction is already active")]
    AlreadyActive,
    #[error("compaction input exceeds the safe request budget")]
    InputTooLarge,
    #[error("compaction summary is degenerate")]
    DegenerateSummary,
    #[error("invalid compaction summary: {message}")]
    InvalidSummary { message: String },
    #[error("compaction model failed: {source}")]
    ModelFailed { source: AgentError },
    #[error("compaction cancelled")]
    Cancelled,
    #[error("compaction persistence failed: {message}")]
    PersistenceFailed { message: String },
    #[error("compaction reconciliation failed: {message}")]
    ReconciliationFailed { message: String },
}
```

Implement `code()` and `retryability()` with the exact `compaction.*` strings from the design. Export the module from `lib.rs`.

- [ ] **Step 4: Extend commands, visible events, and canonical records**

Add to `Command`:

```rust
CompactSession(CompactSession),
CancelCompaction { compaction_id: CompactionId },
```

Add to `EventPayload`:

```rust
CompactionStarted { compaction_id: CompactionId, trigger: CompactionTrigger },
CompactionCompleted {
    compaction_id: CompactionId,
    before: CompactionSize,
    after: CompactionSize,
    checkpoint_id: String,
    warning: Option<AgentError>,
},
CompactionFailed { compaction_id: CompactionId, error: AgentError },
CompactionCancelled { compaction_id: CompactionId },
```

Add to `JournalRecord`:

```rust
CompactionRequested {
    compaction_id: CompactionId,
    trigger: CompactionTrigger,
    user_context: Option<String>,
},
CompactionFailed { compaction_id: CompactionId, error_code: String },
CompactionCancelled { compaction_id: CompactionId },
```

Update journal projection so these lifecycle records advance the canonical cursor without appending model messages or changing unresolved-tool state.

- [ ] **Step 5: Extend the session state machine**

Add:

```rust
pub struct ActiveCompaction {
    pub id: CompactionId,
    pub cancel_requested: bool,
}

pub enum SessionPhase {
    Idle,
    Running(ActiveTurn),
    Compacting(ActiveCompaction),
    Stopped,
}
```

Implement `request_compaction`, `request_compaction_cancel`, and `finish_compaction`. `request_start` must return `TransitionError::CompactionAlreadyActive` from `Compacting`; `request_compaction` must distinguish a running turn from an existing compaction; `stop` remains valid from every phase.

- [ ] **Step 6: Run the core contract and regression tests**

Run: `cargo fmt --all && cargo test -p lato-core`

Expected: all `lato-core` unit, contract, journal, and state-machine tests pass.

- [ ] **Step 7: Commit the core contract**

```bash
git add crates/lato-core/src crates/lato-core/tests/compaction_contract.rs
git commit -m "feat: define manual compaction contracts"
```

---

### Task 2: Add the runtime compaction protocol and composite store boundary

**Files:**
- Modify: `crates/lato-core/src/projection.rs`
- Modify: `crates/lato-runtime/src/driver.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-runtime/src/lib.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`

**Interfaces:**
- Consumes: Task 1 compaction values and lifecycle variants.
- Produces: `CompactionRequest`, `CompactionControl`, and default object-safe methods on `TurnDriver`.
- Produces: `SessionStore`, a combined object-safe store accepted by persisted runtime sessions.
- Produces: runtime state transitions through sampling, cancellation, persistence, and reconciliation.

- [ ] **Step 1: Write failing runtime mutual-exclusion and cancellation tests**

Add a `BlockingCompactionDriver` fixture to `session_runtime.rs`. It must signal when compact sampling starts, block until its cancellation token fires, and count invocations. Add tests that assert:

```rust
session.submit(Command::CompactSession(CompactSession {
    user_context: None,
    trigger: CompactionTrigger::Manual,
})).await.unwrap();
started.notified().await;

let error = session.submit(Command::StartTurn(StartTurn {
    input: UserInput::text("must wait"),
    behavior: StartBehavior::Reject,
})).await.unwrap_err();
assert_eq!(error.code, "compaction.already_active");

session.submit(Command::CancelCompaction {
    compaction_id: observed_id.clone(),
}).await.unwrap();
assert!(matches!(next_terminal(&mut events).await, EventPayload::CompactionCancelled { .. }));
```

Also test compact-during-turn, duplicate compact, mismatched ID, and shutdown during a blocked compaction.

- [ ] **Step 2: Run the focused runtime test and verify failure**

Run: `cargo test -p lato-runtime --test session_runtime compaction -- --nocapture`

Expected: compilation fails because runtime has no compaction driver or command handling.

- [ ] **Step 3: Add the combined session-store trait**

In `lato-core/src/projection.rs` add:

```rust
pub trait SessionStore: crate::EventStore + HistoryProjectionStore {}

impl<T> SessionStore for T where T: crate::EventStore + HistoryProjectionStore {}
```

Change persisted runtime constructors and fields from `Arc<dyn EventStore>` to
`Arc<dyn SessionStore>`. `MemoryEventStore` and `FileEventStore` already
implement both parent traits and therefore use the blanket implementation.

- [ ] **Step 4: Add the driver-side compaction boundary**

In `driver.rs` define:

```rust
pub struct CompactionRequest {
    pub compaction_id: CompactionId,
    pub request: CompactSession,
    pub messages: Vec<ModelMessage>,
    pub policy: CompactionPolicy,
}

pub struct CompactionControl {
    pub cancellation: CancellationToken,
}
```

Extend `TurnDriver` with default methods so existing focused fixtures remain source-compatible:

```rust
async fn compact(
    &self,
    _request: CompactionRequest,
    _control: CompactionControl,
) -> Result<CompactionCandidate, AgentError> {
    Err(AgentError::new(
        "compaction.unsupported",
        ErrorCategory::InvalidInput,
        "this session driver does not support compaction",
        Retryability::Never,
    ))
}

async fn install_history(&self, _messages: Vec<ModelMessage>) -> Result<(), AgentError> {
    Err(AgentError::new(
        "compaction.unsupported",
        ErrorCategory::InvalidInput,
        "this session driver cannot install compacted history",
        Retryability::Never,
    ))
}
```

- [ ] **Step 5: Add runtime compaction operation ownership**

Add `NEXT_COMPACTION_ID`, `ActiveRuntimeCompaction`, and a `CompactionFinished`
driver message. `handle_command` must:

1. call `machine.request_compaction`;
2. commit `CompactionRequested` with `SyncData`;
3. snapshot `bootstrap/replay.projection.messages` through a runtime-owned
   `current_messages` value kept current after every committed conversation
   item;
4. broadcast `CompactionStarted`;
5. spawn `driver.compact` with a child cancellation token;
6. acknowledge command acceptance without waiting for model completion.

The runtime-owned message snapshot prevents compaction from reading mutable
driver history after command acceptance. Update it on each committed
`ConversationItemCommitted` and after a reconciled replacement.

- [ ] **Step 6: Implement cancellation and terminal transitions without persistence replacement**

Route `CancelCompaction` only to the matching active ID. On model failure or
cancellation, commit the canonical terminal record first, then broadcast the
terminal event and call `machine.finish_compaction`. Do not add replacement
logic in this task; a successful candidate should use this exact temporary
terminal error so Task 4 has one obvious red test to replace:

```rust
AgentError::new(
    "compaction.persistence_not_connected",
    ErrorCategory::InternalInvariant,
    "compaction candidate is ready but persistence is not connected",
    Retryability::Never,
)
```

- [ ] **Step 7: Run runtime tests and commit**

Run: `cargo fmt --all && cargo test -p lato-runtime`

Expected: all runtime tests pass, including mutual exclusion, cancellation,
shutdown, and lifecycle durability; the candidate-success fixture reaches the
intentional persistence-not-connected result.

```bash
git add crates/lato-core/src/projection.rs crates/lato-runtime
git commit -m "feat: coordinate typed compaction operations"
```

---

### Task 3: Implement Grok-style summary preparation and bounded model sampling

**Files:**
- Create: `crates/lato-agent/src/compaction.rs`
- Modify: `crates/lato-agent/src/history.rs`
- Modify: `crates/lato-agent/src/journal.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Modify: `crates/lato-agent/Cargo.toml`
- Modify: `crates/lato-ai/src/model_port_adapter.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`
- Modify: `crates/lato-ai/src/lib.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Test: `crates/lato-agent/src/compaction.rs`
- Test: `crates/lato-agent/tests/legacy_driver.rs`

**Interfaces:**
- Consumes: Task 2 `TurnDriver::compact` and `install_history` methods.
- Produces: pure `prepare_compaction_messages`, `build_compaction_prompt`, `normalize_summary`, `validate_summary`, and `build_compacted_history` helpers.
- Produces: `SwitchableModelPort`, which exposes one atomic snapshot of the
  current selection, capabilities, and canonical `ModelPort`.
- Produces: the real `LegacyTurnDriver` compaction implementation using that
  current shared model port.

- [ ] **Step 1: Write failing pure transformation tests**

Create table-driven tests covering:

```rust
#[test]
fn preparation_keeps_intent_but_bounds_tool_payloads() {
    let source = vec![
        system("system"),
        user("fix parser"),
        tool_call("call-1", "read_file"),
        tool_result("call-1", &"x".repeat(100_000)),
        assistant("parser.rs is malformed"),
    ];
    let prepared = prepare_compaction_messages(&source).unwrap();
    let json = serde_json::to_string(&prepared).unwrap();
    assert!(json.contains("fix parser"));
    assert!(json.contains("read_file"));
    assert!(json.contains("call-1"));
    assert!(!json.contains(&"x".repeat(10_000)));
}

#[test]
fn normalized_summary_requires_every_section() {
    let error = validate_summary("<summary>1. Primary Request and Intent: x</summary>")
        .unwrap_err();
    assert_eq!(error.code(), "compaction.invalid_summary");
}

#[test]
fn replacement_has_stable_grok_order() {
    let messages = build_compacted_history(system("s"), user("latest goal"), healthy_summary());
    assert_eq!(messages[0].role, ModelRole::System);
    assert!(text(&messages[1]).contains("<user_query>"));
    assert!(text(&messages[2]).contains("<conversation_summary version=\"1\">"));
}
```

Cover prior-summary carry-forward, image placeholders, reasoning omission where
represented, incomplete tool tails, 500-character floor, 32-KiB cap, and the
20-percent reduction gate.

- [ ] **Step 2: Run pure tests and verify failure**

Run: `cargo test -p lato-agent compaction::tests -- --nocapture`

Expected: compilation fails because `lato_agent::compaction` does not exist.

- [ ] **Step 3: Implement bounded pure preparation and assembly**

Use constants from `lato-core`. Tool annotations must use a fixed format and
bound each result excerpt to 1,024 Unicode scalar values:

```text
[Tool call call-1: read_file]
[Tool result call-1: <bounded outcome>]
```

Wrap the latest real user objective as:

```text
<user_query>
...
</user_query>
```

Wrap the installed summary as:

```text
This session is being continued from a previous conversation whose earlier
turns were compacted.

<conversation_summary version="1">
...
</conversation_summary>
```

Normalize only recognized leading `<analysis>` and outer `<summary>` wrappers;
neutralize nested control tags by inserting a zero-width space after `<`.
Required headings are matched case-insensitively by their numeric prefix and
English canonical name, while section bodies may contain any language.

- [ ] **Step 4: Make `CompactionSummary` round-trip through canonical model messages**

Update `history_item_to_message` so `HistoryItem::CompactionSummary(text)` maps
to a user-role text message containing the versioned wrapper. Update
`model_messages_to_history` to recognize only that exact wrapper as a
`CompactionSummary`; all other user-role messages remain `HistoryItem::User`.
Add a round-trip test with a system message, user query, and prior summary.

- [ ] **Step 5: Expose the same switchable canonical model port to turns and compaction**

Add these exact public values to `lato-ai/src/model_port_adapter.rs`:

```rust
#[derive(Clone)]
pub struct ActiveModelPort {
    pub selection: ModelSelection,
    pub capabilities: ModelCapabilities,
    pub port: Arc<dyn ModelPort>,
}

pub struct SwitchableModelPort {
    inner: tokio::sync::RwLock<ActiveModelPort>,
}

impl SwitchableModelPort {
    pub fn new(selection: ModelSelection, port: Arc<dyn ModelPort>) -> Self {
        let capabilities = port.capabilities();
        Self { inner: tokio::sync::RwLock::new(ActiveModelPort { selection, capabilities, port }) }
    }

    pub async fn snapshot(&self) -> ActiveModelPort {
        self.inner.read().await.clone()
    }

    pub async fn set(&self, selection: ModelSelection, port: Arc<dyn ModelPort>) {
        let capabilities = port.capabilities();
        *self.inner.write().await = ActiveModelPort { selection, capabilities, port };
    }
}

pub fn adapt_model_port(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<ActiveModelPort, ModelSelectionError> {
    let selection = ModelSelection::new(provider, model)?;
    let port: Arc<dyn ModelPort> = Arc::new(LegacyModelPort::new(selection.clone(), stream));
    let capabilities = port.capabilities();
    Ok(ActiveModelPort { selection, capabilities, port })
}
```

Change `adapt_model_stream` to create `ModelPortStreamAdapter` from the
`ActiveModelPort` returned by `adapt_model_port`. In `AcpHost`, retain one
`Arc<SwitchableModelPort>` beside the existing turn stream; update both from
the same new `ActiveModelPort` whenever `lato/model/set` succeeds. Pass the
switchable port through `RuntimeSession` into `LegacyTurnDriver`.

This step is required because `ModelStream` erases typed `ModelError`, usage,
and terminal stop reason. The compactor must read the canonical model stream
directly and must not classify rendered strings.

- [ ] **Step 6: Write failing model-loop tests**

Add fake streams for:

- one valid summary;
- two degenerate summaries followed by a valid summary;
- one deterministic failure;
- three transient failures;
- a tool call during compaction;
- a stream blocked until cancellation.

Assert the request contains `"tools": []` and `"tool_choice": "none"`, the
call count never exceeds three, deterministic errors call once, and dropping
the operation cancels the provider promptly.

- [ ] **Step 7: Implement `LegacyTurnDriver::compact` and `install_history`**

Retain the constructor's `Arc<SwitchableModelPort>` on `LegacyTurnDriver` in
addition to the actor state. Take one `ActiveModelPort` snapshot at operation
start so model switching cannot split attempts across providers. `compact`
must:

1. validate and prepare the immutable request messages;
2. construct the nine-section prompt plus labeled optional user context;
3. build a canonical `ModelRequest` from the frozen active selection and invoke
   its `ModelPort` with no tools and `ToolChoice::None`;
4. race each attempt and backoff against the cancellation token;
5. collect `ModelStreamEvent::TextDelta`, retain usage for reporting, and
   reject every `ToolCallDelta`;
6. require `ModelStreamEvent::Completed { reason: Completed }` and classify
   typed `ModelError` values by `Retryability`, not rendered substrings;
7. normalize, validate, assemble, and measure the candidate;
8. return `CompactionCandidate` without mutating actor history.

`install_history` converts canonical model messages with
`model_messages_to_history`, obtains the actor mutex, and replaces the complete
history. Conversion must finish before taking the mutex so an invalid candidate
cannot partially mutate state.

- [ ] **Step 8: Run agent and model-port tests and commit**

Run: `cargo fmt --all && cargo test -p lato-ai model_port_adapter -- --nocapture && cargo test -p lato-agent compaction -- --nocapture && cargo test -p lato-agent --test legacy_driver`

Expected: pure assembly, retry, cancellation, history round-trip, and legacy
driver regression tests all pass.

```bash
git add crates/lato-ai crates/lato-agent
git commit -m "feat: generate grok-style compaction summaries"
```

---

### Task 4: Persist successful candidates and reconcile every marker boundary

**Files:**
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`
- Modify: `crates/lato-store/src/file.rs`
- Modify: `crates/lato-store/src/memory.rs`
- Modify: `crates/lato-store/src/projection.rs`
- Modify: `crates/lato-store/src/writer.rs`
- Modify: `crates/lato-store/tests/projection_recovery.rs`

**Interfaces:**
- Consumes: Task 2 runtime operation and Task 3 `CompactionCandidate`.
- Consumes: `SessionStore::replace_history` and `SessionStore::replay`.
- Produces: successful checkpoint installation, journal-sequence resynchronization, and fail-closed reconciliation.

- [ ] **Step 1: Write failing successful-compaction and sequence tests**

Add runtime tests that seed a completed turn, return a valid candidate, and
assert:

```rust
let completed = next_compaction_terminal(&mut events).await;
let EventPayload::CompactionCompleted { checkpoint_id, .. } = completed else {
    panic!("expected compaction completion");
};
let replay = store.replay(&sid).await.unwrap();
assert_eq!(replay.projection.active_checkpoint_id.as_deref(), Some(&checkpoint_id));
assert_eq!(replay.projection.messages, compacted);

session.submit(start("after compact")).await.unwrap();
let replay = store.replay(&sid).await.unwrap();
assert_eq!(
    replay.envelopes.last().unwrap().journal_sequence + 1,
    replay.projection.next_journal_sequence
);
```

The second assertion catches runtime journal sequence drift after the store
appends `HistoryProjectionReplaced` internally.

- [ ] **Step 2: Write failing pre-marker and post-marker fault tests**

Use the existing projection fault injector to cover:

- checkpoint temp write failure: old history remains authoritative;
- marker append failure: old history remains authoritative;
- history publish failure after marker: replay selects the new checkpoint;
- metadata publish failure after marker: replay selects the new checkpoint;
- replay failure after a committed marker: session emits no false completion
  and stops with `compaction.reconciliation_failed`.

Run: `cargo test -p lato-runtime --test session_runtime compaction_persistence -- --nocapture`

Expected: failures expose the intentional persistence-not-connected branch
from Task 2.

- [ ] **Step 3: Connect candidate replacement**

On successful driver output:

```rust
let before_checkpoint = self.current_checkpoint_id.clone();
match self.store.replace_history(
    &self.session_id,
    candidate.messages.clone(),
    HistoryReplacementReason::ContextCompaction,
).await {
    Ok(metadata) => {
        self.journal_sequence = metadata.last_journal_sequence.saturating_add(1);
        self.current_checkpoint_id = metadata.active_checkpoint_id.clone();
        self.driver.install_history(candidate.messages.clone()).await?;
        self.current_messages = candidate.messages;
        self.complete_compaction(candidate, metadata).await;
    }
    Err(error) => {
        self.reconcile_compaction_failure(candidate, before_checkpoint, error).await;
    }
}
```

Do not append another success record: the store's
`HistoryProjectionReplaced(ContextCompaction)` marker is the success record.

- [ ] **Step 4: Implement mandatory reconciliation after replacement errors**

`reconcile_compaction_failure` must call `store.replay`, set
`journal_sequence = replay.projection.next_journal_sequence`, and compare the
active checkpoint with the pre-operation value:

- changed checkpoint: install replayed messages and emit
  `CompactionCompleted` with a recoverable storage-warning field;
- unchanged checkpoint: append `CompactionFailed`, then emit failure while
  retaining the old current messages;
- replay error or install error after a changed checkpoint: stop the session
  and emit `compaction.reconciliation_failed` without accepting later turns.

Populate the existing `warning: Option<AgentError>` field on
`CompactionCompleted` rather than encoding a warning in display text.

- [ ] **Step 5: Verify lifecycle records update projection cursors only**

Extend store projection tests so requested, failed, and cancelled records
advance `last_journal_sequence`/`last_record_id` without altering messages,
generation, or active checkpoint. Assert ordinary messages appended after
compaction extend the checkpoint history exactly once.

- [ ] **Step 6: Run runtime/store tests and commit**

Run: `cargo fmt --all && cargo test -p lato-store && cargo test -p lato-runtime`

Expected: all success, fault, sequence, cursor, and existing Phase 4A/4B
recovery tests pass.

```bash
git add crates/lato-runtime crates/lato-store crates/lato-core/src/event.rs
git commit -m "feat: checkpoint manual compaction results"
```

---

### Task 5: Expose the ACP extension and shared runtime facade

**Files:**
- Modify: `crates/lato-protocol/src/methods.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/tests/acp_runtime.rs`
- Modify: `src/client.rs`
- Test: `tests/session_compaction_cli.rs`

**Interfaces:**
- Consumes: completed runtime compaction lifecycle.
- Produces: `RuntimeSession::compact`, `RuntimeCompactionOutcome`, and the `lato/session/compact` extension.
- Produces: `InteractiveAcpClient::compact_streaming` used by the TUI backend.

- [ ] **Step 1: Write failing ACP method tests**

Add `lato/session/compact` to expected method capabilities and test:

```rust
let response = host.handle(req(
    4,
    "lato/session/compact",
    serde_json::json!({
        "sessionId": sid,
        "userContext": "preserve the parser root cause"
    }),
)).await.unwrap();
assert_eq!(response["result"]["status"], "complete");
assert_eq!(response["result"]["before"]["messageCount"], 4);
assert!(response["result"]["checkpointId"].as_str().is_some());
```

Also test unknown session, empty context normalization to `None`, compact while
a turn is active, cancellation through `session/cancel`, and typed error data.

- [ ] **Step 2: Run ACP tests and verify method-not-found failure**

Run: `cargo test -p lato-agent --test acp_runtime compact -- --nocapture`

Expected: failure because the protocol and host do not advertise or route the
method.

- [ ] **Step 3: Add the RuntimeSession compact facade**

Implement a method parallel to `prompt` that locks the submission gate,
subscribes before submit, captures the `CompactionId` from
`CompactionStarted`, releases the gate, and waits only for terminal events with
that ID:

```rust
pub async fn compact(
    &self,
    user_context: Option<String>,
) -> Result<RuntimeCompactionOutcome, AgentError>;
```

Extend `cancel()` to inspect an `ActiveOperation` enum:

```rust
enum ActiveOperation {
    Turn(TurnId),
    Compaction(CompactionId),
}
```

This keeps the existing public `session/cancel` behavior while routing the
typed runtime command correctly.

- [ ] **Step 4: Route `lato/session/compact` through AcpHost**

Parse `sessionId` and optional camelCase `userContext`, look up the existing
session, call `RuntimeSession::compact`, and return camelCase size/checkpoint
fields. Preserve the complete typed error under JSON-RPC error `data`; do not
flatten stable codes into the message.

- [ ] **Step 5: Add client streaming support**

Implement:

```rust
pub async fn compact_streaming(
    &mut self,
    user_context: Option<String>,
    mut on_event: impl FnMut(&serde_json::Value),
) -> Result<CompactionResponse, String>;
```

Use the same response/event multiplexing pattern as `send_streaming`. Extend
`ClientUpdate` with compaction started/completed/failed/cancelled variants,
including sizes, checkpoint, and optional warning.

- [ ] **Step 6: Run protocol, host, and client tests and commit**

Run: `cargo fmt --all && cargo test -p lato-protocol && cargo test -p lato-agent --test acp_runtime && cargo test --test session_compaction_cli`

Expected: method discovery, facade routing, cancellation, typed errors, and
restart fixtures pass.

```bash
git add crates/lato-protocol crates/lato-agent/src crates/lato-agent/tests/acp_runtime.rs src/client.rs tests/session_compaction_cli.rs
git commit -m "feat: expose session compaction over acp"
```

---

### Task 6: Add `/compact` to the TUI without treating it as a chat turn

**Files:**
- Modify: `src/tui/commands.rs`
- Modify: `src/tui/backend.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/render.rs`
- Test: inline unit tests in the same files

**Interfaces:**
- Consumes: Task 5 `InteractiveAcpClient::compact_streaming` and `ClientUpdate` variants.
- Produces: `/compact [context]`, busy/cancel behavior, localized terminal messages, and `/status` compaction state.

- [ ] **Step 1: Write failing slash-command and reducer tests**

Update slash count assertions and add:

```rust
assert_eq!(matches("/COM")[0].name, "/compact");

app.composer.replace("/compact preserve parser details");
assert!(matches!(
    &submit_or_command(&mut app, &trust)[..],
    [Effect::Backend(BackendCommand::Compact(Some(context)))]
        if context == "preserve parser details"
));
assert!(app.messages.iter().all(|message| message.role != MessageRole::User));
```

Add reducer tests for started, completed, failed, cancelled, warning, and
Ctrl-C behavior. Assert typed composer text is not cleared by an incoming
compaction progress update.

- [ ] **Step 2: Run TUI tests and verify failure**

Run: `cargo test tui:: -- --nocapture`

Expected: compilation fails because TUI backend/state variants do not exist.

- [ ] **Step 3: Register and parse `/compact`**

Insert `/compact` after `/clear` in the registry with bilingual descriptions.
In `submit_or_command`, preserve original case in the optional context, trim
only surrounding whitespace, clear the command from the composer, and return
`BackendCommand::Compact(None|Some(text))`. Do not append user or assistant
chat bubbles.

- [ ] **Step 4: Generalize backend active work**

Replace the turn-only active aliases with:

```rust
enum ActiveWorkEnd {
    Turn(TurnEnd),
    Compaction(Result<CompactionResponse, String>),
}
```

Route `BackendCommand::Compact` through `compact_streaming`. While either work
kind is active, reject submit, session switch, rename, delete, new session,
login, and model change consistently. `BackendCommand::Cancel` signals the
existing oneshot; the client then sends `session/cancel` and reports the typed
terminal event.

- [ ] **Step 5: Render a distinct compaction state**

Add `CompactionUiState::{Idle, Running { started_at }, Completed, Failed,
Cancelled}` to `AppState`. Keep `responding` for foreground model turns; add
`is_busy()` for shared command gating. Render a localized status line while
running and append one system message at terminal state:

```text
上下文压缩完成：12 条消息 → 3 条消息
Context compacted: 12 messages → 3 messages
```

For warnings, append a second bounded sentence stating that the committed
checkpoint was recovered. For pre-marker failures, state that the previous
history remains active. `/status` includes the current or most recent
compaction status.

- [ ] **Step 6: Run TUI and CLI regressions and commit**

Run: `cargo fmt --all && cargo test tui:: && cargo test --test tui_cli && cargo test --test cli_headless`

Expected: all slash completion, reducer, render, Unicode layout, and existing
CLI behavior tests pass.

```bash
git add src/tui src/client.rs
git commit -m "feat: add tui compact command"
```

---

### Task 7: Complete restart recovery, provenance, documentation, and deployment

**Files:**
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`
- Modify: `tests/session_compaction_cli.rs`
- Modify: `README.md`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: the complete Phase 4C1 implementation.
- Produces: end-to-end crash/restart evidence, user documentation, upstream provenance, and the installed local binary.

- [ ] **Step 1: Add restart and continuation integration tests**

Create a persisted session through `FileEventStore`, seed enough bounded
history for compaction, run `lato/session/compact`, drop the host, create a new
host, resume the same ID, and submit one more prompt. Assert the model receives:

- one system head;
- one latest-user-query wrapper;
- one versioned conversation-summary wrapper containing all nine sections;
- the new prompt after those items;
- no pre-compaction raw tool payload.

Corrupt only `history.jsonl` after compaction and assert resume rebuilds from
the checkpoint. Remove the referenced checkpoint and assert resume fails with
`projection.checkpoint_missing`.

- [ ] **Step 2: Add interruption and failure recovery tests**

Cover:

- cancellation during each of the first, second, and third attempts;
- process-equivalent drop after `CompactionRequested` but before replacement;
- derived publication failure after marker followed by same-process
  reconciliation;
- next normal turn after compaction uses a dense journal sequence;
- unresolved prepared tool call rejects compact before any checkpoint exists;
- compaction error followed by a normal prompt uses the original history.

Run: `cargo test -p lato-agent --test compaction_runtime && cargo test --test session_compaction_cli`

Expected: every recovery test passes without retries of tool side effects or
mixed old/new history.

- [ ] **Step 3: Update provenance comments and the source ledger**

Add source headers to substantially derived compaction files:

```rust
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs and crates/codegen/xai-chat-state/src/compaction_utils.rs
// License: Apache-2.0
// Lato changes: reduced Grok's compaction pipeline to a tool-free manual operation coordinated by Lato's typed runtime and canonical journal
```

Add separate ledger rows for the compaction coordinator, summary helpers,
chat-state mutation boundary, and persistence ordering. Record the exact tests
that demonstrate each derivation.

- [ ] **Step 4: Document user behavior and phase boundaries**

Update README slash commands and canonical-session sections. State:

- `/compact [context]` uses the current model and may incur one to three model
  calls;
- the operation does not execute tools;
- success replaces model-visible history through a durable checkpoint while
  retaining the canonical journal;
- Ctrl-C cancels sampling and pre-marker failures keep old history;
- automatic compaction and overflow resubmission remain Phase 4C2/4C3 work.

- [ ] **Step 5: Run full repository gates**

Run:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: every command exits zero with no warnings or failed tests.

- [ ] **Step 6: Install the final binary**

Run: `cargo install --path .`

Expected: Cargo reports that `/Users/huangyongzhao/.cargo/bin/lato` was
installed or replaced with `lato v0.1.0-beta.2`.

- [ ] **Step 7: Run installed-binary smoke tests in an isolated home**

Run:

```bash
compact_smoke_home="$(mktemp -d)"
LATO_HOME="$compact_smoke_home" lato -p "reply with hi only"
lato --version
find "$compact_smoke_home/sessions" -maxdepth 3 -type f -print | sort
```

Expected: the first command prints `hi`; version prints
`lato 0.1.0-beta.2`; the isolated session contains `events.jsonl`,
`history.jsonl`, `history.meta.json`, and metadata files. The automated ACP
integration test supplies the non-TTY `/compact` smoke because the installed
TUI requires a terminal.

- [ ] **Step 8: Commit documentation and integration evidence**

```bash
git add README.md docs/superpowers/reference/lato-upstream-sources.md crates/lato-agent/tests/compaction_runtime.rs tests/session_compaction_cli.rs
git commit -m "docs: document durable manual compaction"
```

- [ ] **Step 9: Verify the final worktree scope**

Run: `git status --short && git log --oneline -10`

Expected: only the user's pre-existing dirty/untracked files remain outside
the Phase 4C1 commits; the recent log contains one focused commit for each task
boundary.

---

## Final verification checklist

- [ ] Manual compaction is a typed operation, never a fake user turn.
- [ ] The current session model is used with no tools advertised.
- [ ] Source, summary, attempt, byte, and reduction bounds are enforced.
- [ ] Prior summaries and the latest real user objective survive compaction.
- [ ] Candidate history has one system head and no invalid tool chain.
- [ ] `CompactionRequested` is durable before `CompactionStarted` is visible.
- [ ] Pre-marker failure and cancellation preserve old logical history.
- [ ] A committed marker is reconciled before success becomes visible.
- [ ] Runtime journal sequence advances past the writer-owned replacement marker.
- [ ] Restart and derived-history rebuild select the same checkpoint history.
- [ ] Unknown tool outcomes continue to fail closed.
- [ ] `/compact [context]`, Ctrl-C, busy state, `/status`, and bilingual results work in the TUI.
- [ ] Automatic triggers remain serialized but disabled.
- [ ] Workspace tests, Clippy, format, local install, and installed smoke all pass.
