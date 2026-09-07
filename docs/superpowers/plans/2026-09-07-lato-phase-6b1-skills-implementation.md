# Lato Phase 6B1 Skills Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver snapshot-bound plugin `SKILL.md` discovery, model description injection, explicit invocation, and policy/profile-safe tool narrowing.

**Architecture:** `lato-extensions` materializes an immutable `SkillCatalog` from active Phase 6A plugins. A policy-backed built-in `skill` tool resolves through a session-owned catalog handle, and `SessionActor` injects the catalog listing without mutating persisted history. Skill output metadata installs a one-model-step tool allowlist that is enforced both in model definitions and tool preparation.

**Tech Stack:** Rust 2024, Tokio, serde/serde_json/serde_yaml, regex, sha2, existing `lato-core` journal, `lato-tools` policy runtime, and Phase 6A `PluginSnapshot`.

## Global Constraints

- Behavioral baseline: Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`.
- Read only trusted, enabled component descriptors from the frozen Phase 6A snapshot.
- Keep one immutable skill catalog for an entire turn; reload adoption occurs only at a turn boundary.
- Skill `allowed-tools` can only narrow capabilities and is intersected again with the current session/profile tool catalog.
- `SKILL.md` is capped at 256 KiB, frontmatter at 4 KiB, description/`when-to-use` at 1,024 characters, body peek at 2 KiB, walk depth at five, model listing at 128 complete entries/64 KiB, and expanded body at 128 KiB.
- Plugin skill identities are `<plugin-name>:<skill-name>`; ambiguous bare names fail deterministically.
- Full skill bodies are exposed only by explicit invocation; model listing requires authored `description` or `when-to-use`.
- Preserve user-owned dirty files and commit only files named by each task.
- Add Grok source/license headers to substantially derived production files and update the upstream-source ledger.
- Run relevant tests before `cargo install --path .` as required by `AGENTS.md`.

---

## File Structure

- `crates/lato-extensions/src/skills/mod.rs`: public skill-domain exports and shared limits.
- `crates/lato-extensions/src/skills/types.rs`: immutable descriptors, diagnostics, invocation origin/result, and catalog identity types.
- `crates/lato-extensions/src/skills/discovery.rs`: bounded recursive discovery and Grok-compatible frontmatter parsing.
- `crates/lato-extensions/src/skills/catalog.rs`: collision handling, listing rendering, substitution, body materialization, and invocation lookup.
- `crates/lato-extensions/tests/skills_discovery.rs`: filesystem, parsing, containment, collision, and bound tests.
- `crates/lato-extensions/tests/skills_catalog.rs`: listing, lookup, substitution, and tool-narrowing tests.
- `crates/lato-tools/src/skill.rs`: generic policy-backed built-in skill tool and resolver contract without a dependency on `lato-extensions`.
- `crates/lato-tools/src/runtime.rs`: scoped model definitions and scoped preparation.
- `crates/lato-agent/src/skills.rs`: adapter from `SkillCatalog` to `lato-tools::SkillResolver` and active catalog handle.
- `crates/lato-agent/src/actor.rs`: turn-local listing injection and one-step allowlist enforcement.
- `crates/lato-agent/src/legacy_driver.rs`: installs the immutable turn catalog before actor execution.
- `crates/lato-agent/src/runtime_session.rs`: atomically materializes and binds catalogs with plugin generations.
- `crates/lato-core/src/journal.rs`: canonical bounded skill audit records.
- `crates/lato-agent/tests/skills_runtime.rs`: end-to-end snapshot, policy, reload, and child narrowing tests.

### Task 1: Bounded Skill Discovery and Parsing

**Files:**
- Modify: `crates/lato-extensions/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/lato-extensions/src/lib.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Modify: `crates/lato-core/src/journal.rs`
- Create: `crates/lato-extensions/src/skills/mod.rs`
- Create: `crates/lato-extensions/src/skills/types.rs`
- Create: `crates/lato-extensions/src/skills/discovery.rs`
- Create: `crates/lato-extensions/tests/skills_discovery.rs`

**Interfaces:**
- Consumes: `PluginSnapshot::active_plugins()`, `LoadedPlugin::skill_dirs`, and `LoadedPlugin::canonical_root`.
- Produces:

