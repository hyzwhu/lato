# Session Metadata and Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic session titles, manual rename, and confirmed permanent deletion to Lato's CLI and TUI without changing the canonical journal format or existing ACP `session/list` response.

**Architecture:** Follow Grok Build's per-session mutable summary pattern: `metadata.json` and a stable lock file live beside `events.jsonl`, with locked read-modify-write and atomic replacement. `lato-store` owns metadata and deletion safety, `lato-agent` owns live-session coordination and extension RPCs, and the CLI/TUI consume structured summaries through the same client functions.

**Tech Stack:** Rust 2024, Tokio, Serde/serde_json, fs2 file locks, Clap, Ratatui/Crossterm, existing JSON-RPC ACP host.

## Global Constraints

- Keep journal schema version 1 and existing `session/list` output unchanged.
- Automatic titles are local, deterministic, and derived from the first accepted user prompt; no model request is allowed.
- Titles remove C0/C1 controls, collapse whitespace, trim quotes, and contain at most 60 Unicode scalar values.
- Manual titles override automatic titles and must not be overwritten by automatic initialization.
- Permanent deletion requires explicit confirmation in human-facing clients and never accepts a filesystem path from the caller.
- Old sessions remain resumable with no eager migration.
- Preserve unrelated dirty-worktree files.

---

### Task 1: Per-session metadata store

**Files:**
- Create: `crates/lato-store/src/metadata.rs`
- Modify: `crates/lato-store/src/lib.rs`
- Modify: `crates/lato-store/Cargo.toml`
- Test: `crates/lato-store/tests/session_metadata.rs`

**Interfaces:**
- Produces: `SessionMetadata`, `SessionSummary`, `TitleSource`, `normalize_manual_title`, `derive_automatic_title`.
- Produces methods on `FileEventStore`: `list_session_summaries`, `ensure_automatic_title`, `rename_session`, `delete_session`.
- Consumes: existing `FileEventStore::journal_path`, `EventStore::replay`, and `SessionId` validation.

- [ ] **Step 1: Add failing metadata contract tests**

```rust
#[tokio::test]
async fn automatic_title_is_unicode_safe_and_manual_title_wins() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    store.ensure_automatic_title(&id, "  修复\n登录\u{1b}[31m 流程  ").await.unwrap();
    store.rename_session(&id, "  手动\n标题  ").await.unwrap();
    store.ensure_automatic_title(&id, "ignored").await.unwrap();
    let summaries = store.list_session_summaries().await.unwrap();
    assert_eq!(summaries[0].title, "手动 标题");
    assert_eq!(summaries[0].title_source, TitleSource::Manual);
}
```

- [ ] **Step 2: Run the new test and verify it fails**

Run: `cargo test -p lato-store --test session_metadata`

Expected: compilation fails because the metadata types and methods do not exist.

- [ ] **Step 3: Implement metadata types, normalization, locked atomic updates, legacy fallback, and deletion**

```rust
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource { Automatic, Manual }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub title: String,
    pub title_source: TitleSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub title: String,
    pub title_source: TitleSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
```

Use `fs2::FileExt::lock_exclusive` on `metadata.json.lock`, re-read under the lock, and call an internal `write_metadata_atomic` that writes a unique sibling temporary file with mode `0600` on Unix, flushes, calls `sync_data`, renames, and syncs the parent directory. Validate symlinks and `session_id` equality before mutation. `delete_session` must shut down the store writer first, validate that the target is a real directory beneath `sessions`, then call `remove_dir_all`; `NotFound` succeeds.

- [ ] **Step 4: Expand tests for failure and concurrency paths**

```rust
#[tokio::test]
async fn concurrent_auto_and_manual_updates_never_lose_manual_title() {
    let home = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(FileEventStore::open(home.path()).unwrap());
    let id = SessionId::from("session-1");
    let automatic = tokio::spawn({
        let store = store.clone();
        let id = id.clone();
        async move { store.ensure_automatic_title(&id, "Automatic title").await.unwrap() }
    });
    let manual = tokio::spawn({
        let store = store.clone();
        let id = id.clone();
        async move { store.rename_session(&id, "Manual title").await.unwrap() }
    });
    automatic.await.unwrap();
    manual.await.unwrap();
    let summary = store.list_session_summaries().await.unwrap().remove(0);
    assert_eq!((summary.title.as_str(), summary.title_source), ("Manual title", TitleSource::Manual));
}

#[tokio::test]
async fn corrupt_metadata_blocks_rename() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    let journal = store.journal_path(&id).unwrap();
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(journal.parent().unwrap().join("metadata.json"), b"{").unwrap();
    assert!(store.rename_session(&id, "Manual").await.is_err());
}

#[tokio::test]
async fn newer_metadata_schema_is_rejected() {
    let home = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    let journal = store.journal_path(&id).unwrap();
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(journal.parent().unwrap().join("metadata.json"), br#"{"schema_version":2}"#).unwrap();
    assert!(store.rename_session(&id, "Manual").await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn delete_is_idempotent_and_rejects_symlink_targets() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = FileEventStore::open(home.path()).unwrap();
    let id = SessionId::from("session-1");
    let session_dir = store.journal_path(&id).unwrap().parent().unwrap().to_path_buf();
    std::os::unix::fs::symlink(outside.path(), &session_dir).unwrap();
    assert!(store.delete_session(&id).await.is_err());
    assert!(outside.path().exists());
}
```

