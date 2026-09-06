# Lato Phase 5A Task Coordination Kernel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the complete process-local task coordination kernel—task tree, hierarchical budgets, bounded admission, foreground/background handoff, scoped cancellation, active messages, workspace leases, verification, inspection, and events—without exposing a new model-facing `spawn_subagent` experience.

**Architecture:** A shared `TaskCoordinator` Tokio actor is the sole writer of task lifecycle state. Provider-neutral domain values live in `lato-core`, workspace allocation contracts live in `lato-workspace`, and `lato-runtime` owns the actor, runner boundary, queues, waiters, message admission, and event stream. Runtime behavior is proved with deterministic fake runners and allocators; `lato-agent`, tools, CLI, TUI, ACP, real child sessions, and real Git worktrees remain unchanged.

**Tech Stack:** Rust 2024, Tokio actors/channels/time, `tokio-util::CancellationToken`, `async-trait`, Serde, `thiserror`, `futures-util`, Cargo workspace tests.

## Global Constraints

- Semantic upstream: Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`, Apache-2.0.
- Preserve a real Lato task tree; do not copy Grok Build's nested-child reparenting.
- All coordinator ingress, fan-out, queues, waiters, retained completions, messages, deadlines, retries, and budgets must have integer bounds.
- Only the coordinator actor may mutate task lifecycle state, budget reservations, registries, or admission latches.
- Child permissions, profiles, budgets, and workspace modes may only narrow inherited authority.
- Phase 5A must not change `lato-tools`, `lato-agent`, CLI, TUI, headless, or ACP behavior.
- Do not connect the existing compatibility `spawn_subagent` worktree helper to this coordinator.
- Do not create real child `RuntimeSession` instances or real Git worktrees.
- Do not add task journal recovery or claim crash recovery for live tasks.
- Add the required upstream source header to every substantially derived production file and update `docs/superpowers/reference/lato-upstream-sources.md` before completion.
- Preserve all unrelated dirty-worktree files.
- After tests and lint pass, deploy locally with `cargo install --path .`.

## Planned file structure

```text
crates/lato-core/src/
  id.rs                         # add TaskId, AgentId, LeaseId
  task.rs                       # task domain, lifecycle, profiles, results, errors
  budget.rs                     # multi-dimensional hierarchical budget arithmetic
  lib.rs                        # public exports
crates/lato-core/tests/
  task_contract.rs              # serde, lifecycle, profile, permission contracts
  budget_contract.rs            # reservation, settlement, roll-up invariants

crates/lato-workspace/src/
  task.rs                       # WorkspaceMode, WorkspaceLease, WorkspaceAllocator
  lib.rs                        # public exports
crates/lato-workspace/tests/
  task_workspace.rs             # fake allocation, isolation, idempotent release

crates/lato-runtime/src/task/
  mod.rs                        # public exports and coordinator construction
  protocol.rs                   # commands, replies, snapshots, events, config
  runner.rs                     # TaskRunner, TaskControl, TaskReporter seams
  state.rs                      # actor-owned registries and transition commit path
  admission.rs                  # queue/reject policy and capacity accounting
  queue.rs                      # FIFO queue and cross-root capacity scan
  coordinator.rs                # actor loop, internal event routing, deadlines
  spawn.rs                      # spawn validation, reservation, promotion
  query.rs                      # inspect/list/wait/completed retention
  cancel.rs                     # task/turn/root/workflow cancel and drain
  active_message.rs             # bounded admission lease and finalization
  verification.rs               # verifier seam and terminal verification flow
crates/lato-runtime/src/lib.rs   # public task-runtime exports
crates/lato-runtime/Cargo.toml   # workspace and futures dependencies
crates/lato-runtime/tests/
  task_support/mod.rs            # deterministic runner/allocator/verifier harness
  task_admission.rs              # spawn, queue, fairness, promotion
  task_wait.rs                   # foreground/background and waiters
  task_cancel.rs                 # cancellation and teardown matrix
  task_active_message.rs         # lease and finalization races
  task_budget.rs                 # runtime budget and permission enforcement
  task_events.rs                 # event ordering and global invariants

docs/superpowers/reference/lato-upstream-sources.md
```

---

### Task 1: Freeze task identities, profiles, lifecycle, results, and errors

**Files:**
- Modify: `crates/lato-core/src/id.rs`
- Create: `crates/lato-core/src/task.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/tests/task_contract.rs`

**Interfaces:**
- Produces: `TaskId`, `AgentId`, `LeaseId`, `TaskOwner`, `TaskScope`, `TaskStatus`, `AgentProfile`, `TaskSpec`, `TaskNode`, `TaskUsage`, `TaskProgress`, `TaskResult`, `TaskError`, and `TaskTransitionError`.
- Consumes: existing `SessionId`, `TurnId`, `ToolCapability`, `ErrorCategory`, and `Retryability` from `lato-core`.

- [ ] **Step 1: Add failing ID, serialization, transition, and capability-narrowing tests**

Create `crates/lato-core/tests/task_contract.rs` with concrete tests like:

```rust
use lato_core::{
    AgentProfile, SessionId, TaskId, TaskMachine, TaskOwner, TaskStatus,
    ToolCapability, TurnId, VerificationPolicy, WorkspaceIntent,
};

#[test]
fn task_ids_reject_empty_values() {
    assert!(TaskId::parse(" ").is_err());
}

#[test]
fn task_owner_round_trips_with_explicit_tag() {
    let owner = TaskOwner::Interactive {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
    };
    let value = serde_json::to_value(&owner).unwrap();
    assert_eq!(value["type"], "interactive");
    assert_eq!(serde_json::from_value::<TaskOwner>(value).unwrap(), owner);
}