```rust
pub fn discover_skills(snapshot: &PluginSnapshot) -> SkillDiscovery;

pub struct SkillDiscovery {
    pub generation: u64,
    pub skills: Vec<DiscoveredSkill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

pub struct DiscoveredSkill {
    pub plugin_name: String,
    pub plugin_root: PathBuf,
    pub skill_dir: PathBuf,
    pub source_path: PathBuf,
    pub name: String,
    pub description: String,
    pub has_authored_description: bool,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
    pub body: String,
}

// Defined in lato-core so extension, tool, and journal layers share one type.
pub enum SkillInvocationOrigin { Model, User }
```

- [ ] **Step 1: Add parser dependencies and module exports**

Add `serde_yaml = "0.9"` to normal dependencies, define and export
`SkillInvocationOrigin` from `lato-core`, and export `skills` from
`lato-extensions/src/lib.rs`. In `skills/mod.rs`, define and export the exact
spec bounds:

```rust
pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 4 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_BODY_PEEK_BYTES: usize = 2 * 1024;
pub const MAX_SKILL_WALK_DEPTH: usize = 5;

pub mod discovery;
pub mod types;

pub use discovery::discover_skills;
pub use types::{DiscoveredSkill, SkillDiagnostic, SkillDiscovery};
```

- [ ] **Step 2: Write failing discovery tests**

Create fixtures directly inside `skills_discovery.rs` and assert these named
cases: `discovers_root_and_nested_skill_in_sorted_order`,
`normalizes_frontmatter_then_falls_back_to_directory_name`,
`parses_allowed_tools_string_and_list`, `uses_body_prose_only_as_fallback`,
`rejects_symlink_escape`, `caps_walk_at_depth_five`,
`isolates_oversized_and_malformed_candidates`, and
`inactive_plugins_contribute_nothing`.

The central assertion must prove authored-description provenance:

```rust
let skill = &result.skills[0];
assert_eq!(skill.name, "code-audit");
assert_eq!(skill.description, "Review code safely.");
assert!(skill.has_authored_description);
assert_eq!(skill.allowed_tools.as_deref(), Some(&["read_file".into(), "Bash(git diff:*)".into()][..]));
```

- [ ] **Step 3: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test skills_discovery`

Expected: compilation fails because `lato_extensions::skills` and
`discover_skills` do not exist.

- [ ] **Step 4: Implement Grok-compatible bounded parsing**

Port the normalization, scalar coercion, boolean parsing, top-level tool-list
splitting, frontmatter retry, body-prose derivation, and sorted depth-five walk
from the pinned Grok files. The traversal must canonicalize every candidate and
reject any source path that does not start with `plugin.canonical_root`.
Implement diagnostic insertion through one bounded helper:

```rust
fn push_diagnostic(out: &mut Vec<SkillDiagnostic>, code: &'static str, path: &Path, message: impl std::fmt::Display) {
    if out.len() == 128 { return; }
    out.push(SkillDiagnostic::bounded(code, path, message.to_string(), 512));
}
```

Store the full bounded body in `DiscoveredSkill` during materialization so a
later filesystem change cannot mutate an issued generation.

- [ ] **Step 5: Run discovery tests and extension regressions**

Run: `cargo test -p lato-extensions --test skills_discovery`

Expected: all named discovery tests pass.

Run: `cargo test -p lato-extensions`

Expected: all extension tests pass with no Phase 6A regression.

- [ ] **Step 6: Commit the parser slice**

```bash
git add Cargo.lock crates/lato-core/src/lib.rs crates/lato-core/src/journal.rs crates/lato-extensions/Cargo.toml crates/lato-extensions/src/lib.rs crates/lato-extensions/src/skills crates/lato-extensions/tests/skills_discovery.rs
git commit -m "feat(extensions): discover plugin skills"
```

### Task 2: Immutable Skill Catalog, Listing, and Invocation

**Files:**
- Modify: `crates/lato-extensions/src/skills/mod.rs`
- Modify: `crates/lato-extensions/src/skills/types.rs`
- Create: `crates/lato-extensions/src/skills/catalog.rs`
- Create: `crates/lato-extensions/tests/skills_catalog.rs`

**Interfaces:**
- Consumes: `SkillDiscovery` from Task 1.
- Produces:

```rust
#[derive(Clone, Debug)]
pub struct SkillCatalog {
    generation: u64,
    by_qualified: BTreeMap<String, Arc<DiscoveredSkill>>,
    by_bare: BTreeMap<String, Arc<[String]>>,
    diagnostics: Arc<[SkillDiagnostic]>,
    model_listing: Arc<str>,
    omitted_listing_count: usize,
}

