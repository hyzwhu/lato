# Lato Phase 7B3 Workflow Rhai Host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port Grok Build's Rhai workflow engine and journal, run `agent()` through `ChildSessionRunner`, and compile 7B2 JSON steps into sequential `agent()` scripts so `lato workflow run` actually executes.

**Architecture:** `lato-workflow::script` is a structural copy of `xai-workflow` (host channel, journal, meta, Rhai `run_workflow`, canned validate). `lato-agent::workflow::HostService` is the live host: `SpawnAgent` → coordinator + `ChildSessionRunner`. CLI resolves user/project/plugin scripts, injects `configured_stream` or `default_fake_stream()`, and maps `ScriptOutcome` onto the existing CLI `WorkflowOutcome` JSON.

**Tech Stack:** Rust 2024, Rhai 1.25 (`serde`), tokio mpsc/oneshot, `jsonschema` 0.30 for `output_schema`, existing `ChildSessionRunner` / `GitWorkspaceAllocator` / `FakeModelStream`.

## Global Constraints

- Grok Build pin: `bb7f39d5858cbf5e00de639367f59debbdcb0138`.
- Copy headers: `// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:<path>` plus `License: Apache-2.0` and a one-line Lato change.
- Do not add a `lato-workflow` → `lato-agent` dependency.
- Rename Grok's `WorkflowOutcome` enum to `ScriptOutcome` so it does not collide with `lato_workflow::WorkflowOutcome`.
- Keep 7A `DEFAULT_AGENT_BUDGET: u32 = 128` / `MAX_AGENT_BUDGET: u32 = 1024`; script engine uses `u64` at the host boundary.
- No TUI, no AgentField, no scratch/template/git_diff host impl (return `HostError::Unsupported`).
- After each feature slice: focused tests, then `cargo install --path .` per `AGENTS.md` on the final task.
- English identifiers.

---

## File Structure

### Create

- `crates/lato-workflow/src/script/{mod,host,journal,meta,engine,run,validate}.rs`
- `crates/lato-workflow/src/compile.rs`
- `crates/lato-agent/src/workflow/{mod,host_service,registry,schema_contract}.rs`
- `crates/lato-agent/tests/workflow_host.rs`
- `crates/lato-workflow/tests/script_runtime.rs` (if tests do not stay inline)

### Modify

- `crates/lato-workflow/Cargo.toml` — `rhai`, `sha2`
- `crates/lato-workflow/src/lib.rs` — export script + compile; update crate docs
- `crates/lato-agent/Cargo.toml` — `lato-workflow`, `jsonschema`
- `crates/lato-agent/src/lib.rs` — `pub mod workflow`
- `src/args.rs`, `src/workflow.rs`, `src/cli.rs` as needed
- `tests/workflow_cli.rs`
- `README.md`
- `docs/superpowers/reference/lato-upstream-sources.md`
- spec status after the gate

---

### Task 1: Port script host, journal, meta, ScriptOutcome

**Files:**
- Create: `crates/lato-workflow/src/script/{mod,host,journal,meta,run}.rs`
- Modify: `crates/lato-workflow/Cargo.toml`
- Modify: `crates/lato-workflow/src/lib.rs`

**Interfaces:**
- Consumes: Grok `xai-workflow/src/{host,journal,meta,run}.rs` at the pin above
- Produces:

```rust
pub const MAX_PARALLEL: usize = 1_024;
pub const MAX_HOST_CALLS: u64 = 10_000;

pub struct AgentOpts { /* same fields as Grok */ }
pub struct AgentResult { pub agent_id: String, pub success: bool, pub output: Value, pub cancelled: bool, pub tokens_used: u64, pub duration_ms: u64 }
pub struct BudgetState { pub total: Option<u64>, pub spent: u64, pub reserved: u64, pub remaining: Option<u64> }
pub enum HostError { AgentCallQuotaExceeded { requested, maximum }, BudgetExceeded, Cancelled, Unsupported(String), Failed(String) }
pub enum WorkflowHostRequest { ReserveAgentCalls {..}, ReleaseAgentCalls {..}, SpawnAgent {..}, Phase {..}, Log {..}, Telemetry {..}, BudgetQuery {..}, RenderTemplate {..}, WriteScratchFile {..}, ReadScratchFile {..}, GitDiffSince {..} }

pub enum ScriptOutcome {
    Completed { result: Value },
    Paused { kind: PauseKind, message: String },
    BudgetExceeded { message: String },
    Cancelled,
    Failed { error: String },
}

pub fn extract_meta(script: &str) -> Result<WorkflowMeta, MetaError>;
pub struct Journal { /* load/record/covers/request_hash */ }
```