#[test]
fn terminal_task_cannot_transition_again() {
    let mut machine = TaskMachine::new(TaskStatus::Queued);
    machine.transition(TaskStatus::Preparing).unwrap();
    machine.transition(TaskStatus::Running).unwrap();
    machine.transition(TaskStatus::Verifying).unwrap();
    machine.transition(TaskStatus::Completed).unwrap();
    assert!(machine.transition(TaskStatus::Failed).is_err());
}

#[test]
fn profile_cannot_expand_parent_capabilities() {
    let profile = AgentProfile::worker();
    let parent = vec![ToolCapability::FileRead];
    assert!(profile
        .effective_capabilities(&parent, Some(&[ToolCapability::FileWrite]))
        .is_err());
}

#[test]
fn built_in_profiles_have_expected_workspace_intent() {
    assert_eq!(AgentProfile::explorer().workspace, WorkspaceIntent::SharedReadOnly);
    assert_eq!(AgentProfile::worker().workspace, WorkspaceIntent::IsolatedWorktree);
    assert_eq!(AgentProfile::reviewer().verification, VerificationPolicy::IndependentReview);
}
```

- [ ] **Step 2: Run the task contract test and verify it fails**

Run: `cargo test -p lato-core --test task_contract`

Expected: compilation fails because task IDs and task-domain types are not defined.

- [ ] **Step 3: Add task IDs and the complete domain model**

Extend the existing `string_id!` declarations in `id.rs`:

```rust
string_id!(TaskId);
string_id!(AgentId);
string_id!(LeaseId);
```

Implement `task.rs` with closed, serializable types. Use these exact public shapes as the cross-task contract:

```rust
use crate::{AgentError, ErrorCategory, Retryability, SessionId, ToolCapability, TurnId};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskOwner {
    Interactive { session_id: SessionId, turn_id: TurnId },
    Workflow { run_id: String, session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Preparing,
    Running,
    WaitingForChildren,
    WaitingForApproval,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut)
    }
    pub fn is_queued(self) -> bool { self == Self::Queued }
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::WaitingForChildren | Self::WaitingForApproval | Self::Verifying)
    }
    pub fn is_cancelled(self) -> bool { self == Self::Cancelled }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIntent { SharedReadOnly, SharedSerializedWrite, IsolatedWorktree, ExternalLease }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy { Accept, Schema, Programmatic, IndependentReview, HumanGate }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AgentProfile {
    pub name: String,
    pub instructions: String,
    pub capabilities: Vec<ToolCapability>,
    pub workspace: WorkspaceIntent,
    pub verification: VerificationPolicy,
    pub definition_background: bool,
}

impl AgentProfile {
    pub fn explorer() -> Self;
    pub fn worker() -> Self;
    pub fn reviewer() -> Self;
    pub fn effective_capabilities(
        &self,
        parent: &[ToolCapability],
        requested: Option<&[ToolCapability]>,
    ) -> Result<Vec<ToolCapability>, TaskError>;
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskScope { pub objective: String, pub context_refs: Vec<String> }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResultContract { pub schema: Option<serde_json::Value>, pub max_output_bytes: usize }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub tool_calls: u64,
    pub cost_micros: u64,
    pub retries: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TaskResult {
    pub success: bool,
    pub output: String,
    pub error: Option<TaskError>,
    pub usage: TaskUsage,
    pub duration_ms: u64,
    pub output_ref: Option<String>,
}
```

Add `TaskMachine::transition` with an explicit match table for every allowed edge in the design. Add `TaskError::agent_error()` mapping all codes to `ErrorCategory::Task` and an explicit `Retryability`; never infer retryability from the message.

- [ ] **Step 4: Export the task types and run the focused tests**

Add `mod task;` and `pub use task::*;` to `lato-core/src/lib.rs`, export the three new IDs, then run:

Run: `cargo test -p lato-core --test task_contract`

Expected: all task contract tests pass.

- [ ] **Step 5: Commit the domain contract**

```bash
git add crates/lato-core/src/id.rs crates/lato-core/src/task.rs crates/lato-core/src/lib.rs crates/lato-core/tests/task_contract.rs
git commit -m "feat(core): add task coordination domain model"
```

### Task 2: Implement hierarchical multi-dimensional budget arithmetic

**Files:**
- Create: `crates/lato-core/src/budget.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/tests/budget_contract.rs`

**Interfaces:**
- Consumes: `TaskId` and `TaskUsage` from Task 1.
- Produces: `BudgetLimits`, `BudgetAmount`, `BudgetAccount`, `BudgetReservation`, `BudgetDimension`, and `BudgetError`.

- [ ] **Step 1: Write failing atomic reservation and settlement tests**

Create `budget_contract.rs` covering exact accounting behavior:

```rust
use lato_core::{BudgetAccount, BudgetAmount, BudgetDimension, BudgetLimits, TaskId};

fn amount(tokens: u64, tools: u64, children: u64) -> BudgetAmount {
    BudgetAmount {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: tokens,
        tool_calls: tools,
        cost_micros: 0,
        wall_time_ms: 0,
        retries: 0,
        child_tasks: children,
        worktrees: 0,
    }
}

#[test]
fn failed_multi_dimension_reservation_is_atomic() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 2, 1)));
    let error = account.reserve(TaskId::from("child"), amount(50, 3, 1)).unwrap_err();
    assert_eq!(error.dimension(), Some(BudgetDimension::ToolCalls));
    assert_eq!(account.reserved(), BudgetAmount::ZERO);
}

#[test]
fn settlement_returns_unused_reservation() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account.reserve(TaskId::from("child"), amount(80, 4, 1)).unwrap();
    account.settle(reservation, amount(30, 2, 1)).unwrap();
    assert_eq!(account.spent(), amount(30, 2, 1));
    assert_eq!(account.remaining().total_tokens, Some(70));
    assert_eq!(account.remaining().tool_calls, Some(8));
    assert_eq!(account.remaining().child_tasks, Some(1));
}

