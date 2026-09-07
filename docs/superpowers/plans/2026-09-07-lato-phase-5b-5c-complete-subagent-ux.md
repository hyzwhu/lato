# Lato Phase 5B/5C Complete Subagent UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the complete Grok Build subagent coordinator/backend/task-tool stack into Lato, connect it to real child `RuntimeSession` and Git worktree execution, then expose bounded `spawn`, `send`, `wait`, `cancel`, and `inspect` tools.

**Architecture:** The ported Grok Build `ChannelBackend` and single-writer `SubagentCoordinator` become the authoritative task path behind Lato compatibility conversions. `ChildSessionRunner` creates one isolated runtime session per task, `GitWorkspaceAllocator` owns transactional worktree leases, and result adapters enforce profile schemas and programmatic invariants before terminal completion. Phase 5C switches the model-facing registry only after the Phase 5B end-to-end gate passes.

**Tech Stack:** Rust 2024, Tokio actors/channels/processes, `tokio-util::CancellationToken`, Serde/JSON Schema, `async-trait`, Git CLI, existing Lato `ModelPort`, `ToolRuntime`, `PolicyEngine`, sandbox, session journal, and fake model streams.

## Global Constraints

- Port from Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`, Apache-2.0.
- Preserve source headers and update `docs/superpowers/reference/lato-upstream-sources.md` for every substantially derived production file.
- The ported coordinator/backend stack replaces the Phase 5A production actor; Phase 5A public types remain available through conversions until callers migrate.
- Phase 5B must not expose new model-visible operations or change CLI, TUI, headless, or ACP output.
- Phase 5C exposes only built-in `explorer`, `worker`, and `reviewer` profiles.
- Child capabilities equal parent grant intersection profile allowlist intersection workspace allowance.
- Every model, tool, subprocess, queue, wait, message, output, recursion, callback, cleanup, and shutdown path is bounded and cancellable.
- Worker writes are isolated in distinct Git worktrees; explorer and reviewer are read-only.
- Never merge, commit, push, publish, or automatically apply child changes.
- Preserve unrelated dirty-worktree files.
- Run relevant tests before `cargo install --path .`.

---

## Planned file structure

```text
crates/lato-core/src/
  task.rs                              # profile result schemas and compatibility conversions

crates/lato-runtime/src/task/
  backend.rs                           # ported SubagentBackend + ChannelBackend
  types.rs                             # ported request/result/event values
  coordinator.rs                       # authoritative ported single-writer coordinator
  coordinator/{spawn,queue,query,active_message}.rs
  runner.rs                            # child-runner contract retained at Lato boundary
  compatibility.rs                     # Phase 5A public API conversions
  mod.rs
crates/lato-runtime/tests/
  task_backend.rs                      # channel/session binding parity
  task_port_parity.rs                  # adapted Grok coordinator cases

crates/lato-workspace/src/
  git_allocator.rs                     # real transactional Git allocator
  process.rs                           # PID liveness and ownership markers
  task.rs                              # lease cleanup metadata
  lib.rs
crates/lato-workspace/tests/
  git_allocator.rs                     # isolation, rollback, recovery, release

crates/lato-agent/src/subagent/
  mod.rs
  context.rs                           # bounded context package
  profile.rs                           # built-in profile resolution
  control.rs                           # active messages and cancellation bridge
  runner.rs                            # real ChildSessionRunner
  events.rs                            # runtime-to-task event translation
  result.rs                            # output capture and schema parsing
  verifier.rs                          # programmatic verification
crates/lato-agent/src/runtime_session.rs # injectable child construction/events
crates/lato-agent/src/host.rs           # coordinator ownership and root teardown
crates/lato-agent/tests/
  subagent_runner.rs
  subagent_e2e.rs
  subagent_shutdown.rs

crates/lato-tools/src/task/
  mod.rs                               # lifecycle tool adapters
  schemas.rs                           # exact JSON schemas and bounded inputs
  backend_resource.rs                  # session-bound backend injection
crates/lato-tools/src/
  builtin_adapter.rs                   # remove compatibility spawn dispatch
  registry.rs                          # atomic Phase 5C registry switch
  subagent.rs                          # remove direct worktree helper after switch
  lib.rs
crates/lato-tools/tests/
  task_tools.rs
  tool_runtime.rs

tests/
  subagent_cli_compat.rs               # unchanged presentation regression

