# Lato Phase 6C MCP Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver trusted+enabled MCP runtime over frozen `PluginSnapshot`: stdio and streamable HTTP transports, bounded lifecycle, schema cache, progressive `search_tool`/`use_tool` discovery, and full ToolRuntime safety membrane integration—without a second execution channel.

**Architecture:** `lato-extensions` materializes immutable MCP descriptors from the turn's frozen Phase 6A snapshot. `lato-mcp` owns transport, server lifecycle, initialize/tools-list, schema cache, health, cancel, and process-tree reap. `lato-tools` registers MCP as a ToolRegistry provider (`search_tool`/`use_tool`, optional direct `server__tool` expansion). `lato-agent` binds generation-scoped `McpManager` into session lifecycle and routes every MCP invocation exclusively through `ToolRuntime::prepare_scoped` → PolicyEngine → approval → execute → PostToolUse. Hooks may apply `updatedMCPToolOutput` only after the MCP result type exists.

**Tech Stack:** Rust 2024, Tokio process/time/io/sync, reqwest + rustls, url, serde/serde_json, existing cancellation/process/sandbox primitives, Phase 6A `PluginSnapshot`, Phase 6B2 hook membrane, `bound_tool_output`.

**Status:** A-level gate passed (2026-09-11; see `docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md`)  
**Base commit:** `master @ 2a66a019` (Phase 6B2 hooks already green on master—**do not wait**; build atop this SHA).

## Global Constraints

- Base on green Phase 6B2 hooks already on master (`2a66a019`); do not re-land hooks.
- Consume **only** trusted, enabled MCP descriptors from the turn's frozen `PluginSnapshot`.
- **Critical security invariant:** MCP tools are a new ToolRegistry provider, NOT a second execution channel. Tool name, args, permissions, approval, Hook rewrite, and sandbox/scope must all pass existing unified boundaries. `McpManager` must NOT bypass `ToolRuntime`.
- Support exactly stdio and streamable HTTP in this phase.
- Default model surface: only `search_tool` / `use_tool`; direct expansion of few servers is an explicit opt-in.
- Qualified wire names: `server__tool`. Collisions must not silently overwrite.
- HTTP: SSRF checks after resolution; **no redirects**; redact credentials from user-visible errors/journal.
- Reuse hook-style process-group reap and SessionEnd bounded shutdown patterns.
- Child sessions may only narrow MCP server/tool capability; never restore removed parent perms.
- N→N+1 reload: in-flight turns keep generation N MCP resources; next turn adopts N+1.
- Preserve user-owned dirty files; commit only files named by each task when committing.
- Add Grok source/license headers to substantially derived production files and update the upstream-source ledger.
- Per `AGENTS.md`: after each feature slice is validated, run relevant tests then **`cargo install --path .`** from the repository root before installed-binary smoke.
- SenseNova exit0/no-artifact bug is **parallel** and must not block this plan.

---

## File Structure

### `crates/lato-mcp/`

- `src/lib.rs`: exports, limits, error types.
- `src/config.rs`: `.mcp.json` / inline server descriptor parsing (stdio vs streamable HTTP).
- `src/names.rs`: server normalization, `server__tool` qualification, collision policy.
- `src/transport/mod.rs`: transport traits.
- `src/transport/stdio.rs`: persistent stdio JSON-RPC session (replace one-shot stub semantics).
- `src/transport/http.rs`: streamable HTTP client with SSRF + no-redirect.
- `src/protocol.rs`: `initialize`, `notifications/initialized`, `tools/list`, `tools/call` envelopes.
- `src/lifecycle.rs`: start, health, timeout, cancel, process-tree reap, shutdown.
- `src/registry.rs`: `McpRegistry` / schema cache bound to snapshot generation.
- `src/manager.rs`: `McpManager` session-facing API (still no bypass of ToolRuntime).
- `src/discovery.rs`: progressive discovery helpers / search index builders used by tools.
- `tests/` or crate tests: transport fixtures, lifecycle, SSRF, collision.

### `crates/lato-extensions/`

- `src/mcp/mod.rs` (or thin re-export helpers): materialize MCP specs from `PluginSnapshot::active_plugins()`.
- `tests/mcp_config.rs`: trust/enable isolation, path confinement, inline vs file.

### `crates/lato-tools/`

- `src/mcp_provider.rs`: ToolRegistry provider registering `search_tool`, `use_tool`, optional direct tools.
- `src/runtime.rs` / `catalog.rs`: registration seams (modify as needed).
- `src/output.rs`: ensure MCP results pass `bound_tool_output`.
- `tests/`: provider + membrane unit tests.

### `crates/lato-agent/`