#[test]
fn duplicate_settlement_cannot_charge_twice() {
    let mut account = BudgetAccount::new(BudgetLimits::limited(amount(100, 10, 2)));
    let reservation = account.reserve(TaskId::from("child"), amount(50, 3, 1)).unwrap();
    account.settle(reservation.clone(), amount(20, 1, 1)).unwrap();
    assert!(account.settle(reservation, amount(20, 1, 1)).is_err());
    assert_eq!(account.spent(), amount(20, 1, 1));
}
```

Also test release, mixed limited/unlimited dimensions, checked overflow, usage regression, actual usage above reservation, nested child roll-up, reservation ID uniqueness, and rejection of a reservation presented to a different account.

- [ ] **Step 2: Run the budget tests and verify they fail**

Run: `cargo test -p lato-core --test budget_contract`

Expected: compilation fails because `lato_core::BudgetAccount` does not exist.

- [ ] **Step 3: Implement checked budget values and accounts**

Use one macro or explicit checked methods so every field participates identically:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BudgetAmount {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub tool_calls: u64,
    pub cost_micros: u64,
    pub wall_time_ms: u64,
    pub retries: u64,
    pub child_tasks: u64,
    pub worktrees: u64,
}

impl BudgetAmount {
    pub const ZERO: Self = Self { input_tokens: 0, output_tokens: 0, total_tokens: 0, tool_calls: 0, cost_micros: 0, wall_time_ms: 0, retries: 0, child_tasks: 0, worktrees: 0 };
    pub fn checked_add(self, rhs: Self) -> Result<Self, BudgetError>;
    pub fn checked_sub(self, rhs: Self) -> Result<Self, BudgetError>;
    pub fn first_excess(self, limit: Self) -> Option<BudgetDimension>;
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BudgetLimits {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub cost_micros: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub retries: Option<u64>,
    pub child_tasks: Option<u64>,
    pub worktrees: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetReservation {
    pub task_id: TaskId,
    pub amount: BudgetAmount,
    account_id: u64,
    generation: u64,
}

pub struct BudgetAccount {
    account_id: u64,
    limits: BudgetLimits,
    spent: BudgetAmount,
    reserved: BudgetAmount,
    next_generation: u64,
    open: std::collections::HashMap<(TaskId, u64), BudgetAmount>,
}

impl BudgetAccount {
    pub fn reserve(&mut self, task_id: TaskId, amount: BudgetAmount) -> Result<BudgetReservation, BudgetError>;
    pub fn release(&mut self, reservation: BudgetReservation) -> Result<(), BudgetError>;
    pub fn settle(&mut self, reservation: BudgetReservation, actual: BudgetAmount) -> Result<(), BudgetError>;
    pub fn apply_cumulative_usage(&mut self, previous: BudgetAmount, next: BudgetAmount) -> Result<BudgetAmount, BudgetError>;
}
```

`BudgetLimits::unlimited()` sets every field to `None`, while
`BudgetLimits::limited(amount)` sets every field to `Some`. Callers may mix the
fields to limit only selected dimensions. `BudgetAccount::remaining()` returns
the same per-dimension `BudgetLimits` shape. Assign every account a private,
process-unique `account_id`; both account ID and generation must match before a
reservation can be released or settled. `BudgetError::dimension()` returns
`None` for reservation identity and generation faults. Limit violations,
arithmetic overflow/underflow, and cumulative-usage regression retain
`Some(dimension)` because the affected resource dimension is known.

Perform the full candidate calculation before mutating any field. Use `checked_add`/`checked_sub`; never map arithmetic overflow to unlimited.

- [ ] **Step 4: Run core tests**

Run: `cargo test -p lato-core --test budget_contract && cargo test -p lato-core`

Expected: all `lato-core` tests pass.

- [ ] **Step 5: Commit the budget ledger**

```bash
git add crates/lato-core/src/budget.rs crates/lato-core/src/lib.rs crates/lato-core/tests/budget_contract.rs
git commit -m "feat(core): add hierarchical task budget ledger"
```

### Task 3: Add workspace lease and allocator contracts

**Files:**
- Modify: `crates/lato-workspace/Cargo.toml`
- Create: `crates/lato-workspace/src/task.rs`
- Modify: `crates/lato-workspace/src/lib.rs`
- Create: `crates/lato-workspace/tests/task_workspace.rs`

**Interfaces:**
- Consumes: `LeaseId`, `TaskId`, `TaskError`, and `WorkspaceIntent` from `lato-core`.
- Produces: `WorkspaceLease`, `WorkspaceMode`, `WorkspaceRequest`, and async `WorkspaceAllocator`.

- [ ] **Step 1: Write failing allocator contract tests**

```rust
use lato_core::{TaskId, WorkspaceIntent};
use lato_workspace::{MemoryWorkspaceAllocator, WorkspaceAllocator, WorkspaceRequest};

#[tokio::test]
async fn isolated_write_tasks_receive_distinct_leases() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let first = allocator.allocate(WorkspaceRequest::new(TaskId::from("a"), WorkspaceIntent::IsolatedWorktree)).await.unwrap();
    let second = allocator.allocate(WorkspaceRequest::new(TaskId::from("b"), WorkspaceIntent::IsolatedWorktree)).await.unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.root, second.root);
}

#[tokio::test]
async fn release_is_idempotent() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let lease = allocator.allocate(WorkspaceRequest::new(TaskId::from("a"), WorkspaceIntent::SharedReadOnly)).await.unwrap();
    allocator.release(&lease).await.unwrap();
    allocator.release(&lease).await.unwrap();
    assert_eq!(allocator.live_count().await, 0);
}
```

