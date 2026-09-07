# Lato Phase 6B2 Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Grok-compatible command and HTTPS lifecycle hooks with deterministic dispatch, bounded resources, fail-open isolation, safe argument rewriting, audit, reload, and child-session narrowing.

**Architecture:** `lato-extensions` parses immutable hook registries and owns bounded command/HTTPS runners plus pure dispatch combination. `lato-agent` owns session event timing and maps hook outcomes into prompt, tool, stop, and compaction control flow. Every PreToolUse rewrite re-enters `ToolRuntime::prepare_scoped`, ensuring fresh schema, policy, approval, and sandbox decisions.

**Tech Stack:** Rust 2024, Tokio process/time/sync, reqwest with rustls, url, regex, serde/serde_json, sha2, existing cancellation/process/sandbox primitives, and Phase 6B1 skill/catalog integration.

## Global Constraints

- Start from a completed and green Phase 6B1 checkpoint.
- Behavioral baseline: Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`.
- Dispatch only hooks from trusted, enabled plugins in the turn's frozen Phase 6A snapshot.
- Support exactly `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `PreCompact`, and `PostCompact`, plus their Grok-compatible aliases.
- Command/HTTPS hook failures and timeouts fail open; a healthy explicit gate decision is enforced.
- PreToolUse may replace the complete argument object but cannot change the resolved tool name or authority-bearing session fields.
- Rewritten arguments receive fresh schema, policy, approval, and sandbox evaluation; no old grant or fingerprint is reused.
- Default per-handler timeouts are 5s for observers/PreToolUse, 30s for UserPromptSubmit, 600s for PostToolUse/Stop, and 1500ms for SessionEnd; explicit SessionEnd timeout is capped at 60s.
- Serialized event payload is capped at 128 KiB, runner capture at one MiB, reason at 256 characters, hook feedback at 10,000 characters, and output replacement at 64 KiB.
- HTTPS only; no redirects; loopback allowed; unspecified, link-local, carrier-grade NAT, and private/internal ranges blocked after resolution.
- Stop continuation is capped at eight per turn.
- SessionStart/SessionEnd/PreCompact/PostCompact are observers; they cannot gate or mutate model state.
- Preserve user-owned dirty files and commit only files named by each task.
- Add Grok source/license headers to substantially derived production files and update the upstream-source ledger.
- Run relevant tests before `cargo install --path .` as required by `AGENTS.md`.

---

## File Structure

- `crates/lato-extensions/src/hooks/mod.rs`: hook exports and public bounds.
- `crates/lato-extensions/src/hooks/event.rs`: canonical names, aliases, traits, and bounded event envelopes.
- `crates/lato-extensions/src/hooks/config.rs`: `hooks.json`/inline parsing, matcher compilation, handler normalization, ordering, and timeout defaults.
- `crates/lato-extensions/src/hooks/result.rs`: compatible wire parsing and typed outcomes.
- `crates/lato-extensions/src/hooks/command.rs`: command execution, JSON stdin, identity environment, capture, cancellation, and process-tree cleanup.
- `crates/lato-extensions/src/hooks/http.rs`: HTTPS POST, URL expansion, SSRF checks, no-redirect client, and bounded response parsing.
- `crates/lato-extensions/src/hooks/dispatcher.rs`: sequential dispatch and Grok decision combination.
- `crates/lato-extensions/tests/hooks_config.rs`: aliases, matcher, config isolation, ordering, and timeout tests.
- `crates/lato-extensions/tests/hooks_command.rs`: command result, failure, timeout, environment, overflow, and reap tests.
- `crates/lato-extensions/tests/hooks_http.rs`: HTTPS, DNS/IP, redirect, response, and timeout tests.
- `crates/lato-extensions/tests/hooks_dispatcher.rs`: gate, rewrite, context, replacement, observer, and stop aggregation tests.
- `crates/lato-agent/src/hooks.rs`: session hook runtime, lifecycle payload adapters, cancellation scope, and journal bridge.
- `crates/lato-agent/src/actor.rs`: prompt/tool/stop/automatic-compaction hook seams.
- `crates/lato-agent/src/legacy_driver.rs`: manual compaction and session lifecycle hook seams.
- `crates/lato-agent/src/runtime_session.rs`: generation reconciliation, SessionStart/End, and bounded shutdown.
- `crates/lato-core/src/journal.rs`: canonical hook audit records.
- `crates/lato-agent/tests/hooks_runtime.rs`: full lifecycle, policy rewrite, reload, child, and shutdown integration.