pub struct SkillInvocation {
    pub qualified_name: String,
    pub message: String,
    pub allowed_tools: Option<Arc<[String]>>,
    pub body_hash: String,
}

impl SkillCatalog {
    pub fn from_discovery(discovery: SkillDiscovery) -> Arc<Self>;
    pub fn generation(&self) -> u64;
    pub fn render_model_listing(&self) -> String;
    pub fn invoke(&self, origin: SkillInvocationOrigin, name: &str, args: Option<&str>, session_id: &str) -> Result<SkillInvocation, SkillInvokeError>;
}
```

- [ ] **Step 1: Write failing catalog tests**

Cover `qualified_names_do_not_collide`, `unique_bare_name_resolves`,
`ambiguous_bare_name_lists_sorted_candidates`,
`listing_requires_authored_description_or_when_to_use`,
`listing_excludes_model_disabled_skills`,
`user_and_model_invocation_flags_are_independent`,
`listing_stops_on_complete_entry_boundary`,
`last_catalog_is_immutable_after_source_edit`, and all Grok substitutions:

```rust
let invoked = catalog.invoke(
    SkillInvocationOrigin::User,
    "demo:inspect",
    Some("alpha beta"),
    "session-1",
).unwrap();
assert!(invoked.message.contains("alpha beta / alpha / beta"));
assert!(invoked.message.starts_with("<skill name=\"demo:inspect\""));
assert!(invoked.message.ends_with("\n</skill>"));
```

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test skills_catalog`

Expected: compilation fails because `SkillCatalog` is not defined.

- [ ] **Step 3: Implement catalog construction and collision rules**

Build a `BTreeMap<String, Arc<SkillDescriptor>>` by qualified name plus a bare
name index. Sort descriptors before insertion. Preserve the first canonical
path for an intra-plugin duplicate and append a bounded collision diagnostic.
Do not consult the filesystem after construction.

- [ ] **Step 4: Implement listing and invocation formatting**

Render only complete entries within the 128-entry/64-KiB limits. Port Grok's
`$ARGUMENTS`, `$ARGUMENTS[N]`, `$N`, skill/session/plugin path aliases, unknown
token preservation, and argument suffix behavior. Use this canonical wrapper:

```rust
format!(
    "<skill name=\"{}\" description=\"{}\" path=\"{}\">\n{}\n</skill>",
    xml_escape(&qualified_name),
    xml_escape(&descriptor.description),
    xml_escape(&descriptor.source_path.display().to_string()),
    expanded_body,
)
```

Reject expansion beyond 128 KiB before returning it. Hash the expanded body
with SHA-256 for audit; never use the hash as an authorization token.

- [ ] **Step 5: Run catalog and extension tests**

Run: `cargo test -p lato-extensions --test skills_catalog`

Expected: every lookup, listing, bound, and substitution test passes.

Run: `cargo test -p lato-extensions`

Expected: all extension tests pass.

- [ ] **Step 6: Commit the catalog slice**

```bash
git add crates/lato-extensions/src/skills crates/lato-extensions/tests/skills_catalog.rs
git commit -m "feat(extensions): materialize immutable skill catalogs"
```

### Task 3: Policy-Backed Skill Tool and Scoped Tool Runtime

**Files:**
- Modify: `crates/lato-tools/src/lib.rs`
- Modify: `crates/lato-tools/Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/lato-tools/src/skill.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `crates/lato-tools/src/runtime.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/subagent/runner.rs`
- Modify: `crates/lato-agent/tests/subagent_runner.rs`
- Modify: `crates/lato-tools/tests/sandbox_policy.rs`
- Modify: `crates/lato-tools/tests/tool_runtime.rs`
- Create: `crates/lato-tools/tests/skill_tool.rs`

**Interfaces:**
- Consumes: existing `Tool`, `ToolContext`, `ToolRuntime::prepare`, and `ToolCapability::ExtensionInvoke`.
- Produces:

```rust
#[async_trait]
pub trait SkillResolver: Send + Sync {
    async fn invoke(&self, context: &ToolContext, skill: &str, args: Option<&str>) -> Result<ResolvedSkill, ToolError>;
}