- [ ] **Step 5: Run store tests and commit**

Run: `cargo test -p lato-store`

Expected: all `lato-store` tests pass.

Commit: `git commit -m "feat: add durable session metadata store"`

### Task 2: Agent extension methods and live-session safety

**Files:**
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-protocol/src/methods.rs`
- Test: existing test module in `crates/lato-agent/src/host.rs`

**Interfaces:**
- Consumes: Task 1 `SessionSummary` and `FileEventStore` mutation methods.
- Produces RPC methods `lato/session/list`, `lato/session/rename`, and `lato/session/delete`.
- Produces: `RuntimeSession::is_active() -> bool`.

- [ ] **Step 1: Add failing host tests for structured list, rename, delete, and compatibility**

```rust
#[tokio::test]
async fn session_admin_extensions_preserve_legacy_list_shape() {
    // Create a session, prompt once, assert session/list remains string IDs.
    // Assert lato/session/list returns objects with sessionId/title/timestamps.
    // Rename and verify titleSource == "manual".
    // Delete and verify the session disappears from both lists.
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

Run: `cargo test -p lato-agent host::tests::session_admin_extensions_preserve_legacy_list_shape`

Expected: FAIL with `method not found` for the Lato extension.

- [ ] **Step 3: Implement active-state inspection and extension handlers**

```rust
pub async fn is_active(&self) -> bool {
    self.active_turn.lock().await.is_some()
}
```

Add the three methods to the advertised implemented-method list. Parse session IDs with `SessionId::parse`, validate existence before mutation, reject rename/delete with a stable `session_busy` error when resident `is_active()` is true, and return camelCase structured summaries. Delete removes the resident entry, calls `shutdown`, then delegates to the store.

- [ ] **Step 4: Initialize automatic metadata after durable prompt acceptance**

Pass the original prompt text to `ensure_automatic_title` after `RuntimeSession::prompt` returns or as soon as the accepted journal record is known durable. Treat metadata failure as non-fatal and retain journal-derived fallback behavior.

- [ ] **Step 5: Test busy, unknown, and failure behavior**

Run: `cargo test -p lato-agent`

Expected: all agent tests pass, including unchanged `session/list` assertions.

Commit: `git commit -m "feat: expose safe session administration"`

### Task 3: Shared client and CLI session commands

**Files:**
- Modify: `src/args.rs`
- Modify: `src/client.rs`
- Modify: `src/sessions.rs`
- Modify: `src/cli.rs`
- Test: `src/args.rs` test module
- Test: `tests/sessions_cli.rs`

**Interfaces:**
- Consumes: Task 2 extension methods.
- Produces: `SessionCommand::{List { json }, Rename { session_id, title }, Delete { session_id, yes }}` inside `Invocation::Sessions`.
- Produces client functions `list_session_summaries_over_acp`, `rename_session_over_acp`, `delete_session_over_acp`.

- [ ] **Step 1: Add failing parser tests**

```rust
assert_eq!(
    parse(vec!["sessions".into(), "rename".into(), "s1".into(), "New title".into()]).unwrap(),
    Invocation::Sessions(SessionCommand::Rename { session_id: "s1".into(), title: "New title".into() })
);
assert!(matches!(
    parse(vec!["sessions".into(), "delete".into(), "s1".into(), "--yes".into()]).unwrap(),
    Invocation::Sessions(SessionCommand::Delete { yes: true, .. })
));
```

- [ ] **Step 2: Run parser tests and verify failure**

Run: `cargo test --bin lato args::tests`

Expected: compilation fails because `SessionCommand` does not exist.

- [ ] **Step 3: Implement compatible nested Clap commands and client RPC helpers**

Plain `lato sessions` and `lato sessions --json` remain valid. Rename joins the title argument according to Clap's single quoted shell argument behavior and delegates normalization to the server. JSON output becomes:

```json
{"schema_version":2,"sessions":[{"id":"s1","title":"New title","title_source":"manual","created_at_ms":1,"updated_at_ms":2}]}
```

- [ ] **Step 4: Implement permanent-delete confirmation**

Use `std::io::IsTerminal`. Without `--yes`, require stdin and stderr/stdout to be terminal-backed, print the exact session ID and title, and accept only an explicit `y`/`yes`. Cancellation returns exit code 0 without mutation. Non-interactive use without `--yes` returns exit code 2 with guidance.

- [ ] **Step 5: Run CLI integration tests and commit**

Run: `cargo test --bin lato args::tests && cargo test --test sessions_cli`

Expected: parser and process-level list/rename/delete tests pass.

Commit: `git commit -m "feat: add session management commands"`

### Task 4: TUI structured session display and mutations

**Files:**
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/event.rs`
- Modify: `src/tui/backend.rs`
- Modify: `src/tui/widgets.rs`
- Modify: `src/tui/i18n.rs`
- Test: existing unit tests beside these modules
- Test: `tests/tui_cli.rs`

**Interfaces:**
- Consumes: Task 3 structured summaries and client helpers.
- Produces state effects `RenameSession`, `ArmDeleteSession`, `DeleteSession` and a selection-bound delete deadline.
- Produces slash commands `/rename <title>` and `/delete`.

- [ ] **Step 1: Add failing TUI state tests**

```rust
#[test]
fn delete_confirmation_is_bound_to_selected_session() {
    let mut app = test_app_with_sessions(["s1", "s2"]);
    assert_eq!(app.arm_or_confirm_session_delete(), None);
    app.select_next_session();
    assert_eq!(app.arm_or_confirm_session_delete(), None);
}

#[test]
fn session_filter_matches_title_and_id() {
    let item = SessionItem { id: "s123".into(), title: "Fix login".into(), timestamp: String::new() };
    assert!(item.matches("login"));
    assert!(item.matches("s123"));
    assert!(!item.matches("billing"));
}
```

- [ ] **Step 2: Run focused TUI tests and verify failure**

Run: `cargo test --bin lato tui::`

Expected: FAIL because structured summaries and delete-arm state are absent.

- [ ] **Step 3: Render title-primary session rows**

Construct `SessionItem` from `SessionSummary`; render the title first and a dimmed shortened durable ID second. The current session marker and selection highlight remain unchanged. Empty/corrupt metadata falls back to the derived title supplied by the store.

- [ ] **Step 4: Add rename and two-step delete interactions**

When session focus is active, `r` invokes the existing async input dialog with the current title. `d` stores `(session_id, Instant)`; a second `d` for the same selected ID before expiry calls delete. Navigation, Escape, unrelated keys, and timeout clear the arm. Recheck `responding == false` before emitting either mutation.

- [ ] **Step 5: Add slash commands and bilingual copy**

`/rename <title>` targets the current session and `/delete` opens a Delete/Cancel choice stating that history is permanently removed. Update command palette/help and both English and Chinese translations in one change.

- [ ] **Step 6: Test state transitions, rendering, and failure rollback**

Run: `cargo test --bin lato tui && cargo test --test tui_cli`

Expected: all TUI tests pass; failed mutations keep the row and display the backend error.

Commit: `git commit -m "feat: manage titled sessions in the TUI"`

### Task 5: Documentation, regression gates, and local deployment

**Files:**
- Modify: `README.md`
- Modify: `tests/sessions_cli.rs`

**Interfaces:**
- Consumes all prior tasks.
- Produces user documentation for list, rename, delete, TUI keys, and deletion permanence.

- [ ] **Step 1: Update user documentation**

Document:

```text
lato sessions
lato sessions --json
lato sessions rename SESSION_ID "New title"
lato sessions delete SESSION_ID
lato sessions delete SESSION_ID --yes
```

Remove the limitation claiming sessions expose only IDs and explicitly state that deletion is permanent.

- [ ] **Step 2: Run formatting and targeted lint**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: both commands exit 0.

- [ ] **Step 3: Run the complete regression suite**

Run: `cargo test --workspace`

Expected: all workspace tests pass.

- [ ] **Step 4: Inspect the final diff and protect user-owned files**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; pre-existing changes under `docs/testing`, `.lato`, and `docs/.DS_Store` remain uncommitted and unmodified by this feature.

- [ ] **Step 5: Commit documentation and deploy locally**

Commit: `git commit -m "docs: document titled session management"`

Run: `cargo install --path .`

Expected: installation succeeds and `lato --version` reports the locally built package version.
