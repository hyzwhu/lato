# Lato Phase 7B4 Workflow TUI Runs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Session-owned in-memory workflow runs with display names, same-process pause/resume/stop, ACP methods, TUI `/workflow` board, and CLI `workflow.paused` (no cross-process resume).

**Architecture:** Port Grok `WorkflowTracker` + a trimmed `WorkflowManager` into `lato-agent`. Journals stay `Journal::new(None)`. ACP methods drive the TUI. Child agents reuse `WorkflowHostService` + parent `ToolApproval` and model stream. Main `session/prompt` turn is independent.

**Tech Stack:** Rust 2024, existing `lato-workflow` script engine, tokio mpsc/oneshot, ratatui overlay via `dialog.rs`.

**Spec:** `docs/superpowers/specs/2026-09-13-lato-phase-7b4-workflow-tui-runs-design.md`

**Upstream pin:** Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## Global Constraints

- Do not add `lato-workflow` → `lato-agent`.
- Scratch/template/git_diff stay `HostError::Unsupported`.
- No AgentField. No model-facing workflow tool. No `/workflow save`.
- No CLI `resume|pause|stop` subcommands.
- English identifiers. Copy headers when deriving Grok files.
- After the last task: focused tests, clippy `-D warnings` on touched crates, `cargo install --path .`.
- Implementer is Claude Code with model `gemini-3.8-flash-medium` (local Claude proxy). Do not use the Gemini CLI. Tasks 1–5 then 6–8, sequentially in one worktree.

---

## File Structure

### Create

- `crates/lato-agent/src/workflow/tracker.rs`
- `crates/lato-agent/src/workflow/manager.rs`
- `crates/lato-agent/tests/workflow_manager.rs`

### Modify

- `crates/lato-agent/src/workflow/mod.rs`
- `crates/lato-agent/src/workflow/host_service.rs` — plumb `Option<Arc<dyn ToolApproval>>`
- `crates/lato-agent/src/runtime_session.rs`
- `crates/lato-agent/src/host.rs`
- `crates/lato-protocol/src/methods.rs`
- `crates/lato-workflow/src/error.rs` — `Paused` variant / `workflow.paused`
- `src/workflow.rs`, `src/client.rs`, `src/tui/*`, `src/tui/usability_tests.rs`
- `tests/workflow_cli.rs`
- `README.md`, `docs/superpowers/reference/lato-upstream-sources.md`

---

## Workstream A — backend + ACP (Claude / gemini-3.8-flash-medium)

### Task 1: Tracker with display names and statuses

**Files:**
- Create: `crates/lato-agent/src/workflow/tracker.rs`
- Modify: `crates/lato-agent/src/workflow/mod.rs`

**Interfaces:**

```rust
pub const WORKFLOW_HISTORY_MAX: usize = 64;
pub const WORKFLOW_MAX_ACTIVE_RUNS_PER_SESSION: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Active, UserPaused, BackOffPaused, NoProgressPaused, InfraPaused,
    Blocked, BudgetLimited, Interrupted, Complete, Failed, Cancelled,
}

pub struct WorkflowRunState {
    pub run_id: String,
    pub display_name: String,
    pub status: WorkflowRunStatus,
    pub current_phase: Option<String>,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub pause_message: Option<String>,
    pub elapsed_ms_floor: u64,
}

impl WorkflowTracker {
    pub fn start_run(&mut self, run_id: String, meta_name: String, agent_budget: Option<u64>) -> WorkflowRunState;
    pub fn get(&self, run_id: &str) -> Option<WorkflowRunState>;
    pub fn by_display_name(&self, name: &str) -> Option<WorkflowRunState>;
    pub fn list(&self) -> Vec<WorkflowRunState>;
    pub fn active_count(&self) -> usize;
    pub fn apply_outcome(&mut self, run_id: &str, outcome: &lato_workflow::ScriptOutcome);
    pub fn interrupt(&mut self, run_id: &str, message: &str) -> Option<WorkflowRunState>;
    pub fn resume_run(&mut self, run_id: &str, new_budget: Option<u64>) -> Option<WorkflowRunState>;
}
```

Display-name allocation (Grok `tracker.rs` `start_run`): if `meta_name` is free, use it; else `meta_name-2`, `meta_name-3`, …