pub struct ResolvedSkill {
    pub qualified_name: String,
    pub message: String,
    pub allowed_tool_specs: Option<Vec<String>>,
    pub body_hash: String,
}

pub struct SkillToolScope {
    rules: Vec<AllowedToolRule>,
}

pub struct ValidatedToolCall {
    pub wire_name: String,
    pub canonical_name: ToolName,
    pub arguments: Value,
}

struct AllowedToolRule {
    wire_names: HashSet<String>,
    argument_pattern: Option<Regex>,
}

impl SkillToolScope {
    pub fn compile(specs: &[String], runtime: &ToolRuntime) -> Result<Self, ToolError>;
    pub fn allows_name(&self, wire_name: &str) -> bool;
    pub fn allows_call(&self, wire_name: &str, arguments: &Value) -> bool;
}

impl ToolRuntime {
    pub fn model_definitions_scoped(&self, scope: Option<&SkillToolScope>) -> Vec<Value>;
    pub fn resolve_and_validate(&self, wire_name: &str, arguments: Value) -> Result<ValidatedToolCall, ToolError>;
    pub fn prepare_scoped(&self, context: ToolContext, wire_name: &str, arguments: Value, scope: Option<&SkillToolScope>) -> Result<PreparedToolCall, ToolError>;
}
```

- [ ] **Step 1: Write failing policy and scope tests**

Assert that the skill descriptor has local name `skill`, capability
`ExtensionInvoke`, no side effect, and a schema requiring `skill` with optional
`args`. Add tests proving policy denial prevents resolver invocation, a scoped
runtime omits and rejects a non-allowed tool, the `skill` tool remains available
for the invocation step, and resolver metadata is preserved:

```rust
assert_eq!(output.metadata["kind"], "skill_invocation");
assert_eq!(output.metadata["qualifiedName"], "demo:inspect");
assert_eq!(output.metadata["allowedToolSpecs"], serde_json::json!(["read_file"]));
assert_eq!(output.metadata["bodyHash"], "abc123");
```

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-tools --test skill_tool`

Expected: compilation fails because `SkillResolver` and scoped runtime methods
do not exist.

- [ ] **Step 3: Implement the generic skill tool**

Add `jsonschema = { version = "0.55", default-features = false }` so descriptor
schemas are checked without enabling remote schema resolution. Add
`skill_resolver: Option<Arc<dyn SkillResolver>>` to
`BuiltinToolEnvironment`. Register `SkillTool` only when the resolver exists.
Parse arguments through a typed structure:

```rust
#[derive(serde::Deserialize)]
struct SkillInput {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}
```

Return `ResolvedSkill.message` as content and the four bounded audit/narrowing
fields as metadata. The ordinary `ToolRuntime` authorization path supplies the
ExtensionInvoke policy decision and execution grant.

- [ ] **Step 4: Implement scoped definition and preparation methods**

Compile each registered descriptor schema at runtime build and fail the build
if a schema is invalid. `resolve_and_validate` resolves the immutable canonical
tool identity, canonicalizes the argument object, and validates it against that
compiled schema without evaluating policy. Factor existing `prepare` into
`prepare_scoped`; after wire-name resolution and
before policy work, require both the tool name and canonical arguments to match
the compiled optional scope. Support Grok's bare names, `|` alternatives,
compatibility aliases, `*`, and grouped forms such as `Bash(git diff:*)`.
Return `tool.not_allowed_by_skill` on mismatch. Make `prepare` call
`prepare_scoped` with `scope` set to `None` so existing callers remain
unchanged. Filter `model_definitions_scoped` to names that have at least one
potentially matching rule; keep argument-pattern enforcement in preparation.

- [ ] **Step 5: Run tool tests and workspace compile check**

Run: `cargo test -p lato-tools --test skill_tool`

Expected: all skill tool and scoped runtime tests pass.

Run: `cargo test -p lato-tools`

Run: `cargo check --workspace`

Expected: existing tool tests pass and every updated environment literal
compiles.

- [ ] **Step 6: Commit the policy-backed tool slice**

