# Lato Phase 7A Workflow Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Materialize trusted+enabled plugin workflow descriptors from a frozen `PluginSnapshot` and expose an inert `Workflow` trait that cannot spawn agents, call tools, or debit `BudgetAccount`.

**Architecture:** `lato-workflow` owns names, descriptor types, stable errors, and `InertWorkflow`. `lato-extensions` parses `plugin.json` `workflows` (path or inline), confines paths under the plugin root, and builds a generation-scoped `WorkflowDescriptorSet`. Child snapshots add a monotone `WorkflowCapabilityCeiling`. No script engine, no AgentField, no CLI runner.

**Tech Stack:** Rust 2024, serde/serde_json, thiserror, async-trait, tokio-util `CancellationToken`, existing Phase 6A snapshot / `derive_child` patterns.

## Global Constraints

- Base: current `master` including Streamable HTTP (`be7c450`+) and this design (`8cac704`).
- Consume **only** trusted ∧ enabled plugins from the turn's frozen `PluginSnapshot`.
- Qualified ids are `plugin/workflow`. Collisions keep-first and diagnose; never silent overwrite.
- `agent_budget` default 128, min 1, max 1024; illegal values drop that entry with a diagnostic.
- `InertWorkflow::run` returns `Err(WorkflowError::NotImplemented)` with code `workflow.not_implemented`.
- 7A must not call models, tools, shell, MCP, or `BudgetAccount::reserve`.
- Local CLI must not depend on AgentField.
- After each feature slice: focused tests, then `cargo install --path .` per `AGENTS.md`.
- English identifiers; Grok/Lato source headers on substantially derived files; update upstream ledger if copying MCP materialize structure.

---

## File Structure

### Create

- `crates/lato-workflow/Cargo.toml`
- `crates/lato-workflow/src/lib.rs`
- `crates/lato-workflow/src/error.rs`
- `crates/lato-workflow/src/names.rs`
- `crates/lato-workflow/src/types.rs`
- `crates/lato-workflow/src/inert.rs`
- `crates/lato-extensions/src/workflows/mod.rs`
- `crates/lato-extensions/tests/workflow_config.rs`
- `crates/lato-workflow/tests/inert_runtime.rs`

### Modify

- `Cargo.toml` (workspace members)
- `crates/lato-extensions/Cargo.toml` (depend on `lato-workflow`)
- `crates/lato-extensions/src/lib.rs`
- `crates/lato-extensions/src/manifest.rs`
- `crates/lato-extensions/src/discovery.rs`
- `crates/lato-extensions/src/registry.rs`
- `README.md`
- `docs/superpowers/specs/2026-09-12-lato-phase-7a-workflow-interface-design.md` (status after gate)
- `docs/superpowers/reference/lato-upstream-sources.md`

---

### Task 1: lato-workflow crate (names, types, errors, inert)

**Files:**
- Create: `crates/lato-workflow/Cargo.toml`
- Create: `crates/lato-workflow/src/{lib,error,names,types,inert}.rs`
- Modify: workspace `Cargo.toml` members
- Test: unit tests in `names.rs` / `error.rs` / `inert.rs`

**Interfaces:**
- Produces:

```rust
pub const DEFAULT_AGENT_BUDGET: u32 = 128;
pub const MIN_AGENT_BUDGET: u32 = 1;
pub const MAX_AGENT_BUDGET: u32 = 1024;
pub const MAX_WORKFLOW_NAME_LEN: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 4096;

pub fn normalize_workflow_name(raw: &str) -> Option<String>;
pub fn qualify_workflow(plugin: &str, workflow: &str) -> String; // "{plugin}/{workflow}"
pub fn clamp_agent_budget(raw: Option<u64>) -> Result<u32, ()>;

pub struct WorkflowDescriptor { pub id, plugin_name, name, description, when_to_use, agent_budget, source_dir, generation }
pub struct WorkflowDiagnostic { pub code, plugin_name, path, message }
pub struct WorkflowDescriptorSet { pub generation, pub workflows: Arc<[WorkflowDescriptor]>, pub diagnostics: Arc<[WorkflowDiagnostic]> }

pub struct WorkflowContext { pub run_id: String, pub session_id: SessionId, pub generation: u64, pub cancel: CancellationToken, pub agent_budget: u32 }
pub struct WorkflowOutcome { pub run_id: String, pub status: WorkflowStatus, pub output: Value }
pub enum WorkflowStatus { NotImplemented }

pub enum WorkflowError { NotImplemented, InvalidConfiguration(String), CapabilityDenied(String) }
impl WorkflowError { pub fn code(&self) -> &'static str; }

#[async_trait]
pub trait Workflow: Send + Sync {
    fn descriptor(&self) -> &WorkflowDescriptor;
    async fn run(&self, context: WorkflowContext, input: Value) -> Result<WorkflowOutcome, WorkflowError>;
}

pub struct InertWorkflow { descriptor: WorkflowDescriptor }
```