docs/superpowers/reference/lato-upstream-sources.md
```

---

### Task 1: Port Grok request, result, event, and backend contracts

**Files:**
- Create: `crates/lato-runtime/src/task/types.rs`
- Create: `crates/lato-runtime/src/task/backend.rs`
- Create: `crates/lato-runtime/src/task/compatibility.rs`
- Modify: `crates/lato-runtime/src/task/mod.rs`
- Test: `crates/lato-runtime/tests/task_backend.rs`

**Interfaces:**
- Produces: `SubagentRequest`, `SubagentRuntimeOverrides`, `SubagentResult`, `SubagentSnapshot`, `SubagentEvent`, `SubagentBackend`, `ChannelBackend`, `SubagentBackendResource`.
- Consumes: existing `TaskSpec`, `TaskResult`, `TaskSnapshot`, `TaskError`, `TaskHandle`, and `CancellationToken`.

- [ ] **Step 1: Write failing backend binding and conversion tests**

Add tests proving a session-bound backend injects `parent_session_id`, channel
closure maps to a stable unavailable result, foreign roots are indistinguishable
from unknown tasks, and round-trip conversions retain task ID, profile, owner,
budget, status, usage, progress, output, and error.

```rust
#[tokio::test]
async fn channel_backend_binds_every_request_to_parent_session() {
    let (backend, mut rx) = test_backend("parent-session");
    let call = tokio::spawn(async move { backend.inspect("child-1").await });
    let event = rx.recv().await.unwrap();
    assert_eq!(event.parent_session_id(), Some("parent-session"));
    event.respond_not_found();
    assert!(matches!(call.await.unwrap(), InspectOutcome::NotFound));
}

