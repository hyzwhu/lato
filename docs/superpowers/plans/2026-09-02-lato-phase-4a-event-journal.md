# Lato Phase 4A Canonical Event Journal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `HistoryItem` transcript writes with a bounded canonical journal that supports strict replay, atomic legacy import, and crash-safe non-idempotent tool boundaries.

**Architecture:** `lato-core` owns journal contracts and projection types, `lato-store` owns a secure per-session single-writer JSONL implementation, `lato-runtime` owns sequence assignment and persistence-before-broadcast barriers, and `lato-agent` commits complete conversation/tool/policy records. Live model deltas remain outside the canonical journal.

**Tech Stack:** Rust 2024, Tokio, async-trait, serde/serde_json, sha2, thiserror, fs2, tempfile, platform-gated `libc` file flags.

## Global Constraints

- Preserve the untracked repository-root `AGENTS.md`; never stage, edit, or delete it.
- Keep `lato-core` independent of file paths, ACP, `HistoryItem`, and concrete stores.
- Use one bounded writer task per active session; no second live append path is allowed.
- Persist canonical records before broadcasting related visible state.
- Do not persist `ModelDelta` or `ReasoningDelta` in the canonical journal.
- Use a 64 MiB byte cap and 100,000-record cap in Phase 4A; reject growth before mutating disk or memory.
- Repair only a torn final JSONL record; fail closed on complete malformed lines, sequence gaps, duplicate IDs, session mismatch, schema mismatch, and request-hash divergence.
- Do not automatically retry any prepared tool call that lacks a completion record.
- Preserve legacy `$LATO_HOME/sessions/<id>.jsonl` files unchanged after import.
- Add exact Codex/Grok Build derivation rows and source headers for structurally reused production code.
- After implementation, run workspace tests, Clippy with `-D warnings`, `cargo install --path .`, and installed-binary offline smoke tests.

---

## File Structure

### New files

- `crates/lato-core/src/journal.rs` — canonical envelopes, records, durability, errors, projection, hashing, and `EventStore` trait.
- `crates/lato-core/tests/journal_contract.rs` — serde, hash, projection, and error-code contract tests.
- `crates/lato-store/Cargo.toml` — new storage crate manifest.
- `crates/lato-store/src/lib.rs` — exports constants and store implementations.
- `crates/lato-store/src/file.rs` — secure filesystem open/replay/import behavior.
- `crates/lato-store/src/writer.rs` — bounded per-session writer task and retry barriers.
- `crates/lato-store/src/memory.rs` — deterministic in-memory store for contract/runtime tests.
- `crates/lato-store/tests/store_contract.rs` — shared `EventStore` contract suite.
- `crates/lato-store/tests/file_recovery.rs` — torn-tail, corruption, bounds, permission, and path-security tests.
- `crates/lato-agent/src/journal.rs` — `HistoryItem`/`ModelMessage` conversion and legacy import orchestration.
- `crates/lato-agent/tests/journal_runtime.rs` — canonical actor/tool records and durability ordering.

### Modified files

- `Cargo.toml` — add `crates/lato-store` and the root dependency.
- `crates/lato-core/Cargo.toml`, `crates/lato-core/src/lib.rs`, `crates/lato-core/src/id.rs` — journal module, hash dependency, exports, and `JournalRecordId`.
- `crates/lato-runtime/src/driver.rs`, `crates/lato-runtime/src/session.rs`, `crates/lato-runtime/src/lib.rs`, `crates/lato-runtime/tests/session_runtime.rs` — asynchronous commit messages, injected store, bootstrap, and barrier tests.
- `crates/lato-tools/src/runtime.rs` — expose a redacted prepared-call audit view without exposing executable internals.
- `crates/lato-agent/Cargo.toml`, `crates/lato-agent/src/lib.rs`, `crates/lato-agent/src/actor.rs`, `crates/lato-agent/src/legacy_driver.rs`, `crates/lato-agent/src/runtime_session.rs`, `crates/lato-agent/src/host.rs`, `crates/lato-agent/src/transcript.rs` — canonical commits, store wiring, replay hydration, and read-only legacy compatibility.
- `crates/lato-agent/tests/legacy_driver.rs`, `crates/lato-agent/tests/runtime_session.rs`, `crates/lato-agent/tests/acp_runtime.rs` — constructor and behavior updates.
- `README.md`, `docs/superpowers/reference/lato-upstream-sources.md`, `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md` — runtime architecture, source attribution, and acceptance evidence.

---

### Task 1: Define the canonical journal contract and projector

