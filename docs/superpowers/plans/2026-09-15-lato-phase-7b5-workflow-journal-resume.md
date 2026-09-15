# Lato Phase 7B5 Cross-Process Workflow Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist ACP-session workflow journals under `$LATO_HOME/sessions/<sid>/workflows/<runId>/` so `session/resume` can restore paused runs and continue them. Active-at-exit runs become `interrupted`. CLI `workflow resume|pause|stop` stays unimplemented.

**Architecture:** Keep 7B4 in-memory Manager. Add optional `workflows_dir`. When `Some`, launch writes `run.json` + `script.rhai` and uses `Journal::new(Some(journal.jsonl))`; `new()` restores from that directory. `AcpHost::attach_workflow_manager` passes the session workflows path. `workflows_dir = None` preserves 7B4 (tests + CLI `lato workflow run`).

**Tech Stack:** Rust 2024, existing `lato-workflow::Journal` load/record, 7B4 `WorkflowManager` / tracker / ACP methods.

**Spec:** `docs/superpowers/specs/2026-09-15-lato-phase-7b5-workflow-journal-resume-design.md`

**Upstream pin:** Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## Global Constraints

- Do not add `lato-workflow` → `lato-agent`.
- Scratch/template/git_diff stay `HostError::Unsupported`.
- No AgentField. No model-facing workflow tool. No `/workflow save`.
- No CLI `resume|pause|stop` subcommands.
- Resume must replay the captured script and args, never re-`resolve_workflow`.
- A corrupt per-run directory must skip that run, not fail `session/resume`.
- English identifiers. Copy headers when deriving Grok files.
- After the last task: focused tests, clippy `-D warnings` on touched crates, `cargo install --path .`.
- Implementer: sequential Tasks 1→4 in one worktree, TDD. Do not touch the WIN-19 Python skeleton.

---

## File Structure

### Create

- `crates/lato-agent/src/workflow/persist.rs` — `run.json` serde, directory layout, restore scan

### Modify

- `crates/lato-agent/src/workflow/manager.rs` — `workflows_dir: Option<PathBuf>`; persist on launch/settle; restore in `new`
- `crates/lato-agent/src/workflow/mod.rs` — export persist types if tests need them
- `crates/lato-agent/src/workflow/tracker.rs` — comment: cross-process resumable set (paused family + failed/cancelled; budget_limited gated; active-on-disk → interrupted)
- `crates/lato-agent/src/host.rs` — pass `effective_lato_home()/sessions/<sid>/workflows`
- `crates/lato-agent/tests/workflow_manager.rs` — persist/restore tests; existing `None` call sites
- `crates/lato-agent/src/workflow/manager.rs` `#[cfg(test)]` helper
- `src/workflow.rs` — unchanged CLI (assert no new subcommands in tests if a help test exists)
- `README.md`, `docs/superpowers/reference/lato-upstream-sources.md`
- spec status → 已实施

---

## Task 1: Persist layout + Manager `workflows_dir`

**Files:**
- Create: `crates/lato-agent/src/workflow/persist.rs`
- Modify: `crates/lato-agent/src/workflow/manager.rs`
- Modify: `crates/lato-agent/src/workflow/mod.rs`
- Modify: existing `WorkflowManager::new` call sites (`host.rs`, unit tests, `workflow_manager.rs` tests) — add `workflows_dir: None` unless the test is about persistence

**Interfaces:**

```rust
pub const RUN_RECORD_VERSION: u32 = 1;

pub struct PersistedRun {
    pub version: u32,
    pub run_id: String,
    pub display_name: String,
    pub status: WorkflowRunStatus,
    pub phase: Option<String>,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub pause_message: Option<String>,
    pub elapsed_ms_floor: u64,
    pub workflow_id: String,
    pub source: String,
    pub compiled: bool,
    pub description: String,
    pub args: serde_json::Value,
}

impl WorkflowManager {
    pub fn new(
        session_id: &str,
        cwd: PathBuf,
        trust: SessionTrust,
        locks: Arc<FileLocks>,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        workflows_dir: Option<PathBuf>,
    ) -> Self;
}
```

Layout: `<workflows_dir>/<runId>/{run.json,script.rhai,journal.jsonl}`.

- Launch with `Some(dir)`: create the run dir, write `script.rhai` once, write `run.json`, `Journal::new(Some(journal.jsonl))`. If any write fails, do not spawn the run (no memory-only fallback).
- Every tracker status change (pause intent settle, apply_outcome, interrupt, stop, resume_run): rewrite `run.json`.
- `None`: `Journal::new(None)` as today.

- [ ] **Step 1:** Unit tests in `persist.rs` for round-trip `run.json` and skip-on-malformed.

