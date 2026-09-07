# Lato Phase 6A Plugin Runtime Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Freeze Phase 5B/5C behind a real installed-command release gate, then port Grok Build's manifest, discovery, trust, immutable registry snapshot, reload, and capability-narrowing foundation into Lato.

**Architecture:** A new `lato-extensions` crate owns plugin parsing, deterministic discovery, active-registry construction, immutable generation-stamped snapshots, and child narrowing. `AcpHost` owns one shared registry handle and fans rebuilt snapshots into `RuntimeSession`; each session freezes one snapshot for the active turn and stages reloads for the next turn. Existing `lato-mcp` plugin stubs are removed after all callers migrate, leaving MCP transport execution for Phase 6C.

**Tech Stack:** Rust 2024, Tokio, Serde/JSON, SHA-256, `Arc`, bounded diagnostics, existing Lato `SessionTrust`, `PolicyEngine` capability types, runtime session actor, ACP JSON-RPC, and local OpenAI-compatible SSE fixtures.

## Global Constraints

- Port behavior from Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` under Apache-2.0.
- Add source headers to substantially derived production files and update `docs/superpowers/reference/lato-upstream-sources.md`.
- Preserve all unrelated dirty-worktree files.
- Source precedence is CLI override, project `.lato/plugins`, then user `$LATO_HOME/plugins`.
- CLI and user plugins are trusted; project plugins require the existing folder-trust verdict.
- A plugin must be trusted and enabled to enter the active registry.
- Reload publishes only a fully built snapshot, preserves the last known-good generation on global failure, and never mutates an issued snapshot.
- An active turn keeps its frozen generation; an idle session or next turn adopts the newest staged generation.
- Parent-to-child derivation may only narrow capabilities and never updates an already-running child.
- Skills, hooks, and MCP descriptors remain inert in Phase 6A.
- Every diagnostic list, message, path, and reload response is bounded.
- Run relevant tests before the mandatory final `cargo install --path .`.

---

## Planned file structure

```text
Cargo.toml
Cargo.lock
crates/lato-extensions/
  Cargo.toml
  src/lib.rs                 # public exports and limits
  src/manifest.rs            # plugin.json and convention manifest model
  src/discovery.rs           # deterministic CLI/project/user scanning
  src/trust.rs               # source trust rules
  src/registry.rs            # enablement, conflicts, immutable snapshots
  src/reload.rs              # serialized shared handle and generations
  tests/manifest.rs
  tests/discovery.rs
  tests/registry.rs
  tests/reload.rs
crates/lato-protocol/src/
  plugins.rs                 # typed reload request/response DTOs
  methods.rs
  lib.rs
crates/lato-core/src/
  command.rs                 # session snapshot adoption command
  event.rs                   # canonical snapshot events
  journal.rs                 # durable adoption/reload audit records
crates/lato-runtime/src/session.rs
crates/lato-agent/src/
  extension_runtime.rs       # session snapshot table and child derivation
  runtime_session.rs         # current/pending/turn-frozen snapshot lifecycle
  subagent/runner.rs         # child receives narrowed parent generation
  host.rs                    # shared discovery and lato/plugins/reload fan-out
  lib.rs
crates/lato-mcp/src/lib.rs    # remove superseded plugin discovery stubs
src/args.rs                   # repeatable --plugin-dir
src/cli.rs
src/stdio.rs
tests/phase5_command_smoke.rs
tests/plugin_cli.rs
crates/lato-agent/tests/plugin_runtime.rs
docs/superpowers/reference/lato-upstream-sources.md
docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
README.md
```

---

### Task 1: Freeze the Phase 5 command-level release baseline

**Files:**
- Create: `tests/phase5_command_smoke.rs`
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: the installed or Cargo-built `lato` binary, custom `models.json`, OpenAI-compatible SSE, and the existing `spawn`, `inspect`, and `wait` tool schemas.
- Produces: `phase5_installed_command_smoke_exercises_real_task_chain`, a reproducible offline release command, and a dated Phase 5B/5C baseline record.

- [ ] **Step 1: Write the failing installed-command smoke**

Add a serial integration test that starts a localhost SSE server, writes a fresh
`models.json`, creates a temporary Git repository, invokes the real binary, and
serves this deterministic model sequence:

```rust
const SPAWN: &str = r#"data: {"choices":[{"delta":{"tool_calls":[{"id":"spawn-1","function":{"name":"spawn","arguments":"{\"task_id\":\"phase5-child\",\"profile\":\"worker\",\"task\":\"inspect the fixture\",\"background\":false}"}}]}}]}

data: [DONE]

"#;

const CHILD_RESULT: &str = r#"data: {"choices":[{"delta":{"content":"{\"summary\":\"fixture inspected\",\"changed_files\":[],\"tests\":[],\"artifacts\":[]}"}}]}

data: [DONE]

"#;

const INSPECT: &str = r#"data: {"choices":[{"delta":{"tool_calls":[{"id":"inspect-1","function":{"name":"inspect","arguments":"{\"task_id\":\"phase5-child\"}"}}]}}]}

data: [DONE]

"#;

