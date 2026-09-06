# Lato Phase 5A: Task Coordination Kernel Design

Date: 2026-09-06

Status: approved for implementation planning

## 1. Purpose

Phase 5A introduces the executable, process-local coordination kernel for Lato's
multi-agent runtime. It deliberately stops before the complete model-facing
`spawn_subagent` product experience. The phase establishes the lifecycle,
admission, budget, cancellation, messaging, workspace, verification, and
inspection contracts that later phases can connect to real child
`RuntimeSession` instances, Git worktrees, tools, CLI, TUI, ACP, and workflow
recovery.

The design follows the coordination semantics of Grok Build at commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`, especially its task coordinator,
bounded admission, foreground-to-background handoff, active-message admission,
scope-aware cancellation, and teardown behavior. Lato retains a true task tree
instead of copying Grok Build's nested-child reparenting. This preserves Lato's
domain model while keeping the useful operational behavior.

AgentField remains a Phase 7 adapter. Phase 5A is a deterministic local runtime
facility and does not require a provider key, AgentField control plane, or
reasoner graph.

## 2. Goals

- Provide a single actor that owns every task lifecycle transition.
- Model a real task tree with stable root, parent, session, turn, and optional
  workflow lineage.
- Support internal spawn, send, inspect, list, wait, cancel, root shutdown, and
  admission reopen operations.
- Enforce bounded ingress, concurrency, queueing, depth, child count, retained
  completions, messages, time, and resource budgets.
- Make permissions, profiles, budgets, and workspace modes monotonically
  narrower down the task tree.
- Support foreground waiting, automatic background handoff, completion
  disposition, and multiple waiters without coupling them to UI behavior.
- Resolve cancellation, startup, message-admission, finalization, runner panic,
  caller-drop, and actor-drop races deterministically.
- Expose a runner boundary that can be exercised with fakes now and connected
  to real child sessions later without redesigning the coordinator.
- Produce stable structured snapshots, results, errors, and events.

## 3. Non-goals

Phase 5A does not:

- register, replace, or expand the model-visible `spawn_subagent` tool;
- add `task_output`, `kill_task`, or active-message tool schemas;
- change CLI, TUI, headless, or ACP behavior;
- start a real child `RuntimeSession`;
- create, merge, retain, or clean up real Git worktrees;
- implement persona resolution, model selection, transcript resume, or context
  forking;
- render completion reminders or auto-wake prompts;
- implement the workflow scripting engine or AgentField adapter;
- persist task events to the session journal or recover live tasks after a
  process crash;
- automatically merge, commit, push, deploy, or publish child work.

The existing compatibility implementation in `lato-tools` that creates a
worktree for `spawn_subagent` is not the Phase 5A coordinator and will not be
wired into it during this phase.

## 4. Architecture and crate boundaries

### 4.1 `lato-core`

`lato-core` owns provider-neutral domain values and pure invariants:

- `TaskId`, `AgentId`, and `LeaseId` string IDs;
- `TaskSpec`, `TaskNode`, `TaskStatus`, `TaskOwner`, and `TaskScope`;
- `TaskResult`, `TaskUsage`, `TaskProgress`, and structured task errors;
- `BudgetLimits`, `BudgetAmount`, `BudgetAccount`, and reservation values;
- profile policy values and verification policy values;
- pure task transition and budget arithmetic checks.

It does not depend on Tokio, child session implementations, filesystem
allocation, or presentation code.

### 4.2 `lato-runtime`

`lato-runtime` owns executable coordination:

- `TaskCoordinator` and its actor loop;
- `TaskHandle` and root-bound/scoped clients;
- command envelopes and reply channels;
- `TaskRunner`, `TaskControl`, `TaskReporter`, and child control traits;
- queued, preparing, running, finalizing, and completed registries;
- waiter, drain, deadline, admission, message, usage, and completion handling;
- the authoritative in-memory `TaskTree` and `BudgetLedger` projection;
- stable task event publication.

The coordinator is shared across roots so it can enforce both global and
per-root limits. Only the actor mutates authoritative state.

### 4.3 `lato-workspace`

`lato-workspace` owns:

- `WorkspaceMode`;
- `WorkspaceLease`;
- the `WorkspaceAllocator` contract;
- fake or in-memory allocation support used by Phase 5A tests.

Concrete Git worktree allocation is deferred. A queued task reserves worktree
budget but does not allocate a lease until it enters `Preparing`.

### 4.4 Deferred integration crates

`lato-agent` will provide a real `TaskRunner` in Phase 5B. `lato-tools`, the root
CLI, TUI, and ACP remain unchanged in Phase 5A.

## 5. Domain model

### 5.1 Identity and lineage

Every task records:

```rust
struct TaskNode {
    id: TaskId,
    parent_id: Option<TaskId>,
    root_id: TaskId,
    owner: TaskOwner,
    profile: AgentProfile,
    scope: TaskScope,
    status: TaskStatus,
    budget: BudgetAccount,
    permissions: Vec<ToolCapability>,
    workspace_intent: WorkspaceIntent,
    result_contract: ResultContract,
}
```

The provider-neutral node stores only `WorkspaceIntent`. The concrete
`WorkspaceLease` remains in `lato-runtime`'s actor-owned runtime record so
`lato-core` does not depend on `lato-workspace` and create a crate cycle.

`TaskOwner` reserves both interactive and future workflow semantics:

```rust
enum TaskOwner {
    Interactive { session_id: SessionId, turn_id: TurnId },
    Workflow { run_id: String, session_id: SessionId },
}
```

Phase 5A creates interactive roots in production-facing construction paths. The
workflow variant is supported by domain validation and coordinator scoping so a
later workflow adapter does not require changing task identity or cancellation
rules.

Unlike Grok Build, nested children retain their real `parent_id`. Queries and
limits use the separately stored `root_id` and owner lineage when root-scoped
behavior is required.

### 5.2 Lifecycle

The public lifecycle is:

```text
Queued
  -> Preparing
  -> Running
  -> WaitingForChildren | WaitingForApproval
  -> Running
  -> Verifying
  -> Completed | Failed | Cancelled | TimedOut
