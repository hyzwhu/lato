# Lato Phase 7B6 Workflow Host Helpers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement live `write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since` on `WorkflowHostService`. Scratch lives under the 7B5 run directory for ACP sessions and a temp dir for CLI. `fork_context` stays `Unsupported`.

**Architecture:** Keep request types in `lato-workflow`. Do the IO in `lato-agent` HostService. Add `WorkflowHostParams.scratch_dir: Option<PathBuf>`. Manager passes `run_dir/scratch` when persisting; CLI passes `None` (host-owned tempdir).

**Tech Stack:** Rust 2024, existing HostService, `std::process::Command` for git, 7B5 run directories.

**Spec:** `docs/superpowers/specs/2026-09-15-lato-phase-7b6-workflow-host-helpers-design.md`

**Upstream pin:** Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## Global Constraints

- Do not add `lato-workflow` → `lato-agent`.
- Do not implement a model-facing workflow tool or AgentField.
- No CLI `resume|pause|stop`. No `/workflow save`. No TUI pager for scratch files.
- `fork_context` remains `Unsupported`.
- `--validate-only` stays on the canned host stubs.
- English identifiers. Copy headers when deriving Grok files.
- After the last task: focused tests, clippy `-D warnings` on touched crates, `cargo install --path .`.
- Implementer: sequential Tasks 1→3 in one worktree, TDD. Do not touch the WIN-19 Python skeleton.

---

## File Structure

### Create

- `crates/lato-agent/src/workflow/scratch.rs` — name rules, quotas, read/write
- `crates/lato-agent/src/workflow/templates.rs` — closed catalog + `{ident}` replace

### Modify

- `crates/lato-agent/src/workflow/host_service.rs` — implement the four requests; `scratch_dir`; git diff
- `crates/lato-agent/src/workflow/mod.rs` — modules
- `crates/lato-agent/src/workflow/manager.rs` — pass `Some(run_dir.join("scratch"))` when `workflows_dir` is set, else `None`
- `src/workflow.rs` — CLI host keeps `scratch_dir: None`
- `crates/lato-agent/tests/workflow_host.rs` — helper tests
- `crates/lato-agent/tests/workflow_manager.rs` — one persist+resume readback
- `README.md`, `docs/superpowers/reference/lato-upstream-sources.md`, spec status

Existing `start_host` in `workflow_host.rs` must compile after the new field: pass `scratch_dir: None`.

---

## Task 1: Scratch + templates (TDD)

**Files:**
- Create: `scratch.rs`, `templates.rs`
- Modify: `host_service.rs`, `mod.rs`, `workflow_host.rs`

**Interfaces:**

```rust
pub const MAX_SCRATCH_NAME: usize = 128;
pub const MAX_SCRATCH_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_SCRATCH_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SCRATCH_FILES: usize = 64;

pub fn validate_scratch_name(name: &str) -> Result<(), HostError>; // Failed("invalid scratch name")
pub fn write_scratch(dir: &Path, name: &str, content: &str) -> Result<String, HostError>; // "scratch/{name}"
pub fn read_scratch(dir: &Path, name: &str) -> Result<String, HostError>;

pub fn render_template(name: &str, vars: &serde_json::Value) -> Result<String, HostError>;
```

HostService: if `params.scratch_dir` is `Some`, use it (create_dir_all). If `None`, hold a `tempfile::TempDir` on the service.

Quota accounting: sum sizes of regular files in the scratch dir (follow no symlinks; reject symlink names).

- [ ] **Step 1:** Unit tests in `scratch.rs` / `templates.rs` and host tests: write/read, bad names, missing file, oversize, `identity` template, unknown template.

- [ ] **Step 2:** Implement. Replace the four `unsupported(...)` arms.

- [ ] **Step 3:** `cargo test -p lato-agent --test workflow_host scratch template`. Expected PASS. Existing host tests still green (`start_host` gets `scratch_dir: None`).

- [ ] **Step 4:** Commit `feat(agent): implement workflow scratch files and builtin templates`

---

## Task 2: git_diff_since + Manager/CLI wiring

**Files:**
- Modify: `host_service.rs` — `git diff -- <commit>` in `params.cwd`
- Modify: `manager.rs` — `scratch_dir: workflows_dir.map(|root| persist::run_dir(&root, &run_id).join("scratch"))`
- Modify: `src/workflow.rs` — `scratch_dir: None`
- Modify: `workflow_host.rs` — git tests using existing `init_git_repo`

Commit argument: reject empty, whitespace, NUL, or a string that starts with `-`. Pass as one argv after `--`. Cap stdout+stderr at 1 MiB; over → `Failed`. Non-zero git exit → `Failed` with a short message (no full stderr dump over a few hundred bytes).

- [ ] **Step 1:** Tests: dirty tree vs HEAD contains the edit; not-a-repo fails; `-evil` fails.

- [ ] **Step 2:** Implement git + wiring.

- [ ] **Step 3:** `cargo test -p lato-agent --test workflow_host --test workflow_manager`. Expected PASS.

- [ ] **Step 4:** Commit `feat(agent): add git_diff_since and persist scratch under the run directory`

---

## Task 3: Resume readback, docs, gate

**Files:**
- Modify: `workflow_manager.rs` test: script writes scratch, `await_user`, then reads it; drop manager; new manager with same dir; resume; assert read returns the original body (live host, not only journal — put the **read after** `await_user` so restore must see the file).
- Modify: README (7B4/7B5 in-session section): one paragraph that scratch/template/git_diff now work; CLI still has no resume.
- Modify: ledger HostService row: scratch/template/git_diff implemented.
- Modify: spec status → 已实施.

- [ ] **Step 1:** Resume+scratch test PASS.

- [ ] **Step 2:** clippy `-D warnings` on lato-agent / lato-workflow / bin lato `--all-targets`.

- [ ] **Step 3:** `cargo install --path .`; `lato --version`.

- [ ] **Step 4:** Commit `docs: record phase 7b6 workflow host helpers gate`

---

## Gate (copy into the WIN-24 result comment)

- focused tests, 0 failed
- clippy `-D warnings` on touched crates
- `cargo install --path .`
- README + ledger
- spec marked 已实施
- `fork_context` still Unsupported; CLI still list|run only