Add tests proving shared read-only roots match, shared serialized-write leases carry one resource key, and allocation failure creates no live lease.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test -p lato-workspace --test task_workspace`

Expected: compilation fails because the task workspace contract does not exist.

- [ ] **Step 3: Implement the allocator boundary and deterministic memory allocator**

Add `async-trait = "0.1"` if not already present, and implement:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode { SharedReadOnly, SharedSerializedWrite, IsolatedWorktree, ExternalLease }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct WorkspaceLease {
    pub id: LeaseId,
    pub task_id: TaskId,
    pub mode: WorkspaceMode,
    pub root: std::path::PathBuf,
    pub resource_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceRequest { pub task_id: TaskId, pub intent: WorkspaceIntent }

#[async_trait::async_trait]
pub trait WorkspaceAllocator: Send + Sync + 'static {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError>;
    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError>;
}
```

The memory allocator must generate deterministic unique lease IDs with an atomic counter and track live leases under a Tokio mutex. It must not create directories or invoke Git.

- [ ] **Step 4: Run workspace tests**

Run: `cargo test -p lato-workspace --test task_workspace && cargo test -p lato-workspace`

Expected: all workspace tests pass.

- [ ] **Step 5: Commit workspace contracts**

```bash
git add crates/lato-workspace/Cargo.toml crates/lato-workspace/src/task.rs crates/lato-workspace/src/lib.rs crates/lato-workspace/tests/task_workspace.rs
git commit -m "feat(workspace): add task workspace lease contracts"
```

### Task 4: Establish runtime protocol, runner seam, event stream, and actor skeleton

**Files:**
- Modify: `crates/lato-runtime/Cargo.toml`
- Create: `crates/lato-runtime/src/task/mod.rs`
- Create: `crates/lato-runtime/src/task/protocol.rs`
- Create: `crates/lato-runtime/src/task/runner.rs`
- Create: `crates/lato-runtime/src/task/state.rs`
- Create: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/lib.rs`
- Create: `crates/lato-runtime/tests/task_support/mod.rs`
- Create: `crates/lato-runtime/tests/task_events.rs`

**Interfaces:**
- Consumes: task and budget types from Tasks 1–2; workspace allocator types from Task 3.
- Produces: `TaskCoordinator`, `TaskHandle`, `ScopedTaskHandle`, `CoordinatorConfig`, `TaskRunner`, `TaskControl`, `TaskReporter`, `TaskEventEnvelope`, `TaskEventPayload`, `TaskSnapshot`, and `TaskEventSink`.

- [ ] **Step 1: Write failing root-registration and event-order tests**

Create a reusable `ControlledTaskRunner` in `tests/task_support/mod.rs`, then add:

```rust
mod task_support;

use lato_core::{AgentProfile, BudgetLimits, SessionId, TaskId, ToolCapability, TurnId};
use lato_runtime::{CoordinatorConfig, TaskEventPayload, TaskRootRequest, spawn_task_coordinator};
use task_support::Harness;

#[tokio::test]
async fn root_registration_is_actor_owned_and_evented() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;
    let event = harness.next_event().await;
    assert_eq!(event.sequence, 1);
    assert_eq!(event.task_id, TaskId::from("root-1"));
    assert!(matches!(event.payload, TaskEventPayload::RootRegistered));
}

#[tokio::test]
async fn duplicate_root_is_rejected_without_second_event() {
    let harness = Harness::new(CoordinatorConfig::default()).await;
    harness.register_root("root-1", "session-1", "turn-1").await;
    assert!(harness.try_register_root("root-1", "session-1", "turn-1").await.is_err());
    assert_eq!(harness.registry_counts().await.roots, 1);
}
```

- [ ] **Step 2: Verify the runtime tests fail**

Run: `cargo test -p lato-runtime --test task_events`

Expected: compilation fails because the task runtime module is absent.

- [ ] **Step 3: Define the exact runtime protocol and runner seam**

Add dependencies on `lato-workspace` and `futures-util`. Define:

```rust
pub struct CoordinatorConfig {
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub active_message_capacity: usize,
    pub active_messages_per_task: usize,
    pub max_global_running: usize,
    pub max_running_per_root: usize,
    pub max_queue: usize,
    pub max_depth: u32,
    pub max_children_per_parent: usize,
    pub max_total_tasks: usize,
    pub max_completed: usize,
    pub foreground_budget: std::time::Duration,
    pub waiter_timeout_cap: std::time::Duration,
    pub cancel_grace: std::time::Duration,
    pub teardown_drain_timeout: std::time::Duration,
    pub queued_reap_interval: std::time::Duration,
    pub admission_behavior: LimitBehavior,
}

#[derive(Clone)]
pub struct TaskHandle { command_tx: tokio::sync::mpsc::Sender<TaskCommand>, event_tx: tokio::sync::broadcast::Sender<TaskEventEnvelope> }

#[derive(Clone)]
pub struct ScopedTaskHandle { root_id: TaskId, parent_id: TaskId, inner: TaskHandle }

#[async_trait::async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    type Control: TaskChildControl;
    async fn run(&self, request: TaskRunRequest, reporter: TaskReporter<Self::Control>) -> TaskRunOutput;
    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError>;
    fn on_completed(&self, completion: TaskCompletion);
}