- [ ] **Step 1: Add deps and empty `script` module**

`crates/lato-workflow/Cargo.toml` dependencies: `rhai = { version = "1.25", features = ["serde"] }`, `sha2 = "0.10"`. Keep existing deps.

`src/script/mod.rs` starts as `pub mod host; pub mod journal; pub mod meta; pub mod run;`

- [ ] **Step 2: Copy Grok files with the rename**

Copy verbatim from `/Users/huangyongzhao/Documents/work/grok-build/crates/codegen/xai-workflow/src/{host,journal,meta,run}.rs`.

In `run.rs` rename `enum WorkflowOutcome` → `enum ScriptOutcome` and update every match. Keep `PauseKind`.

Add derivation headers. Drop Grok-only `libc` unix bits in journal if they do not compile; journal tests must still cover torn-tail / divergence / byte cap.

Carry Grok's inline `#[test]` modules.

- [ ] **Step 3: Export from `lib.rs`**

```rust
pub mod script;
pub use script::{
    AgentOpts, AgentResult, BudgetState, HostError, Journal, ScriptOutcome, WorkflowHostRequest,
    extract_meta, run::PauseKind,
};
```

Do not glob-export `WorkflowOutcome` from script.

- [ ] **Step 4: Run** `cargo test -p lato-workflow`

Expected: PASS (existing 7A/7B tests + copied journal/meta tests).

- [ ] **Step 5: Commit**

```bash
git add crates/lato-workflow
git commit -m "feat(workflow): port Grok script host, journal, and meta"
```

---

### Task 2: Port Rhai engine and canned validate

**Files:**
- Create: `crates/lato-workflow/src/script/{engine,validate}.rs`
- Modify: `crates/lato-workflow/src/script/mod.rs`
- Create or extend: `crates/lato-workflow/tests/script_runtime.rs`

**Interfaces:**
- Consumes: Task 1 types; Grok `engine.rs` / `validate.rs`
- Produces:

```rust
pub struct WorkflowRunParams {
    pub script: String,
    pub args: Value,
    pub journal: Journal,
    pub host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    pub cancel: CancellationToken,
    pub max_ops: u64,
}
pub fn run_workflow(params: WorkflowRunParams) -> ScriptOutcome;
pub fn validate_script(script: &str, args: Option<Value>) -> Result<ValidationReport, ValidationError>;
pub fn validate_script_with_agent_budget(script: &str, args: Option<Value>, agent_budget: u64) -> Result<ValidationReport, ValidationError>;
```

Inside copied `engine.rs`, replace every `WorkflowOutcome` with `ScriptOutcome`. Register `json_encode` as Grok does.

- [ ] **Step 1: Write `script_runtime.rs` first** (fail until engine exists)

```rust
#[test]
fn validate_only_completes_without_live_host_side_effects() {
    let script = r#"
let meta = #{ name: "ok", description: "d" };
let r = agent("hello", #{ label: "a" });
complete(r.output);
"#;
    let report = lato_workflow::script::validate_script(script, None).unwrap();
    assert!(report.outcome_ok);
}

#[test]
fn empty_agent_prompt_is_rejected() {
    let script = r#"
let meta = #{ name: "bad", description: "d" };
agent("");
"#;
    assert!(lato_workflow::script::validate_script(script, None).is_err());
}
```

- [ ] **Step 2: Run** `cargo test -p lato-workflow --test script_runtime`

Expected: FAIL (module/function missing).

- [ ] **Step 3: Copy `engine.rs` and `validate.rs`**, fix `ScriptOutcome`, export `run_workflow` / `validate_script*`. Carry Grok validate tests. Canned host must still enforce `agent_budget`.

- [ ] **Step 4: Run** `cargo test -p lato-workflow`