#[test]
fn phase_5a_snapshot_round_trip_preserves_public_fields() {
    let original = completed_task_snapshot();
    let ported = SubagentSnapshot::try_from(original.clone()).unwrap();
    assert_eq!(TaskSnapshot::try_from(ported).unwrap(), original);
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

Run: `cargo test -p lato-runtime --test task_backend`

Expected: compilation fails because the ported backend and conversion types do not exist.

- [ ] **Step 3: Port and adapt the contracts**

Copy the structure from upstream `task/types.rs` and `task/backend.rs`, replace
unbounded channels with the existing configured Lato capacities, use Lato IDs
and error types at the membrane, and add the required source headers. The core
backend contract must be:

```rust
#[async_trait::async_trait]
pub trait SubagentBackend: Send + Sync + 'static {
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, TaskError>;
    async fn send(&self, request: ActiveMessageRequest) -> ActiveMessageOutcome;
    async fn wait(&self, request: WaitRequest) -> WaitOutcome;
    async fn cancel(&self, task_id: &TaskId) -> CancelOutcome;
    async fn inspect(&self, task_id: &TaskId) -> InspectOutcome;
    async fn list_running(&self) -> Vec<SubagentSnapshot>;
    async fn validate_profile(&self, profile: &str) -> ValidateProfileOutcome;
}
```

- [ ] **Step 4: Run tests and clippy for the crate**

Run: `cargo test -p lato-runtime --test task_backend && cargo clippy -p lato-runtime --all-targets -- -D warnings`

Expected: all focused tests pass and clippy emits no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_backend.rs
git commit -m "feat(runtime): port grok subagent backend contracts"
```

---

### Task 2: Replace the Phase 5A production actor with the ported coordinator

**Files:**
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Create: `crates/lato-runtime/src/task/coordinator/spawn.rs`
- Create: `crates/lato-runtime/src/task/coordinator/queue.rs`
- Create: `crates/lato-runtime/src/task/coordinator/query.rs`
- Create: `crates/lato-runtime/src/task/coordinator/active_message.rs`
- Modify: `crates/lato-runtime/src/task/mod.rs`
- Test: `crates/lato-runtime/tests/task_port_parity.rs`
- Test: existing `crates/lato-runtime/tests/task_*.rs`

**Interfaces:**
- Consumes: Task 1 backend events and existing `TaskRunner`, `WorkspaceAllocator`, `TaskVerifier`.
- Produces: `spawn_subagent_coordinator(config, runner, allocator, verifier) -> CoordinatorRuntime` and compatibility `spawn_task_coordinator*` constructors.

- [ ] **Step 1: Add parity tests for the ported actor**

Port upstream coordinator tests for duplicate IDs, root fairness, cancelled queue
entries, prepare/promotion races, multiple waiters, foreground backgrounding,
message permit finalization, stale generation events, one terminal state, scoped
cancellation, retained completion, and bounded root drain.

```rust
#[tokio::test(start_paused = true)]
async fn cancellation_winning_promotion_tears_down_unpublished_child() {
    let harness = PortedHarness::paused_before_started().await;
    let spawn = harness.spawn(worker_request("child-1")).await;
    harness.cancel("child-1").await;
    harness.release_started_ack().await;
    assert_eq!(harness.inspect("child-1").await.status(), TaskStatus::Cancelled);
    assert_eq!(harness.runner.live_children(), 0);
    assert_eq!(harness.allocator.live_count().await, 0);
    drop(spawn);
}
```

- [ ] **Step 2: Confirm the tests fail on the missing port path**

Run: `cargo test -p lato-runtime --test task_port_parity`

Expected: compilation fails on `spawn_subagent_coordinator` and ported event types.

- [ ] **Step 3: Port coordinator modules and wire compatibility constructors**

Retain the single-writer rule. Keep all existing Phase 5A hard limits by mapping
`CoordinatorConfig` into the ported admission values. Route old `TaskHandle`
methods through `ChannelBackend` conversions; do not run two actors.

```rust
pub struct CoordinatorRuntime {
    pub backend: ChannelBackend,
    pub events: broadcast::Receiver<TaskEvent>,
    join: JoinHandle<()>,
}

pub fn spawn_task_coordinator<R, A>(
    config: CoordinatorConfig,
    runner: Arc<R>,
    allocator: Arc<A>,
) -> TaskHandle
where
    R: TaskRunner,
    A: WorkspaceAllocator,
{
    spawn_subagent_coordinator(config, runner, allocator, Arc::new(DefaultTaskVerifier))
        .compatibility_handle()
}
```

- [ ] **Step 4: Run the port and legacy contract suites**

Run: `cargo test -p lato-runtime --test task_port_parity && cargo test -p lato-runtime --tests`

Expected: both the adapted Grok parity suite and every existing Phase 5A test pass against one coordinator.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_port_parity.rs
git commit -m "feat(runtime): replace task actor with grok coordinator"
```

---

### Task 3: Implement transactional Git workspace allocation and recovery

**Files:**
- Create: `crates/lato-workspace/src/process.rs`
- Create: `crates/lato-workspace/src/git_allocator.rs`
- Modify: `crates/lato-workspace/src/task.rs`
- Modify: `crates/lato-workspace/src/lib.rs`
- Modify: `crates/lato-workspace/Cargo.toml`
- Test: `crates/lato-workspace/tests/git_allocator.rs`

**Interfaces:**
- Produces: `GitWorkspaceAllocator::new(repo_root, worktrees_root, limits)`, `recover_stale()`, and `WorkspaceAllocator` implementation.
- Consumes: `WorkspaceRequest`, `WorkspaceLease`, `TaskId`, `TaskError`, Git CLI.

- [ ] **Step 1: Write failing real-Git tests**

Create temporary repositories with an initial commit and test shared read-only
leases, two isolated worker paths and branches, transactional rollback after
injected failures, idempotent release, dead-owner recovery, live-owner
preservation, non-repository rejection, and refusal to clean outside the
configured worktree root.

```rust
#[tokio::test]
async fn parallel_worker_leases_are_git_isolated() {
    let repo = TestRepo::new();
    let allocator = repo.allocator();
    let (left, right) = tokio::join!(
        allocator.allocate(worker_request("left")),
        allocator.allocate(worker_request("right")),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.root, right.root);
    assert_ne!(left.branch, right.branch);
    std::fs::write(left.root.join("shared.txt"), "left").unwrap();
    assert!(!right.root.join("shared.txt").exists());
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-workspace --test git_allocator`

Expected: compilation fails because `GitWorkspaceAllocator` and lease cleanup metadata do not exist.

- [ ] **Step 3: Implement the allocator**

Use `tokio::process::Command` with explicit argument arrays, a per-repository
Tokio mutex, and a marker written atomically inside the configured worktree.
Validate task IDs before using them in paths or branch names.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeOwnerMarker {
    pub task_id: TaskId,
    pub pid: u32,
    pub repository: PathBuf,
    pub created_unix_ms: u64,
}

#[async_trait::async_trait]
impl WorkspaceAllocator for GitWorkspaceAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError>;
    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError>;
}
```

Allocation rollback must undo marker, worktree registration, directory, and
new branch in reverse order. Release must use `git worktree remove --force`,
then guarded `git worktree prune`; failures retain cleanup authority and a
recovery marker.

- [ ] **Step 4: Run workspace tests and clippy**

Run: `cargo test -p lato-workspace && cargo clippy -p lato-workspace --all-targets -- -D warnings`

Expected: all workspace tests pass without warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-workspace
git commit -m "feat(workspace): add recoverable git task allocator"
```