pub trait TaskChildControl: Send + Sync + 'static {
    fn progress(&self) -> TaskProgress;
    fn send_active_message(&self, delivery: ActiveMessageDelivery) -> futures_util::future::BoxFuture<'static, ActiveMessageAdmission>;
    fn cancel(&self);
}
```

`TaskReporter::started` must use an acknowledgement. `TaskEventSink` receives cloned events after the state commit. The default sink is no-op; the test sink stores events.

- [ ] **Step 4: Implement the actor skeleton and one transition path**

Create bounded command and broadcast channels. `TaskCoordinator::run` uses `tokio::select!` over commands and internal runner events. Implement only root registration, inspection, registry counts, and actor shutdown in this task. Route root mutation through:

```rust
fn commit_transition(&mut self, task_id: TaskId, payload: TaskEventPayload) -> TaskEventEnvelope {
    self.sequence = self.sequence.checked_add(1).expect("task event sequence overflow");
    let envelope = TaskEventEnvelope::new(self.sequence, &self.state, task_id, payload);
    let _ = self.event_tx.send(envelope.clone());
    self.event_sink.on_event(envelope.clone());
    envelope
}
```

Keep mutation immediately before this call and do not expose mutable state outside `CoordinatorState`.
The actor-owned runtime record wraps the provider-neutral node and concrete
runtime resources without creating a crate cycle:

```rust
struct RuntimeTaskRecord {
    node: TaskNode,
    budget: BudgetAccount,
    workspace_lease: Option<WorkspaceLease>,
    reservation: Option<BudgetReservation>,
}
```

- [ ] **Step 5: Run focused and crate tests**

Run: `cargo test -p lato-runtime --test task_events && cargo test -p lato-runtime`

Expected: all tests pass; existing session runtime tests remain green.

- [ ] **Step 6: Commit the runtime foundation**

```bash
git add crates/lato-runtime/Cargo.toml crates/lato-runtime/src/task crates/lato-runtime/src/lib.rs crates/lato-runtime/tests/task_support crates/lato-runtime/tests/task_events.rs
git commit -m "feat(runtime): add task coordinator actor foundation"
```

### Task 5: Implement spawn validation, admission queue, preparation, and promotion

**Files:**
- Create: `crates/lato-runtime/src/task/admission.rs`
- Create: `crates/lato-runtime/src/task/queue.rs`
- Create: `crates/lato-runtime/src/task/spawn.rs`
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/task/state.rs`
- Modify: `crates/lato-runtime/src/task/protocol.rs`
- Create: `crates/lato-runtime/tests/task_admission.rs`

**Interfaces:**
- Consumes: actor, runner, root state, profiles, budgets, and workspace allocator from Tasks 1–4.
- Produces: `SpawnTaskRequest`, `SpawnMode`, `SpawnDisposition`, `Admission`, `SpawnQueue`, and preparing/running promotion.

- [ ] **Step 1: Add failing admission and fairness tests**

Cover immediate start, duplicate IDs, depth, child count, total count, queue capacity, queue mode, reject mode, cancelled queued work, promotion cancellation, and this cross-root case:

```rust
#[tokio::test]
async fn saturated_root_does_not_block_another_root() {
    let harness = Harness::with_limits(2, 1).await;
    harness.register_root("root-a", "session-a", "turn-a").await;
    harness.register_root("root-b", "session-b", "turn-b").await;
    let a1 = harness.spawn_blocked("root-a", "a1").await;
    let a2 = harness.spawn("root-a", "a2").await;
    assert!(a2.is_queued());
    let b1 = harness.spawn_blocked("root-b", "b1").await;
    assert!(harness.inspect("b1").await.unwrap().status.is_running());
    harness.finish(a1).await;
    harness.finish(b1).await;
}
```

- [ ] **Step 2: Run the admission test and verify failure**

Run: `cargo test -p lato-runtime --test task_admission`

Expected: compilation fails because spawn and admission types are not implemented.

- [ ] **Step 3: Implement admission and FIFO queue types**

Use these decisions:

```rust
pub enum LimitBehavior { Queue, Reject }
pub enum AdmissionDecision { Start, Enqueue, Reject(TaskError) }

pub struct SpawnQueue { entries: std::collections::VecDeque<QueuedTask> }

impl SpawnQueue {
    pub fn push_back(&mut self, task: QueuedTask) -> Result<(), TaskError>;
    pub fn drain_startable(&mut self, capacity: impl Fn(&QueuedTask) -> bool) -> Vec<QueuedTask>;
    pub fn remove_matching(&mut self, predicate: impl FnMut(&QueuedTask) -> bool) -> Vec<QueuedTask>;
}
```

`drain_startable` must preserve relative order among kept entries and among started entries while skipping saturated roots.

- [ ] **Step 4: Implement spawn validation and promotion acknowledgement**

`SpawnTaskRequest` must contain task ID, objective/scope, profile, requested capabilities, budget envelope, result contract, mode, parent/root-bound identity, and cancellation token. Validate in the exact order from the spec, reserve budget before node visibility, and allocate workspace only after `Preparing` begins.

The runner reports:

```rust
pub async fn started(&self, started: StartedTask<C>) -> bool
```

If the actor no longer has a live preparing record, return `false`; the runner must cancel its control and the coordinator must release any allocated lease and open reservation.

- [ ] **Step 5: Run admission tests and all runtime tests**

Run: `cargo test -p lato-runtime --test task_admission && cargo test -p lato-runtime`

Expected: all tests pass.

- [ ] **Step 6: Commit spawn and admission**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_admission.rs
git commit -m "feat(runtime): add bounded task spawn admission"
```

### Task 6: Implement foreground handoff, querying, waiters, progress, and retention

**Files:**
- Create: `crates/lato-runtime/src/task/query.rs`
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/task/state.rs`
- Modify: `crates/lato-runtime/src/task/protocol.rs`
- Modify: `crates/lato-runtime/src/task/runner.rs`
- Create: `crates/lato-runtime/tests/task_wait.rs`

**Interfaces:**
- Consumes: queued/preparing/running registries and task handles from Tasks 4–5.
- Produces: `TaskSnapshot`, `TaskInspection`, `WaitOutcome`, `CompletionDisposition`, progress polling, foreground deadlines, and bounded completed retention.

