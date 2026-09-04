# Lato Phase 4B Grok-Style Session Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Grok-style replaceable and rebuildable model-history projection beside Lato's append-only canonical journal, with durable checkpoint ordering and unchanged fail-closed side-effect recovery.

**Architecture:** `events.jsonl` remains authoritative. `lato-core` gains projection/checkpoint contracts and a validation reducer; `lato-store` adds `history.jsonl`, metadata, quarantine/rebuild, and checkpoint publication behind the existing per-session writer; `lato-agent` resumes from the validated derived messages returned by the same `EventStore::replay` API. Canonical writes always happen before derived writes, and a derived failure never changes the result of a successful canonical append.

**Tech Stack:** Rust 2024, Tokio, async-trait, serde/serde_json, sha2, thiserror, fs2, tempfile, platform-gated `libc` file flags.

## Global Constraints

- Follow Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` for the source-stream/derived-history/checkpoint split.
- Keep `events.jsonl` append-only; do not rotate, truncate, archive, compress, or delete canonical records.
- Preserve the Phase 4A 64 MiB and 100,000-record canonical limits.
- Validate the complete canonical journal before trusting a derived projection.
- Never retry or resolve a `ToolCallPrepared` without `ToolCallCompleted`.
- Keep one bounded writer task per active session; no second live append path is allowed.
- Publish checkpoint artifacts before canonical checkpoint markers, and publish derived metadata last.
- Use file mode `0600` and directory mode `0700` on Unix; reject symlinks and non-regular files.
- Preserve the user's unrelated dirty files under `docs/testing`, `.lato/`, and `docs/.DS_Store`; stage only files named by each task.
- Add exact Grok Build source headers and source-ledger rows for structurally derived production files.
- Run relevant tests after every task, then workspace tests, Clippy, local install, and isolated offline smoke tests.

---

## File Structure

### New files

- `crates/lato-core/src/projection.rs` — projection entries, metadata, snapshots, checkpoints, replacement reasons, validation summaries, hashes, and stable projection errors.
- `crates/lato-core/tests/projection_contract.rs` — serde, digest, error-code, and secret-exclusion contracts.
- `crates/lato-store/src/projection.rs` — secure derived-history load, append, atomic replacement, quarantine, rebuild, and checkpoint I/O.
- `crates/lato-store/tests/projection_recovery.rs` — filesystem corruption, bounds, permissions, quarantine, rebuild, and crash-boundary tests.
- `tests/session_projection_cli.rs` — installed-shape create/resume migration and rebuild integration tests.

### Modified files

- `crates/lato-core/src/journal.rs` — add replacement marker handling and validation-only reduction.
- `crates/lato-core/src/lib.rs` — export projection contracts.
- `crates/lato-core/tests/journal_contract.rs` — validate checkpoint marker rules and validation/full-projection equivalence.
- `crates/lato-store/Cargo.toml` — add `sha2`.
- `crates/lato-store/src/lib.rs` — register projection module and projection limits.
- `crates/lato-store/src/file.rs` — derive projection paths, validate canonical stream before projection load, rebuild when necessary, and expose test paths.
- `crates/lato-store/src/writer.rs` — serialize canonical append followed by derived update and checkpoint/replacement commands.
- `crates/lato-store/src/memory.rs` — implement deterministic in-memory projection/checkpoint behavior.
- `crates/lato-store/tests/store_contract.rs` — require replay parity with derived messages.
- `crates/lato-agent/src/host.rs` — retain the common replay path and surface projection errors without bypasses.
- `crates/lato-agent/src/runtime_session.rs` — assert unresolved-side-effect behavior is identical for derived replay.
- `crates/lato-agent/tests/journal_runtime.rs` — verify projection order and resume hydration.
- `docs/superpowers/reference/lato-upstream-sources.md` — record Grok-derived projection implementation.
- `README.md` — document authoritative journal, derived history, rebuild, and checkpoint behavior.

---

### Task 1: Define projection contracts and validation-only reduction

**Files:**
- Create: `crates/lato-core/src/projection.rs`
- Create: `crates/lato-core/tests/projection_contract.rs`
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Modify: `crates/lato-core/tests/journal_contract.rs`

**Interfaces:**
- Consumes: existing `JournalEnvelope`, `JournalRecord`, `JournalRecordId`, `SessionId`, `SessionProjection`, and `ModelMessage`.
- Produces: `HistoryProjectionEntry`, `HistoryProjectionMetadata`, `HistoryReplacementReason`, `HistoryCheckpoint`, `SessionSnapshot`, `JournalValidation`, `ProjectionError`, `history_digest`, `projection_message`, and `validate_journal`.

- [ ] **Step 1: Add failing contract tests for projection hashes, serde, errors, and secret exclusion**

```rust
use lato_core::{
    HistoryProjectionEntry, HistoryProjectionMetadata, JournalRecordId, ModelMessage, ModelRole,
    ProjectionError, SessionId, history_digest,
};