### Task 1: Hook Event, Config, Matcher, and Result Contracts

**Files:**
- Modify: `crates/lato-extensions/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/lato-extensions/src/lib.rs`
- Create: `crates/lato-extensions/src/hooks/mod.rs`
- Create: `crates/lato-extensions/src/hooks/event.rs`
- Create: `crates/lato-extensions/src/hooks/config.rs`
- Create: `crates/lato-extensions/src/hooks/result.rs`
- Create: `crates/lato-extensions/tests/hooks_config.rs`

**Interfaces:**
- Consumes: `PluginSnapshot::active_plugins()`, `LoadedPlugin::hooks_path`, `LoadedPlugin::inline_hooks`, and canonical plugin roots.
- Produces:

```rust
pub enum HookEventName {
    SessionStart, SessionEnd, UserPromptSubmit, PreToolUse,
    PostToolUse, Stop, PreCompact, PostCompact,
}

pub enum HandlerType { Command, Http }

pub enum HookMode { Observe, Prompt, Tool, PostTool, Stop }

pub struct HookEventEnvelope {
    pub schema_version: u16,
    pub event: HookEventName,
    pub generation: u64,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub payload: Value,
}

pub struct HookMatcher {
    configured: String,
    compiled: MatcherKind,
}

pub struct HookDiagnostic {
    pub code: String,
    pub plugin_name: String,
    pub path: Option<PathBuf>,
    pub message: String,
}

pub struct HookSpec {
    pub id: String,
    pub plugin_name: String,
    pub event: HookEventName,
    pub handler_type: HandlerType,
    pub matcher: Option<HookMatcher>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
    pub extra_env: BTreeMap<String, String>,
}

pub struct HookRegistry {
    generation: u64,
    by_event: BTreeMap<HookEventName, Arc<[HookSpec]>>,
    diagnostics: Arc<[HookDiagnostic]>,
}

pub fn materialize_hooks(snapshot: &PluginSnapshot) -> Arc<HookRegistry>;
```

- [ ] **Step 1: Add dependencies, limits, and module exports**

Add `regex = "1"`, `reqwest = { version = "0.12", features = ["json", "rustls-tls"] }`,
`url = "2"`, and Tokio `process`, `io-util`, and `time` features. Export these
limits from `hooks/mod.rs`:

```rust
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
pub const MAX_RUNNER_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_REASON_CHARS: usize = 256;
pub const MAX_FEEDBACK_CHARS: usize = 10_000;
pub const MAX_REPLACEMENT_CHARS: usize = 64 * 1024;
pub const MAX_STOP_CONTINUATIONS: usize = 8;
```

- [ ] **Step 2: Write failing contract tests**

Cover all canonical event aliases, observer/gate traits, simple exact match,
alias-expanded exact match, unanchored and anchored regex, invalid regex
isolation, missing/empty/star match-all, stable plugin/group/handler ordering,
file and inline configs, reserved environment stripping, missing command/url,
unknown handler/event isolation, and every default/explicit timeout.

Add result fixtures proving nested permission decision precedence, legacy
top-level fallback, exit-two blocking, malformed optional-field isolation,
`continue:false`, PostToolUse block/context/replacement, and ignored observer
mutations.

