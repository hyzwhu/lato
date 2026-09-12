# Resume Session Selector Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Let `lato resume` accept an exact session ID or title and ask the user to choose when that title belongs to multiple sessions.

**Architecture:** Add a pure reference resolver over existing `SessionSummary` values, then run it before the normal interactive startup prompts. Reuse the existing async TUI choice dialog for ambiguous titles and pass only the selected canonical session ID into the unchanged ACP hydration path.

**Tech Stack:** Rust, Clap, Tokio, Ratatui/Crossterm, existing ACP client, Cargo integration tests, Python PTY smoke test

## Global Constraints

- Exact ID matching takes precedence over title matching.
- Title matching is exact and case-sensitive.
- Duplicate title candidates are newest-first and explicitly selected by the user.
- Unknown references fail before model, trust, or sandbox prompts.
- Existing uncommitted changes outside this fix must be preserved.
- Run relevant tests, then deploy locally with `cargo install --path .`.

---

### Task 1: Pure resume-reference resolution

**Files:**
- Create: `src/resume.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `crate::client::SessionSummary`
- Produces: `ResumeResolution::{Match(String), Ambiguous(Vec<SessionSummary>), Missing}`, `resolve_resume_reference(&str, &[SessionSummary])`, and `resume_choice_label(&SessionSummary)`

- [x] **Step 1: Write failing unit tests**

Add tests that construct `SessionSummary` fixtures and assert ID precedence, unique exact-title matching, case-sensitive missing results, and newest-first duplicate candidates.

```rust
assert_eq!(
    resolve_resume_reference("Shared", &sessions),
    ResumeResolution::Ambiguous(vec![newer, older]),
);
```

- [x] **Step 2: Run tests to verify failure**

Run: `cargo test --bin lato resume::tests -- --nocapture`

Expected: FAIL because the resolver module and functions do not exist.

- [x] **Step 3: Implement the resolver**

```rust
pub fn resolve_resume_reference(reference: &str, sessions: &[SessionSummary]) -> ResumeResolution {
    if let Some(session) = sessions.iter().find(|s| s.session_id == reference) {
        return ResumeResolution::Match(session.session_id.clone());
    }
    let mut matches = sessions.iter().filter(|s| s.title == reference).cloned().collect::<Vec<_>>();
    matches.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms).then_with(|| b.session_id.cmp(&a.session_id)));
    match matches.len() {
        0 => ResumeResolution::Missing,
        1 => ResumeResolution::Match(matches[0].session_id.clone()),
        _ => ResumeResolution::Ambiguous(matches),
    }
}
```

Make labels unique and inspectable by including title, UTC update time, and complete ID.

- [x] **Step 4: Run focused unit tests**

Run: `cargo test --bin lato resume::tests -- --nocapture`

Expected: all resolver tests PASS.

### Task 2: Wire resolution into interactive resume

**Files:**
- Modify: `src/args.rs`
- Modify: `src/cli.rs`
- Test: `tests/cli_headless.rs`
- Test: `tests/tui_cli.rs`
- Test: `tests/tui_pty_smoke.py`

**Interfaces:**
- Consumes: Task 1's `ResumeResolution`, `resolve_resume_reference`, and `resume_choice_label`
- Produces: `resume_interactive(reference, language, sandbox, plugin_dirs) -> i32` and the `InteractiveStartup::ChooseResume` startup state

- [x] **Step 1: Write failing CLI tests**

Add integration coverage proving that an existing exact ID and a unique title reach the TTY gate, while an unknown reference returns exit code 1 without creating a session. Extend the PTY smoke to create duplicate titled sessions, select one from the dialog, and verify its prior context is hydrated.

```rust
assert!(stderr.contains("no session has the supplied ID or title"));
assert_eq!(created_session_count, 0);
```

- [x] **Step 2: Run tests to verify failure**

Run: `cargo test --test cli_headless --test tui_cli`

Expected: new title and missing-reference assertions FAIL against ID-only startup.

- [x] **Step 3: Resolve before interactive setup**

List session summaries in `resume_interactive`, call the pure resolver, and dispatch direct matches to `InteractiveStartup::Resume`. Dispatch ambiguous matches to `InteractiveStartup::ChooseResume`; print the stable missing-reference error for `Missing`.

```rust
match resolve_resume_reference(&reference, &sessions) {
    ResumeResolution::Match(id) => interactive(InteractiveStartup::Resume(id), language, sandbox, plugin_dirs).await,
    ResumeResolution::Ambiguous(items) => interactive(InteractiveStartup::ChooseResume(items), language, sandbox, plugin_dirs).await,
    ResumeResolution::Missing => { eprintln!("error: no session has the supplied ID or title"); 1 }
}
```

Inside the existing startup dialog, present duplicate labels before model/trust/sandbox setup, map the selected label back to its canonical ID, and let Escape return the existing `dialog::CANCELLED` result.

- [x] **Step 4: Update CLI help**

Describe the argument as `REFERENCE` and advertise `lato resume ID|TITLE` without changing the underlying `Invocation::Resume` representation.

- [x] **Step 5: Run focused regression tests**

Run: `cargo test --bin lato resume::tests -- --nocapture && cargo test --test cli_headless --test tui_cli --test sessions_cli`

Expected: all selected Rust tests PASS.

- [x] **Step 6: Run the PTY smoke test**

Run: `cargo build --bin lato && python3 tests/tui_pty_smoke.py target/debug/lato`

Expected: smoke output includes PASS for ID resume, title resume, duplicate selection, cancellation, and context preservation.

- [x] **Step 7: Validate formatting and deploy**

Run: `cargo fmt --check && cargo install --path .`

Expected: formatting passes and the final line reports installation/replacement of the `lato` executable.