- [ ] **Step 1: Write failing unit tests** in `names.rs` for `review-changes` ok, empty/`-x`/`has space` rejected, `qualify_workflow("demo","review") == "demo/review"`. In `error.rs` assert `NotImplemented.code() == "workflow.not_implemented"`.

- [ ] **Step 2: Run** `cargo test -p lato-workflow`

Expected: compile failure (crate missing).

- [ ] **Step 3: Add crate + implement names/types/error/inert**

`Cargo.toml`: edition 2024, deps `async-trait`, `lato-core`, `serde`, `serde_json`, `thiserror`, `tokio-util` (rt). No reqwest.

`InertWorkflow::run` ignores input and returns `Err(WorkflowError::NotImplemented)`.

- [ ] **Step 4: Run** `cargo test -p lato-workflow`  
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/lato-workflow
git commit -m "feat(workflow): add inert workflow types and stable errors"
```

---

### Task 2: Plugin manifest + snapshot fields

**Files:**
- Modify: `crates/lato-extensions/src/manifest.rs`
- Modify: `crates/lato-extensions/src/discovery.rs`
- Modify: `crates/lato-extensions/src/registry.rs`
- Modify: existing `crates/lato-extensions/tests/manifest.rs` / `discovery.rs` if they construct `PluginManifest` / `LoadedPlugin` exhaustively

**Interfaces:**
- `PluginManifest.workflows: Option<PathOrInline>`
- `PluginManifest::workflow_config_path(root) -> Option<PathBuf>` convention file `workflows.json`
- `PluginManifest::inline_workflows() -> Option<&Value>`
- `LoadedPlugin.workflow_config_path` / `inline_workflows`
- `PluginComponentKind::Workflows` when either field is present
- Convention discovery: treat `workflows.json` like `.mcp.json` so a nameless dir with only that file can be a convention plugin

- [ ] **Step 1: Failing tests** in `crates/lato-extensions/tests/manifest.rs` (or new cases): `plugin.json` with `"workflows":{"review-changes":{}}` is inline; `"workflows":"workflows.json"` resolves under root; `"workflows":"../escape.json"` does not resolve.

- [ ] **Step 2: Run** `cargo test -p lato-extensions --test manifest`

Expected: field missing / compile fail.

- [ ] **Step 3: Implement fields** mirroring `mcp_servers` / `mcp_config_path`. Update `LoadedPlugin::components`, `load_plugin`, `derive_child` (clear workflow fields when `!extension_allowed`).

- [ ] **Step 4: Run** `cargo test -p lato-extensions --test manifest` and `cargo test -p lato-extensions --test discovery`

Expected: green; no MCP/skills regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-extensions
git commit -m "feat(extensions): declare plugin workflow manifest fields"
```

---

### Task 3: materialize_workflows (W-1..W-4, W-8)

**Files:**
- Create: `crates/lato-extensions/src/workflows/mod.rs`
- Create: `crates/lato-extensions/tests/workflow_config.rs`
- Modify: `crates/lato-extensions/src/lib.rs` export `materialize_workflows`
- Modify: `crates/lato-extensions/Cargo.toml` `lato-workflow = { path = "../lato-workflow" }`
- Put parsing helpers in `lato-workflow` (`parse_workflow_config`, `ParseContext`, `push_diagnostic`) so extensions stays a thin materializer like MCP.

**Interfaces:**
- `pub fn materialize_workflows(snapshot: &PluginSnapshot) -> Arc<WorkflowDescriptorSet>`
- Apply `snapshot.workflow_ceiling()` filter if already present; if ceiling is Task 4, Task 3 can leave ceiling default-allow-all.

Parse shapes:
- Inline object of name → `{ description?, whenToUse?, agentBudget? }`
- File JSON: same map, or `{ "workflows": { ... } }`
- Path list of files: each file one map
- Do **not** recurse into script files; unknown extra keys ignored

Limits: max 32 workflows per plugin; diagnostic overflow 128 × 512 bytes (copy MCP constants under `workflow.*` names).

- [ ] **Step 1: Write** `workflow_config.rs` tests:
  - only active trusted+enabled materialize
  - untrusted project plugin → empty set
  - path escape rejected (already none from manifest) + malformed JSON diagnostic isolation
  - collision keep-first `workflow.collision`
  - default budget 128; `agentBudget: 0` and `1025` dropped with `workflow.invalid_budget`
  - `PluginComponentKind::Workflows` present