- [ ] **Step 3: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test hooks_config`

Expected: compilation fails because the hook module and contracts do not exist.

- [ ] **Step 4: Port event, matcher, config, and result parsing**

Port the pinned Grok behavior with Lato event names. Preserve matcher semantics:

```rust
let kind = if pattern.is_empty() || pattern == "*" {
    MatcherKind::All
} else if pattern.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'|') {
    MatcherKind::Exact(expand_compatibility_aliases(pattern))
} else {
    MatcherKind::Regex(regex::Regex::new(pattern)?)
};
```

Materialize file and inline hook data from active plugins only. Sort plugins
and configuration paths, preserve vector order inside each event, assign stable
IDs from plugin/event/group/handler indices, and retain bounded per-component
diagnostics without dropping unrelated plugins or events.

- [ ] **Step 5: Run contract and extension tests**

Run: `cargo test -p lato-extensions --test hooks_config`

Expected: all event/config/matcher/result tests pass.

Run: `cargo test -p lato-extensions`

Expected: Phase 6A/6B1 tests remain green.

- [ ] **Step 6: Commit the hook contract slice**

```bash
git add Cargo.lock crates/lato-extensions/Cargo.toml crates/lato-extensions/src/lib.rs crates/lato-extensions/src/hooks crates/lato-extensions/tests/hooks_config.rs
git commit -m "feat(extensions): parse plugin hook registries"
```

### Task 2: Bounded Command Hook Runner

**Files:**
- Create: `crates/lato-extensions/src/hooks/command.rs`
- Modify: `crates/lato-extensions/src/hooks/mod.rs`
- Create: `crates/lato-extensions/tests/hooks_command.rs`

**Interfaces:**
- Consumes: `HookSpec`, `HookEventEnvelope`, and `HookMode` from Task 1.
- Produces:

```rust
pub struct HookRunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a Path,
    pub cancellation: CancellationToken,
}

pub struct RawHookRun {
    pub stdout: String,
    pub stderr_preview: String,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    pub truncated: bool,
}

