# Slash Command Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a keyboard-navigable, real-time filtered list of every slash command when `/` is entered in the TUI composer on either the welcome or main screen.

**Architecture:** Add a focused `tui::commands` module as the single slash-command registry and completion-query API. Keep transient selection and dismissal state in `AppState`, route completion keys before ordinary composer keys, and render a bounded popup above the composer without replacing the existing Cmd/Ctrl+K palette.

**Tech Stack:** Rust 2024, Crossterm, Ratatui, existing TUI unit/render/integration tests.

## Global Constraints

- Preserve all existing slash-command behavior and the Cmd/Ctrl+K command palette.
- Show aliases because they are accepted commands.
- Use case-insensitive prefix filtering and hide completion after arguments begin.
- Keep welcome and main-screen behavior identical.
- Add no dependency and introduce no backend side effects.
- Preserve unrelated dirty-worktree changes.

---

### Task 1: Shared command registry and completion query

**Files:**
- Create: `src/tui/commands.rs`
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/input.rs`
- Test: `src/tui/commands.rs`
- Test: `src/tui/input.rs`

**Interfaces:**
- Produces: `pub struct SlashCommand { pub name: &'static str, pub description_zh: &'static str, pub description_en: &'static str }`.
- Produces: `pub const SLASH_COMMANDS: &[SlashCommand]` in help-display order.
- Produces: `pub fn matches(input: &str) -> Vec<&'static SlashCommand>` for case-insensitive prefix matches only when `input` starts with `/` and contains no whitespace.
- Produces: `pub fn help_line() -> String` generated from the registry.
- Produces: `InputBuffer::replace(&mut self, value: &str)` with the cursor at the new text end.

- [x] **Step 1: Add failing registry tests**

Add tests proving that `matches("/")` returns all 17 accepted names, `matches("/MO")` returns only `/model`, `matches("/rename title")` and `matches("/unknown")` are empty, aliases `/language` and `/quit` are present, and `help_line()` includes every registry name in order.

- [x] **Step 2: Run the focused tests and verify failure**

Run: `cargo test --bin lato tui::commands -- --nocapture`

Expected: compilation fails because `tui::commands` and its API do not exist.

- [x] **Step 3: Implement the registry and query**

Create `src/tui/commands.rs` with the exact accepted command names and bilingual concise descriptions. Implement `matches` by rejecting empty input, non-slash input, or any whitespace, lowercasing the input, and filtering `SLASH_COMMANDS` with `command.name.starts_with(&prefix)`. Generate `/help` text with `SLASH_COMMANDS.iter().map(|command| command.name).collect::<Vec<_>>().join("  ")`.

Expose the module from `src/tui/mod.rs` with:

```rust
mod commands;
```

Add the composer replacement operation in `src/tui/input.rs`:

```rust
pub fn replace(&mut self, value: &str) {
    self.text.clear();
    self.text.push_str(value);
    self.cursor = self.text.len();
}
```

Add a test asserting that replacement discards prior text and places subsequent inserted text at the end.

- [x] **Step 4: Run focused tests**

Run: `cargo test --bin lato tui:: -- --nocapture`

Expected: all command-registry and input-buffer tests pass.

- [x] **Step 5: Commit the registry**

```bash
git add src/tui/commands.rs src/tui/mod.rs src/tui/input.rs
git commit -m "feat: centralize slash command metadata"
```

### Task 2: Completion state and keyboard behavior

**Files:**
- Modify: `src/tui/state.rs`
- Modify: `src/tui/mod.rs`
- Test: `src/tui/mod.rs`

**Interfaces:**
- Consumes: `commands::matches(&str) -> Vec<&'static SlashCommand>` and `InputBuffer::replace(&str)` from Task 1.
- Produces: `AppState::slash_completion() -> Vec<&'static SlashCommand>`.
- Produces: composer-edit synchronization that resets dismissal and clamps selection.
- Produces: completion handling for Up, Down, Enter, and Escape.

- [ ] **Step 1: Add failing keyboard tests**

Extend the existing `slash_commands_and_palette_stay_in_the_tui` coverage and add focused tests asserting:

```rust
// '/' opens all candidates, '/mo' filters to /model.
// Down changes selection and never exceeds the filtered length.
// Enter on '/' replaces composer text with the selected command and emits no effect.
// Enter on an exact '/model' produces Effect::ConfigureModel.
// Escape dismisses completion without clearing '/'.
// A following edit reopens completion.
// Backspace from '/mo' to '/m' recomputes candidates.
// Paste of '/sta' exposes only '/status'.
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test --bin lato slash_completion -- --nocapture`

Expected: tests fail because completion state and key routing do not exist.

- [ ] **Step 3: Add state and edit synchronization**

Add `slash_completion_index: usize` and `slash_completion_dismissed: bool` to `AppState`, initialize them to `0` and `false`, and add methods which derive candidates from the current composer. After every character insertion, paste, Backspace, Delete, and replacement, set dismissal to false and clamp the index to `candidate_count.saturating_sub(1)`.

Keep cursor-only Left/Right/Home/End operations unchanged because they do not change the completion query.

- [ ] **Step 4: Route completion keys**

Before ordinary composer handling, when non-dismissed candidates exist:

```rust
Up    => decrement slash_completion_index,
Down  => increment it within candidate bounds,
Esc   => set slash_completion_dismissed = true,
Enter => if composer text exactly equals the selected command (ignoring ASCII case),
         call submit_or_command; otherwise replace composer text with the selected name.
```

Do not intercept keys while approval, search, or the Cmd/Ctrl+K palette is active. Preserve Tools-panel key ownership. Replace the hard-coded `/help` message with `commands::help_line()`.

- [ ] **Step 5: Run TUI behavior tests**

Run: `cargo test --bin lato tui:: -- --nocapture`

Expected: all TUI unit tests pass, including exact-match execution and existing palette behavior.

- [ ] **Step 6: Commit keyboard behavior**

```bash
git add src/tui/state.rs src/tui/mod.rs
git commit -m "feat: navigate slash command completion"
```

### Task 3: Popup rendering, regression verification, and deployment

**Files:**
- Modify: `src/tui/render.rs`
- Modify: `src/tui/widgets.rs`
- Test: `src/tui/render.rs`
- Test: `tests/tui_pty_smoke.py`

**Interfaces:**
- Consumes: `AppState::slash_completion()` and `AppState::slash_completion_index` from Task 2.
- Produces: `widgets::slash_completion(frame: &mut Frame<'_>, composer_area: Rect, app: &AppState)`.

- [ ] **Step 1: Add failing render tests**

Refactor the render-test helper to accept composer text, then assert that `/` renders the first visible candidates such as `/help` and `/new` on both `Screen::Welcome` and `Screen::Main`, `/mo` renders `/model` but not `/help`, and a 60-column narrow layout retains both the composer cursor and the selected candidate. Registry tests remain responsible for proving all 17 commands are available even when terminal height requires a scrolling window.

- [ ] **Step 2: Run render tests and verify failure**

Run: `cargo test --bin lato tui::render::tests -- --nocapture`

Expected: the new assertions fail because no completion popup is rendered.

- [ ] **Step 3: Implement the popup**

Add `widgets::slash_completion` using `Clear`, `Block`, and `List`. Pass the composer rectangle from the shared `composer` renderer so the popup is anchored correctly in welcome, wide, medium, and narrow layouts. Place a bounded popup immediately above that rectangle, cap its height to available terminal space, keep the selected item visible by choosing a window around `slash_completion_index`, and render each row as `command.name` plus the localized description. Use `AMBER` for the selected row and existing `RAISED`, `TEXT`, and `MUTED` colors elsewhere.

Call the widget at the end of the shared `composer` renderer. Do not render it when any modal overlay or approval is active; modal overlays are still drawn afterward by `render`, so they remain authoritative.

- [ ] **Step 4: Extend the PTY smoke interaction**

In `tests/tui_pty_smoke.py`, type `/`, wait for a known command such as `/permissions`, type `mo`, verify the filtered `/model` result, press Enter to complete, and press Enter again or Escape to leave the fixture in its existing deterministic state.

- [ ] **Step 5: Run formatting and focused regression tests**

Run:

```bash
cargo fmt --check
cargo test --bin lato tui:: -- --nocapture
cargo test --test tui_cli -- --nocapture
python3 tests/tui_pty_smoke.py
```

Expected: formatting is clean; all Rust tests pass; the PTY test passes or reports a documented environment-only skip.

- [ ] **Step 6: Run workspace checks**

Run:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: all tests pass and Clippy reports no warnings.

- [ ] **Step 7: Commit rendering and tests**

```bash
git add src/tui/render.rs src/tui/widgets.rs tests/tui_pty_smoke.py
git commit -m "feat: show slash command suggestions"
```

- [ ] **Step 8: Install and smoke-test the local command**

Run:

```bash
cargo install --path .
lato --version
```

Expected: installation succeeds and `lato --version` reports the repository package version.