```bash
git add Cargo.lock crates/lato-tools/Cargo.toml crates/lato-tools/src crates/lato-tools/tests crates/lato-agent/src/actor.rs crates/lato-agent/src/host.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/src/subagent/runner.rs crates/lato-agent/tests/subagent_runner.rs
git commit -m "feat(tools): add policy-backed skill invocation"
```

Before committing, inspect `git diff --cached --name-only` and unstage any path
not required solely for `BuiltinToolEnvironment` compilation.

### Task 4: Turn-Bound Catalog Binding and Prompt Injection

**Files:**
- Modify: `crates/lato-agent/src/lib.rs`
- Create: `crates/lato-agent/src/skills.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Create: `crates/lato-agent/tests/skills_runtime.rs`

**Interfaces:**
- Consumes: `SkillCatalog`, `SkillResolver`, `model_definitions_scoped`, and `prepare_scoped`.
- Produces:

```rust
#[derive(Clone)]
pub struct SessionSkillHandle {
    catalog: Arc<RwLock<Arc<SkillCatalog>>>,
}

impl SessionSkillHandle {
    pub async fn install(&self, catalog: Arc<SkillCatalog>);
    pub async fn snapshot(&self) -> Arc<SkillCatalog>;
}

impl LegacyTurnDriver {
    pub async fn bind_turn_skills(&self, catalog: Arc<SkillCatalog>);
}
```

- [ ] **Step 1: Write failing actor and runtime tests**

Use a recording model stream to assert:

1. the provider context's first system message includes one
   `<available_skills>` block while persisted history remains unchanged;
2. model definitions include `skill` only when the resolver is installed;
3. invoking `skill` yields the XML body and compiles `allowedToolSpecs` for the
   immediately following model sample;
4. both definitions and execution reject a tool outside that allowlist;
5. the allowlist clears after that model step;
6. generation N remains active when N+1 is staged mid-turn and N+1 appears on
   the next turn;
7. an inactive/untrusted plugin injects no text;
8. a derived child catalog cannot recover a parent-only tool.

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-agent --test skills_runtime`

Expected: compilation fails because the session skill adapter and binding API
do not exist.

- [ ] **Step 3: Implement the resolver adapter and immutable binding**

`SessionSkillHandle` resolves through the currently installed immutable
catalog and maps catalog errors to stable `skill.*` `ToolError` codes. In
`RuntimeSession::begin_plugin_turn`, materialize or fetch the catalog paired
with `state.current`, store it in the active-turn slot, and call
`driver.bind_turn_skills` before submitting `StartTurn`. Clear only the
active-turn reference on finish; then atomically materialize/adopt pending N+1.

- [ ] **Step 4: Inject listing without changing journaled history**

Replace direct `history_to_messages(&self.history)` provider assembly with a
helper that clones the messages and appends the listing to the first system
text:

```rust
fn messages_with_skill_listing(history: &[HistoryItem], listing: &str) -> Vec<serde_json::Value> {
    let mut messages = history_to_messages(history);
    if !listing.is_empty() {
        append_to_first_system_message(&mut messages, format!("\n\n{listing}"));
    }
    messages
}
```

Do not append the listing to `self.history` or a journal record.

- [ ] **Step 5: Apply one-model-step tool narrowing**

When a successful tool result has metadata `kind == "skill_invocation"`,
compile its `allowedToolSpecs` into `SkillToolScope`. Use that scope for the
next provider call's definitions and every tool call emitted by that provider
call. Clear it only after that stream round has fully completed, including all
native and text-embedded tool calls. The `skill` tool itself is available in
the invocation round before the scope is activated, and no rule can add a tool
missing from the runtime catalog or bypass argument/policy checks.

- [ ] **Step 6: Run runtime and regression tests**

Run: `cargo test -p lato-agent --test skills_runtime`

Expected: all eight integration behaviors pass.

Run: `cargo test -p lato-agent --test plugin_runtime --test runtime_session --test subagent_runner`

Expected: Phase 6A reload and Phase 5 child-session tests remain green.

- [ ] **Step 7: Commit the agent binding slice**

```bash
git add crates/lato-agent/src/lib.rs crates/lato-agent/src/skills.rs crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/tests/skills_runtime.rs
git commit -m "feat(agent): bind skills to immutable turns"
```