`resume_run` for `BudgetLimited` returns `None` unless `new_budget` is `Some(n)` and `n > agents_used`.

- [ ] **Step 1:** Unit tests in `tracker.rs` `#[cfg(test)]`: two launches of `review` → `review` and `review-2`; fifth *active* is counted by manager not tracker; pause mapping from `PauseKind`.

- [ ] **Step 2:** Implement from Grok tracker, drop token leases / persistence fields.

- [ ] **Step 3:** `cargo test -p lato-agent tracker -- --nocapture` (or module filter). Expected PASS.

- [ ] **Step 4:** Commit `feat(agent): track workflow runs and display names`

---

### Task 2: WorkflowManager (in-memory journal)

**Files:**
- Create: `crates/lato-agent/src/workflow/manager.rs`
- Modify: `crates/lato-agent/src/workflow/mod.rs`
- Modify: `crates/lato-agent/src/workflow/host_service.rs` — add `pub approval: Option<Arc<dyn ToolApproval>>` to `WorkflowHostParams` and pass it into `ChildSessionRunner`

**Interfaces:**

```rust
pub struct WorkflowManager { /* session_id, cwd, snapshot, stream, locks, trust, approval, tracker, active */ }

pub struct LaunchSpec {
    pub args: serde_json::Value,
    pub agent_budget: Option<u64>,
    pub resume_display_name: Option<String>,
}

pub enum LaunchError {
    UnknownRun(String),
    NotResumable(String),
    BudgetNotRaised { used: u64, limit: u64 },
    TooManyActiveRuns,
    Resolve(lato_workflow::WorkflowError),
}

impl WorkflowManager {
    pub fn launch(&mut self, resolved: ResolvedWorkflow, spec: LaunchSpec) -> Result<WorkflowRunState, LaunchError>;
    pub fn pause(&self, display_name: &str) -> Result<WorkflowRunState, LaunchError>;
    pub fn stop(&mut self, display_name: &str) -> Result<WorkflowRunState, LaunchError>;
    pub fn resume(&mut self, display_name: &str, agent_budget: Option<u64>) -> Result<WorkflowRunState, LaunchError>;
    pub fn list(&self) -> Vec<WorkflowRunState>;
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<WorkflowRunState>;
    pub async fn shutdown(&mut self);
}
```

Launch path: `run_id = format!("wf_{}", uuid)` (use existing uuid dep or `SessionId` style); `Journal::new(None)`; `spawn_workflow_host_service`; `tokio::spawn(run_workflow)`.

On `ScriptOutcome`, update tracker and emit snapshot. `pause_intent` + `CancellationToken` like Grok. `fork_context` remains unsupported in host.

Subscribe channel is how ACP forwards `session/update`.

- [ ] **Step 1:** `crates/lato-agent/tests/workflow_manager.rs` with fake stream:

```rust
#[tokio::test]
async fn second_launch_gets_numbered_display_name() { /* … */ }

#[tokio::test]
async fn fifth_active_run_is_rejected() { /* hold 4 pause-scripts, launch 5th */ }

#[tokio::test]
async fn await_user_then_resume_completes() {
    // script: await_user("user", "need human"); complete("ok");
}

#[tokio::test]
async fn budget_limited_bare_resume_rejected() { /* agent_budget: 1, two agent() */ }

#[tokio::test]
async fn stop_cancels_active_run() { /* */ }
```

Scripts must `pause`/`await_user` so they stay active without a live model. Use `validate`-style canned host only for validate; live manager uses real host with `default_fake_stream()`.

- [ ] **Step 2:** Implement manager. Reuse `list_workflows` / `resolve_workflow`. Maintain `tracker`, active run handles with `CancellationToken`, `pause_intent`, and output listeners. Plumb `approval: Option<Arc<dyn ToolApproval>>` in `WorkflowHostParams` to `ChildSessionRunner`.

- [ ] **Step 3:** `cargo test -p lato-agent --test workflow_manager`. Expected PASS.

- [ ] **Step 4:** Commit `feat(agent): implement in-memory WorkflowManager`

---

### Task 3: Align ACP workflows catalog with CLI registry

**Files:**
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `src/client.rs`