- [ ] **Step 2:** Extend `WorkflowManager::new` with `workflows_dir`. Update every call site to pass `None`. Persist on launch/settle when `Some`.

- [ ] **Step 3:** `cargo test -p lato-agent --test workflow_manager` plus persist module tests. Expected PASS (existing in-memory tests still green).

- [ ] **Step 4:** Commit `feat(agent): persist workflow run journals under the session directory`

---

## Task 2: Restore + active-at-exit → interrupted

**Files:**
- Modify: `crates/lato-agent/src/workflow/persist.rs`
- Modify: `crates/lato-agent/src/workflow/manager.rs`
- Modify: `crates/lato-agent/tests/workflow_manager.rs`

Restore in `new()` when `workflows_dir` is `Some`: scan child dirs, skip bad ones, cap at `WORKFLOW_HISTORY_MAX`. Disk `status == Active` → set `Interrupted` and rewrite `run.json`. Fill `tracker` with the **stored** display name (do not re-allocate). Fill `inner.workflows` from `script.rhai` + `run.json` fields, `inner.args`, `inner.journals` from `Journal::load`.

- [ ] **Step 1:** Tests (two Manager instances, same temp dir):

```rust
#[tokio::test]
async fn paused_run_survives_new_manager_and_resumes() { /* await_user; drop; new; resume; complete */ }

#[tokio::test]
async fn active_on_disk_restores_as_interrupted() { /* write run.json status=active; new; resume → NotResumable */ }

#[tokio::test]
async fn budget_limited_restore_rejects_bare_resume() { /* */ }

#[tokio::test]
async fn corrupt_journal_skips_that_run_only() { /* */ }

#[tokio::test]
async fn restored_display_name_is_not_reallocated() { /* */ }
```

Keep existing `await_user_then_resume_completes` on `None`.

- [ ] **Step 2:** Implement restore. Do not re-resolve the workflow by id.

- [ ] **Step 3:** `cargo test -p lato-agent --test workflow_manager`. Expected PASS.

- [ ] **Step 4:** Commit `feat(agent): restore paused workflow runs across process restart`

---

## Task 3: ACP attach path + session/resume snapshots

**Files:**
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs` if restore should emit `session/update` after attach
- Modify: an existing ACP host test if one covers `session/resume` (prefer `crates/lato-agent` host tests; add a focused test rather than a giant new suite)

`attach_workflow_manager` must pass `Some(effective_lato_home().join("sessions").join(sid).join("workflows"))`. After restore, emit one `session/update` (`sessionUpdate: "lato/workflow"`) per restored run (current snapshot only).

`session/close` already calls `workflow_shutdown`; ensure interrupted/paused final states are flushed to `run.json` (Task 1 settle path).

- [ ] **Step 1:** Test: create session, launch + `await_user` until paused, tear down host (or drop manager via close), `session/resume` same id, `lato/session/workflow/runs` contains the paused display name; `workflow/resume` then completes. Isolate LATO_HOME to a temp dir.

- [ ] **Step 2:** Wire the persist dir. Emit restore snapshots.

- [ ] **Step 3:** `cargo test -p lato-agent` focused host/manager tests. Expected PASS.

- [ ] **Step 4:** Commit `feat(agent): restore workflow runs on session/resume`

---

## Task 4: CLI evaluation freeze, docs, gate

**Files:**
- Modify: `README.md` — 7B4 section: session resume restores paused runs; CLI still has no resume/pause/stop
- Modify: `docs/superpowers/reference/lato-upstream-sources.md` — tracker/manager row: journals now persist under session `workflows/`
- Modify: spec status → 已实施
- Modify: `tests/workflow_cli.rs` only if needed to assert help text has no resume subcommand (do not add the subcommands)

- [ ] **Step 1:** Focused tests: `lato-workflow`, `lato-agent` lib + `workflow_manager` + host restore test, `workflow_cli`, TUI `workflow_*` (no behavior change expected).

- [ ] **Step 2:** `cargo clippy -D warnings` on lato-workflow / lato-agent / lato-protocol / bin lato `--all-targets`.

- [ ] **Step 3:** `cargo install --path .`; `lato --version`; `lato workflow --help` has list/run/validate, not resume/pause/stop.

- [ ] **Step 4:** README + ledger + spec status.

- [ ] **Step 5:** Commit `docs: record phase 7b5 workflow journal resume gate`

---

## Gate (copy into the WIN-24 result comment)

- focused tests, 0 failed
- clippy `-D warnings` on touched crates
- `cargo install --path .`
- README 7B5 semantics
- ledger row
- spec marked 已实施
