# Grok-style TUI Session Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `/new`, `/clear`, and `/rename` follow `grok-build` session semantics and produce visible, backend-confirmed TUI state changes.

**Architecture:** Keep persistence in `InteractiveAcpClient` and the backend task, but give fresh-session creation explicit command/event names instead of treating it as a visual clear. The reducer resets the UI only after the backend confirms the new session; rename completion stays composer-driven and rename success adds an acknowledgment without leaving the conversation.

**Tech Stack:** Rust, Tokio channels, Ratatui/Crossterm TUI state reducer, Rust unit and integration tests.

## Global Constraints

- `/new` starts a fresh persisted session and presents Welcome only after backend success.
- `/clear` is an alias for `/new`.
- `/rename <title>` preserves entered case, keeps the conversation open, and visibly reports success.
- Exact `/rename` uses composer completion and does not open a dialog.
- Backend failures retain the current session and conversation.
- Do not add `/rename --auto`.
- Run relevant tests before `cargo install --path .`.

---

### Task 1: Make fresh-session creation acknowledgment-driven

**Files:**
- Modify: `src/tui/backend.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/mod.rs`
- Test: inline tests in `src/tui/state.rs` and `src/tui/mod.rs`

**Interfaces:**
- Consumes: `InteractiveAcpClient::clear(&mut self) -> Result<(), String>`, which already calls ACP `session/new` and replaces the client session ID.
- Produces: `BackendCommand::NewSession`, `BackendEvent::NewSessionCreated(String)`, and `AppEvent::NewSession`.

- [ ] **Step 1: Write failing reducer and command-dispatch tests**

Add a reducer test in `src/tui/state.rs` that populates messages, tools, scroll, focus, error, and a Main screen, applies `BackendEvent::NewSessionCreated("session-2".into())`, and asserts:

```rust
assert_eq!(app.session_id, "session-2");
assert_eq!(app.screen, Screen::Welcome);
assert!(app.messages.is_empty());
assert!(app.tools.is_empty());
assert_eq!(app.scroll, 0);
assert_eq!(app.focus, Focus::Chat);
assert!(app.error.is_none());
```

Add a command test in `src/tui/mod.rs` that submits `/new` and `/clear` from a populated Main screen and checks each result:

```rust
assert!(matches!(effects.as_slice(), [Effect::Backend(BackendCommand::NewSession)]));
assert_eq!(app.session_id, "current");
assert_eq!(app.screen, Screen::Main);
assert_eq!(app.messages, original_messages);
```

- [ ] **Step 2: Run focused tests and verify failure**

```bash
cargo test tui::state::tests::new_session_acknowledgement_resets_to_welcome
cargo test tui::tests::new_and_clear_request_a_fresh_session_without_premature_reset
```

Expected: compilation or assertion failure because the explicit variants and acknowledgment transition do not exist.

- [ ] **Step 3: Introduce explicit backend session messages**

In `src/tui/backend.rs`, replace ambiguous clear variants with:

```rust
pub enum BackendCommand {
    Submit(String),
    Cancel,
    NewSession,
    Resume(String),
    RenameSession { session_id: String, title: String },
    DeleteSession(String),
    Shutdown,
}

pub enum BackendEvent {
    SessionReady(String),
    Update(ClientUpdate),
    TurnCompleted(String),
    TurnCancelled,
    NewSessionCreated(String),
    Resumed(String),
    Sessions(Vec<SessionSummary>),
    SessionRenamed(SessionSummary),
    SessionDeleted {
        session_id: String,
        replacement_session_id: Option<String>,
        sessions: Vec<SessionSummary>,
    },
    Error(String),
}
```

Route `BackendCommand::NewSession` through the existing `owned.clear().await` call and emit:

```rust
BackendEvent::NewSessionCreated(owned.session_id().to_string())
```

Update the active-turn rejection match to reject `BackendCommand::NewSession` with the existing running-turn error.

- [ ] **Step 4: Move UI reset to backend success**

In `src/tui/state.rs`, replace `AppEvent::ClearConversation` with `AppEvent::NewSession`. Its reducer arm checks `responding` and otherwise returns `BackendCommand::NewSession` without clearing messages or changing screens.

Handle the acknowledgment separately from initial `SessionReady`:

```rust
BackendEvent::SessionReady(id) => self.session_id = id,
BackendEvent::NewSessionCreated(id) => {
    self.session_id = id;
    self.messages.clear();
    self.tools.clear();
    self.tool_panel = ToolPanelState::default();
    self.composer.clear();
    self.overlay = None;
    self.scroll = 0;
    self.focus = Focus::Chat;
    self.screen = Screen::Welcome;
    self.error = None;
    self.select_current_session();
}
```

In `src/tui/mod.rs`, route `/new`, `/clear`, and the command-palette New/Clear entries through `AppEvent::NewSession`.

- [ ] **Step 5: Run focused tests and commit**