const WAIT: &str = r#"data: {"choices":[{"delta":{"tool_calls":[{"id":"wait-1","function":{"name":"wait","arguments":"{\"task_id\":\"phase5-child\",\"timeout_ms\":3000}"}}]}}]}

data: [DONE]

"#;

const COMPLETE: &str = r#"data: {"choices":[{"delta":{"content":"phase5-smoke-ok"}}]}

data: [DONE]

"#;

#[test]
fn phase5_installed_command_smoke_exercises_real_task_chain() {
    let binary = std::env::var_os("LATO_SMOKE_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_lato")));
    let outcome = run_scripted_task_smoke(&binary, [SPAWN, CHILD_RESULT, INSPECT, WAIT, COMPLETE]);
    assert_eq!(outcome.stdout.trim(), "phase5-smoke-ok");
    assert!(outcome.session_journal.is_file());
    assert!(outcome.worktree_root_is_empty);
}
```

`run_scripted_task_smoke` must parse every HTTP request body, assert that the
parent advertises `spawn`, `send`, `wait`, `cancel`, and `inspect` but not
`spawn_subagent`, distinguish the child request by its injected worker profile,
and enforce a 30-second process timeout. The fixture must fail if a request is
missing, duplicated, or arrives after the terminal response.

- [ ] **Step 2: Run the smoke against the Cargo binary and verify the initial result**

Run:

```bash
cargo test --test phase5_command_smoke -- --nocapture
```

Expected before the fixture is complete: FAIL because
`phase5_command_smoke.rs` or `run_scripted_task_smoke` does not exist. Expected
after completing the fixture: one passing test whose child runs through the
real command, model adapter, tool runtime, coordinator, child session, verifier,
and cleanup path.

- [ ] **Step 3: Run the complete mandatory Phase 5 gate**

Run exactly:

```bash
cargo fmt --all -- --check
cargo test -p lato-runtime
cargo test -p lato-workspace --test git_allocator --test task_workspace --test sandbox_obligation
cargo test -p lato-tools --test tool_runtime --test policy_runtime
cargo test -p lato-agent --test subagent_runner --test acp_runtime --test runtime_session
cargo test --test cli_headless --test sessions_cli --test tui_cli --test phase5_command_smoke
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: every command exits 0. The full workspace run reports zero failed and
zero ignored mandatory tests. Do not convert a failure into a documentation-only
exception.

- [ ] **Step 4: Install and rerun the command smoke against the installed binary**

Run:

```bash
cargo install --path .
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase5_command_smoke -- --nocapture
test "$(lato --version)" = "lato 0.1.0-beta.2"
```

Expected: installation succeeds, the installed-binary smoke passes, and the
version assertion exits 0.

- [ ] **Step 5: Record only measured gate evidence**

Append a `Phase 5B/5C local release gate (2026-09-07)` table to the acceptance
results. Each row must name the exact command, its actual pass count or exit
status, and any skipped optional check. Add a short README note linking the
five model-visible task names and the offline smoke command:

````markdown
## Phase 5 subagent baseline

The model-facing task surface is `spawn`, `send`, `wait`, `cancel`, and
`inspect`; the former `spawn_subagent` compatibility tool is not exposed.
The local release gate drives these tools through a real `lato` process and a
localhost scripted model:

```bash
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase5_command_smoke -- --nocapture
```
````

- [ ] **Step 6: Commit the frozen baseline**

```bash
git add tests/phase5_command_smoke.rs README.md docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
git commit -m "test: freeze phase 5 command baseline"
```

---

### Task 2: Create `lato-extensions` and port the manifest contract

**Files:**
- Create: `crates/lato-extensions/Cargo.toml`
- Create: `crates/lato-extensions/src/lib.rs`
- Create: `crates/lato-extensions/src/manifest.rs`
- Create: `crates/lato-extensions/tests/manifest.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `PluginManifest`, `Author`, `PathOrPaths`, `PathOrInline`, `ManifestLoadResult`, `ManifestError`, `load_manifest`, and `name_from_dirname`.
- Consumes: a canonical plugin root and the three Phase 6A convention component locations.

- [ ] **Step 1: Add failing manifest contract tests**

```rust
#[test]
fn canonical_manifest_is_forward_compatible_and_resolves_components() {
    let root = fixture_plugin(r#"{
      "name":"demo-plugin",
      "futureField":true,
      "skills":["skills","extra-skills"],
      "hooks":"hooks/hooks.json",
      "mcpServers":{"demo":{"command":"demo"}}
    }"#);
    create_dir(root.path(), "skills");
    create_dir(root.path(), "extra-skills");
    create_file(root.path(), "hooks/hooks.json", "{}");
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert_eq!(manifest.name, "demo-plugin");
    assert_eq!(manifest.skill_dirs(root.path()).len(), 2);
    assert!(manifest.hooks_path(root.path()).is_some());
    assert!(manifest.inline_mcp_servers().is_some());
}