- [ ] **Step 2: Run** `cargo test -p lato-extensions --test workflow_config`  
Expected: missing `materialize_workflows`.

- [ ] **Step 3: Implement parse + materialize.** Sort plugins by name then canonical_root. Keep-first on `id`.

- [ ] **Step 4: Run** `cargo test -p lato-extensions --test workflow_config`  
Expected: green.

- [ ] **Step 5: `cargo install --path .` then commit**

```bash
git add crates/lato-workflow crates/lato-extensions
git commit -m "feat(workflow): materialize trusted workflow descriptors from snapshots"
```

---

### Task 4: Child workflow ceiling (W-7)

**Files:**
- Modify: `crates/lato-extensions/src/registry.rs`
- Modify: `crates/lato-extensions/src/workflows/mod.rs` to filter by ceiling
- Test: `crates/lato-extensions/tests/workflow_config.rs` additional cases
- Mirror `McpCapabilityCeiling`: `WorkflowCapabilityCeiling { allowed: Option<BTreeSet<String>> }` where names are qualified `plugin/workflow`

**Interfaces:**
- `CapabilityCeiling.workflows: WorkflowCapabilityCeiling`
- `PluginSnapshot.workflow_ceiling`
- `derive_child` intersects ceilings; `!ExtensionInvoke` → `deny_all` and clears loaded workflow fields
- `materialize_workflows` drops descriptors not allowed by ceiling

- [ ] **Step 1: Failing tests** — parent A+B, child allowlist A → only A; child cannot restore disabled plugin workflow.

- [ ] **Step 2: Confirm red** `cargo test -p lato-extensions --test workflow_config`

- [ ] **Step 3: Implement ceiling + intersect** (copy `intersect_optional_sets`).

- [ ] **Step 4: Tests green.**

- [ ] **Step 5: Commit** `feat(extensions): narrow child workflow capability ceilings`

---

### Task 5: Inert run does not debit budget (W-5, W-6)

**Files:**
- Create: `crates/lato-workflow/tests/inert_runtime.rs`
- `lato-workflow/Cargo.toml` dev-deps: `tokio` macros, `lato-core`

**Interfaces:**
- Construct `BudgetAccount` (or the public constructor used in core tests) with unlimited limits, snapshot spent/reserved, call `InertWorkflow::run`, assert amounts unchanged and error code `workflow.not_implemented`.

- [ ] **Step 1: Write the test.** If `BudgetAccount` construction is private, use whatever `lato-core` tests use (`BudgetAccount::new` / `unlimited`). Do not spawn tasks.

- [ ] **Step 2: Run** `cargo test -p lato-workflow --test inert_runtime`  
Expected: fail until test compiles; then pass if Task 1 already returns NotImplemented.

- [ ] **Step 3: Fix only if run accidentally succeeds or touches budget.**

- [ ] **Step 4: Green.**

- [ ] **Step 5: Commit** `test(workflow): inert run does not reserve budget`

Cancel: do **not** add a new API. Add a one-paragraph comment on `WorkflowContext` pointing at `TaskCoordinator::cancel_workflow`. Existing `lato-runtime` tests remain the cancel authority (W from spec §8).

---

### Task 6: Docs and release gate (W-9)

**Files:**
- Modify: `README.md` — short **Plugin Workflows** section after Plugin MCP: config shape, inert, 7B will execute, no doctor listing
- Modify: design spec status → A-level gate passed + date
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Create: `docs/testing/reports/phase-7a-workflow-interface-release-gate-2026-09-12.md`

- [ ] **Step 1: Write README + ledger + gate report skeleton**

- [ ] **Step 2:**

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path .
lato --version
```

Expected: all pass; 7A tests included.

- [ ] **Step 3: Fill gate report with SHAs and counts.**

- [ ] **Step 4: Commit** `docs: record phase 7a workflow interface gate`

---

## Spec coverage

| Spec ID | Task |
| --- | --- |
| W-1 trust/enable | 3 |
| W-2 path/JSON | 2+3 |
| W-3 names/collision | 1+3 |
| W-4 budget clamp | 1+3 |
| W-5 no spawn/budget | 5 |
| W-6 error code | 1+5 |
| W-7 child narrow | 4 |
| W-8 component kind | 2+3 |
| W-9 gate | 6 |
| No AgentField / no doctor list | 6 README |

## Type consistency

- Ids: `plugin/workflow` everywhere (not `plugin__workflow`).
- `WorkflowError::NotImplemented` / `workflow.not_implemented`.
- Ceiling field name: `allowed` on `WorkflowCapabilityCeiling`.
- Convention file: `workflows.json`.