---

### Task 4: Freeze built-in profiles, context packages, and capability narrowing

**Files:**
- Modify: `crates/lato-core/src/task.rs`
- Create: `crates/lato-agent/src/subagent/mod.rs`
- Create: `crates/lato-agent/src/subagent/profile.rs`
- Create: `crates/lato-agent/src/subagent/context.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Test: `crates/lato-agent/tests/subagent_runner.rs`

**Interfaces:**
- Produces: `BuiltinProfileName`, `ResolvedChildProfile`, `ContextPackage`, `ContextPackageBuilder`.
- Consumes: parent capabilities, workspace lease, task description, selected evidence/artifacts, limits.

- [ ] **Step 1: Add failing profile and context tests**

Test exact tool capability sets, read-only shell denial, rejected custom profile
names, parent/profile intersection, nested-spawn removal at max depth, byte and
item truncation, artifact references, secret omission, and no full transcript
copy.

```rust
#[test]
fn worker_cannot_recover_capability_absent_from_parent() {
    let parent = capabilities![FileRead, FileWrite];
    let profile = resolve_profile("worker", &parent, WorkspaceMode::IsolatedWorktree).unwrap();
    assert!(profile.capabilities.contains(&ToolCapability::FileWrite));
    assert!(!profile.capabilities.contains(&ToolCapability::ShellExecute));
}

#[test]
fn context_package_is_bounded_and_omits_unselected_turns() {
    let package = ContextPackageBuilder::new(test_limits())
        .task("inspect parser")
        .parent_summary("bounded summary")
        .selected_evidence(vec![evidence("src/parser.rs")])
        .build()
        .unwrap();
    assert!(package.encoded_len() <= test_limits().max_bytes);
    assert!(!package.render().contains("unrelated parent turn canary"));
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-agent --test subagent_runner profile context`

Expected: compilation fails because the subagent profile/context modules do not exist.

- [ ] **Step 3: Implement exact built-in profiles and context builder**

Define profile result schemas as serializable Rust structs. Reject all names
except `explorer`, `worker`, and `reviewer`. Render natural-language context for
the model and retain typed fields for routing/verification.

```rust
pub struct ContextPackage {
    pub task: String,
    pub constraints: Vec<String>,
    pub parent_summary: Option<String>,
    pub evidence: Vec<EvidenceRef>,
    pub artifacts: Vec<ArtifactRef>,
    pub workspace_root: PathBuf,
    pub remaining_budget: BudgetAmount,
}

pub fn narrow_capabilities(
    parent: &[ToolCapability],
    profile: &ResolvedChildProfile,
    workspace: WorkspaceMode,
) -> Result<Vec<ToolCapability>, TaskError>;
```

- [ ] **Step 4: Run focused tests**

Run: `cargo test -p lato-agent --test subagent_runner`

Expected: profile and context tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-core/src/task.rs crates/lato-agent/src/subagent crates/lato-agent/src/lib.rs crates/lato-agent/tests/subagent_runner.rs
git commit -m "feat(agent): add bounded child profiles and context"
```

---

### Task 5: Make RuntimeSession child-execution and cancellation contracts explicit

**Files:**
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-tools/src/shell.rs`
- Modify: `crates/lato-tools/src/runtime.rs`
- Test: `crates/lato-agent/tests/subagent_shutdown.rs`
- Test: `crates/lato-tools/tests/tool_runtime.rs`

**Interfaces:**
- Produces: `RuntimeSession::new_child(ChildSessionConfig)`, `subscribe()`, `cancel_and_join(deadline)`, and process-group-aware shell cancellation.
- Consumes: child model port, narrowed tool runtime, cancellation token, context history.

- [ ] **Step 1: Add failing cancellation tests**

Test cancellation before model start, blocked model stream, active tool future,
shell subprocess with a spawned descendant, and shutdown idempotence. Assert the
join deadline completes and no child PID remains alive.

```rust
#[tokio::test]
async fn cancel_and_join_aborts_blocked_model_stream() {
    let model = BlockingModelPort::new();
    let session = child_session(model.clone()).await;
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("block").await }
    });
    model.wait_until_started().await;
    session.cancel_and_join(Duration::from_secs(1)).await.unwrap();
    assert!(prompt.await.unwrap().is_err());
    assert_eq!(model.live_streams(), 0);
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-agent --test subagent_shutdown && cargo test -p lato-tools --test tool_runtime shell_cancel`

Expected: missing child constructor/join APIs or descendant process remains live.

- [ ] **Step 3: Implement child construction and downward cancellation**

Inject a prebuilt `ToolRuntime` and root `CancellationToken` into the child
driver. Expose the existing broadcast receiver through `subscribe`. On Unix,
start shell commands in a new process group and send TERM then KILL within fixed
deadlines; keep the non-Unix behavior compile-safe.

```rust
pub struct ChildSessionConfig {
    pub session_id: SessionId,
    pub model: Arc<dyn ModelPort>,
    pub tool_runtime: Arc<ToolRuntime>,
    pub initial_history: Vec<HistoryItem>,
    pub cancellation: CancellationToken,
}