- [ ] **Step 1: Write failing foreground and waiter tests**

Use paused time to prove the deadline starts at enqueue:

```rust
#[tokio::test(start_paused = true)]
async fn queued_time_consumes_the_foreground_budget() {
    let harness = Harness::with_foreground_budget(std::time::Duration::from_secs(45)).await;
    harness.register_default_root().await;
    let blocker = harness.spawn_blocked("root", "blocker").await;
    let pending = harness.spawn_foreground("root", "queued");
    tokio::time::advance(std::time::Duration::from_secs(45)).await;
    let disposition = pending.await.unwrap();
    assert!(disposition.backgrounded);
    assert!(harness.inspect("queued").await.unwrap().status.is_queued());
    harness.finish(blocker).await;
}
```

Also test await-to-completion, explicit background, caller drop, multiple waiters, independent timeouts, terminal immediate response, progress fields, output references, and FIFO eviction.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test -p lato-runtime --test task_wait`

Expected: compilation fails because wait/query/disposition APIs are incomplete.

- [ ] **Step 3: Implement query and waiter registries**

Use concrete types:

```rust
pub struct TaskInspection { pub snapshot: TaskSnapshot, pub owner: TaskOwner, pub parent_id: Option<TaskId>, pub root_id: TaskId }
pub enum WaitOutcome { Finished(TaskSnapshot), TimedOut(TaskSnapshot), NotFoundOrNotOwned }
pub struct CompletionDisposition {
    pub foreground_delivered: bool,
    pub waiter_delivered: bool,
    pub backgrounded: bool,
    pub explicitly_killed: bool,
    pub should_surface: bool,
}
```

Store `HashMap<TaskId, Vec<BlockingWaiter>>`; remove dropped or expired waiters without affecting peers. A live waiter suppresses duplicate asynchronous surfacing, but a timed-out or dropped waiter does not.

- [ ] **Step 4: Implement deadlines, progress, completion, and retention**

Drive the next foreground/waiter/queue deadline from one `next_deadline()` calculation in the actor. Move terminal results to an insertion-ordered completed cache capped by `config.max_completed`. Eviction removes the oldest ID and publishes `CompletedEvicted`.

If a runner returns an `output_ref`, retain only the capped inline output and load the full output through `TaskRunner::load_persisted_output` during inspection.

- [ ] **Step 5: Run wait and runtime tests**

Run: `cargo test -p lato-runtime --test task_wait && cargo test -p lato-runtime`

Expected: all tests pass.

- [ ] **Step 6: Commit waiting and retention**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_wait.rs
git commit -m "feat(runtime): add task waiting and background handoff"
```

### Task 7: Implement scoped cancellation, descendant shutdown, and teardown drain

**Files:**
- Create: `crates/lato-runtime/src/task/cancel.rs`
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/task/state.rs`
- Modify: `crates/lato-runtime/src/task/protocol.rs`
- Create: `crates/lato-runtime/tests/task_cancel.rs`

**Interfaces:**
- Consumes: task tree lineage, runtime registries, waiters, queue, controls, and deadlines.
- Produces: `CancelTarget`, `CancelOutcome`, admission latches, root/workflow drain waiters, cancel grace abort.

- [ ] **Step 1: Add failing cancellation and teardown tests**

Include queued, preparing, running, nested descendant, turn, root, workflow, foreign root, late spawn, multiple drain waiter, actor drop, and non-cooperative runner cases. The central tree test is:

```rust
#[tokio::test]
async fn cancelling_parent_cancels_all_descendants_only() {
    let harness = Harness::new_default().await;
    harness.register_default_root().await;
    harness.spawn_tree(&[("parent", "root"), ("child", "parent"), ("grandchild", "child"), ("sibling", "root")]).await;
    harness.cancel_task("parent").await.unwrap();
    for id in ["parent", "child", "grandchild"] {
        assert!(harness.wait_terminal(id).await.status.is_cancelled());
    }
    assert!(harness.inspect("sibling").await.unwrap().status.is_running());
}
```

- [ ] **Step 2: Verify the cancellation tests fail**

Run: `cargo test -p lato-runtime --test task_cancel`

Expected: compilation fails because scoped cancellation is absent.

- [ ] **Step 3: Implement scope resolution and admission latches**

Define:

```rust
pub enum CancelTarget { Task(TaskId), Turn { session_id: SessionId, turn_id: TurnId }, Root(TaskId), Workflow { run_id: String, root_id: Option<TaskId> } }
pub struct CancelOutcome { pub matched: usize, pub newly_requested: usize, pub already_terminal: usize }
```

Resolve descendants from the authoritative tree before mutating registries. Set the root admission latch before signalling cancellation. `open_spawn_admission` must not reopen a root while a teardown drain is active.

- [ ] **Step 4: Implement cooperative cancellation, grace abort, and drains**

Queued tasks terminalize immediately without starting. Preparing/running tasks receive both token cancellation and `control.cancel()`. Track a cancel deadline per task; at grace expiry abort the coordinator-owned run future and commit one terminal result.

`teardown_root_and_drain` waits until the selected root has no queued, preparing, running, finalizing, or waiting descendants. At its backstop, resolve teardown callers and reopen only that root's admission; do not unblock foreign state or synthesize success for unfinished tasks.

- [ ] **Step 5: Run cancellation and runtime tests**

Run: `cargo test -p lato-runtime --test task_cancel && cargo test -p lato-runtime`

Expected: all tests pass.

- [ ] **Step 6: Commit cancellation and teardown**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_cancel.rs
git commit -m "feat(runtime): add hierarchical task cancellation"
```

### Task 8: Implement bounded active-message admission and terminalization