#[cfg(unix)]
#[test]
fn component_symlink_escaping_root_is_excluded() {
    let root = fixture_plugin(r#"{"name":"demo","skills":"outside"}"#);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert!(manifest.skill_dirs(root.path()).is_empty());
}
```

Also cover invalid names, absolute paths, `..`, missing components, the
convention fallback, an empty convention directory, unknown fields, and inline
hook/MCP preservation without execution.

- [ ] **Step 2: Run the tests and verify failure**

Run:

```bash
cargo test -p lato-extensions --test manifest
```

Expected: Cargo fails because the crate and manifest types do not exist.

- [ ] **Step 3: Add the crate and port the manifest parser**

Use these public definitions and limits:

```rust
pub const MAX_PLUGIN_NAME_LEN: usize = 64;
pub const MAX_COMPONENT_PATHS: usize = 64;
pub const MAX_COMPONENT_PATH_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)] pub version: Option<String>,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub author: Option<Author>,
    #[serde(default)] pub homepage: Option<String>,
    #[serde(default)] pub repository: Option<String>,
    #[serde(default)] pub license: Option<String>,
    #[serde(default)] pub keywords: Vec<String>,
    #[serde(default)] pub skills: Option<PathOrPaths>,
    #[serde(default)] pub hooks: Option<PathOrInline>,
    #[serde(default)] pub mcp_servers: Option<PathOrInline>,
}

pub enum ManifestLoadResult {
    Found(Box<PluginManifest>),
    Convention(Box<PluginManifest>),
    NotFound,
}

pub fn load_manifest(plugin_root: &std::path::Path)
    -> Result<ManifestLoadResult, ManifestError>;
```

Only `plugin.json` is canonical in Lato. Resolve existing paths with
`dunce::canonicalize`, reject absolute and parent components before joining,
and fail closed when containment cannot be proven. Add the exact Grok source
header at the top of `manifest.rs`.

- [ ] **Step 4: Run focused tests and formatting**

```bash
cargo fmt --all
cargo test -p lato-extensions --test manifest
```

Expected: all manifest tests pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/lato-extensions
git commit -m "feat(extensions): port plugin manifest loading"
```

---

### Task 3: Port deterministic discovery and source trust

**Files:**
- Create: `crates/lato-extensions/src/discovery.rs`
- Create: `crates/lato-extensions/src/trust.rs`
- Create: `crates/lato-extensions/tests/discovery.rs`
- Modify: `crates/lato-extensions/src/lib.rs`

**Interfaces:**
- Consumes: `DiscoveryConfig { cwd, lato_home, cli_plugin_dirs, project_trusted }` and `load_manifest`.
- Produces: `PluginScope`, `PluginOrigin`, `PluginId`, `DiscoveredPlugin`, `DiscoveryDiagnostic`, and `discover_plugins`.

- [ ] **Step 1: Write failing source, precedence, trust, and containment tests**

```rust
#[test]
fn source_precedence_and_project_trust_match_grok_build() {
    let fixture = DiscoveryFixture::new();
    fixture.plugin(Source::User, "same", r#"{"name":"same"}"#);
    fixture.plugin(Source::Project, "same", r#"{"name":"same"}"#);
    let cli = fixture.plugin(Source::Cli, "same", r#"{"name":"same"}"#);

    let untrusted = discover_plugins(fixture.config(false, vec![cli.clone()]));
    let winner = untrusted.plugins.iter().find(|p| p.name() == "same").unwrap();
    assert_eq!(winner.scope, PluginScope::CliOverride);
    assert!(winner.trusted);

    let project_only = discover_plugins(fixture.config(false, vec![]));
    let project = project_only.plugins.iter().find(|p| p.scope == PluginScope::Project).unwrap();
    assert!(!project.trusted);
}

#[cfg(unix)]
#[test]
fn canonical_alias_is_loaded_once() {
    let fixture = DiscoveryFixture::new();
    let plugin = fixture.plugin(Source::User, "demo", r#"{"name":"demo"}"#);
    let alias = fixture.root().join("alias");
    std::os::unix::fs::symlink(&plugin, &alias).unwrap();
    let result = discover_plugins(fixture.config(true, vec![alias, plugin]));
    assert_eq!(result.plugins.iter().filter(|p| p.name() == "demo").count(), 1);
}
```

Cover stable same-scope sorting, direct CLI roots versus child-directory scans,
unreadable roots, invalid manifests, broken symlinks, duplicate canonical roots,
and bounded diagnostic count/message length.

- [ ] **Step 2: Run the tests and verify failure**

```bash
cargo test -p lato-extensions --test discovery
```

Expected: compilation fails because discovery types are absent.

- [ ] **Step 3: Port discovery with exact Lato sources**

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PluginScope {
    CliOverride = 0,
    Project = 1,
    User = 2,
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub cwd: std::path::PathBuf,
    pub lato_home: std::path::PathBuf,
    pub cli_plugin_dirs: Vec<std::path::PathBuf>,
    pub project_trusted: bool,
}

#[derive(Clone, Debug)]
pub struct DiscoveryResult {
    pub plugins: Vec<DiscoveredPlugin>,
    pub diagnostics: Vec<DiscoveryDiagnostic>,
}