```bash
cargo test tui::state::tests::new_session_acknowledgement_resets_to_welcome
cargo test tui::tests::new_and_clear_request_a_fresh_session_without_premature_reset
git add src/tui/backend.rs src/tui/state.rs src/tui/mod.rs
git commit -m "fix: make tui new session acknowledgement-driven"
```

Expected: both tests pass before the commit.

### Task 2: Match Grok-style rename completion and feedback

**Files:**
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/state.rs`
- Test: inline tests in `src/tui/mod.rs` and `src/tui/state.rs`

**Interfaces:**
- Consumes: `BackendCommand::RenameSession { session_id: String, title: String }` and `BackendEvent::SessionRenamed(SessionSummary)`.
- Produces: composer expansion `/rename <current title>` and a localized `MessageRole::System` acknowledgment.

- [ ] **Step 1: Write failing rename tests**

In `src/tui/mod.rs`, construct an app whose active session title is `Current Title`. Enter exact `/rename` through `handle_slash_completion_key` and assert:

```rust
assert!(effects.is_empty());
assert_eq!(app.composer.as_str(), "/rename Current Title");
```

Invoke Enter again and assert:

```rust
assert!(matches!(effects.as_slice(), [Effect::Backend(
    BackendCommand::RenameSession { session_id, title }
)] if session_id == "current" && title == "Current Title"));
```

Also assert `/rename Keep This Case` preserves case, and a bare `/rename` without an active summary clears the composer, sets a usage error, and emits no rename effect.

In `src/tui/state.rs`, apply `BackendEvent::SessionRenamed` with existing messages and assert the title changes, old messages remain, Main remains active, and the last message is a system acknowledgment containing the new title.

- [ ] **Step 2: Run focused tests and verify failure**

```bash
cargo test tui::tests::exact_rename_completes_current_title_then_submits
cargo test tui::state::tests::rename_acknowledgement_updates_title_and_preserves_conversation
```

Expected: failure because exact `/rename` opens a dialog and rename success does not append feedback.

- [ ] **Step 3: Implement exact-command argument completion**

In `handle_slash_completion_key`, before executing exact `/rename`, look up the active non-empty session title and replace the composer with:

```rust
format!("/rename {}", title.trim())
```

Refresh completion and return no effect. Preserve execute-on-Enter for other exact commands.

Change the bare `/rename` arm in `submit_or_command` to return no effect and show:

```rust
app.error = Some(match app.language {
    Language::ZhCn => "用法：/rename <新标题>".into(),
    Language::En => "Usage: /rename <new title>".into(),
});
app.composer.clear();
```

Keep the parameterized path based on `raw_command` so surrounding whitespace is trimmed while title case is preserved.

- [ ] **Step 4: Add rename success feedback**

After updating the matching session in `BackendEvent::SessionRenamed`, append:

```rust
let title = summary.title.clone();
self.messages.push(Message {
    role: MessageRole::System,
    content: match self.language {
        Language::ZhCn => format!("会话已重命名为“{title}”"),
        Language::En => format!("Session renamed to \"{title}\""),
    },
    expanded: true,
});
self.error = None;
```

Do not change the screen, active session ID, existing messages, or tools.

- [ ] **Step 5: Run focused tests and commit**

```bash
cargo test tui::tests::exact_rename_completes_current_title_then_submits
cargo test tui::state::tests::rename_acknowledgement_updates_title_and_preserves_conversation
git add src/tui/mod.rs src/tui/state.rs
git commit -m "fix: align tui rename flow with grok build"
```

Expected: both tests pass before the commit.

### Task 3: Regression verification and local deployment

**Files:**
- Modify only for formatter corrections: `src/tui/backend.rs`, `src/tui/state.rs`, `src/tui/mod.rs`

**Interfaces:**
- Consumes: Tasks 1 and 2 behavior.
- Produces: a formatted, tested, locally installed `lato` binary.

- [ ] **Step 1: Format and inspect scoped changes**

```bash
cargo fmt --check
git diff --check
git diff -- src/tui/backend.rs src/tui/state.rs src/tui/mod.rs
```

Expected: checks pass and the diff contains only planned session-command changes.

- [ ] **Step 2: Run TUI and session regression tests**

```bash
cargo test tui::
cargo test --test tui_cli
cargo test client::tests::fresh_persisted_session_can_be_renamed_before_first_prompt
```

Expected: all tests pass.

- [ ] **Step 3: Run full workspace tests**

```bash
cargo test --workspace
```

Expected: all tests pass. Report any unrelated pre-existing failure without changing unrelated code.

- [ ] **Step 4: Install the verified binary locally**

```bash
cargo install --path .
```

Expected: Cargo reports that `lato` was installed or replaced successfully.

- [ ] **Step 5: Commit formatter corrections if any**

```bash
git add src/tui/backend.rs src/tui/state.rs src/tui/mod.rs
git commit -m "style: format tui session command changes"
```

Run focused tests again before this commit. Skip the step when formatting made no changes.