- [ ] **Step 1:** Update `RuntimeSession::list_workflows` to call `lato_agent::workflow::list_workflows` using cwd, home, snapshot, and project trust.
- [ ] **Step 2:** Update client `WorkflowEntry` fields to reflect catalog structure (`id`, `name`, `description`, `steps`, `agent_budget`, etc.).
- [ ] **Step 3:** Ensure ACP `lato/session/workflows` returns the aligned list.
- [ ] **Step 4:** Commit `feat(agent): align ACP workflow catalog with registry`

---

### Task 4: Plumb ACP workflow lifecycle methods

**Files:**
- Modify: `crates/lato-protocol/src/methods.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`

- [ ] **Step 1:** Add `lato/session/workflow`, `lato/session/workflow/runs`, `lato/session/workflow/pause`, `lato/session/workflow/resume`, `lato/session/workflow/stop` to `METHODS_IMPLEMENTED`.
- [ ] **Step 2:** Wire `WorkflowManager` onto `RuntimeSession`.
- [ ] **Step 3:** Implement request handlers in `AcpHost::handle` forwarding to `RuntimeSession`. Forward notifications via `session/update` (`sessionUpdate: "lato/workflow"`).
- [ ] **Step 4:** Unit and integration tests for ACP methods.
- [ ] **Step 5:** Commit `feat(protocol,agent): add ACP workflow lifecycle methods`

---

### Task 5: CLI ScriptOutcome::Paused mapping and error code

**Files:**
- Modify: `crates/lato-workflow/src/error.rs`
- Modify: `src/workflow.rs`
- Modify: `tests/workflow_cli.rs`

- [ ] **Step 1:** Add `WorkflowError::Paused(String)` with code `workflow.paused`.
- [ ] **Step 2:** Update `src/workflow.rs` to map `ScriptOutcome::Paused` to `WorkflowError::Paused`.
- [ ] **Step 3:** CLI integration test asserting `workflow.paused` error and exit code.
- [ ] **Step 4:** Commit `feat(cli): map ScriptOutcome::Paused to workflow.paused`

---

## Workstream B — TUI & Gate

### Task 6: TUI /workflow commands and completions

**Files:**
- Modify: `src/tui/commands.rs`
- Modify: `src/tui/completion.rs`
- Modify: `src/tui/input.rs`

- [ ] **Step 1:** Add `/workflow` to `SLASH_COMMANDS` with description.
- [ ] **Step 2:** Add subcommand completion (`runs`, `pause`, `resume`, `stop`, `<workflowId>`).
- [ ] **Step 3:** Add display name completion for `pause`, `resume`, `stop`.
- [ ] **Step 4:** Commit `feat(tui): add /workflow slash command and completions`

---

### Task 7: TUI /workflow runs overlay board & session/update

**Files:**
- Modify: `src/client.rs`
- Modify: `src/tui/backend.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/dialog.rs`
- Modify: `src/tui/render.rs`
- Modify: `src/tui/usability_tests.rs`

- [ ] **Step 1:** Parse `lato/workflow` updates in `src/client.rs`.
- [ ] **Step 2:** Maintain workflow run states in `TuiState`.
- [ ] **Step 3:** Implement workflow runs dialog in `src/tui/dialog.rs` and render in `render.rs`.
- [ ] **Step 4:** Wire `p`, `r`, `x` keybindings to dispatch pause/resume/stop ACP calls.
- [ ] **Step 5:** Usability tests for `/workflow runs` dialog.
- [ ] **Step 6:** Commit `feat(tui): workflow runs overlay board`

---

### Task 8: Verification, Docs & Ledger Gate

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `docs/superpowers/specs/2026-09-13-lato-phase-7b4-workflow-tui-runs-design.md`

- [ ] **Step 1:** Run all focused tests across `lato-workflow`, `lato-agent`, `tests/workflow_cli.rs`, `src/tui`.
- [ ] **Step 2:** Run `cargo clippy -D warnings` on touched crates.
- [ ] **Step 3:** Run `cargo install --path .` and test `lato --version` / `lato workflow list`.
- [ ] **Step 4:** Update `README.md`, `lato-upstream-sources.md`, mark spec implemented.
- [ ] **Step 5:** Commit `docs: record phase 7b4 workflow TUI runs gate`