- `src/mcp.rs`: generation-paired manager binding, SessionEnd shutdown, reload staging.
- `src/actor.rs` / `runtime_session.rs` / `subagent/runner.rs`: lifecycle seams.
- `tests/mcp_runtime.rs`: membrane, reload, child narrowing, progressive discovery.

### `tests/` + docs

- `tests/phase6c_mcp_smoke.rs`: installed-command smoke (stdio + HTTP fixtures).
- `README.md`: MCP section (replace “remains Phase 6C”).
- `docs/superpowers/reference/lato-upstream-sources.md`: attribution.
- `docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md`: gate report.

### Existing stub to evolve (do not invent a parallel crate)

- Today `lato-mcp` only exposes `call_stdio` / `call_streamable_http` in `transport.rs` (one-shot, 30s, HTTP without SSRF/no-redirect). Tasks below **replace/extend** this stub rather than leaving a second client path.

---

## Task 1: MCP Config & Descriptor Contract

**Workstream:** 1 — MCP config & descriptor contract

**Files:**
- Create: `crates/lato-mcp/src/config.rs`
- Create: `crates/lato-mcp/src/names.rs`
- Modify: `crates/lato-mcp/src/lib.rs`
- Modify: `crates/lato-mcp/Cargo.toml`
- Create: `crates/lato-extensions/src/mcp/mod.rs` (if thin materializer lives here)
- Create: `crates/lato-extensions/tests/mcp_config.rs`
- Modify: `crates/lato-extensions/src/lib.rs`

**Interfaces:**
- Consumes: `PluginSnapshot::active_plugins()`, `LoadedPlugin::mcp_config_path`, `LoadedPlugin::inline_mcp_servers`, canonical plugin roots.
- Produces:

```rust
pub enum McpTransportKind { Stdio, StreamableHttp }

pub struct McpServerSpec {
    pub id: String,                    // stable: plugin/server
    pub plugin_name: String,
    pub server_name: String,
    pub transport: McpTransportKind,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<Url>,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
}

pub struct McpDescriptorSet {
    pub generation: u64,
    pub servers: Arc<[McpServerSpec]>,
    pub diagnostics: Arc<[McpDiagnostic]>,
}

pub fn materialize_mcp(snapshot: &PluginSnapshot) -> Arc<McpDescriptorSet>;
pub fn qualify_tool(server: &str, tool: &str) -> String; // server__tool
```

- [ ] **Step 1: Add modules, limits, and exports**

Export parse limits (max servers per plugin, max env/header entries, max timeout clamp). Keep IDs/API names in English.

- [ ] **Step 2: Write failing contract tests**

Cover: only active trusted+enabled plugins; untrusted project `.mcp.json` yields empty set; path escape rejected; stdio vs HTTP shape; inline metadata if in-scope; reserved env stripping; collision diagnostics; empty/malformed file isolation.

- [ ] **Step 3: Run focused test and confirm red**

Run: `cargo test -p lato-extensions --test mcp_config`

Expected: compile failure or missing `materialize_mcp`.

- [ ] **Step 4: Implement parsing and materialization**

Parse file and approved inline forms. Canonicalize command/cwd under plugin root policy. Normalize server names. Do not start processes in this task.

- [ ] **Step 5: Run contract tests**

Run: `cargo test -p lato-extensions --test mcp_config`  
Run: `cargo test -p lato-mcp`

Expected: green; Phase 6A/6B tests unaffected.

- [ ] **Step 6: Commit descriptor slice**

```bash
git add crates/lato-mcp crates/lato-extensions
git commit -m "feat(mcp): materialize trusted MCP descriptors from snapshots"
```

---

## Task 2: Bounded Server Lifecycle & Transports

**Workstream:** 2 — Bounded server lifecycle

**Files:**
- Replace/extend: `crates/lato-mcp/src/transport.rs` → `transport/stdio.rs`, `transport/http.rs`
- Create: `crates/lato-mcp/src/protocol.rs`
- Create: `crates/lato-mcp/src/lifecycle.rs`
- Create: `crates/lato-mcp/src/manager.rs` (skeleton)
- Create: crate tests / `tests` for fixtures

**Interfaces:**

```rust
pub struct McpServerHandle { /* generation, server_name, transport */ }

pub async fn start_server(
    spec: &McpServerSpec,
    cancel: CancellationToken,
) -> Result<McpServerHandle, McpError>;

pub async fn initialize(handle: &McpServerHandle) -> Result<InitializeResult, McpError>;
pub async fn shutdown_server(handle: McpServerHandle, deadline: Instant) -> Result<(), McpError>;
```

- [ ] **Step 1: Write failing lifecycle tests**