**Files:**
- Create: `crates/lato-runtime/src/task/active_message.rs`
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/task/protocol.rs`
- Modify: `crates/lato-runtime/src/task/runner.rs`
- Create: `crates/lato-runtime/tests/task_active_message.rs`

**Interfaces:**
- Consumes: active task control, ownership, actor deadlines, and finalization from prior tasks.
- Produces: `ActiveMessageOperation`, `ActiveMessageRequest`, `ActiveMessage`, `ActiveMessageDelivery`, `ActiveMessageAdmission`, `ActiveMessageOutcome`, and admission leases.

- [ ] **Step 1: Write failing message validation and race tests**

Port the semantic cases from Grok Build without copying its product names. Include empty input, UTF-8 boundary, foreign ownership, queued/finalizing/terminal rejection, global and per-task saturation, queue/steer, timeout, channel close, committed/revoked/claimed leases, stale generation, independent children, and terminalization waiting.

```rust
#[tokio::test]
async fn admitted_requires_proven_synchronous_commit() {
    let lease = ActiveMessageAdmissionLease::new_for_test();
    assert!(lease.commit_admission(|| ()).is_some());
    assert!(lease.settle(ActiveMessageAdmission::Admitted));
}

#[tokio::test]
async fn claimed_but_unsettled_admission_is_uncertain() {
    let harness = Harness::with_message_runner(MessageBehavior::ClaimWithoutSettle).await;
    harness.register_default_root().await;
    harness.spawn_running("root", "child").await;
    let outcome = harness.send_message("child", "hello", ActiveMessageOperation::Queue).await;
    assert_eq!(outcome, ActiveMessageOutcome::AdmissionUncertain);
}
```

- [ ] **Step 2: Verify active-message tests fail**

Run: `cargo test -p lato-runtime --test task_active_message`

Expected: compilation fails because the active-message protocol is absent.

- [ ] **Step 3: Implement the atomic admission lease and bounded ingress**

Implement exact states `Open`, `Claimed`, `Committed`, and `Revoked` with `AtomicU8`. The only insertion API is synchronous:

```rust
pub fn commit_admission<T>(&self, insert: impl FnOnce() -> T) -> Option<T>
```

Use a semaphore before enqueueing active-message work so an unbounded sender cannot bypass the configured in-flight cap. Enforce `MAX_ACTIVE_MESSAGE_BYTES` on UTF-8 bytes and a per-task selected-admission cap.

- [ ] **Step 4: Implement generation-aware delivery and bounded finalization**

Assign a generation to each promoted child. Attach it to every message future and ignore stale completions. On runner completion, stop new admission, wait for selected admissions to settle until the finalization timeout, and commit a clean task result only if every selected admission is proven committed or revoked. Otherwise produce `task.active_message_uncertain`.

- [ ] **Step 5: Run active-message and runtime tests**

Run: `cargo test -p lato-runtime --test task_active_message && cargo test -p lato-runtime`

Expected: all tests pass.

- [ ] **Step 6: Commit active messages**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_active_message.rs
git commit -m "feat(runtime): add bounded active task messages"
```

### Task 9: Integrate usage budgets, permission enforcement, workspace cleanup, and verification

**Files:**
- Create: `crates/lato-runtime/src/task/verification.rs`
- Modify: `crates/lato-runtime/src/task/spawn.rs`
- Modify: `crates/lato-runtime/src/task/coordinator.rs`
- Modify: `crates/lato-runtime/src/task/state.rs`
- Modify: `crates/lato-runtime/src/task/protocol.rs`
- Modify: `crates/lato-runtime/src/task/runner.rs`
- Create: `crates/lato-runtime/tests/task_budget.rs`
- Extend: `crates/lato-runtime/tests/task_events.rs`

**Interfaces:**
- Consumes: `BudgetAccount`, effective capabilities, workspace leases, runner usage reports, and verification policy.
- Produces: `TaskVerifier`, `VerificationRequest`, `VerificationOutcome`, cumulative usage enforcement, lease cleanup, and resume commands for external verification.

- [ ] **Step 1: Write failing runtime budget and verification tests**

Test atomic spawn reservation, permission expansion rejection before visibility, cumulative usage deltas, decreasing/late reports, runtime exhaustion cancellation, nested roll-up, workspace failure/release, Accept/Schema/Programmatic verification, and external waiting states.

```rust
#[tokio::test]
async fn budget_exhaustion_cancels_and_settles_once() {
    let harness = Harness::with_root_token_budget(100).await;
    harness.register_default_root().await;
    harness.spawn_running_with_budget("root", "child", 80).await;
    harness.report_total_tokens("child", 81).await;
    let terminal = harness.wait_terminal("child").await;
    assert_eq!(terminal.error.unwrap().code, "task.budget_exceeded.total_tokens");
    assert_eq!(harness.root_budget().await.spent.total_tokens, 80);
}

#[tokio::test]
async fn programmatic_verification_precedes_completed() {
    let harness = Harness::with_verifier(VerifierBehavior::Pass).await;
    harness.register_default_root().await;
    harness.finish_successfully("root", "child", "ok").await;
    let events = harness.events_for("child").await;
    assert!(matches!(events[events.len() - 2].payload, TaskEventPayload::VerificationStarted { .. }));
    assert!(matches!(events.last().unwrap().payload, TaskEventPayload::Completed { .. }));
}
```

- [ ] **Step 2: Verify the integration tests fail**

Run: `cargo test -p lato-runtime --test task_budget`

Expected: tests fail because live usage, verification, and complete cleanup are not connected.

- [ ] **Step 3: Implement cumulative usage and hierarchical settlement**

`TaskReporter::usage(next)` sends cumulative values. The actor checks monotonicity, calculates the delta, updates the child's account, and rolls reserved/spent values through ancestors. On exhaustion it closes descendant spawn admission and follows the normal cancellation path. Ignore reports for an older generation or terminal task and emit no second budget event.