pub fn discover_plugins(config: &DiscoveryConfig) -> DiscoveryResult;
```

Scan CLI roots first, then `<cwd>/.lato/plugins/*`, then
`<lato_home>/plugins/*`. Canonicalize before deduplication, sort immediate child
directories, and resolve same-name conflicts after collection. Trust is an
exhaustive `match`: CLI/User `true`, Project `config.project_trusted`.

- [ ] **Step 4: Run focused tests**

```bash
cargo fmt --all
cargo test -p lato-extensions --test discovery
```

Expected: all discovery tests pass deterministically on two consecutive runs.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-extensions/src crates/lato-extensions/tests/discovery.rs
git commit -m "feat(extensions): port deterministic plugin discovery"
```

---

### Task 4: Build immutable registries, enablement, and child narrowing

**Files:**
- Create: `crates/lato-extensions/src/registry.rs`
- Create: `crates/lato-extensions/tests/registry.rs`
- Modify: `crates/lato-extensions/src/lib.rs`

**Interfaces:**
- Consumes: `DiscoveryResult`, persisted `PluginConfig`, `ToolCapability`, and child profile/workspace permissions.
- Produces: `PluginConfig`, `LoadedPlugin`, `PluginSnapshot`, `PluginComponentKind`, `CapabilityCeiling`, `PluginSnapshot::derive_child`.

- [ ] **Step 1: Write failing active-set and narrowing tests**

```rust
#[test]
fn active_requires_both_trust_and_enablement() {
    let snapshot = snapshot_fixture(
        vec![
            discovered("cli", PluginScope::CliOverride, true),
            discovered("user", PluginScope::User, true),
            discovered("project", PluginScope::Project, false),
        ],
        PluginConfig {
            enabled: vec!["user".into(), "project".into()],
            disabled: vec![],
        },
    );
    assert_eq!(snapshot.active_names(), vec!["cli", "user"]);
}

#[test]
fn child_derivation_can_only_remove_extension_capability() {
    let parent = active_snapshot_with_all_components();
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::ExtensionInvoke, ToolCapability::FileRead],
        profile: vec![ToolCapability::FileRead],
        workspace: vec![ToolCapability::FileRead],
    });
    assert!(child.active_plugins().is_empty());
    assert_eq!(child.parent_generation(), Some(parent.generation()));
}
```

Also test stable IDs, conflict diagnostics, CLI default-enabled behavior,
project/user default-disabled behavior, explicit disable precedence, unknown
configured names, immutable `Arc` cloning, and diagnostic byte/count limits.

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test -p lato-extensions --test registry
```

Expected: compilation fails because registry types are absent.

- [ ] **Step 3: Implement the immutable snapshot contract**

```rust
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct PluginConfig {
    #[serde(default)] pub enabled: Vec<String>,
    #[serde(default)] pub disabled: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PluginSnapshot {
    generation: u64,
    parent_generation: Option<u64>,
    built_at_ms: u64,
    project_trusted: bool,
    cli_plugin_dirs: std::sync::Arc<[std::path::PathBuf]>,
    plugins: std::sync::Arc<[LoadedPlugin]>,
    diagnostics: std::sync::Arc<[DiscoveryDiagnostic]>,
}

impl PluginSnapshot {
    pub fn generation(&self) -> u64;
    pub fn parent_generation(&self) -> Option<u64>;
    pub fn plugins(&self) -> &[LoadedPlugin];
    pub fn active_plugins(&self) -> impl Iterator<Item = &LoadedPlugin>;
    pub fn derive_child(&self, ceiling: &CapabilityCeiling) -> std::sync::Arc<Self>;
}

pub fn build_snapshot(
    generation: u64,
    discovery: DiscoveryResult,
    config: &PluginConfig,
) -> Result<std::sync::Arc<PluginSnapshot>, RegistryBuildError>;
```

`active` must be computed once during construction as `trusted && enabled`.
Never expose mutable collections. `derive_child` retains source identity and
generation provenance but removes every executable component when
`ExtensionInvoke` is absent from any ceiling set.

- [ ] **Step 4: Run focused tests**

```bash
cargo fmt --all
cargo test -p lato-extensions --test registry
```

Expected: all registry tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-extensions/src crates/lato-extensions/tests/registry.rs
git commit -m "feat(extensions): add immutable plugin snapshots"
```

---

### Task 5: Add serialized reload and last-known-good publication

**Files:**
- Create: `crates/lato-extensions/src/reload.rs`
- Create: `crates/lato-extensions/tests/reload.rs`
- Modify: `crates/lato-extensions/src/lib.rs`

**Interfaces:**
- Consumes: `DiscoveryConfig`, `PluginConfig`, and a `force` flag.
- Produces: `SharedPluginRegistryHandle`, `ReloadOutcome`, `snapshot`, `reload`, and per-session CLI-root rebuilding.

- [ ] **Step 1: Write failing generation and concurrency tests**

```rust
#[tokio::test]
async fn failed_global_reload_preserves_last_known_good_generation() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let before = handle.snapshot().await.unwrap();
    fixture.remove_discovery_root();
    let error = handle.reload(fixture.request(true)).await.unwrap_err();
    assert_eq!(error.code(), "plugin.reload_root_unavailable");
    assert_eq!(handle.snapshot().await.unwrap().generation(), before.generation());
}

#[tokio::test]
async fn concurrent_reloads_publish_monotonic_generations() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let (a, b) = tokio::join!(
        handle.reload(fixture.request(true)),
        handle.reload(fixture.request(true)),
    );
    let mut generations = [a.unwrap().generation, b.unwrap().generation];
    generations.sort_unstable();
    assert_eq!(generations[1], generations[0] + 1);
    assert_eq!(handle.snapshot().await.unwrap().generation(), generations[1]);
}
```

Also test malformed-candidate isolation, forced rediscovery, identical snapshot
publication, session-specific CLI roots, and no cross-session root leakage.

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test -p lato-extensions --test reload
```

Expected: compilation fails because the shared handle is absent.

- [ ] **Step 3: Implement the shared handle**

```rust
#[derive(Clone)]
pub struct SharedPluginRegistryHandle {
    state: std::sync::Arc<tokio::sync::RwLock<Option<std::sync::Arc<PluginSnapshot>>>>,
    reload_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
    next_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Debug)]
pub struct ReloadRequest {
    pub discovery: DiscoveryConfig,
    pub plugin_config: PluginConfig,
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReloadOutcome {
    pub generation: u64,
    pub discovered: usize,
    pub active: usize,
    pub diagnostic_count: usize,
}

impl SharedPluginRegistryHandle {
    pub async fn snapshot(&self) -> Option<std::sync::Arc<PluginSnapshot>>;
    pub async fn reload(&self, request: ReloadRequest) -> Result<ReloadOutcome, ReloadError>;
    pub async fn build_for_session(
        &self,
        request: ReloadRequest,
        session_cli_dirs: Vec<std::path::PathBuf>,
    ) -> Result<std::sync::Arc<PluginSnapshot>, ReloadError>;
}
```

Hold `reload_gate` across build and publication, but perform filesystem work in
`spawn_blocking`. Increment and publish a generation only after successful
construction. A missing configured CLI root is a global reload error; malformed
plugins under scanned project/user parents remain candidate diagnostics.

- [ ] **Step 4: Run focused tests**

```bash
cargo fmt --all
cargo test -p lato-extensions --test reload
```

Expected: all reload tests pass without deadlock under Tokio's multi-thread runtime.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-extensions/src crates/lato-extensions/tests/reload.rs
git commit -m "feat(extensions): add atomic plugin reload"
```

---

### Task 6: Add typed protocol and canonical plugin audit records

**Files:**
- Create: `crates/lato-protocol/src/plugins.rs`
- Modify: `crates/lato-protocol/src/lib.rs`
- Modify: `crates/lato-protocol/src/methods.rs`
- Modify: `crates/lato-core/src/command.rs`
- Modify: `crates/lato-core/src/event.rs`
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/tests/serde_contract.rs`
- Modify: `crates/lato-core/tests/journal_contract.rs`

**Interfaces:**
- Produces: `PluginReloadRequest`, `PluginReloadResponse`, `PluginDiagnosticDto`, `PluginSnapshotSummary`, `Command::AdoptPluginSnapshot`, plugin event payloads, and journal records.
- Consumes: only serializable summaries; `lato-core` and `lato-protocol` never depend on `lato-extensions` filesystem types.

- [ ] **Step 1: Write failing wire and journal round-trip tests**

```rust
#[test]
fn plugin_reload_response_is_stable_camel_case_json() {
    let response = PluginReloadResponse {
        generation: 7,
        discovered: 3,
        active: 2,
        diagnostics: vec![PluginDiagnosticDto {
            code: "plugin.untrusted_project".into(),
            plugin_id: Some("project/abcd1234/demo".into()),
            message: "project plugin is not trusted".into(),
        }],
    };
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["generation"], 7);
    assert_eq!(json["diagnostics"][0]["pluginId"], "project/abcd1234/demo");
}

#[test]
fn plugin_snapshot_adoption_round_trips_through_journal() {
    let record = JournalRecord::PluginSnapshotAdopted {
        summary: PluginSnapshotSummary {
            generation: 4,
            discovered: 2,
            active: 1,
            project_trusted: true,
        },
    };
    let encoded = serde_json::to_vec(&record).unwrap();
    assert_eq!(serde_json::from_slice::<JournalRecord>(&encoded).unwrap(), record);
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test -p lato-protocol
cargo test -p lato-core --test serde_contract --test journal_contract
```

Expected: compilation fails because the DTOs and variants are absent.

- [ ] **Step 3: Implement typed summaries and events**

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginReloadRequest {
    #[serde(default)] pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginReloadResponse {
    pub generation: u64,
    pub discovered: usize,
    pub active: usize,
    pub diagnostics: Vec<PluginDiagnosticDto>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PluginSnapshotSummary {
    pub generation: u64,
    pub discovered: usize,
    pub active: usize,
    pub project_trusted: bool,
}
```

Define `PluginSnapshotSummary` in `lato-core` so the core does not depend on
`lato-protocol`; the protocol DTO maps to that summary at the host boundary.
Add `Command::AdoptPluginSnapshot { summary }`,
`EventPayload::PluginSnapshotAdopted { summary }`, and
`JournalRecord::PluginSnapshotAdopted { summary }`. Reload request/completion
events may be ACP notifications; only session adoption must be durable.

- [ ] **Step 4: Run protocol/core tests**

```bash
cargo fmt --all
cargo test -p lato-protocol
cargo test -p lato-core --test serde_contract --test journal_contract
```

Expected: all tests pass and `lato/plugins/reload` remains advertised exactly once.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-protocol crates/lato-core
git commit -m "feat(protocol): add plugin snapshot audit contracts"
```

---

### Task 7: Freeze and stage snapshots in runtime sessions

**Files:**
- Modify: `crates/lato-runtime/Cargo.toml`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-agent/Cargo.toml`
- Create: `crates/lato-agent/src/extension_runtime.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Create: `crates/lato-agent/tests/plugin_runtime.rs`

**Interfaces:**
- Consumes: an initial `Arc<PluginSnapshot>` and reload snapshots.
- Produces: `SessionPluginSnapshots`, `RuntimeSession::stage_plugin_snapshot`, `RuntimeSession::plugin_snapshot`, and `RuntimeSession::active_turn_plugin_snapshot`.

- [ ] **Step 1: Write failing turn-freeze and fan-out tests**

```rust
#[tokio::test]
async fn active_turn_keeps_old_snapshot_and_next_turn_adopts_pending() {
    let old = snapshot(1, "old");
    let new = snapshot(2, "new");
    let session = blocking_runtime_session(old.clone()).await;
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("wait".into()).await }
    });
    session.wait_until_turn_started().await;
    session.stage_plugin_snapshot(new.clone()).await.unwrap();
    assert_eq!(session.active_turn_plugin_snapshot().await.unwrap().generation(), 1);
    session.release_model();
    prompt.await.unwrap().unwrap();
    assert_eq!(session.plugin_snapshot().await.generation(), 2);
}

#[tokio::test]
async fn snapshot_table_updates_every_live_session_without_cross_leakage() {
    let table = SessionPluginSnapshots::default();
    table.register(SessionId::from("a"), snapshot(1, "a")).await;
    table.register(SessionId::from("b"), snapshot(1, "b")).await;
    table.adopt(SessionId::from("a"), snapshot(2, "a2")).await;
    assert_eq!(table.get(&SessionId::from("a")).await.unwrap().generation(), 2);
    assert_eq!(table.get(&SessionId::from("b")).await.unwrap().generation(), 1);
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test -p lato-agent --test plugin_runtime
```

Expected: compilation fails because session plugin state is absent.

- [ ] **Step 3: Add runtime adoption and audit sequencing**

```rust
struct SessionPluginState {
    current: std::sync::Arc<lato_extensions::PluginSnapshot>,
    active_turn: Option<std::sync::Arc<lato_extensions::PluginSnapshot>>,
    pending: Option<std::sync::Arc<lato_extensions::PluginSnapshot>>,
}

impl RuntimeSession {
    pub async fn stage_plugin_snapshot(
        &self,
        snapshot: std::sync::Arc<lato_extensions::PluginSnapshot>,
    ) -> Result<(), lato_core::AgentError>;
    pub async fn plugin_snapshot(&self)
        -> std::sync::Arc<lato_extensions::PluginSnapshot>;
    pub async fn active_turn_plugin_snapshot(&self)
        -> Option<std::sync::Arc<lato_extensions::PluginSnapshot>>;
}
```

At prompt admission, set `active_turn = Some(current.clone())`. During an active
turn, `stage_plugin_snapshot` keeps only the newest generation in `pending`.
After terminal turn handling, promote `pending`, clear `active_turn`, and submit
`Command::AdoptPluginSnapshot` so the session actor journals and emits the
summary in order. Idle adoption updates immediately and submits the same
command. Reject generation rollback.

- [ ] **Step 4: Implement the session command handler**

In `lato-runtime/src/session.rs`, accept `AdoptPluginSnapshot` only when the
session is idle and commit before emitting:

```rust
Command::AdoptPluginSnapshot { summary } => {
    if !matches!(self.machine.phase(), SessionPhase::Idle) {
        return Err(invalid_state(
            "plugin.snapshot_session_busy",
            "plugin snapshot adoption requires an idle session",
        ));
    }
    self.commit(
        None,
        JournalRecord::PluginSnapshotAdopted { summary: summary.clone() },
        JournalDurability::Flush,
    ).await?;
    self.emit(None, EventPayload::PluginSnapshotAdopted { summary }).await
}
```

- [ ] **Step 5: Run focused runtime and session tests**

```bash
cargo fmt --all
cargo test -p lato-runtime
cargo test -p lato-agent --test plugin_runtime --test runtime_session --test journal_runtime
```

Expected: all tests pass; existing prompt, cancellation, compaction, and journal
ordering remain unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/lato-runtime crates/lato-agent
git commit -m "feat(agent): bind immutable plugin snapshots to sessions"
```

---

### Task 8: Wire shared reload through `AcpHost` and remove the old stub

**Files:**
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/subagent/runner.rs`
- Modify: `crates/lato-agent/tests/acp_runtime.rs`
- Modify: `crates/lato-agent/tests/subagent_runner.rs`
- Modify: `crates/lato-mcp/src/lib.rs`
- Modify: `crates/lato-mcp/Cargo.toml`

**Interfaces:**
- Consumes: `SharedPluginRegistryHandle`, host plugin config, session CLI roots, and session snapshot table.
- Produces: typed `lato/plugins/reload`, live-session fan-out, and child snapshot derivation keyed by `TaskOwner::session_id()`.

- [ ] **Step 1: Replace the placeholder reload test with failing generation tests**

```rust
#[tokio::test]
async fn plugins_reload_publishes_and_fans_out_a_new_generation() {
    let fixture = PluginHostFixture::new(false);
    fixture.write_project_plugin("demo");
    let mut host = fixture.host().await;
    let sid = host.create_session().await;
    let before = host.session_snapshot(&sid).await.generation();
    let response = host.handle(req(
        1,
        "lato/plugins/reload",
        serde_json::json!({"force":true}),
    )).await.unwrap();
    assert!(response["result"]["generation"].as_u64().unwrap() > before);
    assert_eq!(response["result"]["active"], 0);
    assert_eq!(host.session_snapshot(&sid).await.generation(), response["result"]["generation"]);
}

#[tokio::test]
async fn running_child_keeps_parent_generation_after_reload() {
    let fixture = running_child_fixture().await;
    let child_generation = fixture.child_snapshot().generation();
    fixture.reload_parent().await;
    assert_eq!(fixture.child_snapshot().generation(), child_generation);
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test -p lato-agent --test acp_runtime --test subagent_runner
```

Expected: the new assertions fail because the host still stores
`Vec<lato_mcp::PluginPackage>` and reload does not fan out.

- [ ] **Step 3: Replace host plugin storage and handler**

Replace `plugins: Vec<PluginPackage>` with:

```rust
plugin_registry: lato_extensions::SharedPluginRegistryHandle,
plugin_config: lato_extensions::PluginConfig,
cli_plugin_dirs: Vec<std::path::PathBuf>,
session_plugins: lato_agent::SessionPluginSnapshots,
```

Parse `PluginReloadRequest`, call `reload` with `force: true` for the explicit
method, rebuild each session's view with its CLI roots, and await
`stage_plugin_snapshot` for every live session. Return a bounded
`PluginReloadResponse`. A session fan-out error is returned with the generation
and failed session IDs; already-adopted sessions are not rolled backward.

- [ ] **Step 4: Give child sessions a narrowed parent snapshot**

Extend `ChildSessionRunner` with the shared snapshot table. In `run`, resolve
the immutable parent snapshot by `request.node.owner.session_id()`, derive a
child snapshot using `request.node.permissions`, profile capabilities, and
workspace allowance, then pass it through `ChildSessionConfig`:

```rust
let parent = self.session_plugins
    .get(request.node.owner.session_id())
    .await
    .unwrap_or_else(|| self.empty_snapshot.clone());
let child_plugins = parent.derive_child(&CapabilityCeiling::for_task(
    &request.node.permissions,
    &request.node.profile.capabilities,
    request.node.workspace_intent,
));
let session = RuntimeSession::new_child(ChildSessionConfig {
    plugin_snapshot: child_plugins,
    ..child_config
}).await?;
```

Register the child's own session ID in the table for its lifetime and remove it
after bounded shutdown. Never read the shared latest snapshot after child
construction.

- [ ] **Step 5: Remove superseded plugin code from `lato-mcp`**

Delete `PluginOrigin`, `PluginPackage`, `discover_plugin`, `collect_skills`, and
their tests from `crates/lato-mcp/src/lib.rs`. Leave only MCP transport exports
and transport tests; Phase 6C will replace transport internals separately.

- [ ] **Step 6: Run host, child, MCP, and workspace tests**

```bash
cargo fmt --all
cargo test -p lato-agent --test acp_runtime --test subagent_runner --test plugin_runtime
cargo test -p lato-mcp
cargo test -p lato-workspace
```

Expected: all tests pass; project plugins remain inactive when the folder is
untrusted; child generations never increase after construction.

- [ ] **Step 7: Commit**

```bash
git add crates/lato-agent crates/lato-mcp
git commit -m "feat(agent): fan out trusted plugin snapshots"
```

---

### Task 9: Expose repeatable CLI plugin roots without cross-session leakage

**Files:**
- Modify: `src/args.rs`
- Modify: `src/cli.rs`
- Modify: `src/stdio.rs`
- Create: `tests/plugin_cli.rs`

**Interfaces:**
- Consumes: repeatable global `--plugin-dir <PATH>` values.
- Produces: canonical CLI override roots passed into `AcpHost` and session snapshots.

- [ ] **Step 1: Write failing parsing and real-process tests**

```rust
#[test]
fn repeated_plugin_dirs_reach_the_session_snapshot() {
    let fixture = PluginCliFixture::new();
    let first = fixture.plugin("first");
    let second = fixture.plugin("second");
    let response = fixture.acp_round_trip(
        &["--plugin-dir", path(&first), "--plugin-dir", path(&second), "acp"],
        json_rpc(1, "session/new", serde_json::json!({})),
    );
    assert!(response.status.success());
    let listed = fixture.reload_response();
    assert_eq!(listed["active"], 2);
}

#[test]
fn missing_cli_plugin_root_fails_before_session_start() {
    let output = lato(&["--plugin-dir", "/definitely/missing", "-p", "hi"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("plugin directory not found"));
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cargo test --test plugin_cli
```

Expected: argument parsing or assertions fail because `--plugin-dir` is absent.

- [ ] **Step 3: Add and propagate the argument**

Add to `Cli`:

```rust
/// Load a trusted plugin root for this process; may be repeated.
#[arg(long = "plugin-dir", global = true, value_name = "PATH")]
plugin_dirs: Vec<std::path::PathBuf>,
```

Add `plugin_dirs` to `PromptArgs`, `InteractiveNew`, `Resume`, and `Acp` startup
data. Canonicalize and validate each CLI root before constructing `AcpHost`.
Do not persist CLI roots to `$LATO_HOME/config.json`, and do not include one
session's roots in another host or session.

- [ ] **Step 4: Run CLI and presentation regressions**

```bash
cargo fmt --all
cargo test --test plugin_cli --test cli_headless --test tui_cli --test sessions_cli
cargo test -p lato-agent --test acp_runtime
```

Expected: all tests pass and existing invalid option combinations retain exit 2.

- [ ] **Step 5: Commit**

```bash
git add src/args.rs src/cli.rs src/stdio.rs tests/plugin_cli.rs
git commit -m "feat(cli): accept trusted plugin directories"
```

---

### Task 10: Complete documentation, attribution, verification, and deployment

**Files:**
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: the implemented source headers, measured tests, and installed binary.
- Produces: auditable upstream provenance, Phase 6A usage documentation, final verification evidence, and a locally deployed `lato`.

- [ ] **Step 1: Add source-ledger entries**

Record one row for each structurally ported file. Use this exact shape:

```markdown
| `crates/lato-extensions/src/manifest.rs` | Grok Build `xai-grok-agent/src/plugins/manifest.rs` @ `bb7f39d5858cbf5e00de639367f59debbdcb0138` | Structural derivation | Manifest fields, forward compatibility, component resolution, containment | Drops Grok/Claude fallback manifests, LSP, and execution |
| `crates/lato-extensions/src/discovery.rs` | Grok Build `xai-grok-agent/src/plugins/discovery.rs` @ `bb7f39d5858cbf5e00de639367f59debbdcb0138` | Structural derivation | Scope precedence, canonical dedupe, stable conflicts, trust inputs | Uses `.lato` and `$LATO_HOME`; excludes marketplaces and config paths |
| `crates/lato-extensions/src/registry.rs` and `reload.rs` | Grok Build `xai-grok-agent/src/plugins/registry.rs` and Grok Shell `hooks_plugins.rs` @ `bb7f39d5858cbf5e00de639367f59debbdcb0138` | Structural derivation | Immutable active registry, shared handle, force reload, session-specific roots | Uses Lato session staging and capability narrowing; consumers remain inert |
```

- [ ] **Step 2: Document Phase 6A behavior**

Add to README:

```markdown
## Plugins

Lato discovers plugin roots from repeatable `--plugin-dir` arguments,
`.lato/plugins/*` in the project, and `$LATO_HOME/plugins/*`, in that priority
order. CLI and user plugins are trusted; project plugins activate only for a
trusted folder. A plugin must also be enabled before it is active.

The canonical manifest is `plugin.json`. Component paths are confined to the
plugin root. `lato/plugins/reload` atomically rebuilds the registry and updates
live sessions at the next safe turn boundary.

Phase 6A catalogs skill, hook, and MCP components but does not execute them.
```

Append a Phase 6A table to the acceptance results with the exact final commands
and measured outcomes. Do not claim live-provider or unavailable platform checks.

- [ ] **Step 3: Run the full final verification gate**

```bash
cargo fmt --all -- --check
cargo test -p lato-extensions
cargo test -p lato-core
cargo test -p lato-protocol
cargo test -p lato-runtime
cargo test -p lato-agent --test plugin_runtime --test acp_runtime --test subagent_runner --test runtime_session --test journal_runtime
cargo test --test plugin_cli --test phase5_command_smoke --test cli_headless --test tui_cli --test sessions_cli
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
git diff --check
```

Expected: all mandatory tests pass, Clippy emits no warnings, and diff check is clean.

- [ ] **Step 4: Deploy locally and verify the installed binary**

```bash
cargo install --path .
test "$(lato --version)" = "lato 0.1.0-beta.2"
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase5_command_smoke -- --nocapture
```

Expected: install exits 0, the version assertion exits 0, and the real installed
command smoke passes without leaving a worktree or child process.

- [ ] **Step 5: Commit final documentation and measured evidence**

```bash
git add README.md docs/superpowers/reference/lato-upstream-sources.md docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
git commit -m "docs: record phase 6a plugin runtime gate"
```

- [ ] **Step 6: Confirm only user-owned unrelated changes remain**

```bash
git status --short
```

Expected: no Phase 6A files remain uncommitted. The pre-existing user-owned
changes under `docs/testing`, `.lato/`, and `docs/.DS_Store` remain untouched.