**Files:**
- Create: `crates/lato-core/src/journal.rs`
- Create: `crates/lato-core/tests/journal_contract.rs`
- Modify: `crates/lato-core/Cargo.toml`
- Modify: `crates/lato-core/src/id.rs`
- Modify: `crates/lato-core/src/lib.rs`

**Interfaces:**
- Consumes: existing `SessionId`, `TurnId`, `ToolCallId`, `ModelMessage`, `ToolName`, `ToolOutput`, `ToolError`, `ToolCapability`, `SideEffect`, `ToolIdempotency`, `SandboxObligation`, `ApprovalFingerprint`, `AgentError`, and `CancelReason`.
- Produces: `JournalEnvelope`, `JournalRecord`, `JournalRecordId`, `JournalDurability`, `JournalError`, `JournalReplay`, `SessionProjection`, `UnresolvedToolCall`, `EventStore`, `canonical_json`, and `journal_request_hash`.

- [ ] **Step 1: Add failing serde, hash, and projection tests**

Create `crates/lato-core/tests/journal_contract.rs` with focused fixtures. The first tests must assert tagged JSON, dense projection, and stable canonical hashes:

```rust
use lato_core::{
    JournalEnvelope, JournalRecord, JournalRecordId, ModelContent, ModelMessage, ModelRole,
    SessionId, ToolCallId, ToolName, TurnId, journal_request_hash, project_journal,
};

#[test]
fn canonical_hash_ignores_object_key_order() {
    let a = serde_json::json!({"b": 2, "a": {"d": 4, "c": 3}});
    let b = serde_json::json!({"a": {"c": 3, "d": 4}, "b": 2});
    assert_eq!(journal_request_hash("tool", &a), journal_request_hash("tool", &b));
}

#[test]
fn projection_restores_requested_and_rejected_tool_messages() {
    let sid = SessionId::from("s1");
    let tid = TurnId::from("t1");
    let call_id = ToolCallId::from("c1");
    let records = vec![
        envelope(&sid, Some(&tid), 0, JournalRecord::SessionStarted),
        envelope(
            &sid,
            Some(&tid),
            1,
            JournalRecord::ToolCallRequested {
                call_id: call_id.clone(),
                name: ToolName::parse("builtin:read_file").unwrap(),
                arguments: serde_json::json!({"path": "README.md"}),
                request_hash: "sha256:v1:test".into(),
            },
        ),
        envelope(
            &sid,
            Some(&tid),
            2,
            JournalRecord::ToolCallRejected {
                call_id,
                request_hash: "sha256:v1:test".into(),
                error: lato_core::ToolError::new(
                    "policy.denied",
                    "denied",
                    lato_core::Retryability::Never,
                ),
            },
        ),
    ];
    let projection = project_journal(&sid, &records).unwrap();
    assert!(matches!(projection.messages[0].content[0], ModelContent::ToolCall { .. }));
    assert!(matches!(projection.messages[1].content[0], ModelContent::ToolResult { .. }));
    assert!(projection.unresolved_tools.is_empty());
}

fn envelope(
    sid: &SessionId,
    tid: Option<&TurnId>,
    sequence: u64,
    record: JournalRecord,
) -> JournalEnvelope {
    JournalEnvelope {
        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
        record_id: JournalRecordId::from(format!("{}-record-{sequence}", sid.as_str())),
        session_id: sid.clone(),
        turn_id: tid.cloned(),
        journal_sequence: sequence,
        timestamp_ms: sequence,
        record,
    }
}
```

Also add tests for every `JournalError::code()`, unsupported schema, session mismatch, sequence gap, duplicate record ID, completion without preparation, rejected-after-prepared, and prepared-without-completion projecting exactly one `UnresolvedToolCall`.

- [ ] **Step 2: Run the core contract test and verify it fails**

Run:

```bash
cargo test -p lato-core --test journal_contract
```

Expected: compilation fails because the journal symbols do not exist.

- [ ] **Step 3: Add identifiers, records, errors, and the store trait**

Add a `JournalRecordId` newtype in `id.rs` using the same checked-string pattern as `EventId`. Add `sha2 = "0.10"` to `lato-core`.

Create `journal.rs` with these public shapes:

```rust
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalDurability {
    Flush,
    SyncData,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalEnvelope {
    pub schema_version: u32,
    pub record_id: JournalRecordId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub journal_sequence: u64,
    pub timestamp_ms: u64,
    pub record: JournalRecord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    SessionStarted,
    TurnInputAccepted { input: UserInput },
    ConversationItemCommitted { message: ModelMessage },
    PolicyDecisionCommitted { audit: PolicyAuditRecord },
    ToolCallRequested {
        call_id: ToolCallId,
        name: ToolName,
        arguments: serde_json::Value,
        request_hash: String,
    },
    ToolCallPrepared { audit: PreparedToolAudit },
    ToolCallCompleted {
        call_id: ToolCallId,
        request_hash: String,
        result: Result<ToolOutput, ToolError>,
    },
    ToolCallRejected {
        call_id: ToolCallId,
        request_hash: String,
        error: ToolError,
    },
    TurnCompleted { output: TurnOutput },
    TurnFailed { error: AgentError },
    TurnCancelled { reason: CancelReason },
    SessionStopped,
    LegacyTranscriptImported {
        source_version: u32,
        item_count: u64,
        content_digest: String,
    },
}

#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError>;
    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError>;
    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError>;
    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError>;
    async fn shutdown(&self, session_id: &SessionId) -> Result<(), JournalError>;
}
```

`JournalReplay` must contain `exists: bool`, `envelopes: Vec<JournalEnvelope>`, and `projection: SessionProjection`; `JournalReplay::empty(session_id)` returns `exists == false` and sequence zero. A missing journal is therefore distinguishable from an existing corrupt journal without overloading I/O error strings.

Define explicit serializable `PolicyAuditStage`, `PolicyAuditDecision`, `PolicyAuditRecord`, and `PreparedToolAudit` structs using only core types. `PreparedToolAudit` must carry `request_hash`, `ApprovalFingerprint`, `ToolIdempotency`, `SideEffect`, `SandboxObligation`, and capabilities. `JournalError` must implement `code()` and `retryability()` with the stable codes from the design.

- [ ] **Step 4: Implement canonical hashing and strict projection**

Implement recursive key sorting and a versioned SHA-256 digest:

```rust
pub fn journal_request_hash(kind: &str, payload: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"lato-journal-request-v1\0");
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_json(payload).to_string().as_bytes());
    format!("sha256:v1:{:x}", hasher.finalize())
}
```

`project_journal(session_id, envelopes)` must validate schema, session, dense sequence, unique IDs, tool lifecycle, and terminal consistency before returning `SessionProjection`. It must return no projection on the first fatal error. A prepared call without completion is valid input but appears in `unresolved_tools`; it is never replayed.

- [ ] **Step 5: Export the contract and run tests**

Export the journal module from `lib.rs`, then run:

```bash
cargo fmt --all
cargo test -p lato-core --test journal_contract
cargo test -p lato-core
```

Expected: all `lato-core` tests pass.

- [ ] **Step 6: Commit the core contract**

```bash
git add crates/lato-core/Cargo.toml crates/lato-core/src/id.rs crates/lato-core/src/journal.rs crates/lato-core/src/lib.rs crates/lato-core/tests/journal_contract.rs Cargo.lock
git commit -m "feat: define canonical journal contract"
```

---

### Task 2: Implement bounded memory and filesystem event stores

**Files:**
- Create: `crates/lato-store/Cargo.toml`
- Create: `crates/lato-store/src/lib.rs`
- Create: `crates/lato-store/src/memory.rs`
- Create: `crates/lato-store/src/file.rs`
- Create: `crates/lato-store/src/writer.rs`
- Create: `crates/lato-store/tests/store_contract.rs`
- Create: `crates/lato-store/tests/file_recovery.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: Task 1 `EventStore`, `JournalEnvelope`, `JournalReplay`, `JournalDurability`, `JournalError`, `project_journal`.
- Produces: `MemoryEventStore`, `FileEventStore::open(&Path)`, `MAX_JOURNAL_BYTES = 64 * 1024 * 1024`, `MAX_JOURNAL_RECORDS = 100_000`.

- [ ] **Step 1: Scaffold the crate with failing shared contract tests**

Add `crates/lato-store` to workspace members and create its manifest with `lato-core`, `async-trait`, `serde_json`, `tokio` (`fs`, `io-util`, `macros`, `rt`, `sync`), `fs2`, `thiserror`, and Unix-target `libc`; add `tempfile` for tests.

Write `store_contract.rs` so the same async function runs against memory and file stores:

```rust
async fn append_replay_and_resume(store: Arc<dyn EventStore>) {
    let sid = SessionId::from("contract-session");
    store.append(envelope(&sid, 0), JournalDurability::Flush).await.unwrap();
    store.append(envelope(&sid, 1), JournalDurability::SyncData).await.unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes.len(), 2);
    assert_eq!(replay.projection.next_journal_sequence, 2);
    assert_eq!(store.list_sessions().await.unwrap(), vec![sid]);
}
```

Add cases proving a failed/gapped append does not advance state, duplicate IDs fail, import is atomic/idempotent, and shutdown drains accepted commands.

- [ ] **Step 2: Run tests and verify missing implementations**

```bash
cargo test -p lato-store --test store_contract
```

Expected: compilation fails because the store types do not exist.

- [ ] **Step 3: Implement `MemoryEventStore` minimally**

Use `tokio::sync::Mutex<BTreeMap<SessionId, Vec<JournalEnvelope>>>`. Validate a candidate vector with `project_journal` before replacing stored state. `import_if_absent` must return the existing journal unchanged when the session already exists. `shutdown` is a no-op after accepted appends.

Run:

```bash
cargo test -p lato-store --test store_contract memory
```

Expected: all memory-store contract cases pass.

- [ ] **Step 4: Write failing file-recovery and security tests**

Cover exact behavior:

```rust
#[tokio::test]
async fn invalid_unterminated_tail_is_truncated_but_complete_bad_line_fails() {
    // Write one valid line plus an invalid unterminated suffix; replay keeps one record.
    // Then write the same invalid suffix with '\n'; replay returns journal.parse.
}