- [ ] **Step 4: Enforce permissions and workspace lifecycle at spawn/terminal boundaries**

Compute effective capabilities before budget reservation. Convert `WorkspaceIntent` to `WorkspaceRequest` only after admission to `Preparing`. Store the lease before runner promotion. Every terminalization path calls one idempotent release helper; release failure is recorded in the terminal inspection without replacing a previously committed runner/verification failure.

- [ ] **Step 5: Implement the verifier seam and waiting resumes**

```rust
#[async_trait::async_trait]
pub trait TaskVerifier: Send + Sync + 'static {
    async fn verify(&self, request: VerificationRequest) -> VerificationOutcome;
}

pub enum VerificationOutcome {
    Passed,
    Failed(TaskError),
    WaitingForChild { reviewer_task_id: TaskId },
    WaitingForApproval { approval_id: String },
}
```

Implement `AcceptVerifier`, JSON Schema shape validation for `Schema`, and an injected callback for `Programmatic`. Add typed `resume_verification` commands that require the matching reviewer task or approval ID. Do not start a reviewer or prompt a user in Phase 5A.

For Phase 5A, `Schema` supports the explicitly tested JSON subset: top-level
`type`, `required`, and `properties` entries whose property types are
`string`, `number`, `integer`, `boolean`, `object`, or `array`. Reject unsupported
schema keywords with `task.verification.schema_unsupported`; do not silently
pretend to implement the full JSON Schema standard and do not add a new schema
dependency.

- [ ] **Step 6: Run budget, event, and runtime tests**

Run: `cargo test -p lato-runtime --test task_budget && cargo test -p lato-runtime --test task_events && cargo test -p lato-runtime`

Expected: all tests pass.

- [ ] **Step 7: Commit budget and verification integration**

```bash
git add crates/lato-runtime/src/task crates/lato-runtime/tests/task_budget.rs crates/lato-runtime/tests/task_events.rs
git commit -m "feat(runtime): enforce task budgets and verification"
```

### Task 10: Complete invariant coverage, attribution, workspace gates, and local deployment

**Files:**
- Modify: `crates/lato-runtime/tests/task_events.rs`
- Modify: `crates/lato-runtime/tests/task_admission.rs`
- Modify: `crates/lato-runtime/tests/task_wait.rs`
- Modify: `crates/lato-runtime/tests/task_cancel.rs`
- Modify: `crates/lato-runtime/tests/task_active_message.rs`
- Modify: `crates/lato-runtime/tests/task_budget.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: the complete Phase 5A kernel.
- Produces: fault/race regression coverage, source attribution, lint-clean workspace, and installed `lato` binary with unchanged product behavior.

- [ ] **Step 1: Add a deterministic invariant audit helper and randomized command-sequence test**

Add a test-only coordinator inspection that returns counts and invariant failures without exposing mutable state. Run a deterministic sequence of spawn, queue, message, usage, wait, cancel, finish, and eviction operations after which every step asserts:

```rust
let audit = harness.audit().await;
assert!(audit.failures.is_empty(), "{:#?}", audit.failures);
assert_eq!(audit.live_runners, audit.preparing + audit.running + audit.finalizing);
assert_eq!(audit.terminal_with_open_reservations, 0);
assert_eq!(audit.terminal_with_live_workspace_leases, 0);
assert_eq!(audit.cycle_count, 0);
```

Use a fixed local xorshift seed in the test; do not add a production randomness dependency.

- [ ] **Step 2: Run each Phase 5A integration test independently**

Run:

```bash
cargo test -p lato-runtime --test task_admission
cargo test -p lato-runtime --test task_wait
cargo test -p lato-runtime --test task_cancel
cargo test -p lato-runtime --test task_active_message
cargo test -p lato-runtime --test task_budget
cargo test -p lato-runtime --test task_events
```

Expected: every command exits 0 with all tests passing and no test exceeding its bounded timeout.

- [ ] **Step 3: Record upstream derivation**

Append concrete rows to `docs/superpowers/reference/lato-upstream-sources.md` for:

```text
crates/lato-runtime/src/task/{admission,queue,coordinator,spawn,query,cancel}.rs
  <- Grok Build task/{admission,coordinator,coordinator_state,coordinator/queue,coordinator/spawn}.rs

crates/lato-runtime/src/task/active_message.rs
  <- Grok Build task/{active_message,coordinator/active_message}.rs

crates/lato-core/src/budget.rs
  <- Grok Build xai-workflow/src/{engine,host}.rs
```

Each substantially derived Rust file must start with the exact pinned commit, source path, Apache-2.0 license, and a one-line statement of Lato's changes.

- [ ] **Step 4: Run formatting and focused crate gates**

Run:

```bash
cargo fmt --check
cargo test -p lato-core
cargo test -p lato-workspace
cargo test -p lato-runtime
```

Expected: formatting is clean and all focused tests pass.

- [ ] **Step 5: Run workspace lint and regression tests**

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: no warnings and all existing plus Phase 5A tests pass. Confirm that no test or source diff adds a model-visible task tool or changes CLI/TUI/ACP output.

- [ ] **Step 6: Install the finished repository locally**

Run: `cargo install --path .`

Expected: Cargo installs `lato v0.1.0-beta.2` successfully and replaces the local executable if it already exists.

- [ ] **Step 7: Smoke-test unchanged product behavior**

Run:

```bash
lato --help
cargo test --test cli_headless --test tui_cli --test sessions_cli
```

Expected: `lato --help` exits 0; the existing CLI integration tests pass without a new public subagent command or tool surface.

- [ ] **Step 8: Commit final tests and attribution**

```bash
git add crates/lato-runtime/tests docs/superpowers/reference/lato-upstream-sources.md
git commit -m "test: complete phase 5a coordination coverage"
```