pub async fn run_command_hook(spec: &HookSpec, envelope: &HookEventEnvelope, context: &HookRunContext<'_>) -> Result<RawHookRun, HookRunError>;
```

- [ ] **Step 1: Write failing command-runner tests**

Test a simple relative executable resolved from `source_dir`, a shell command
run with workspace cwd, JSON stdin, reserved identity environment overriding
configured spoof values, extra environment preservation, exit zero/two/other,
one-MiB capture, timeout, cancellation, and a child process that must be gone
after timeout/session cancellation.

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test hooks_command`

Expected: compilation fails because `run_command_hook` is absent.

- [ ] **Step 3: Implement spawn and bounded collection**

Use a direct executable for simple paths and the platform shell for strings
containing shell metacharacters. Configure piped stdin/stdout/stderr,
workspace cwd, authentic `LATO_HOOK_EVENT`, `LATO_HOOK_NAME`,
`LATO_SESSION_ID`, `LATO_WORKSPACE_ROOT`, and compatibility `CLAUDE_PROJECT_DIR`.
On Unix create a fresh process group before spawn. Serialize one bounded event
object to stdin and concurrently drain both output streams up to the one-MiB
cap.

- [ ] **Step 4: Implement timeout, cancellation, and tree reap**

Wrap the process future in `tokio::select!` over handler timeout and cancellation.
On either branch, terminate the process group, wait up to 500ms, force-kill the
group if still alive, and await the child to avoid zombies. Return a typed
timeout/cancel/overflow error without returning captured secret-bearing output.

- [ ] **Step 5: Run command runner tests**

Run: `cargo test -p lato-extensions --test hooks_command`

Expected: all command and cleanup tests pass on the current platform; platform
specific tests are guarded with `cfg` rather than skipped at runtime.

- [ ] **Step 6: Commit the command runner**

```bash
git add crates/lato-extensions/src/hooks/command.rs crates/lato-extensions/src/hooks/mod.rs crates/lato-extensions/tests/hooks_command.rs
git commit -m "feat(extensions): run bounded command hooks"
```

### Task 3: HTTPS Hook Runner and SSRF Boundary

**Files:**
- Create: `crates/lato-extensions/src/hooks/http.rs`
- Modify: `crates/lato-extensions/src/hooks/mod.rs`
- Create: `crates/lato-extensions/tests/hooks_http.rs`

**Interfaces:**
- Consumes: the same `HookSpec`, envelope, context, raw result parser, and bounds as the command runner.
- Produces:

```rust
#[async_trait]
pub trait HookDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

pub async fn validate_hook_url(url: &Url, resolver: &dyn HookDnsResolver) -> Result<(), HookRunError>;
pub async fn run_http_hook(spec: &HookSpec, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, client: &reqwest::Client, resolver: &dyn HookDnsResolver) -> Result<RawHookRun, HookRunError>;
```

- [ ] **Step 1: Write failing HTTPS and SSRF tests**

Use an injected fake resolver for deterministic public/private/link-local/CGNAT/
unspecified/IPv4-mapped IPv6/loopback cases. Use a local TLS fixture for valid
POST body, response cap, non-success status, malformed JSON, timeout, and no
redirect following. Assert HTTP scheme rejection and redact the expanded URL
from errors.

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test hooks_http`

Expected: compilation fails because URL validation and HTTP runner APIs are
absent.

- [ ] **Step 3: Implement URL expansion and SSRF validation**

Accept only `https`, require a host, allow loopback, and reject the exact
address classes in the design. Resolve hostnames inside the handler timeout and
reject when any returned address is blocked. Do not include the expanded URL
in user-visible errors or journal records.

- [ ] **Step 4: Implement the no-redirect bounded client path**

Build the client with `reqwest::redirect::Policy::none()` and the effective
handler timeout. POST `Content-Type: application/json`, stream the response to
the one-MiB cap, retain only the bounded status/preview metadata, and feed the
body through the same event-specific parser as command output. Do not attach
ambient authorization or plugin-defined headers.

- [ ] **Step 5: Run HTTPS and extension tests**

Run: `cargo test -p lato-extensions --test hooks_http`

Expected: all HTTPS, SSRF, redirect, timeout, and redaction cases pass.

Run: `cargo test -p lato-extensions`

Expected: every extension test remains green.

- [ ] **Step 6: Commit the HTTPS runner**

```bash
git add crates/lato-extensions/src/hooks/http.rs crates/lato-extensions/src/hooks/mod.rs crates/lato-extensions/tests/hooks_http.rs
git commit -m "feat(extensions): run bounded https hooks"
```

### Task 4: Sequential Hook Dispatcher

**Files:**
- Create: `crates/lato-extensions/src/hooks/dispatcher.rs`
- Modify: `crates/lato-extensions/src/hooks/mod.rs`
- Create: `crates/lato-extensions/tests/hooks_dispatcher.rs`

**Interfaces:**
- Consumes: `HookRegistry`, both runners, compatible result parsing, and cancellation context.
- Produces:

```rust
pub struct PreToolUseResult {
    pub decision: HookDecision,
    pub updated_input: Option<Value>,
    pub additional_context: Vec<HookContext>,
    pub runs: Vec<HookRunRecord>,
}

pub struct PostToolUseResult {
    pub blocks: Vec<HookBlock>,
    pub additional_context: Vec<HookContext>,
    pub replacement: Option<Value>,
    pub runs: Vec<HookRunRecord>,
}

pub struct StopResult {
    pub blocks: Vec<HookBlock>,
    pub additional_context: Vec<HookContext>,
    pub prevent_continuation: Option<HookBlock>,
    pub runs: Vec<HookRunRecord>,
}

pub enum HookDecision {
    Allow,
    Ask { hook_id: String, reason: Option<String> },
    Defer { hook_id: String },
    Deny { hook_id: String, reason: String },
}

pub struct PromptResult {
    pub block: Option<HookBlock>,
    pub runs: Vec<HookRunRecord>,
}

pub async fn dispatch_pre_tool_use(registry: &HookRegistry, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, executor: &dyn HookExecutor) -> PreToolUseResult;
pub async fn dispatch_post_tool_use(registry: &HookRegistry, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, executor: &dyn HookExecutor) -> PostToolUseResult;
pub async fn dispatch_prompt_submit(registry: &HookRegistry, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, executor: &dyn HookExecutor) -> PromptResult;
pub async fn dispatch_stop(registry: &HookRegistry, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, executor: &dyn HookExecutor) -> StopResult;
pub async fn dispatch_observer(registry: &HookRegistry, event: HookEventName, envelope: &HookEventEnvelope, context: &HookRunContext<'_>, executor: &dyn HookExecutor) -> Vec<HookRunRecord>;
```

- [ ] **Step 1: Write failing dispatcher tests**

Use an injected runner trait rather than real subprocesses. Prove sequential
latest-input delivery, last rewrite wins, later ask replaces earlier ask, deny
terminates and discards accumulated rewrite/context, failure continues, prompt
supports block only, PostToolUse aggregates blocks/context and last replacement,
Stop runs every handler with `continue:false` dominance, observer ignores every
mutation, matcher skip records, and aggregate context stops at 64 KiB on a
complete-entry boundary.

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-extensions --test hooks_dispatcher`

Expected: compilation fails because dispatch APIs are absent.

- [ ] **Step 3: Implement injectable handler execution**

Define an async `HookExecutor` trait implemented by `DefaultHookExecutor` that
selects command or HTTPS. The dispatcher iterates the immutable event slice,
checks matcher eligibility, constructs the next envelope from current rewritten
input, calls the executor, records elapsed/outcome, and applies only healthy
event-valid fields.

- [ ] **Step 4: Implement Grok combination precedence**

For PreToolUse, return immediately on deny with no rewrite/context, otherwise
retain the final ask/defer and last rewrite. For PostToolUse, run all handlers,
append bounded blocks/context, and replace the prior built-in replacement. For
Stop, run all handlers and make any `continue:false` prevent continuation. For
observers, retain only run status and bounded `systemMessage` feedback.

- [ ] **Step 5: Run dispatcher and extension tests**

Run: `cargo test -p lato-extensions --test hooks_dispatcher`

Expected: every ordering, precedence, and isolation test passes.

Run: `cargo test -p lato-extensions`

Expected: all extension tests pass.

- [ ] **Step 6: Commit the dispatcher**

```bash
git add crates/lato-extensions/src/hooks/dispatcher.rs crates/lato-extensions/src/hooks/mod.rs crates/lato-extensions/tests/hooks_dispatcher.rs
git commit -m "feat(extensions): dispatch plugin hooks"
```

### Task 5: Prompt, Tool, and Stop Lifecycle Integration

**Files:**
- Replace: `crates/lato-agent/src/hooks.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Create: `crates/lato-agent/tests/hooks_runtime.rs`

**Interfaces:**
- Consumes: `HookRegistry`, dispatcher outcomes, `SessionSkillHandle`, `ToolRuntime::prepare_scoped`, `ToolApproval`, and `TurnEventEmitter`.
- Produces:

```rust
#[derive(Clone)]
pub struct SessionHookRuntime {
    registry: Arc<RwLock<Arc<HookRegistry>>>,
    cancellation: CancellationToken,
}

impl LegacyTurnDriver {
    pub async fn bind_turn_hooks(&self, runtime: SessionHookRuntime, registry: Arc<HookRegistry>);
}
```

- [ ] **Step 1: Write failing lifecycle tests**

Test prompt block before history commit, prompt failure fail-open, PreToolUse
tool-name immutability, sequential argument rewrite, rewrite followed by schema
failure, rewrite followed by policy denial, rewrite requiring fresh approval,
no reuse of an approval for old arguments, PostToolUse after success and error,
replacement rebounding through `bound_tool_output`, stop block continuation,
`continue:false`, timeout fail-open, and the eight-continuation cap.

- [ ] **Step 2: Run the focused test and confirm red state**

Run: `cargo test -p lato-agent --test hooks_runtime`

Expected: tests fail because the actor still calls `noop_hooks()` and has no
runtime binding.

- [ ] **Step 3: Bind the frozen registry before turn start**

Extend the Phase 6B1 generation-paired extension state to carry
`Arc<HookRegistry>`. `begin_plugin_turn` installs the same generation into the
driver before `StartTurn`; pending reload stays staged. Session cancellation
cancels in-flight hook runner work without cancelling the next generation's
fresh token.

- [ ] **Step 4: Gate the user prompt before persistence**

Move `history.push(HistoryItem::User(text))` after prompt dispatch. On healthy
block, return a stable `hook.prompt_blocked` error without committing
`TurnInputAccepted`; on failure continue with the original prompt. Do not allow
prompt hooks to append model context or rewrite the user text.

- [ ] **Step 5: Rebuild the tool authorization pipeline around PreToolUse**

Call `resolve_and_validate` on the provider request to freeze its canonical
tool name and reject invalid original input, emit PreToolUse, then call
`prepare_scoped` with final rewritten arguments so the schema is validated
again before policy. Construct the
approval request from that new prepared call and execute only its new grant.
Journal `ToolCallRequested` for the provider request, an argument-rewrite audit,
then `ToolCallPrepared` with the final hash. A hook `allow` never skips policy.

- [ ] **Step 6: Apply PostToolUse and Stop results**

Dispatch PostToolUse after the invocation result exists and before the
model-visible `HistoryItem::ToolResult` is appended. Re-bound any replacement
with `bound_tool_output`. At the would-stop branch, dispatch Stop; inject blocks
and context as bounded system-side continuation input, clear per-sample skill
allowlists correctly, and count continuations on the current turn. Stop at
eight even when hooks continue requesting work.

- [ ] **Step 7: Run lifecycle and regression tests**

Run: `cargo test -p lato-agent --test hooks_runtime`

Expected: prompt/tool/stop tests pass.

Run: `cargo test -p lato-agent --test skills_runtime --test plugin_runtime --test journal_runtime`

Expected: Skills, reload, and journal behavior remain green.

- [ ] **Step 8: Commit lifecycle gates**

```bash
git add crates/lato-agent/src/hooks.rs crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/tests/hooks_runtime.rs
git commit -m "feat(agent): wire prompt tool and stop hooks"
```

### Task 6: Session, Compaction, Reload, Child, and Shutdown Integration

**Files:**
- Modify: `crates/lato-agent/src/hooks.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/subagent/runner.rs`
- Modify: `crates/lato-agent/tests/hooks_runtime.rs`
- Modify: `crates/lato-agent/tests/subagent_runner.rs`

**Interfaces:**
- Consumes: session extension state and `dispatch_observer`.
- Produces:

```rust
impl SessionHookRuntime {
    pub async fn session_start(&self, context: &SessionHookContext) -> Vec<HookRunRecord>;
    pub async fn session_end(&self, reason: SessionEndReason, deadline: Instant) -> Vec<HookRunRecord>;
    pub async fn pre_compact(&self, context: &CompactionHookContext) -> Vec<HookRunRecord>;
    pub async fn post_compact(&self, context: &CompactionHookContext) -> Vec<HookRunRecord>;
    pub async fn shutdown(&self, deadline: Instant);
}
```

- [ ] **Step 1: Write failing boundary tests**

Cover SessionStart once before first model request, SessionEnd once after every
started session path, observer mutation ignored, manual and automatic
PreCompact before replacement, PostCompact only after durable replacement,
failed compaction without PostCompact, mid-turn N to N+1 reload isolation,
retired-generation process cleanup after the final reference drops, child
registry derived from parent snapshot/profile, and bounded SessionEnd shutdown.

- [ ] **Step 2: Run boundary tests and confirm red state**

Run: `cargo test -p lato-agent --test hooks_runtime`

Expected: tests fail because the observer seams and generation cleanup do not
exist.

- [ ] **Step 3: Wire SessionStart and SessionEnd exactly once**

Track `session_started_hooks: bool` and `session_end_hooks: bool` in
`RuntimeSession`. Dispatch SessionStart after initial extension materialization
and before the first accepted turn. Every shutdown/drop orchestration path that
started hooks calls the same idempotent SessionEnd method. Enforce the enclosing
shutdown deadline even when individual handler configuration is longer.

- [ ] **Step 4: Wire both compaction paths at commit boundaries**

For manual compaction, dispatch PreCompact before submitting the durable
replacement and PostCompact only after `CompactionCompleted`. For automatic
compaction inside `SessionActor`, dispatch PreCompact immediately before
`events.compact` and PostCompact only after the returned compacted messages are
installed. Treat all outputs as observer status; never alter summary/history.

- [ ] **Step 5: Reconcile generations and derive child runtimes**

Materialize candidate SkillCatalog/HookRegistry pairs before changing current
state. Publish the pair atomically. Retain old registry/process-scope resources
through every active turn reference, then cancel and reap them at retirement.
For a child, call `PluginSnapshot::derive_child` first, then materialize its
registry; never clone an unfiltered parent registry.

- [ ] **Step 6: Run boundary and full agent tests**

Run: `cargo test -p lato-agent --test hooks_runtime`

Expected: all lifecycle boundary cases pass.

Run: `cargo test -p lato-agent`

Expected: all agent unit/integration tests pass.

- [ ] **Step 7: Commit lifecycle completion**

```bash
git add crates/lato-agent/src/hooks.rs crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/src/subagent/runner.rs crates/lato-agent/tests/hooks_runtime.rs crates/lato-agent/tests/subagent_runner.rs
git commit -m "feat(agent): complete hook lifecycle integration"
```

### Task 7: Hook Audit, Documentation, and Phase 6B Gate

**Files:**
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/src/command.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-agent/src/hooks.rs`
- Modify: `crates/lato-agent/tests/hooks_runtime.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `README.md`
- Create: `tests/phase6b_hooks_smoke.rs`
- Create: `docs/testing/reports/phase-6b-skills-hooks-release-gate-2026-09-07.md`

**Interfaces:**
- Consumes: bounded `HookRunRecord` and existing journal hashing/redaction.
- Extends Phase 6B1's restricted `ExtensionAuditRecord` with one typed hook
  variant:

```rust
ExtensionAuditRecord::Hook {
    generation: u64,
    hook_id: String,
    event: String,
    phase: HookAuditPhase,
    outcome: HookAuditOutcome,
    duration_ms: Option<u64>,
    effective_timeout_ms: u64,
    input_hash: String,
    output_hash: Option<String>,
    replaced_prior_hook_id: Option<String>,
    truncated: bool,
    redacted_reason: Option<String>,
}
```

- [ ] **Step 1: Write failing audit tests**

Assert start precedes completion/failure, rewrite/replacement follows its
successful handler and precedes the next handler/tool preparation, final tool
records reference final argument hashes, timeout/failure records remain
non-projecting, and serialized journal bytes contain none of fixture prompt,
arguments, stdout, output, environment secret, URL credential, or additional
context values.

- [ ] **Step 2: Run audit tests and confirm red state**

Run: `cargo test -p lato-core journal`

Run: `cargo test -p lato-agent --test hooks_runtime audit`

Expected: compilation fails because hook audit types are absent.

- [ ] **Step 3: Implement canonical audit emission and replay**

Add serde-stable `HookAuditPhase` values `dispatch_started`, `completed`,
`failed`, `timed_out`, `arguments_rewritten`, `output_replaced`,
`stop_continuation`, `stop_capped`, `generation_adopted`, and
`generation_retired`. Hash canonical payload/result values, store only bounded
redacted reasons, submit session-level records through
`Command::RecordExtensionAudit`, and accept records during replay without
projecting messages or terminal state.

- [ ] **Step 4: Update source attribution and README**

Record every substantially ported Grok hook file. Document trusted-plugin
prerequisite, JSON config shape, event table, command/HTTPS behavior, matcher
rules, return shapes, timeout defaults, fail-open semantics, rewrite
reauthorization, Stop cap, reload boundary, child narrowing, and diagnostics.
State that MCP remains Phase 6C.

- [ ] **Step 5: Run the complete quality gate**

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
the workspace version.

- [ ] **Step 6: Run installed-command lifecycle smoke**

Create `tests/phase6b_hooks_smoke.rs` using the bounded installed-binary fixture
style in `tests/phase5_command_smoke.rs`. Its test
`phase6b_installed_command_smoke_exercises_hook_lifecycle` creates a temporary
trusted workspace with one skill, one command hook, one loopback HTTPS hook
using a test certificate, and one timeout hook. It proves safe argument rewrite,
PostToolUse replacement rebounding, timeout fail-open, stop continuation,
Pre/PostCompact observation, generation N/N+1 turn isolation, SessionEnd
bounded cleanup, and child inability to regain a disallowed tool.

Run:

```bash
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6b_hooks_smoke -- --nocapture
```

Expected: one lifecycle smoke passes, its output ends with
`phase6b-hooks-smoke-ok`, and the temporary fixture leaves no hook process.
Record exact commands, counts, platform-specific unchanged retry, and output in
the Phase 6B gate report.

- [ ] **Step 7: Commit the Phase 6B release gate**

```bash
git add crates/lato-core/src/command.rs crates/lato-core/src/journal.rs crates/lato-runtime/src/session.rs crates/lato-agent/src/hooks.rs crates/lato-agent/tests/hooks_runtime.rs tests/phase6b_hooks_smoke.rs docs/superpowers/reference/lato-upstream-sources.md README.md docs/testing/reports/phase-6b-skills-hooks-release-gate-2026-09-07.md
git commit -m "docs: record phase 6b skills and hooks gate"
```

## Phase 6B Completion Checkpoint

Stop after Task 7 and report every commit, exact workspace test count, clippy
result, installed binary version, command/HTTPS smoke results, and unchanged
pre-existing dirty paths. Phase 6C begins only after this checkpoint is
reviewed.