stdio fixture: start → initialize → shutdown leaves no child. Timeout kills process group. Cancel mid-call. One server panic/isolation. HTTP: valid loopback call; blocked private IP; redirect not followed.

- [ ] **Step 2: Confirm red**

Run: `cargo test -p lato-mcp`

- [ ] **Step 3: Implement persistent stdio transport**

Replace one-shot `call_stdio` with a session that multiplexes JSON-RPC over stdin/stdout, respects cancel/timeout, creates a process group on Unix, and reaps on drop/shutdown. Keep a compatibility wrapper only if tests still need it—prefer deleting unsafe one-shot semantics from the hot path.

- [ ] **Step 4: Implement streamable HTTP with SSRF + no-redirect**

Reuse the address-class policy pattern from `lato-extensions` hooks HTTPS (`validate_hook_url` / `blocked`). `redirect::Policy::none()`. Do not attach ambient auth beyond spec headers. Redact URL credentials in errors.

- [ ] **Step 5: Wire initialize + health + SessionEnd-friendly shutdown**

- [ ] **Step 6: Run lifecycle tests; `cargo install --path .` after green per AGENTS.md**

- [ ] **Step 7: Commit**

```bash
git add crates/lato-mcp
git commit -m "feat(mcp): bound stdio and streamable HTTP server lifecycle"
```

---

## Task 3: Tool Discovery & Schema Cache

**Workstream:** 3 — Tool discovery & schema cache

**Files:**
- Create: `crates/lato-mcp/src/registry.rs`
- Modify: `crates/lato-mcp/src/manager.rs`
- Create: discovery-focused tests

**Interfaces:**

```rust
pub struct McpToolDescriptor {
    pub server: String,
    pub name: String,
    pub qualified_name: String, // server__tool
    pub description: String,
    pub input_schema: Value,
}

pub struct McpSchemaCache {
    generation: u64,
    by_qualified: BTreeMap<String, McpToolDescriptor>,
    by_server: BTreeMap<String, Arc<[McpToolDescriptor]>>,
}

impl McpManager {
    pub async fn ensure_discovered(&self, server: &str) -> Result<(), McpError>;
    pub fn cache(&self) -> Arc<McpSchemaCache>;
    pub fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor>;
}
```

- [ ] **Step 1: Failing tests for tools/list caching, collision, bad schema isolation**

- [ ] **Step 2: Implement initialize → tools/list → stable cache for generation**

- [ ] **Step 3: Collision policy (diagnose + keep first / reject later)—document choice in code**

- [ ] **Step 4: Tests green; commit**

```bash
git commit -m "feat(mcp): cache MCP tool schemas per snapshot generation"
```

---

## Task 4: Progressive Discovery Tools

**Workstream:** 4 — Progressive discovery

**Files:**
- Create: `crates/lato-tools/src/mcp_provider.rs`
- Modify: `crates/lato-tools/src/lib.rs`, builder registration paths
- Tests under `lato-tools` / `lato-agent`

**Interfaces:**

```rust
// Built-in tools registered into ToolCatalog:
// - search_tool { query: string } -> matches from McpSchemaCache
// - use_tool { tool: string, arguments: object } OR { server, name, arguments }
// Optional config: direct_expand_servers: Vec<String> adds those server__tool specs to model_definitions
```

- [ ] **Step 1: Failing tests — default model_definitions exclude raw MCP tools; search/use work**

- [ ] **Step 2: Implement `search_tool` / `use_tool` as normal `Tool` impls that call manager only **after** runtime grant path is established (invoke body may talk to manager; registration still via catalog)

- [ ] **Step 3: Optional direct expansion behind explicit allowlist**

- [ ] **Step 4: Tests + `cargo install --path .`; commit**

```bash
git commit -m "feat(tools): progressive MCP discovery via search_tool and use_tool"
```

---

## Task 5: Unified Tool Safety Membrane

**Workstream:** 5 — Unified tool safety membrane

**Files:**
- Modify: `crates/lato-agent/src/actor.rs`, `hooks.rs` seams as needed
- Create: `crates/lato-agent/src/mcp.rs`
- Create: `crates/lato-agent/tests/mcp_runtime.rs`
- Possibly PostToolUse MCP result type in extensions/agent

**Critical steps:**

- [ ] **Step 1: Write failing membrane tests**

Assert ordering: PreToolUse → `prepare_scoped` → PolicyEngine decision → approval → `execute` → PostToolUse. Assert deny/ask paths. Assert rewrite re-enters `prepare_scoped`. Assert no agent path calls `McpManager::call_tool` without going through `ToolRuntime`.

- [ ] **Step 2: Confirm red**