Expected: PASS including Grok validate tests and `script_runtime`.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-workflow
git commit -m "feat(workflow): port Rhai engine and canned validate"
```

---

### Task 3: Compile 7B2 JSON steps to sequential agent()

**Files:**
- Create: `crates/lato-workflow/src/compile.rs`
- Modify: `crates/lato-workflow/src/lib.rs`
- Test: unit tests in `compile.rs`

**Interfaces:**
- Consumes: `WorkflowDescriptor`, `WorkflowStep`, `WorkflowProfile`
- Produces:

```rust
pub fn compile_declarative_workflow(descriptor: &WorkflowDescriptor) -> String;
```

Generated script shape (name: replace `_` with `-` for `meta.name` only):

```rhai
let meta = #{
    name: "review-changes",
    description: "Review a diff",
};
let last = ();
last = agent("<prompt>\n\ninput: " + json_encode(args), #{
    agent_type: "explorer",
    capability_mode: "read-only",
});
complete(last.output);
```

Profile map: explorer/reviewer → `read-only`; worker → `read-write`. Empty steps already synthesized by 7B2 parse.

Escape Rhai string literals in prompt/description (`\`, `"`, newlines as `\n`).

- [ ] **Step 1: Failing test**

```rust
#[test]
fn compiles_explorer_step_to_agent_call() {
    let descriptor = WorkflowDescriptor {
        id: "demo/review-changes".into(),
        plugin_name: "demo".into(),
        name: "review_changes".into(),
        description: "Review a diff".into(),
        when_to_use: String::new(),
        agent_budget: 128,
        steps: vec![WorkflowStep {
            prompt: "Inspect the patch".into(),
            profile: WorkflowProfile::Explorer,
        }],
        source_dir: PathBuf::from("."),
        generation: 1,
    };
    let script = compile_declarative_workflow(&descriptor);
    assert!(script.contains("name: \"review-changes\""));
    assert!(script.contains("agent_type: \"explorer\""));
    assert!(script.contains("capability_mode: \"read-only\""));
    extract_meta(&script).unwrap();
}
```

- [ ] **Step 2: Run** `cargo test -p lato-workflow compile`

Expected: FAIL.

- [ ] **Step 3: Implement `compile_declarative_workflow`**. Also `validate_script` the compiled output in the test.

- [ ] **Step 4: Run** `cargo test -p lato-workflow`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-workflow
git commit -m "feat(workflow): compile JSON steps into sequential agent scripts"
```

---

### Task 4: Workflow registry (user / project / plugin)

**Files:**
- Create: `crates/lato-agent/src/workflow/mod.rs`
- Create: `crates/lato-agent/src/workflow/registry.rs`
- Modify: `crates/lato-agent/Cargo.toml` (`lato-workflow` dep)
- Modify: `crates/lato-agent/src/lib.rs` (`pub mod workflow`)
- Test: `registry.rs` `#[cfg(test)]` with temp dirs

**Interfaces:**
- Consumes: `extract_meta`, `compile_declarative_workflow`, `materialize_workflows`
- Produces:

```rust
pub struct ResolvedWorkflow {
    pub id: String,           // qualified or short name used to run
    pub display_name: String,
    pub script: String,
    pub agent_budget: u32,
    pub source: &'static str, // "user" | "project" | "plugin"
    pub compiled: bool,
}

pub fn list_workflows(cwd: &Path, lato_home: &Path, snapshot: &PluginSnapshot, project_trusted: bool) -> Vec<ResolvedWorkflow>;
pub fn resolve_workflow(..., id: &str) -> Result<ResolvedWorkflow, WorkflowError>;
```

Scan `$lato_home/workflows/*.rhai` then (if trusted) `<git-root|cwd>/.lato/workflows/*.rhai` then plugin descriptors. Keep-first by `meta.name` / plugin qualified id. Same-scope duplicate short names → `InvalidConfiguration("workflow.duplicate_name")`. Symlink or >1 MiB skip/diagnose. Filename must equal `meta.name` + `.rhai` for file-backed scripts.

Resolve: exact qualified id, else unique short name.

- [ ] **Step 1: Failing tests** in `registry.rs`: user file listed; untrusted project dir ignored; plugin JSON compiles `compiled: true`; unknown id `NotFound`.

- [ ] **Step 2: Run** `cargo test -p lato-agent workflow::registry`

Expected: FAIL.

- [ ] **Step 3: Implement scan/resolve.** Git root: `std::process::Command::new("git").args(["rev-parse","--show-toplevel"])` or walk `.git`; on failure use cwd.