#[tokio::test]
async fn valid_unterminated_tail_is_retained_and_terminated() {
    // A valid final JSON object without '\n' survives and the file gains one newline.
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_journal_is_rejected_without_following_it() {
    // Symlink events.jsonl to another file and expect journal.unsafe_restore.
}
```

Also test `0600` journal files, `0700` session directories, traversal IDs, metadata substitution checks, byte/record caps, and first-materialization parent-directory syncing through an injectable filesystem fault point.

- [ ] **Step 5: Implement secure bounded replay**

In `file.rs`, validate the ID before joining `$root/sessions/<id>/events.jsonl`. Port only the bounded-open and final-tail logic from the pinned Grok Build journal, preserving source headers. The algorithm must:

```text
lstat → reject symlink/non-file → reject metadata length > cap
→ open with O_NOFOLLOW on Unix → compare opened metadata
→ read at most cap + 1 → scan complete newline-terminated records
→ repair only the final unterminated tail → project the full candidate
```

Complete malformed records return `journal.parse`; structural errors come from `project_journal`.

- [ ] **Step 6: Implement the per-session writer task**

In `writer.rs`, use a bounded `mpsc::channel(128)` and commands carrying oneshot acknowledgements:

```rust
enum WriterCommand {
    Append {
        envelope: JournalEnvelope,
        durability: JournalDurability,
        ack: oneshot::Sender<Result<(), JournalError>>,
    },
    Shutdown {
        ack: oneshot::Sender<Result<(), JournalError>>,
    },
}
```

The writer validates capacity and the complete projected candidate before writing. It removes a pending item only after the full JSON line and newline are written and the requested barrier succeeds. On the first I/O error, close, reopen, re-read/validate, and retry the unwritten suffix once. Never append the same acknowledged record twice.

`FileEventStore` owns a mutex map of session IDs to writer handles. `import_if_absent` writes `events.jsonl.<nonce>.tmp`, flushes, syncs, replays it with the same validator, atomically renames it, and syncs the parent directory on supported platforms.

- [ ] **Step 7: Run the store contract and recovery suites**

```bash
cargo fmt --all
cargo test -p lato-store --test store_contract
cargo test -p lato-store --test file_recovery
cargo test -p lato-store
```

Expected: every store, crash-tail, permission, cap, and security test passes.

- [ ] **Step 8: Record upstream derivations and commit**

Add ledger rows for:

- `crates/lato-store/src/file.rs` from Grok Build `crates/codegen/xai-workflow/src/journal.rs`;
- `crates/lato-store/src/writer.rs` from Codex `codex-rs/rollout/src/recorder.rs`.

Then commit:

```bash
git add Cargo.toml Cargo.lock crates/lato-store docs/superpowers/reference/lato-upstream-sources.md
git commit -m "feat: add bounded file event store"
```

---

### Task 3: Add runtime persistence barriers and bootstrap replay

**Files:**
- Modify: `crates/lato-runtime/src/driver.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-runtime/src/lib.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`
- Modify: `crates/lato-runtime/Cargo.toml`

**Interfaces:**
- Consumes: `Arc<dyn EventStore>`, `JournalReplay`, `JournalRecord`, `JournalDurability`.
- Produces: `TurnEventEmitter::commit`, `spawn_session_with_store`, `SessionBootstrap`, durable runtime-generated session/turn records.

- [ ] **Step 1: Add failing runtime barrier tests**

Extend `session_runtime.rs` with a recording store that logs `append-start`, `append-done`, and broadcast observation. Add these tests:

```rust
#[tokio::test]
async fn canonical_state_is_persisted_before_broadcast() {
    // Block the store append for TurnInputAccepted.
    // Assert no TurnStarted is received while blocked.
    // Release append and assert TurnStarted is then received.
}

#[tokio::test]
async fn journal_failure_stops_the_turn_without_broadcasting_committed_state() {
    // Fail the next append with journal.io.
    // Assert the related event is absent and the command observes a structured failure.
}

#[tokio::test]
async fn model_deltas_are_live_and_never_appended() {
    // EchoDriver emits ModelDelta; recording store contains no delta record.
}
```

Add a driver that calls `events.commit(record, JournalDurability::SyncData).await` and prove it remains blocked until the store acknowledges.

- [ ] **Step 2: Run the focused tests and verify failure**

```bash
cargo test -p lato-runtime --test session_runtime canonical_state_is_persisted_before_broadcast
cargo test -p lato-runtime --test session_runtime driver_commit_waits_for_store_ack
```

Expected: compilation fails because durable runtime APIs do not exist.

- [ ] **Step 3: Make driver commits acknowledged and asynchronous**

Add an acknowledged message variant:

```rust
pub(crate) enum DriverMessage {
    LiveEvent { turn_id: TurnId, event: DriverEvent },
    Commit {
        turn_id: TurnId,
        record: JournalRecord,
        durability: JournalDurability,
        ack: oneshot::Sender<Result<(), AgentError>>,
    },
    Finished { turn_id: TurnId, result: Result<TurnOutput, AgentError> },
}
```

Expose:

```rust
impl TurnEventEmitter {
    pub async fn commit(
        &self,
        record: JournalRecord,
        durability: JournalDurability,
    ) -> Result<(), AgentError>;
}
```

Keep `model_delta` and `reasoning_delta` synchronous live sends. Map `JournalError` to `AgentError` without losing the stable journal code.

- [ ] **Step 4: Inject the store and replay bootstrap**

Add:

```rust
#[derive(Clone)]
pub struct SessionBootstrap {
    pub replay: JournalReplay,
}

pub fn spawn_session_with_store(
    session_id: SessionId,
    driver: Arc<dyn TurnDriver>,
    store: Arc<dyn EventStore>,
    bootstrap: SessionBootstrap,
) -> SessionHandle;
```

Keep `spawn_session(session_id, driver)` as a compatibility/test helper backed by an internal volatile store. Production callers must use `spawn_session_with_store`.

Initialize the next journal sequence from `bootstrap.replay.projection.next_journal_sequence`. New sessions commit `SessionStarted`; resumed sessions do not duplicate it.

- [ ] **Step 5: Convert session state transitions to async commit barriers**

Make `request_start`, `launch`, `handle_driver_message`, `shutdown`, and the canonical emit helper asynchronous where needed. The ordering must be:

```rust
self.commit(turn_id.clone(), JournalRecord::TurnInputAccepted { input }, Flush).await?;
self.emit(Some(turn_id), EventPayload::TurnStarted);
```

`TurnInputAccepted` is the only durable start record; `TurnStarted` is a derived live event emitted only after its acknowledgement. Terminal runtime events use `SyncData`.

- [ ] **Step 6: Run all runtime tests**

```bash
cargo fmt --all
cargo test -p lato-runtime --test session_runtime
cargo test -p lato-runtime
```

Expected: ordering, cancellation, replacement, shutdown, event identity, and new journal-barrier tests all pass.

- [ ] **Step 7: Commit runtime integration**

```bash
git add crates/lato-runtime
git commit -m "feat: persist runtime state before broadcast"
```

---

### Task 4: Journal actor conversation, policy, and tool boundaries

**Files:**
- Modify: `crates/lato-tools/src/runtime.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Create: `crates/lato-agent/tests/journal_runtime.rs`
- Modify: `crates/lato-agent/tests/legacy_driver.rs`

**Interfaces:**
- Consumes: Task 3 `TurnEventEmitter::commit` and Task 1 audit records.
- Produces: `PreparedToolCall::audit()`, `ToolRuntime::authorize`, `ToolRuntime::execute_authorized`, complete model-message commits, and write-ahead tool lifecycle commits.

- [ ] **Step 1: Add failing tool-order and crash-boundary tests**

Create `journal_runtime.rs` with an injected recording store and a counting mutating tool. Assert exact order:

```text
ToolCallRequested acknowledged
PolicyDecisionCommitted acknowledged
ToolCallPrepared SyncData acknowledged
tool invocation begins
ToolCallCompleted SyncData acknowledged
```

Add fault cases:

- fail `ToolCallPrepared`: tool counter remains zero;
- block `ToolCallPrepared`: tool counter remains zero until release;
- let the tool increment, then fail `ToolCallCompleted`: replay contains one unresolved prepared call and no second invocation occurs;
- denied approval records `ToolCallRejected` and never records `ToolCallPrepared`;
- model deltas stream normally but do not appear in store records.

- [ ] **Step 2: Run the focused test and verify it fails**

```bash
cargo test -p lato-agent --test journal_runtime
```

Expected: compilation fails because the actor has no durable recorder.

- [ ] **Step 3: Expose a redacted audit view and split authorization from invocation**

Store the cloned `ToolDescriptor` inside `PreparedToolCall`, then add an audit conversion without exposing `tool`, raw grant internals, or executable mutable state:

```rust
impl PreparedToolCall {
    pub fn request(&self) -> &PolicyRequest { &self.request }
    pub fn fingerprint(&self) -> &ApprovalFingerprint { &self.fingerprint }
    pub fn audit(&self, request_hash: String) -> PreparedToolAudit {
        PreparedToolAudit {
            call_id: self.request.call_id.clone(),
            tool_name: self.request.tool_name.clone(),
            request_hash,
            approval_fingerprint: self.fingerprint.clone(),
            idempotency: self.descriptor.idempotency,
            side_effect: self.descriptor.side_effect,
            sandbox: self.request.sandbox.clone(),
            capabilities: self.request.capabilities.clone(),
        }
    }
}
```

Split the current `execute` method so grant validation and consumption complete before any tool code runs:

```rust
pub struct AuthorizedToolCall {
    prepared: PreparedToolCall,
    context: ToolContext,
}

pub fn authorize(
    &self,
    prepared: PreparedToolCall,
    grant: ExecutionGrant,
) -> Result<AuthorizedToolCall, ToolError>;

pub async fn execute_authorized(
    &self,
    authorized: AuthorizedToolCall,
) -> Result<ToolOutput, ToolError>;
```

`authorize` performs fingerprint recomputation and consumes the grant. `execute_authorized` performs the cancellation check, policy/tool audit emission, and tool invocation. Keep `execute(prepared, grant)` as a compatibility wrapper that calls these two methods so existing callers and tests retain behavior.

- [ ] **Step 4: Pass the durable emitter into the actor turn**

Extend `SessionActor::prompt_with_context` with a `TurnEventEmitter` argument. `LegacyTurnDriver::run` passes its existing emitter clone. Keep the JSON passthrough only for ACP notifications that are not yet canonical; canonical tool/model state must use typed commits.

Commit each complete non-tool assistant message as:

```rust
events
    .commit(
        JournalRecord::ConversationItemCommitted {
            message: ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text { text: complete_text }],
            },
        },
        JournalDurability::Flush,
    )
    .await?;
```

The runtime already commits accepted user input; do not duplicate it in the actor.

- [ ] **Step 5: Implement the policy and tool write-ahead protocol**

Refactor `process_tool_call` without changing approval or sandbox semantics:

1. resolve/normalize the call ID;
2. commit `ToolCallRequested` before policy evaluation becomes externally visible;
3. prepare the tool and commit the evaluated policy audit;
4. commit approval request before invoking `ToolApproval::approve`;
5. commit approval resolution, call `ToolRuntime::authorize`, then commit the grant-consumption stage;
6. on rejection, bound the model-visible error and commit `ToolCallRejected`;
7. on authorization, compute durability from descriptor side effect/idempotency and commit `ToolCallPrepared`;
8. invoke `execute_authorized` only after acknowledgement;
9. retain the full `Result<ToolOutput, ToolError>`, apply current output bounding, then commit `ToolCallCompleted`;
10. update `HistoryItem` only from the acknowledged canonical record.

Mutating, external, or non-idempotent calls use `SyncData` for prepared/completed. Explicitly read-only and idempotent calls use `Flush`.

- [ ] **Step 6: Run actor, tool, and policy regression tests**

```bash
cargo fmt --all
cargo test -p lato-agent --test journal_runtime
cargo test -p lato-agent --test legacy_driver
cargo test -p lato-tools --tests
cargo test -p lato-policy --tests
```

Expected: new durability ordering passes and all existing policy, approval, sandbox, tool-history, cancellation, and repetition tests remain green.

- [ ] **Step 7: Commit actor/tool integration**

```bash
git add crates/lato-tools/src/runtime.rs crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/tests/journal_runtime.rs crates/lato-agent/tests/legacy_driver.rs
git commit -m "feat: journal tool side-effect boundaries"
```

---

### Task 5: Add replay hydration and atomic legacy transcript import

**Files:**
- Create: `crates/lato-agent/src/journal.rs`
- Modify: `crates/lato-agent/src/history.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Modify: `crates/lato-agent/Cargo.toml`
- Modify: `crates/lato-agent/src/transcript.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/tests/runtime_session.rs`
- Modify: `crates/lato-agent/tests/acp_runtime.rs`

**Interfaces:**
- Consumes: `FileEventStore`, `JournalReplay`, `SessionProjection`, legacy `TranscriptStore` reader.
- Produces: `history_to_model_messages`, `model_messages_to_history`, `import_legacy_if_needed`, journal-backed `RuntimeSession` and `AcpHost` resume/list behavior.

- [ ] **Step 1: Write failing conversion and migration tests**

Add round-trip tests for user, assistant, tool call, and tool result `HistoryItem` values. Add ACP integration cases:

- legacy-only resume creates `<id>/events.jsonl`, preserves `<id>.jsonl`, and hydrates identical history;
- second resume does not import again;
- when both files exist, journal wins even if legacy content differs;
- malformed legacy transcript creates no authoritative journal;
- `session/list` returns legacy and journal sessions once each;
- migration failure leaves the original transcript untouched;
- journal replay with an unresolved prepared tool returns `journal.incomplete_side_effect` and never starts the driver.

- [ ] **Step 2: Run migration tests and verify failure**

```bash
cargo test -p lato-agent --test acp_runtime legacy_resume_imports_atomically
cargo test -p lato-agent --test runtime_session replay_hydrates_canonical_history
```

Expected: tests fail because resume still reads and writes transcripts directly.

- [ ] **Step 3: Implement lossless history conversion**

In `journal.rs`, map:

```text
HistoryItem::User          ↔ ModelRole::User + ModelContent::Text
HistoryItem::Assistant     ↔ ModelRole::Assistant + ModelContent::Text
HistoryItem::ToolCall      ↔ ModelRole::Assistant + ModelContent::ToolCall
HistoryItem::ToolResult    ↔ ModelRole::Tool + ModelContent::ToolResult
```

Reject model messages that cannot be represented by the current `HistoryItem` compatibility type rather than silently dropping content. Preserve tool call IDs and JSON arguments exactly.

- [ ] **Step 4: Implement lazy atomic import**

`import_legacy_if_needed` must:

```rust
pub async fn import_legacy_if_needed(
    session_id: &SessionId,
    transcripts: Option<&TranscriptStore>,
    events: &dyn EventStore,
) -> Result<JournalReplay, JournalError>;
```

First try journal replay. Only `JournalReplay { exists: false, .. }` permits legacy lookup. Fully load and convert the old transcript, construct dense envelopes beginning with `SessionStarted`, append converted conversation/tool records, finish with `LegacyTranscriptImported`, and call `import_if_absent`. Do not delete or rewrite the old file.

- [ ] **Step 5: Make runtime sessions store-aware and replay-aware**

Add a production constructor accepting `Arc<dyn EventStore>` and `JournalReplay`. Hydrate `LegacyTurnDriver` history from `replay.projection.messages` before spawning the loop, then call `spawn_session_with_store` with the same replay.

Keep the existing `RuntimeSession::new` only as a test/compatibility constructor using a volatile store. Remove transcript append bookkeeping from prompt completion; journal commits now occur inside the turn.

- [ ] **Step 6: Wire `AcpHost` to `FileEventStore`**

Add `lato-store` to `lato-agent` dependencies. When `LATO_HOME` is present, open one `Arc<FileEventStore>` and retain `TranscriptStore` only as a legacy reader. Make `make_runtime_session` asynchronous so it can replay/import before constructing the runtime session.

Update handlers:

- `session/new`: create an empty replay bootstrap and journal-backed session;
- `session/resume`: call import/replay, reject unsafe/corrupt/unresolved sessions, hydrate, then register;
- `session/list`: union in-memory sessions, journal store IDs, and legacy IDs;
- `session/prompt`: remove transcript append and `persisted` indexes;
- `session/close`: await runtime shutdown so the writer drains.

- [ ] **Step 7: Run agent and ACP tests**

```bash
cargo fmt --all
cargo test -p lato-agent --test runtime_session
cargo test -p lato-agent --test acp_runtime
cargo test -p lato-agent
```

Expected: canonical resume, migration, list deduplication, sequential prompts, cancellation, and close all pass.

- [ ] **Step 8: Commit replay and migration**

```bash
git add crates/lato-agent
git commit -m "feat: migrate sessions to canonical journal"
```

---

### Task 6: Harden failure injection and cross-platform behavior

**Files:**
- Modify: `crates/lato-store/src/file.rs`
- Modify: `crates/lato-store/src/writer.rs`
- Modify: `crates/lato-store/tests/file_recovery.rs`
- Modify: `crates/lato-agent/tests/journal_runtime.rs`
- Modify: `crates/lato-agent/tests/acp_runtime.rs`

**Interfaces:**
- Consumes: complete writer/runtime/agent journal pipeline.
- Produces: deterministic crash-boundary evidence and Windows-safe platform fallbacks.

- [ ] **Step 1: Add a test-only filesystem fault injector**

Define internal fault points:

```rust
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultPoint {
    BeforeWrite,
    AfterWrite,
    BeforeFlush,
    AfterFlush,
    BeforeSyncData,
    AfterSyncData,
    BeforeRename,
    AfterRename,
}
```

Inject by trait/callback rather than environment variables so tests can run concurrently.

- [ ] **Step 2: Prove every crash boundary**

For each fault point, reopen the store and assert one of only two valid states: the full acknowledged prefix, or that prefix plus one repairable torn tail. Assert no duplicate record, no advanced in-memory sequence after a failed ack, and no automatic tool reinvocation.

- [ ] **Step 3: Add platform gates**

On Unix, assert `O_NOFOLLOW`, `0600`, `0700`, file `sync_data`, and directory sync. On Windows, use normal reparse-point/metadata checks available through `std`, skip unsupported directory sync with an explicit platform implementation, and ensure the crate compiles without Unix-only imports.

Run when the target is installed:

```bash
cargo check --workspace --target x86_64-pc-windows-gnu
```

Expected: PASS, or if the target is absent, record that only as an unavailable optional gate rather than a product failure.

- [ ] **Step 4: Run focused fault suites repeatedly**

```bash
for i in 1 2 3; do cargo test -p lato-store --test file_recovery; done
for i in 1 2 3; do cargo test -p lato-agent --test journal_runtime; done
```

Expected: all six runs pass without timing flakes.

- [ ] **Step 5: Commit crash hardening**

```bash
git add crates/lato-store crates/lato-agent/tests
git commit -m "test: harden journal crash recovery"
```

---

### Task 7: Update documentation, validate the workspace, and deploy locally

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`
- Verify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: complete Phase 4A implementation.
- Produces: user-facing architecture notes, acceptance evidence, and installed `lato` binary.

- [ ] **Step 1: Update runtime architecture documentation**

Document:

- canonical journal versus live deltas;
- new `$LATO_HOME/sessions/<id>/events.jsonl` layout;
- lazy legacy import and retained old file;
- strict replay/torn-tail behavior;
- `OutcomeUnknown` behavior for interrupted tools;
- journal sensitivity and local permission guarantees;
- Phase 4B snapshot/compaction deferral.

- [ ] **Step 2: Run formatting and focused suites**

```bash
cargo fmt --all -- --check
cargo test -p lato-core --test journal_contract
cargo test -p lato-store
cargo test -p lato-runtime
cargo test -p lato-agent
```

Expected: every command passes.

- [ ] **Step 3: Run full workspace gates**

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: zero test failures and zero Clippy warnings.

- [ ] **Step 4: Run installed-binary journal smoke tests**

Use a fresh explicit temporary home:

```bash
LATO_PHASE4_SMOKE_HOME=$(mktemp -d)
LATO_HOME="$LATO_PHASE4_SMOKE_HOME" cargo run -q -- -p "reply with hi only"
test -n "$(find "$LATO_PHASE4_SMOKE_HOME/sessions" -name events.jsonl -print -quit)"
LATO_HOME="$LATO_PHASE4_SMOKE_HOME" cargo run -q -- doctor --json
```

Expected: output is `hi`, a journal exists, and doctor JSON is valid with no credential values.

- [ ] **Step 5: Update acceptance results with exact evidence**

Record test counts, Clippy outcome, migration/fault-injection results, optional Windows gate status, journal smoke path shape, and any intentionally unrun live-provider tests. Do not claim a gate that was unavailable.

- [ ] **Step 6: Commit documentation and acceptance evidence**

```bash
git add README.md docs/superpowers/specs/2026-08-31-lato-acceptance-results.md docs/superpowers/reference/lato-upstream-sources.md
git commit -m "docs: record phase 4a journal acceptance"
```

- [ ] **Step 7: Deploy the completed feature locally**

```bash
cargo install --path .
LATO_PHASE4_INSTALLED_HOME=$(mktemp -d)
LATO_HOME="$LATO_PHASE4_INSTALLED_HOME" lato doctor --json
LATO_HOME="$LATO_PHASE4_INSTALLED_HOME" lato -p "reply with hi only"
```

Expected: installation replaces the local binary, doctor succeeds, and the installed binary prints `hi`.

- [ ] **Step 8: Confirm final repository state**

```bash
git status --short --branch
git log --oneline -8
```

Expected: implementation and documentation commits are present; the only unrelated working-tree item remains the user-owned untracked `AGENTS.md`.