pub async fn cancel_and_join(&self, deadline: Duration) -> Result<(), AgentError>;
```

- [ ] **Step 4: Run agent and tool tests**

Run: `cargo test -p lato-agent --test subagent_shutdown && cargo test -p lato-tools --test tool_runtime`

Expected: all cancellation and existing runtime tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-agent/src/runtime_session.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/tests/subagent_shutdown.rs crates/lato-tools/src/shell.rs crates/lato-tools/src/runtime.rs crates/lato-tools/tests/tool_runtime.rs
git commit -m "feat(agent): make child session cancellation bounded"
```

---

### Task 6: Implement the real child runner, event relay, and control bridge

**Files:**
- Create: `crates/lato-agent/src/subagent/control.rs`
- Create: `crates/lato-agent/src/subagent/events.rs`
- Create: `crates/lato-agent/src/subagent/result.rs`
- Create: `crates/lato-agent/src/subagent/runner.rs`
- Modify: `crates/lato-agent/src/subagent/mod.rs`
- Test: `crates/lato-agent/tests/subagent_runner.rs`

**Interfaces:**
- Produces: `ChildSessionRunner: TaskRunner` and `ChildSessionControl: TaskChildControl`.
- Consumes: Tasks 2–5 coordinator, profiles, context, allocator lease, runtime session.

- [ ] **Step 1: Add failing runner lifecycle tests**

Test distinct child session IDs, acknowledged promotion, active-message queue
and steer delivery, monotonic usage/progress, output capture, persisted oversized
output reference, cancellation, and `on_completed` ordering after shutdown and
workspace release.

```rust
#[tokio::test]
async fn real_runner_relays_child_usage_and_returns_output() {
    let fixture = RunnerFixture::new(scripted_model("child answer", usage(11, 7))).await;
    let result = fixture.run(explorer_task("inspect")).await;
    assert_eq!(result.result.output.as_deref(), Some("child answer"));
    let snapshots = fixture.reported_progress();
    assert!(snapshots.windows(2).all(|w| w[0].tokens <= w[1].tokens));
    assert_eq!(fixture.final_usage().total_tokens, 18);
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-agent --test subagent_runner runner`

Expected: missing runner/control/event adapter symbols.

- [ ] **Step 3: Implement the runner**

Build one child tool runtime per request, render the context package into the
initial history, create the session, publish `StartedTask` only after all
resources exist, translate runtime events in a bounded relay task, and always
call `cancel_and_join` before returning.

```rust
#[async_trait::async_trait]
impl TaskRunner for ChildSessionRunner {
    type Control = ChildSessionControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput;

    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError>;
    async fn load_persisted_output(&self, output_ref: &str) -> Result<Option<String>, TaskError>;
    fn on_completed(&self, completion: TaskCompletion);
}
```

- [ ] **Step 4: Run runner tests**

Run: `cargo test -p lato-agent --test subagent_runner`