- [ ] **Step 4: Run** `cargo test -p lato-agent workflow::registry`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-agent
git commit -m "feat(agent): resolve user, project, and plugin workflow scripts"
```

---

### Task 5: HostService SpawnAgent + budget

**Files:**
- Create: `crates/lato-agent/src/workflow/host_service.rs`
- Create: `crates/lato-agent/src/workflow/schema_contract.rs`
- Modify: `crates/lato-agent/src/workflow/mod.rs`
- Modify: `crates/lato-agent/Cargo.toml` (`jsonschema = "0.30"`)
- Test: `crates/lato-agent/tests/workflow_host.rs`

**Interfaces:**
- Consumes: `WorkflowHostRequest`, `ChildSessionRunner`, `spawn_subagent_coordinator_with_verifier`, `GitWorkspaceAllocator` (fallback `MemoryWorkspaceAllocator` if cwd is not a git repo), `ProfileResultVerifier`, `SessionPluginSnapshots`
- Produces:

```rust
pub struct WorkflowHostParams {
    pub run_id: String,
    pub session_id: SessionId,
    pub max_concurrent_agents: usize,
    pub agent_budget: u64,
    pub cwd: PathBuf,
    pub stream: Arc<dyn ModelStream>,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
    pub snapshot: Arc<PluginSnapshot>,
    pub cancel: CancellationToken,
}

pub fn spawn_workflow_host_service(
    params: WorkflowHostParams,
    rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
) -> tokio::task::JoinHandle<()>;

pub fn workflow_max_concurrent_agents(configured: usize) -> usize; // default 32, clamp parallelism, min 2
```

`SpawnAgent` mapping is the spec §6.3 table. `general-purpose` → worker. `fork_context` → `Unsupported`. Schema retry: one extra child, same budget.

`ReserveAgentCalls` uses an `AtomicU64` spent counter plus `BudgetAccount` on the workflow root (`child_tasks = agent_budget`).

Wire a single coordinator for the run: `register_root` with `TaskOwner::Workflow` before the first spawn; each agent is `spawn_and_wait`. Cancel the host token → `cancel_workflow(run_id)` and wait up to 20s.

- [ ] **Step 1: Write `workflow_host.rs`**

```rust
#[tokio::test]
async fn spawn_agent_runs_child_session_with_fake_stream() { /* FakeModelStream text reply; AgentResult.success */ }

#[tokio::test]
async fn read_only_mode_uses_explorer_workspace() { /* inspect spawned profile / capabilities */ }

#[tokio::test]
async fn fork_context_is_unsupported() { /* AgentOpts.fork_context = true */ }

#[tokio::test]
async fn parallel_reserve_rejects_over_budget_without_spawns() { /* budget 1, ReserveAgentCalls 2 */ }
```

Use `default_fake_stream()` / `FakeModelStream::new(vec![StreamPiece::Text(...)])` as in `crates/lato-agent/tests/subagent_runner.rs`.

- [ ] **Step 2: Run** `cargo test -p lato-agent --test workflow_host`

Expected: FAIL.

- [ ] **Step 3: Implement host_service + schema_contract.** Copy Grok `schema_contract.rs` constants (`SCHEMA_CONTRACT_RETRIES = 1`) and `RejectExternalSchemaRefs`. Copy spawn/cancel/drain structure from `host_service.rs` but call Lato coordinator APIs instead of `SubagentRequest`.

Unsupported replies for scratch/template/git_diff.

- [ ] **Step 4: Run** `cargo test -p lato-agent --test workflow_host` and `cargo test -p lato-agent --test subagent_runner`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-agent
git commit -m "feat(agent): run workflow agents through ChildSessionRunner"
```

---

### Task 6: CLI run/list through registry + host

**Files:**
- Modify: `src/args.rs` — `--model`, `--sandbox`, `--validate-only`, `--agent-budget` on `workflow run`
- Modify: `src/workflow.rs` — drop `CompletingTaskRunner` production path
- Modify: `tests/workflow_cli.rs`
- Modify: clap `after_help` string

**Interfaces:**
- Consumes: `list_workflows` / `resolve_workflow`, `run_workflow`, `spawn_workflow_host_service`, `compile` (via registry), `cli::configured_stream`, `default_fake_stream`
- Produces: exit 0 JSON `{ runId, status: "completed", output }` on `ScriptOutcome::Completed`; map other variants to `WorkflowError` codes

`runId` format stays `wf-{seq}` using a process-local counter or `Uuid`-free `wf-1` for CLI (single run per process is enough).

`--validate-only` calls `validate_script_with_agent_budget` and prints `{ "status": "validated", "name", "outcome": report.outcome_summary }` without starting HostService.

`--agent-budget` overrides descriptor budget; reject 0 and >1024 with exit 2.

Sandbox: parse `--sandbox` like `-p`; default `SessionTrust::for_headless_prompt`.