#[test]
fn history_digest_is_stable_and_metadata_round_trips() {
    let messages = vec![ModelMessage { role: ModelRole::User, content: vec![] }];
    let digest = history_digest(&messages).unwrap();
    let metadata = HistoryProjectionMetadata::new(
        SessionId::from("projection-contract"), 0, JournalRecordId::from("r0"),
        1, 12, digest.clone(), None,
    );
    let encoded = serde_json::to_vec(&metadata).unwrap();
    let decoded: HistoryProjectionMetadata = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.history_digest, digest);
    assert_eq!(decoded, metadata);
}

#[test]
fn projection_errors_have_stable_codes() {
    assert_eq!(ProjectionError::Corrupt { message: "bad".into() }.code(), "projection.corrupt");
    assert_eq!(ProjectionError::CheckpointMissing { checkpoint_id: "c1".into() }.code(), "projection.checkpoint_missing");
}

#[test]
fn projection_entry_contains_only_model_visible_state() {
    let entry = HistoryProjectionEntry::new(
        1, JournalRecordId::from("r1"),
        ModelMessage { role: ModelRole::Assistant, content: vec![] },
    ).unwrap();
    let json = serde_json::to_string(&entry).unwrap();
    for forbidden in ["api_key", "access_token", "approval_grant", "environment"] {
        assert!(!json.contains(forbidden));
    }
}
```

- [ ] **Step 2: Run the new tests and verify the contracts do not exist yet**

Run: `cargo test -p lato-core --test projection_contract`

Expected: compilation fails with unresolved projection imports.

- [ ] **Step 3: Implement the core projection types and canonical hashes**

```rust
pub const HISTORY_PROJECTION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryProjectionEntry {
    pub schema_version: u32,
    pub journal_sequence: u64,
    pub record_id: JournalRecordId,
    pub message: ModelMessage,
    pub entry_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryProjectionMetadata {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub generation: u64,
    pub last_journal_sequence: u64,
    pub last_record_id: JournalRecordId,
    pub entry_count: u64,
    pub byte_length: u64,
    pub history_digest: String,
    pub active_checkpoint_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReplacementReason { ContextCompaction, Rewind, Repair }

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryCheckpoint {
    pub schema_version: u32,
    pub checkpoint_id: String,
    pub session_id: SessionId,
    pub replaced_through_sequence: u64,
    pub replaced_through_record_id: JournalRecordId,
    pub prior_checkpoint_id: Option<String>,
    pub messages: Vec<ModelMessage>,
    pub content_digest: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub messages: Vec<ModelMessage>,
    pub next_journal_sequence: u64,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
}
```

Implement `HistoryProjectionEntry::new(journal_sequence: u64, record_id: JournalRecordId, message: ModelMessage) -> Result<Self, ProjectionError>`, `HistoryProjectionMetadata::new(session_id: SessionId, last_journal_sequence: u64, last_record_id: JournalRecordId, entry_count: u64, byte_length: u64, history_digest: String, active_checkpoint_id: Option<String>) -> Self`, and `history_digest(&[ModelMessage]) -> Result<String, ProjectionError>` with `sha256:v1:` prefixes over canonical `serde_json::Value`. Define every `ProjectionError` variant and exact `code()` mapping from design section 11; all variants use `Retryability::Never` except `WriteFailed`, which uses `AfterBackoff`. Add `JournalError::Projection(ProjectionError)` and forward `code()` and `retryability()` so file replay can preserve `projection.*` codes.

- [ ] **Step 4: Add validation-only journal reduction and replacement-marker validation**

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalValidation {
    pub session_id: SessionId,
    pub next_journal_sequence: u64,
    pub last_record_id: Option<JournalRecordId>,
    pub message_count: u64,
    pub history_digest: String,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
}

pub fn validate_journal(
    session_id: &SessionId,
    envelopes: &[JournalEnvelope],
) -> Result<JournalValidation, JournalError>;

pub fn projection_message(record: &JournalRecord) -> Option<ModelMessage>;

// Add to JournalRecord:
HistoryProjectionReplaced {
    checkpoint_id: String,
    checkpoint_digest: String,
    replaced_through_sequence: u64,
    replaced_through_record_id: JournalRecordId,
    replacement_entry_count: u64,
    history_digest: String,
    reason: HistoryReplacementReason,
    prior_checkpoint_id: Option<String>,
},
```

Refactor `project_journal` to share one reducer with `validate_journal`. The validation path updates a streaming SHA-256 digest and count but does not retain `ModelMessage` values. Add `JournalRecord::HistoryProjectionReplaced` and validate that checkpoint IDs do not repeat and replacement markers occur only without a prepared unresolved tool.

- [ ] **Step 5: Run focused tests and formatting**

Run: `cargo fmt --all && cargo test -p lato-core --test projection_contract && cargo test -p lato-core --test journal_contract`

Expected: both test binaries pass with no warnings.

- [ ] **Step 6: Commit the core contracts**

```bash
git add crates/lato-core/src/projection.rs crates/lato-core/src/journal.rs crates/lato-core/src/lib.rs crates/lato-core/tests/projection_contract.rs crates/lato-core/tests/journal_contract.rs
git commit -m "feat: define history projection contracts"
```

---

### Task 2: Add secure derived-history load, rebuild, and quarantine

**Files:**
- Create: `crates/lato-store/src/projection.rs`
- Create: `crates/lato-store/tests/projection_recovery.rs`
- Modify: `crates/lato-store/Cargo.toml`
- Modify: `crates/lato-store/src/lib.rs`
- Modify: `crates/lato-store/src/file.rs`

**Interfaces:**
- Consumes: Task 1 projection types, `JournalReplay`, `JournalValidation`, and the existing secure path/permission helpers.
- Produces: `ProjectionPaths`, `load_or_rebuild`, `append_projection_entry`, `replace_projection`, `quarantine_projection`, and public test-only path accessors on `FileEventStore`.

- [ ] **Step 1: Write failing missing/stale/corrupt projection recovery tests**

```rust
#[tokio::test]
async fn missing_projection_is_built_from_canonical_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(dir.path()).unwrap();
    let sid = SessionId::from("projection-rebuild");
    append_conversation(&store, &sid, "hello").await;
    let history = store.history_path(&sid).unwrap();
    std::fs::remove_file(&history).unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages.len(), 1);
    assert!(history.is_file());
}

#[tokio::test]
async fn corrupt_projection_is_quarantined_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(dir.path()).unwrap();
    let sid = SessionId::from("projection-corrupt");
    append_conversation(&store, &sid, "hello").await;
    std::fs::write(store.history_path(&sid).unwrap(), b"bad\n").unwrap();
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.projection.messages.len(), 1);
    assert!(store.history_corrupt_path(&sid).unwrap().is_file());
}
```

Add tests for wrong session ID, unsupported schema, bad entry hash, metadata length/digest mismatch, oversize rejection, symlinks, `0600`/`0700`, and first-corrupt-artifact-wins.

- [ ] **Step 2: Run the recovery tests and verify path/rebuild APIs are missing**

Run: `cargo test -p lato-store --test projection_recovery`

Expected: compilation fails on missing `history_path` and projection functions.

- [ ] **Step 3: Implement focused projection filesystem primitives**

```rust
pub(crate) struct ProjectionPaths {
    pub history: PathBuf,
    pub metadata: PathBuf,
    pub corrupt_history: PathBuf,
    pub corrupt_metadata: PathBuf,
    pub checkpoints: PathBuf,
}

pub(crate) fn load_or_rebuild(
    session_id: &SessionId,
    session_dir: &Path,
    validation: &JournalValidation,
    canonical_messages: impl FnOnce() -> Result<Vec<ModelMessage>, JournalError>,
    faults: &dyn FileFaultInjector,
) -> Result<Vec<ModelMessage>, JournalError>;
```

Use unique sibling temp files, `create_new`, `O_NOFOLLOW`, private permissions, `flush`, `sync_data`, atomic rename, and parent-directory sync. Read at most `MAX_HISTORY_BYTES + 1`; decode at most `MAX_HISTORY_RECORDS`. Set both limits equal to the Phase 4A constants.

- [ ] **Step 4: Implement validation and backup-gated quarantine**

Validate metadata first, then file metadata before/after open, each entry hash, byte length, count, final source cursor, active checkpoint, and whole-history digest. On recoverable derived damage, preserve the first bad history as `history.jsonl.corrupt` and its metadata as `history.meta.json.corrupt`; if preservation fails, return `projection.quarantine_failed` without replacing the live file.

Missing files and stale but well-formed files rebuild without quarantine. Rebuild serializes canonical messages to temporary history and metadata files, publishes history first and metadata last, and syncs the session directory.

- [ ] **Step 5: Connect `FileEventStore::replay` to validation-first derived load**

```rust
let envelopes = decode_and_repair(path, &content)?;
let validation = validate_journal(session_id, &envelopes)?;
let messages = projection::load_or_rebuild(
    session_id,
    path.parent().expect("journal has session directory"),
    &validation,
    || Ok(project_journal(session_id, &envelopes)?.messages),
    faults,
)?;
let mut projection = project_journal(session_id, &envelopes)?;
projection.messages = messages;
```

Keep a single full projection call during this task; Task 4 removes it from the healthy derived path after checkpoint integration makes validation state sufficient.

- [ ] **Step 6: Run store recovery and existing file tests**

Run: `cargo fmt --all && cargo test -p lato-store --test projection_recovery && cargo test -p lato-store --test file_recovery`

Expected: all tests pass; corrupt canonical journal tests still return `journal.*`, not `projection.*`.

- [ ] **Step 7: Commit secure projection recovery**

```bash
git add crates/lato-store/Cargo.toml crates/lato-store/src/lib.rs crates/lato-store/src/file.rs crates/lato-store/src/projection.rs crates/lato-store/tests/projection_recovery.rs
git commit -m "feat: rebuild derived session history"
```

---

### Task 3: Serialize source-first journal and derived-history writes

**Files:**
- Modify: `crates/lato-store/src/writer.rs`
- Modify: `crates/lato-store/src/file.rs`
- Modify: `crates/lato-store/src/projection.rs`
- Modify: `crates/lato-store/src/memory.rs`
- Modify: `crates/lato-store/tests/projection_recovery.rs`
- Modify: `crates/lato-store/tests/store_contract.rs`

**Interfaces:**
- Consumes: `projection_message`, `HistoryProjectionEntry`, and Task 2 filesystem primitives.
- Produces: writer-owned `Append` behavior that commits canonical data first, then updates derived history and metadata; projection failures are logged with stable codes and mark the writer projection dirty.

- [ ] **Step 1: Add failing source-first and recoverable-derived-failure tests**

```rust
#[tokio::test]
async fn canonical_append_survives_projection_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let faults = CountingFault::once(FaultPoint::BeforeProjectionWrite);
    let store = FileEventStore::open_with_fault_injector(dir.path(), faults).unwrap();
    let sid = SessionId::from("projection-failure");
    store.append(message_envelope(&sid, 0, "saved"), JournalDurability::SyncData).await.unwrap();
    let reopened = FileEventStore::open(dir.path()).unwrap();
    let replay = reopened.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes.len(), 1);
    assert_eq!(replay.projection.messages.len(), 1);
}

#[tokio::test]
async fn non_message_records_advance_projection_cursor_without_adding_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(dir.path()).unwrap();
    let sid = SessionId::from("projection-cursor");
    store.append(session_started(&sid, 0), JournalDurability::SyncData).await.unwrap();
    let meta = store.read_history_metadata(&sid).unwrap();
    assert_eq!(meta.last_journal_sequence, 0);
    assert_eq!(meta.entry_count, 0);
}
```

Extend `FaultPoint` with projection write/flush/sync/rename/metadata boundaries and test each boundary.

- [ ] **Step 2: Run the focused tests and verify they fail**

Run: `cargo test -p lato-store --test projection_recovery canonical_append_survives_projection_write_failure`

Run: `cargo test -p lato-store --test projection_recovery non_message_records_advance_projection_cursor_without_adding_history`

Expected: tests fail because ordinary writer appends do not maintain derived history.

- [ ] **Step 3: Move projection maintenance inside `WriterHandle`**

Change `WriterHandle::spawn` to accept the session directory. After `append_with_recovery` returns canonical success, call:

```rust
match projection::apply_committed_envelope(&session_id, &session_dir, &envelope, faults.as_ref()) {
    Ok(()) => projection_dirty = false,
    Err(error) => {
        projection_dirty = true;
        tracing::warn!(code = error.code(), %error, "canonical record committed; history projection needs rebuild");
    }
}
```

Do not return the projection error through the canonical append acknowledgement. Before the next derived append, rebuild a dirty projection from the canonical journal; if rebuild still fails, keep logging one structured warning per attempted canonical append without changing canonical success.

- [ ] **Step 4: Mirror projection state in `MemoryEventStore`**

Store `MemorySession { envelopes, messages, metadata, checkpoints }` rather than a bare envelope vector. Apply the candidate canonical envelopes first, then recompute derived messages and metadata deterministically. Keep the public `EventStore` behavior unchanged so runtime tests remain transport-independent.

- [ ] **Step 5: Run writer, recovery, and shared contract suites**

Run: `cargo fmt --all && cargo test -p lato-store --test projection_recovery && cargo test -p lato-store --test store_contract && cargo test -p lato-store --test file_recovery`

Expected: all suites pass, including every existing journal fault boundary.

- [ ] **Step 6: Commit source-first dual persistence**

```bash
git add crates/lato-store/src/writer.rs crates/lato-store/src/file.rs crates/lato-store/src/projection.rs crates/lato-store/src/memory.rs crates/lato-store/tests/projection_recovery.rs crates/lato-store/tests/store_contract.rs
git commit -m "feat: persist derived history after journal commits"
```

---

### Task 4: Implement checkpoint-first logical history replacement

**Files:**
- Modify: `crates/lato-core/src/projection.rs`
- Modify: `crates/lato-store/src/projection.rs`
- Modify: `crates/lato-store/src/writer.rs`
- Modify: `crates/lato-store/src/file.rs`
- Modify: `crates/lato-store/src/memory.rs`
- Modify: `crates/lato-store/tests/projection_recovery.rs`
- Modify: `crates/lato-core/tests/journal_contract.rs`

**Interfaces:**
- Consumes: Task 1 `HistoryCheckpoint` and `HistoryProjectionReplaced` record.
- Produces: `HistoryProjectionStore::replace_history`, `FileEventStore::replace_history`, writer `ReplaceHistory` command, and checkpoint validation during replay.

- [ ] **Step 1: Write failing checkpoint-order and divergence tests**

```rust
#[tokio::test]
async fn checkpoint_is_durable_before_replacement_marker() {
    let dir = tempfile::tempdir().unwrap();
    let trace = RecordingFaultInjector::default();
    let store = FileEventStore::open_with_fault_injector(dir.path(), Arc::new(trace.clone())).unwrap();
    let sid = seeded_session(&store).await;
    let summary = ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text { text: "summary".into() }],
    };
    store.replace_history(&sid, vec![summary], HistoryReplacementReason::Repair).await.unwrap();
    assert!(trace.position(FaultPoint::AfterCheckpointRename)
        < trace.position(FaultPoint::BeforeCheckpointMarkerAppend));
    assert!(store.checkpoint_paths(&sid).unwrap().len() == 1);
}

#[tokio::test]
async fn missing_checkpoint_referenced_by_journal_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(dir.path()).unwrap();
    let sid = seeded_and_replaced_session(&store).await;
    std::fs::remove_file(store.checkpoint_paths(&sid).unwrap().remove(0)).unwrap();
    assert_eq!(store.replay(&sid).await.unwrap_err().code(), "projection.checkpoint_missing");
}
```

Add fault tests before/after checkpoint write, sync, rename, marker append, history publish, and metadata publish. Assert a prepared unresolved call rejects replacement without writing any checkpoint.

- [ ] **Step 2: Run checkpoint tests and verify replacement APIs are missing**

Run: `cargo test -p lato-store --test projection_recovery checkpoint_is_durable_before_replacement_marker`

Run: `cargo test -p lato-store --test projection_recovery missing_checkpoint_referenced_by_journal_fails_closed`

Expected: compilation fails on `replace_history` and checkpoint helpers.

- [ ] **Step 3: Add the object-safe replacement contract**

```rust
#[async_trait::async_trait]
pub trait HistoryProjectionStore: Send + Sync {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError>;
}
```

Implement it for file and memory stores. `FileEventStore` queues a writer `ReplaceHistory` command; it never writes checkpoint files directly from the caller task.

- [ ] **Step 4: Implement the exact checkpoint-first writer transaction**

The writer validates idle terminal state and no unresolved tools by replaying the canonical journal. It creates a unique checkpoint ID from session ID, next sequence, and the checkpoint digest; writes/syncs/renames the checkpoint; appends `HistoryProjectionReplaced` with `SyncData`; atomically publishes replacement history then metadata; and returns metadata only after directory sync.

If the marker is committed but derived publication fails, return `projection.write_failed`; resume must use the marker and checkpoint to rebuild. If the marker was not committed, leave any orphan checkpoint ignored and eligible for conservative cleanup after its digest and session are validated.

- [ ] **Step 5: Remove the healthy-path full model projection**

Use `validate_journal` for canonical safety state and digest. If derived metadata matches, fill `JournalReplay.projection.messages` from `history.jsonl`. Call `project_journal` only inside the rebuild closure or when no derived projection exists. Preserve `JournalReplay.envelopes` for current runtime bootstrap compatibility.

- [ ] **Step 6: Run core and store suites**

Run: `cargo fmt --all && cargo test -p lato-core --test journal_contract && cargo test -p lato-store`

Expected: all tests pass; healthy replay uses the derived message vector, checkpoint mismatch fails closed, and source journals remain unchanged.

- [ ] **Step 7: Commit checkpoint replacement**

```bash
git add crates/lato-core/src/projection.rs crates/lato-core/tests/journal_contract.rs crates/lato-store/src/projection.rs crates/lato-store/src/writer.rs crates/lato-store/src/file.rs crates/lato-store/src/memory.rs crates/lato-store/tests/projection_recovery.rs
git commit -m "feat: checkpoint history replacements"
```

---

### Task 5: Integrate resume behavior and migration across clients

**Files:**
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/tests/journal_runtime.rs`
- Create: `tests/session_projection_cli.rs`

**Interfaces:**
- Consumes: the unchanged `EventStore::replay -> JournalReplay` facade, now backed by validated derived messages.
- Produces: lazy projection creation for existing sessions and identical ACP/headless/TUI resume behavior.

- [ ] **Step 1: Add failing agent tests for lazy migration and unknown-side-effect parity**

```rust
#[tokio::test]
async fn phase_4a_session_resume_materializes_history_projection() {
    let home = tempfile::tempdir().unwrap();
    let sid = seed_phase_4a_journal(home.path()).await;
    assert!(!history_path(home.path(), &sid).exists());
    let mut host = test_host_with_home(home.path());
    host.resume_for_test(&sid).await.unwrap();
    assert!(history_path(home.path(), &sid).is_file());
}

#[tokio::test]
async fn derived_history_never_masks_unknown_side_effect() {
    let home = tempfile::tempdir().unwrap();
    let sid = seed_unresolved_prepared_call_with_valid_history(home.path()).await;
    let error = test_host_with_home(home.path()).resume_for_test(&sid).await.unwrap_err();
    assert!(error.contains("journal.incomplete_side_effect"));
}
```

- [ ] **Step 2: Run focused agent tests and verify migration behavior is absent**

Run: `cargo test -p lato-agent --test journal_runtime projection`

Expected: at least the materialization assertion fails.

- [ ] **Step 3: Keep all clients on the common host replay path**

Do not add TUI-, CLI-, or ACP-specific projection loading. In `AcpHost::make_runtime_session`, continue passing the `JournalReplay` returned by `FileEventStore`; map `ProjectionError` through its stable code and `ErrorCategory::Storage`. In `RuntimeSession::new_with_store`, retain the unresolved-tool check before hydrating driver history from `replay.projection.messages`.

- [ ] **Step 4: Add CLI integration coverage**

Create a pre-Phase-4B `events.jsonl` fixture through `FileEventStore`, then exercise the binary's stdio ACP entry point so the test does not require a terminal:

```rust
let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_lato"))
    .arg("acp")
    .env("LATO_HOME", home.path())
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
use std::io::Write;
writeln!(child.stdin.as_mut().unwrap(),
    "{}", serde_json::json!({
        "jsonrpc":"2.0", "id":1, "method":"session/resume",
        "params":{"sessionId":sid.as_str()}
    })
).unwrap();
drop(child.stdin.take());
let output = child.wait_with_output().unwrap();
assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
```

Assert `history.jsonl` and `history.meta.json` exist and a second ACP resume preserves the conversation. Corrupt only `history.jsonl` and assert resume succeeds plus `.corrupt` exists; corrupt `events.jsonl` and assert nonzero response containing `journal.parse`.

- [ ] **Step 5: Run client and integration tests**

Run: `cargo fmt --all && cargo test -p lato-agent --test journal_runtime && cargo test --test session_projection_cli && cargo test --test sessions_cli`

Expected: all tests pass and no client has an alternate persistence path.

- [ ] **Step 6: Commit resume integration**

```bash
git add crates/lato-agent/src/host.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/tests/journal_runtime.rs tests/session_projection_cli.rs
git commit -m "feat: resume sessions through derived history"
```

---

### Task 6: Document provenance, validate, and deploy locally

**Files:**
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: completed Phase 4B implementation.
- Produces: source attribution, user-facing storage documentation, complete validation evidence, and installed `lato` binary.

- [ ] **Step 1: Add exact derivation headers and source-ledger rows**

Add this header to structurally derived production files:

```rust
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs
// License: Apache-2.0
// Lato changes: reduced chat-history persistence to a canonical-journal-derived history projection with strict validation
```

Add separate ledger references for `session/persistence.rs`, `xai-chat-state/src/persistence.rs`, `session/compaction.rs`, and `session/helpers/replay.rs` wherever their transaction or replay structure is substantially reused.

- [ ] **Step 2: Update README recovery documentation**

Document that `events.jsonl` remains authoritative; `history.jsonl` and metadata are private derived state; missing/stale/damaged derived history is rebuilt only after canonical validation; checkpoint mismatches and canonical damage fail closed; canonical retention and size caps remain unchanged.

- [ ] **Step 3: Run the full validation gates**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `cargo test --workspace`

Expected: all workspace tests pass.

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

Run: `git diff --check HEAD~5`

Expected: exit 0; inspect `git status --short` and verify unrelated pre-existing dirty files remain unstaged and unchanged.

- [ ] **Step 4: Install the verified binary**

Run: `cargo install --path .`

Expected: release build succeeds and installs/replaces the local `lato` executable.

- [ ] **Step 5: Run isolated installed-binary smoke tests**

```bash
LATO_SMOKE_HOME=$(mktemp -d)
LATO_HOME="$LATO_SMOKE_HOME" lato -p "reply with hi only"
lato --version
```

Expected: first command prints `hi`; second command prints the current Lato version. Preserve the temporary directory path in test output until the smoke result is recorded; do not delete user session directories.

- [ ] **Step 6: Commit documentation and provenance**

```bash
git add README.md docs/superpowers/reference/lato-upstream-sources.md
git commit -m "docs: document grok-style session recovery"
```

---

## Completion Checklist

- [ ] `events.jsonl` remains append-only and authoritative.
- [ ] Healthy derived history supplies model messages after complete canonical validation.
- [ ] Missing, stale, or corrupt derived history rebuilds deterministically.
- [ ] The first corrupt projection is preserved and quarantine failure gates replacement.
- [ ] Checkpoint file publication precedes its canonical marker.
- [ ] Metadata publication is the derived-generation commit point.
- [ ] Unresolved side effects fail exactly as in Phase 4A.
- [ ] Legacy and Phase 4A sessions migrate lazily.
- [ ] Workspace tests and Clippy pass.
- [ ] `cargo install --path .` completes and the installed binary passes offline smoke tests.