### Task 5: Skill Audit, Documentation, and 6B1 Gate

**Files:**
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/src/command.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-agent/src/skills.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/tests/skills_runtime.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `README.md`
- Create: `tests/phase6b_skills_smoke.rs`
- Create: `docs/testing/reports/phase-6b1-skills-release-gate-2026-09-07.md`

**Interfaces:**
- Consumes: `TurnEventEmitter::commit` and skill invocation/catalog hashes.
- Produces a restricted audit command and non-projecting journal record:

```rust
pub enum ExtensionAuditRecord {
    SkillCatalogMaterialized { generation: u64, visible_count: u64, omitted_count: u64, catalog_hash: String },
    SkillInvoked { qualified_name: String, origin: SkillInvocationOrigin, body_hash: String, allowed_tools_hash: Option<String> },
    SkillRejected { requested_name_hash: String, origin: SkillInvocationOrigin, error_code: String },
}

Command::RecordExtensionAudit { audit: ExtensionAuditRecord }
JournalRecord::ExtensionAudit { audit: ExtensionAuditRecord }
```

- [ ] **Step 1: Write failing journal tests**

Extend `skills_runtime.rs` and core journal tests to prove records replay without
changing projected conversation state, contain hashes instead of body/prompt
text, and order catalog materialization before invocation and invocation before
the next model request.

- [ ] **Step 2: Run the focused tests and confirm red state**

Run: `cargo test -p lato-core journal`

Run: `cargo test -p lato-agent --test skills_runtime audit`

Expected: compilation fails because the skill journal variants are absent.

- [ ] **Step 3: Implement bounded audit records**

Add serde-stable snake-case variants. `RecordExtensionAudit` accepts only the
typed audit enum, so callers cannot inject conversation or terminal records.
The runtime accepts it before or during a live turn and appends it with Flush
durability in mailbox order. Treat the resulting journal record like
`PluginSnapshotAdopted` during replay: validate sequence but do not project a
message or terminal state. Hash canonical qualified-name request data with the
existing `journal_request_hash`; never persist full skill body, arguments, or
expanded message.

- [ ] **Step 4: Update attribution and user documentation**

Add the exact Grok source paths listed in the design spec to the source ledger.
Document in README: plugin skill location, authored description requirement,
qualified invocation, user/model flags, argument tokens, `allowed-tools`
narrowing, snapshot/reload behavior, and diagnostic commands that already
exist. Do not document Phase 6C MCP as available.

- [ ] **Step 5: Run the full quality gate**

Run in order:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path .
lato --version
```

Expected: formatting succeeds, all workspace tests pass, clippy emits no
warning, installation replaces the local binary, and `lato --version` reports
the workspace package version.

- [ ] **Step 6: Run installed-command skill smoke**

Create `tests/phase6b_skills_smoke.rs` using the bounded localhost model-server
pattern in `tests/phase5_command_smoke.rs`. Its single test must be named
`phase6b_installed_command_smoke_exercises_skill_invocation`; it creates a
temporary trusted workspace and CLI plugin, verifies the first request contains
the qualified authored description, returns a `skill` call with two arguments,
verifies the second request contains the expanded XML body and only scoped
tools, then returns `phase6b-skills-smoke-ok`.

Run:

```bash
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6b_skills_smoke -- --nocapture
```

Expected: one test passes and no temporary plugin/session/process survives the
fixture. Record the command, test totals, and output in the Phase 6B1 gate
report.

- [ ] **Step 7: Commit the Phase 6B1 gate**

```bash
git add crates/lato-core/src/command.rs crates/lato-core/src/journal.rs crates/lato-runtime/src/session.rs crates/lato-agent/src/skills.rs crates/lato-agent/src/actor.rs crates/lato-agent/tests/skills_runtime.rs tests/phase6b_skills_smoke.rs docs/superpowers/reference/lato-upstream-sources.md README.md docs/testing/reports/phase-6b1-skills-release-gate-2026-09-07.md
git commit -m "docs: record phase 6b1 skills gate"
```

## Phase 6B1 Completion Checkpoint

Stop after Task 5 and report the commit list, exact test count, clippy result,
installed binary version, smoke result, and unchanged pre-existing dirty paths.
Begin Phase 6B2 only after this checkpoint is reviewed.