- [ ] **Step 1: Extend `tests/workflow_cli.rs`**

Keep `workflow_list_and_run_use_plugin_dir` expecting Completed.

Add:

```rust
#[test]
fn workflow_validate_only_does_not_need_a_model() { /* --validate-only, status validated */ }

#[test]
fn workflow_run_unknown_id_fails() { /* already present; keep */ }
```

Add a unit test in `src/args.rs` for the new flags.

- [ ] **Step 2: Run** `cargo test --test workflow_cli --bin lato`

Expected: FAIL on new flags / still-stub Completing runner if you assert on `output` from fake child text.

- [ ] **Step 3: Rewrite `src/workflow.rs::execute`**

```rust
let resolved = resolve_workflow(...)?;
if validate_only { /* validate_script_with_agent_budget; print; return 0/1 */ }
let stream = match model {
    Some(sel) => configured_stream(&sel).await?,
    None => default_fake_stream(),
};
let (host_tx, host_rx) = mpsc::unbounded_channel();
let cancel = CancellationToken::new();
let run_id = "wf-1".to_string();
let _host = spawn_workflow_host_service(WorkflowHostParams { run_id: run_id.clone(), session_id: SessionId::from("cli-workflow"), ... }, host_rx);
let outcome = tokio::task::spawn_blocking(move || {
    run_workflow(WorkflowRunParams {
        script: resolved.script,
        args: input,
        journal: Journal::new(None),
        host_tx,
        cancel,
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
    })
})
.await
.map_err(|error| error.to_string())?;
```

`run_workflow` is sync/Rhai (Grok runs it on a dedicated thread). Use `spawn_blocking`. Map `ScriptOutcome` → stdout JSON / errors.

Ctrl-C: if practical, cancel the token; otherwise rely on process exit.

- [ ] **Step 4: Run** `cargo test --test workflow_cli` and `cargo test -p lato --bin lato args`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src tests/workflow_cli.rs
git commit -m "feat(cli): run workflows through the Rhai host"
```

---

### Task 7: Docs, ledger, clippy, install

**Files:**
- Modify: `README.md` Plugin Workflows section
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: spec status → **待实施门禁** then **已实施** after the gate
- Create: `docs/testing/reports/phase-7b3-workflow-rhai-host-release-gate-2026-09-12.md`

README must say: workflows are Rhai scripts (`agent`/`parallel`/`phase`/`complete`); JSON `prompt`/`steps` compile to sequential `agent()`; `lato workflow run` uses the session model or fake stream; no TUI runner; `--validate-only` is canned.

Ledger rows:

| Lato target | Upstream |
| --- | --- |
| `crates/lato-workflow/src/script/*` | `xai-workflow/src/*` |
| `crates/lato-agent/src/workflow/host_service.rs` | `xai-grok-shell/.../host_service.rs` |
| `crates/lato-agent/src/workflow/registry.rs` | `xai-grok-shell/.../registry.rs` |
| `crates/lato-agent/src/workflow/schema_contract.rs` | `xai-grok-shell/.../schema_contract.rs` |

- [ ] **Step 1: Update README + ledger + spec status**

- [ ] **Step 2: Run**

```bash
cargo fmt --all -- --check
cargo clippy -p lato-workflow -p lato-agent --all-targets -- -D warnings
cargo test -p lato-workflow
cargo test -p lato-agent --test workflow_host --test subagent_runner
cargo test --test workflow_cli
cargo install --path .
lato --version
```

Expected: all green. Write the gate report.

- [ ] **Step 3: Commit**

```bash
git add README.md docs src crates
git commit -m "docs: record phase 7b3 Rhai host gate"
```

---

## Spec coverage

| Spec ID | Task |
| --- | --- |
| R-1 meta | 1 |
| R-2 validate_only | 2, 6 |
| R-3 budget live vs schema retry | 5 |
| R-4 parallel cap | 2 (engine) + 5 (reserve) |
| R-5 read-only | 5 |
| R-6 fork_context | 5 |
| R-7 cancel | 5 |
| R-8 JSON CLI | 3, 6 |
| R-9 unknown id | 4, 6 |
| R-10 untrusted project | 4 |
| R-11 gate | 7 |

## Type consistency

- CLI JSON still uses `lato_workflow::WorkflowOutcome` / `status: Completed`.
- Engine returns `ScriptOutcome`.
- Host talks `WorkflowHostRequest` / `AgentResult`.
- Registry returns `ResolvedWorkflow.script: String`.