Run: `cargo test -p lato-agent --test mcp_runtime`

- [ ] **Step 3: Bind McpManager into session extension state (generation-paired with SkillCatalog/HookRegistry)**

- [ ] **Step 4: Ensure `use_tool` / direct MCP tools use the same authorization pipeline as builtins**

Hook `allow` never skips policy. Tool name immutable under PreToolUse.

- [ ] **Step 5: Enable `updatedMCPToolOutput` application now that MCP result type exists; re-bound via `bound_tool_output`**

- [ ] **Step 6: Tests green; commit**

```bash
git commit -m "feat(agent): route MCP calls through ToolRuntime safety membrane"
```

---

## Task 6: Result & Fault Boundaries

**Workstream:** 6 — Result & fault boundaries

**Files:**
- Modify: `crates/lato-mcp` error mapping, HTTP validator
- Modify: `crates/lato-tools/src/output.rs` usage from MCP provider
- Modify: `crates/lato-core` journal audit variant if needed
- Tests: overflow, cancel, crash, SSRF, redact

- [ ] **Step 1: Failing tests for truncation/spill, cancel, protocol error codes, crash isolation, SSRF, no redirect, journal redaction**

- [ ] **Step 2: Implement bounds and audit records (hash args/results; no secrets)**

- [ ] **Step 3: Tests green; commit**

```bash
git commit -m "feat(mcp): harden MCP results, SSRF, and fault isolation"
```

---

## Task 7: Reload & Parent/Child Capability Narrowing

**Workstream:** 7 — Reload & parent/child sessions

**Files:**
- Modify: `crates/lato-extensions/src/registry.rs` (`derive_child` / new MCP ceiling)
- Modify: `crates/lato-agent/src/mcp.rs`, `runtime_session.rs`, `subagent/runner.rs`
- Extend: `crates/lato-agent/tests/mcp_runtime.rs`, subagent tests

**Interfaces (illustrative):**

```rust
pub struct McpCapabilityCeiling {
    pub allowed_servers: Option<BTreeSet<String>>, // None = inherit all from snapshot
    pub allowed_tools: Option<BTreeSet<String>>,   // qualified names
}

// Child materialization: PluginSnapshot::derive_child then filter McpDescriptorSet
// Never re-add servers/tools absent from parent grant
```

- [ ] **Step 1: Failing tests — mid-turn N/N+1 isolation; child cannot restore removed MCP; parent reload does not mutate running child**

- [ ] **Step 2: Implement generation adopt/retire for McpManager (cancel+reap on retire)**

- [ ] **Step 3: Implement server/tool narrowing on child derivation**

- [ ] **Step 4: Tests green; commit**

```bash
git commit -m "feat(agent): isolate MCP generations and narrow child capabilities"
```

---

## Task 8: Docs, Smoke, and Phase 6C Release Gate

**Workstream:** 8 — Phase 6C release gate

**Files:**
- Create: `tests/phase6c_mcp_smoke.rs`
- Modify: `README.md`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Create: `docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md`

- [ ] **Step 1: README — document MCP config, progressive discovery, trust prerequisite, shutdown, child narrowing; remove “MCP remains Phase 6C”**

- [ ] **Step 2: Installed-command smoke**

Fixture: trusted temp plugin with stdio server + loopback streamable HTTP server. Prove search→use, SSRF reject sample, SessionEnd reap, child narrowing. Emit `phase6c-mcp-smoke-ok`.

- [ ] **Step 3: Full quality gate**

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path .
lato --version
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6c_mcp_smoke -- --nocapture
```

- [ ] **Step 4: Write release report with exact commands, counts, platform notes**

- [ ] **Step 5: Commit gate**

```bash
git add tests/phase6c_mcp_smoke.rs README.md docs/superpowers/reference/lato-upstream-sources.md docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md
git commit -m "docs: record phase 6c mcp runtime gate"
```

---

## Phase 6C Completion Checkpoint

Stop after Task 8 and report: every commit, workspace test count, clippy result, installed binary version, stdio + HTTP smoke results, and confirmation that SenseNova bug remained parallel/non-blocking. Subsequent phases may deepen MCP Resources/Prompts only after this checkpoint is reviewed.

## Mapping to acceptance IDs

| Task | Primary acceptance IDs |
| --- | --- |
| 1 | M-1, M-2, M-3 |
| 2 | M-4, M-5, M-6, M-16 |
| 3 | M-7, M-8 |
| 4 | M-9, M-10, M-11 |
| 5 | M-12, M-13, M-14, M-20 |
| 6 | M-15, M-16, M-17 |
| 7 | M-18, M-19 |
| 8 | M-21, M-22 |