```

`Finalizing` is an internal coordinator state used to close active-message
admission and settle selected deliveries before committing a public terminal
state. It is visible in inspection as a non-messageable state but is not a
recoverable workflow phase.

Terminal transitions are unique. Duplicate completion, late reporter messages,
and repeated cancellation cannot replace an existing terminal result.

### 5.3 Profiles

`AgentProfile` is data, not a hierarchy of hard-coded Rust role classes. It
contains instructions, model policy, tool filter, workspace mode, verification
policy, and optional definition-level background behavior.

Phase 5A provides built-in constructors for:

- `explorer`: read-only shared workspace and schema/evidence verification;
- `worker`: isolated-write intent and programmatic verification;
- `reviewer`: read-only workspace and independent-review intent.

These constructors are not selected automatically by the coordinator.

## 6. Coordinator interfaces

The internal handle exposes:

- `register_root`
- `spawn`
- `send_active_message`
- `inspect`
- `list_running`
- `wait`
- `cancel_task`
- `cancel_turn`
- `cancel_root`
- `cancel_workflow`
- `close_spawn_admission`
- `open_spawn_admission`
- `teardown_root_and_drain`
- `registry_counts`

Root-bound and task-bound handles inject their identity into requests. A child
handle cannot name an unrelated parent or access a foreign root. Unknown and
foreign tasks intentionally share a `NotFoundOrNotOwned` result to avoid leaking
cross-session state.

### 6.1 Runner boundary

`TaskRunner` is the only runtime-specific seam. It:

- validates or describes a profile/type when requested;
- prepares and runs one admitted task;
- reports promotion from `Preparing` to `Running` through a coordinator-owned
  acknowledgement;
- exposes a `TaskControl` for progress, active messages, and cancellation;
- reports usage and phase changes through `TaskReporter`;
- returns structured execution output and optional external snapshot/output
  references;
- receives an `on_completed` callback only after the coordinator has committed
  terminal state.

The promotion acknowledgement closes the cancellation-at-start race. If
cancellation wins while the runner is creating a child runtime, promotion is
rejected and the runner must tear down the half-initialized resource.

Runner futures are panic-contained. A panic becomes a structured task failure
and cannot terminate the coordinator actor.

## 7. Spawn admission and queueing

Spawn processing uses this fixed order:

1. Authenticate the root-bound or task-bound caller.
2. Validate parent existence, liveness, lineage, and spawn admission state.
3. Reject duplicate task IDs across queued, preparing, running, finalizing, and
   retained completed records.
4. Enforce maximum depth, children per parent, total tasks, and root/global
   concurrency policy.
5. Resolve the profile and validate that all overrides narrow inherited policy.
6. Atomically reserve the child budget, including a worktree slot when needed.
7. Create the task node.
8. Start immediately when capacity exists, enqueue under `Queue`, or complete as
   rejected under `Reject`.

The queue is FIFO in arrival order. Its capacity scan skips entries whose root
is currently saturated so one root cannot block another. Cancellation tokens
for queued tasks are reaped on a bounded interval even when no other command
arrives.

Rejected and cancelled-before-start tasks become retained terminal records.
This ensures waiters resolve and later inspection can distinguish an unknown ID
from a known task that never reached the runner.

## 8. Foreground and background semantics

A spawn distinguishes:

- explicit background execution;
- await-to-completion execution with no foreground deadline;
- foreground waiting with a configured await budget;
- definition-level background behavior supplied by a profile.

The foreground deadline begins when the request is enqueued, not when execution
starts. When it expires, the caller receives a background handle while the task
continues. The task remains queued if no execution slot is available.

Dropping a normal interactive caller releases turn-blocking ownership but does
not cancel coordinator-owned work. Workflow-owned spawn futures use a
cancel-on-drop guard because abandoning a workflow host call must release its
reserved workflow agent-call budget.

Completion disposition records whether the result was delivered inline,
delivered to a waiter, explicitly killed, backgrounded, or eligible for later
surfacing. Phase 5A computes this disposition but does not render reminders or
synthetic prompts.

## 9. Waiting, querying, and completed retention

Snapshots distinguish queued, preparing, running, waiting, verifying,
finalizing, completed, failed, cancelled, and timed-out tasks. Running snapshots
include duration and monotonic progress such as turns, tool calls, token usage,
context use, tools used, and error count when the runner provides them.

Multiple waiters may observe one task. Each waiter has its own deadline. A wait
timeout ends only that wait and does not cancel the task. Completed, cancelled,
and failed tasks answer immediately.

Completed records are retained in an insertion-ordered bounded cache. Eviction
removes the oldest completed record and emits an event. Large output may be
replaced by a runner-owned persisted reference; snapshot loading goes through
the runner boundary. Phase 5A tests this contract with an in-memory runner.

## 10. Active messages

Active-message delivery supports the closed operations `Queue` and `Steer`.
Requests are bounded by UTF-8 byte length and carry no caller-supplied trusted
sender identity. The coordinator creates the message ID and binds the sender's
session/root identity.

Admission is bounded twice:

- an absolute in-flight admission limit for the coordinator;
- a selected-admission limit for each active task.

The coordinator issues an admission lease with these states:

```text
Open -> Claimed -> Committed
Open -> Revoked
```

The runner may insert the protected message only synchronously while claiming
the open lease. Returning `Admitted` is valid only after the lease is committed.
During task finalization, new leases are refused and selected leases receive a
bounded interval to settle. If the coordinator cannot prove committed or
revoked state, it returns `AdmissionUncertain` and fails clean terminalization
rather than claiming the message was safely handled.

Queued, unknown, foreign, completed, or finalizing tasks cannot accept active
messages. Stable outcomes distinguish ownership failure, inactive state,
saturation, size limits, deadline expiry, unsupported runners, channel closure,
and uncertain admission.

## 11. Cancellation and teardown

Cancellation is idempotent and scope-aware:

- `cancel_task` cancels the selected task and its descendants;
- `cancel_turn` cancels non-workflow tasks owned by one interactive turn;
- `cancel_root` cancels all non-workflow descendants of one session root;
- `cancel_workflow` cancels tasks owned by one workflow run and waits for drain;
- actor drop cancels all live tasks and resolves queued callers without invoking
  host callbacks during teardown.

Closing root admission creates a latch. Late detached spawn requests are
rejected until an explicit reopen at the next turn. Root deletion closes
admission, cancels owned descendants, and waits for them to drain. A bounded
backstop clears the latch if a broken runner never finishes, preventing a
permanent coordinator-wide deadlock.

Queued cancellation resolves waiters immediately and never starts the runner.
Preparing and running cancellation trigger both the inherited cancellation
token and runner control. A configurable grace deadline allows cooperative
cleanup; after the deadline the coordinator aborts its owned run future and
commits `Cancelled` or `TimedOut` exactly once.

Parent terminalization closes descendant spawn admission before cancelling or
waiting on descendants, preventing a late child from escaping a dying scope.

## 12. Budget ledger

The ledger generalizes Grok Build's `total / spent / reserved / remaining`
model across:

- child task count;
- input, output, and total tokens;
- tool calls;
- cost in integer micro-units;
- wall-clock time;
- retries;
- worktree leases.

Every dimension uses integer values and checked arithmetic. `None` represents
an intentionally unlimited dimension; it is never produced by overflow.

Spawn atomically reserves the child's full envelope from the parent's remaining
budget before the task becomes visible. Failure or queued cancellation releases
the reservation. Terminalization settles actual usage into `spent` and returns
the unused portion. Nested reservations are charged inside the child's envelope
and roll up through ancestors without double-counting.

Usage reports are cumulative and must be monotonic. The coordinator calculates
the delta and rejects stale or decreasing reports. Repeated terminal output and
late reports cannot charge twice. Wall-clock usage is measured by the
coordinator rather than trusted from the runner.

Exhausting a hard runtime dimension requests cancellation and produces a
structured `BudgetExceeded` result. Concurrency is an admission constraint, not
a consumption dimension.

## 13. Permissions and workspace

Effective capabilities are computed as:

```text
parent ceiling intersect profile filter intersect explicit spawn override
```

An omitted override inherits the narrowed parent/profile result. It never means
all capabilities. Any explicit attempt to expand capabilities is rejected
before queue insertion or budget reservation.

Workspace modes are:

- `SharedReadOnly`
- `SharedSerializedWrite`
- `IsolatedWorktree`
- `ExternalLease`

Queued tasks reserve the applicable workspace budget. Allocation begins only in
`Preparing`. The lease is stored on the node before runner promotion. Startup
failure, cancellation, and terminal completion all invoke idempotent release.
The fake allocator records allocations and releases so tests can prove that
parallel write tasks never share a write lease and that every allocated lease
is eventually released.

## 14. Verification

A successful runner output enters `Verifying` rather than `Completed`.
`TaskVerifier` applies the profile's policy:

- `Accept`
- `Schema`
- `Programmatic`
- `IndependentReview`
- `HumanGate`

Phase 5A implements the common interface and concrete behavior for the first
three policies. `IndependentReview` and `HumanGate` produce explicit
`WaitingForChildren` and `WaitingForApproval` states plus typed resume commands,
but Phase 5A does not spawn a reviewer or expose an approval UI.

Verification failure produces a stable error code and retains the original
runner output for inspection. Cancellation remains effective while waiting or
verifying.

## 15. Events and observability

Every mutation flows through one `commit_transition` path. It validates the
transition, updates the task tree, budget ledger, workspace and runtime indexes,
then emits an event. `TaskRunner::on_completed` runs only after terminal state is
committed.

`TaskEventEnvelope` contains:

- schema version;
- coordinator-global sequence;
- task, parent, root, session, turn, and optional workflow IDs;
- timestamp;
- structured payload.

Events cover root registration/closure, spawn acceptance and rejection,
queueing, preparation, start, phase change, usage, budget exhaustion, message
outcomes, foreground release, background handoff, cancellation, finalization,
verification, terminal results, workspace lease changes, admission latch
changes, and completed-record eviction.

The event channel is bounded. Lagging consumers must recover by querying an
authoritative snapshot. Phase 5A provides a `TaskEventSink` and memory collector
but does not add disk persistence. The serializable schema and single transition
commit point are the stable seam for later journal integration.

## 16. Error model

Control flow never depends on matching free-form strings. `TaskError` carries a
stable code, retryability, and safe message. Codes cover:

- invalid, unknown, duplicate, or foreign identity;
- invalid parent or terminal parent;
- depth, child-count, queue, concurrency, message, and retention limits;
- budget reservation or runtime exhaustion;
- capability expansion or invalid profile;
- spawn admission closure;
- workspace allocation or release failure;
- runner initialization failure, panic, or protocol violation;
- message admission uncertainty;
- verification failure or pending external verification;
- cancellation, timeout, coordinator closure, and event lag.

Sensitive prompts, messages, paths, and outputs are not embedded into admission
or ownership errors.

## 17. Test strategy

Tests use deterministic fake runners, fake workspace allocators, paused Tokio
time, explicit barriers, and bounded timeouts. The required matrix includes:

### 17.1 Lifecycle and admission

- immediate start and terminal completion;
- queue and reject policies at capacity;
- FIFO queue order and cross-root non-starvation;
- duplicate IDs in every registry;
- cancelled queued work never starts;
- preparing-to-running promotion racing cancellation;
- runner startup failure and panic containment;
- completed FIFO eviction.

### 17.2 Foreground, wait, and completion

- inline foreground completion;
- await-to-completion without a foreground deadline;
- deadline handoff without stopping the child;
- queue time included in the foreground budget;
- caller drop removing turn-blocking ownership without orphaning work;
- independent waiter deadlines;
- surviving waiters continuing to suppress duplicate completion delivery;
- immediate terminal and unknown-ID queries;
- completion disposition for foreground, background, killed, and waiter cases.

### 17.3 Cancellation and teardown

- task, subtree, turn, root, and workflow cancellation;
- external token cancellation without unrelated actor traffic;
- foreign roots unaffected by scoped cancellation;
- session stop sparing workflow-owned work;
- admission remaining closed against late detached spawns;
- teardown drain, multiple drain waiters, and backstop reopen;
- actor drop resolving queued callers and cancelling live work;
- grace expiry aborting a non-cooperative runner exactly once.

### 17.4 Budget, permission, and workspace

- atomic multi-dimension reservation;
- release after rejection, cancellation, and startup failure;
- settlement and unused-budget return;
- nested roll-up without double charging;
- repeated and late usage reports;
- monotonic usage enforcement and overflow safety;
- runtime exhaustion cancellation;
- permission and profile narrowing;
- allocation timing, isolation, and idempotent lease release.

### 17.5 Active messages

- UTF-8 byte and empty-message validation;
- coordinator-wide and per-task saturation;
- ownership and lifecycle rejection;
- queue and steer delivery;
- admission timeout and channel closure;
- commit, revoke, claimed-but-unsettled, and stale-generation races;
- finalization waiting for selected admission;
- independent admission state per child;
- failure to prove settlement preventing a false clean completion.

### 17.6 Events and invariants

- dense global sequence and after-commit observation;
- exactly one terminal event;
- parent/root lineage and acyclic task tree;
- registry counts matching authoritative indexes;
- no live runner without a nonterminal node;
- no workspace lease or budget reservation leaked by a terminal task;
- bounded-channel lag surfaced explicitly.

## 18. Delivery and verification

Implementation will update the upstream source ledger for every substantially
derived production file and retain the required Apache-2.0 source headers.
Existing unrelated worktree changes must remain untouched.

The implementation gate is:

```bash
cargo fmt --check
cargo test -p lato-core
cargo test -p lato-runtime
cargo test -p lato-workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo install --path .
```

Because Phase 5A deliberately exposes no product surface, the installed `lato`
command must retain its existing behavior after deployment.

## 19. Completion criteria

Phase 5A is complete when:

- all mutations are actor-owned and pass through one transition path;
- the internal APIs implement the full coordination surface described above;
- fake runners demonstrate bounded concurrent execution across multiple roots;
- hierarchical cancellation, budgets, permissions, workspace leases, waits,
  background handoff, active messages, and verification obey their invariants;
- the required test matrix passes under deterministic time and race control;
- no model-visible task tool or user-facing subagent behavior changes;
- upstream attribution is recorded;
- the workspace passes formatting, lint, tests, and local installation.