Expected: all runner lifecycle tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-agent/src/subagent crates/lato-agent/tests/subagent_runner.rs
git commit -m "feat(agent): run tasks in real child sessions"
```

---

### Task 7: Verify real explorer, worker, and reviewer outputs

**Files:**
- Create: `crates/lato-agent/src/subagent/verifier.rs`
- Modify: `crates/lato-agent/src/subagent/result.rs`
- Modify: `crates/lato-agent/src/subagent/mod.rs`
- Test: `crates/lato-agent/tests/subagent_runner.rs`

**Interfaces:**
- Produces: `ProfileResultVerifier: TaskVerifier` and typed `ExplorerOutput`, `WorkerOutput`, `ReviewerOutput`.
- Consumes: child tool-event evidence, workspace lease, result contract, raw child output.

- [ ] **Step 1: Add failing schema and invariant tests**

Cover valid/invalid JSON, missing explorer citation, unrecorded remote citation,
worker artifact outside its lease, worker test claim contradicting recorded tool
events, invalid reviewer severity/location, empty reviewer findings, and explicit
review-to-worker context transfer.

```rust
#[tokio::test]
async fn worker_cannot_claim_a_test_that_recorded_tool_events_show_failed() {
    let verifier = verifier_with_events(vec![failed_command("cargo test")]);
    let output = worker_output_with_test("cargo test", true);
    let error = verifier.verify(worker_node(), output.into()).await.unwrap_err();
    assert_eq!(error.code, TaskErrorCode::VerificationFailed);
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-agent --test subagent_runner verification`

Expected: missing typed outputs and verifier.

- [ ] **Step 3: Implement schema parsing and programmatic checks**

Parse once into the profile-specific type. Resolve paths using canonical nearest
existing ancestors, never lexical prefix alone. Match citations and test claims
against recorded bounded evidence.

```rust
#[derive(Deserialize, Serialize)]
pub struct ReviewerOutput {
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Deserialize, Serialize)]
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub message: String,
    pub evidence: String,
    pub file: Option<PathBuf>,
    pub line: Option<u32>,
}
```

- [ ] **Step 4: Run verification tests**

Run: `cargo test -p lato-agent --test subagent_runner verification`

Expected: all valid outputs pass and every invalid invariant fails with a stable code.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-agent/src/subagent crates/lato-agent/tests/subagent_runner.rs
git commit -m "feat(agent): verify profile-specific task results"
```

---

### Task 8: Own the coordinator in the parent host and prove Phase 5B end to end

**Files:**
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Create: `crates/lato-agent/tests/subagent_e2e.rs`
- Modify: `crates/lato-agent/tests/subagent_shutdown.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Produces: one shared coordinator per `AgentHost`, one registered root per parent session, bounded `shutdown_root` and host shutdown.
- Consumes: ported coordinator, `ChildSessionRunner`, `GitWorkspaceAllocator`, `ProfileResultVerifier`.

- [ ] **Step 1: Add the complete Phase 5B end-to-end matrix**

Use fake model streams with real child sessions, tool runtime, policy engine,
filesystem, subprocesses, and temporary Git repositories. Implement all 15
cases from the approved design, including two concurrent workers and complete
tree shutdown.

```rust
#[tokio::test]
async fn two_real_workers_write_only_their_own_worktrees() {
    let host = E2eHost::new_git_repo().await;
    let (left, right) = tokio::join!(
        host.spawn_worker("write shared.txt as left"),
        host.spawn_worker("write shared.txt as right"),
    );
    assert_eq!(read(left.worktree().join("shared.txt")), "left");
    assert_eq!(read(right.worktree().join("shared.txt")), "right");
    assert!(!host.repo_root().join("shared.txt").exists());
}
```

- [ ] **Step 2: Run the Phase 5B suite and verify failure**

Run: `cargo test -p lato-agent --test subagent_e2e --test subagent_shutdown`

Expected: host does not yet own/register the real coordinator and tests fail.

- [ ] **Step 3: Wire host lifecycle and root teardown**

Create the coordinator once, bind backend resources per session, reopen spawn
admission at turn start, close/cancel turn-owned children on turn cancellation,
and drain the whole root during session shutdown.

```rust
pub struct AgentHost {
    sessions: HashMap<String, Arc<RuntimeSession>>,
    task_runtime: Arc<CoordinatorRuntime>,
}

async fn shutdown_session_tasks(&self, session_id: &SessionId) -> Result<(), AgentError> {
    self.task_runtime
        .backend_for(session_id)
        .teardown_root_and_drain(self.task_runtime.config().shutdown_timeout)
        .await
}
```

- [ ] **Step 4: Run the Phase 5B acceptance gate**

Run: `cargo test -p lato-agent --test subagent_runner --test subagent_e2e --test subagent_shutdown && cargo test -p lato-runtime --tests && cargo test -p lato-workspace`

Expected: all Phase 5B focused suites pass.

- [ ] **Step 5: Lock presentation compatibility**

Run: `cargo test --test cli_headless --test tui_cli --test sessions_cli --test model_port_wiring`

Expected: existing output assertions pass without updates.

- [ ] **Step 6: Update source ledger and commit**

```bash
git add crates/lato-agent docs/superpowers/reference/lato-upstream-sources.md
git commit -m "feat(agent): connect complete subagent runtime"
```

---

### Task 9: Add the Phase 5C lifecycle tool schemas and adapters

**Files:**
- Create: `crates/lato-tools/src/task/mod.rs`
- Create: `crates/lato-tools/src/task/schemas.rs`
- Create: `crates/lato-tools/src/task/backend_resource.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Modify: `crates/lato-tools/src/registry.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Test: `crates/lato-tools/tests/task_tools.rs`

**Interfaces:**
- Produces model-visible `spawn`, `send`, `wait`, `cancel`, `inspect` tools.
- Consumes: session-bound `SubagentBackendResource` and exact bounded inputs.

- [ ] **Step 1: Add failing schema and authorization tests**

Assert exact tool names, required fields, profile enum, bounded string/array
sizes, wait deadline maximum, no arbitrary cwd/grant/profile schema, session
binding, foreign-task denial, and backend error mapping.

```rust
#[test]
fn phase_5c_registry_exposes_only_bounded_task_lifecycle_tools() {
    let names = v1_tool_names();
    for expected in ["spawn", "send", "wait", "cancel", "inspect"] {
        assert!(names.contains(&expected));
    }
    assert!(!names.contains(&"spawn_subagent"));
    assert_eq!(spawn_schema()["properties"]["profile"]["enum"], json!([
        "explorer", "worker", "reviewer"
    ]));
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-tools --test task_tools`

Expected: lifecycle tools are absent and compatibility `spawn_subagent` remains.

- [ ] **Step 3: Implement tools over the backend resource**

Each tool validates its input before allocating coordinator capacity. Bind the
resource to the current session/root during tool-runtime construction. Reuse
Grok Build's foreground/background semantics and active-message operations,
while preserving Lato error/output encoding.

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnInput {
    pub task: String,
    pub description: String,
    pub profile: BuiltinProfileName,
    #[serde(default)]
    pub context: Vec<ContextReference>,
    #[serde(default)]
    pub background: bool,
    pub await_timeout_ms: Option<u64>,
}
```

- [ ] **Step 4: Run task-tool and existing tool-runtime tests**

Run: `cargo test -p lato-tools --test task_tools --test tool_runtime`

Expected: all tests pass with the new registry.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-tools
git commit -m "feat(tools): expose bounded task lifecycle operations"
```

---

### Task 10: Remove compatibility spawn and connect background session events

**Files:**
- Delete: `crates/lato-tools/src/subagent.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `crates/lato-tools/src/dispatch.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Test: `crates/lato-agent/tests/subagent_e2e.rs`
- Test: `tests/subagent_cli_compat.rs`

**Interfaces:**
- Produces: bounded task completion/progress session events and one production spawn path.
- Consumes: Task 9 lifecycle tools and host-owned coordinator.

- [ ] **Step 1: Add failing switch-over and background tests**

Prove no direct worktree helper remains reachable, background completion is
visible through `wait` and `inspect`, completion events are bounded and emitted
once, and existing rendered CLI/TUI/ACP output is byte-compatible for sessions
that do not invoke the new tools.

```rust
#[tokio::test]
async fn background_completion_is_observable_once_without_history_injection() {
    let fixture = ToolLoopFixture::new().await;
    let id = fixture.spawn_background("explorer", "inspect").await;
    fixture.finish_child(&id, "done").await;
    assert_eq!(fixture.wait(&id).await.output(), Some("done"));
    assert_eq!(fixture.completion_events(&id).len(), 1);
    assert!(!fixture.parent_history().contains("synthetic completion reminder"));
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p lato-agent --test subagent_e2e background && cargo test --test subagent_cli_compat`

Expected: compatibility spawn is still reachable or completion event path is absent.

- [ ] **Step 3: Delete direct worktree spawn and wire session events**

Remove `invoke_compat_spawn_subagent`, legacy dispatch arms, definitions, and
exports. Translate coordinator completion/progress into internal session events
without adding new presentation rendering.

- [ ] **Step 4: Run Phase 5C and compatibility tests**

Run: `cargo test -p lato-tools --test task_tools --test tool_runtime && cargo test -p lato-agent --test subagent_e2e && cargo test --test subagent_cli_compat --test cli_headless --test tui_cli`

Expected: one spawn path, observable background tasks, and unchanged existing presentation output.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-tools crates/lato-agent tests/subagent_cli_compat.rs
git commit -m "feat(agent): switch to complete task product entry"
```

---

### Task 11: Run the full fault matrix and remove the superseded actor implementation

**Files:**
- Modify/Delete: superseded files under `crates/lato-runtime/src/task/` identified by compiler-unused and parity coverage
- Modify: `crates/lato-runtime/src/task/mod.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Test: all task/runtime/agent/workspace/tool tests

**Interfaces:**
- Produces: one coordinator implementation with no compatibility actor running in parallel.
- Consumes: all previous tasks.

- [ ] **Step 1: Run coverage-oriented fault suites before deletion**

Run: `cargo test -p lato-runtime --tests && cargo test -p lato-agent --test subagent_runner --test subagent_e2e --test subagent_shutdown && cargo test -p lato-workspace && cargo test -p lato-tools`

Expected: all tests pass before removing unused production modules.

- [ ] **Step 2: Remove only superseded production code**

Use `rg` and compiler references to identify Phase 5A actor modules no longer
called by either the ported coordinator or public compatibility conversions.
Delete those modules and their exports; retain tests that express still-valid
public invariants and point them at the ported path.

- [ ] **Step 3: Prove there is one coordinator and one spawn path**

Run: `rg -n "spawn_task_coordinator|spawn_subagent_coordinator|spawn_subagent|create_subagent_worktree" crates src tests`

Expected: constructors route to the ported coordinator; no compatibility worktree spawn remains; direct allocator creation exists only in `lato-workspace` and tests.

- [ ] **Step 4: Re-run all focused suites**

Run: `cargo test -p lato-runtime --tests && cargo test -p lato-agent && cargo test -p lato-workspace && cargo test -p lato-tools`

Expected: all tests pass after cleanup.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-runtime docs/superpowers/reference/lato-upstream-sources.md
git commit -m "refactor(runtime): retire phase 5a compatibility actor"
```

---

### Task 12: Final verification, local deployment, and smoke tests

**Files:**
- Modify only files required to fix failures discovered by the gates

**Interfaces:**
- Produces: locally installed `lato` with Phase 5B/5C complete.
- Consumes: complete repository.

- [ ] **Step 1: Format and inspect the final diff**

Run: `cargo fmt --all -- --check && git diff --check && git status --short`

Expected: formatting and whitespace checks pass; unrelated dirty files remain untouched.

- [ ] **Step 2: Run the complete test suite**

Run: `cargo test --workspace`

Expected: every unit, integration, CLI, TUI, ACP, task, workspace, and agent test passes.

- [ ] **Step 3: Run strict clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: no warnings.

- [ ] **Step 4: Install locally as required by AGENTS.md**

Run: `cargo install --path .`

Expected: installation succeeds and replaces the local `lato` binary.

- [ ] **Step 5: Run command-level compatibility smoke tests**

Run: `lato --help && cargo test --test cli_headless --test tui_cli --test sessions_cli`

Expected: the binary starts, help remains compatible, and existing output tests pass.

- [ ] **Step 6: Commit any gate-only repairs**

If the gates required source changes, commit only those files:

```bash
git add <exact-files-fixed-by-the-gate>
git commit -m "fix: close phase 5b 5c verification gaps"
```

If no source changes were required, do not create an empty commit.

---

## Self-review record

- Spec coverage: all Phase 5B runner, workspace, permissions, verification,
  fault, shutdown, and compatibility requirements map to Tasks 3–8; Phase 5C
  model tools and event surfacing map to Tasks 9–10; replacement cleanup and
  deployment map to Tasks 11–12.
- Placeholder scan: no deferred implementation markers are present.
- Type consistency: the ported backend owns lifecycle requests; the existing
  `TaskRunner`/allocator/verifier seams connect child execution; compatibility
  conversions preserve existing public callers during migration.
- Scope: Phase 5B is an explicit acceptance gate before any Phase 5C registry
  switch.
