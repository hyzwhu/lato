# Lato Phase 5B/5C: Complete Subagent Product UX Design

Date: 2026-09-07

Status: approved in conversation; awaiting written-spec review

## 1. Purpose

Phase 5B replaces the Phase 5A fake-runner integration seam with the complete
Grok Build subagent execution stack pinned at commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`. Phase 5C then exposes that stack
through a minimal model-facing task product surface.

This is a structural port, not an independent reimplementation. The Grok Build
`SubagentCoordinator`, `ChannelBackend`, task request/result types, active-message
admission, runner lifecycle, and task tools become the authoritative execution
path. Lato-specific public domain types remain as compatibility types at the
outer boundary. Lato's provider-neutral model port, tool runtime, policy engine,
sandbox, journal, and presentation surfaces remain authoritative where Grok
Build depends on product-specific xAI services.

## 2. Scope and phase boundary

### 2.1 Phase 5B

Phase 5B delivers:

- a real child-session runner in `lato-agent`;
- a real Git worktree allocator in `lato-workspace`;
- profile-derived child configuration and context packages;
- capability and tool narrowing through the existing Lato policy stack;
- real child output, usage, progress, and event propagation;
- schema and programmatic result verification;
- bounded cancellation, timeout, failure, and root shutdown behavior;
- end-to-end tests from parent session through child tools and cleanup.

Phase 5B does not expose new model-facing operations and must not change CLI,
TUI, headless, or ACP output.

### 2.2 Phase 5C

Only after the Phase 5B acceptance suite passes, Phase 5C:

- replaces compatibility `spawn_subagent`;
- exposes `spawn`, `send`, `wait`, `cancel`, and `inspect` as model-visible tools;
- surfaces background task events and completion results through existing
  session events and tool results;
- allows only the built-in `explorer`, `worker`, and `reviewer` profiles;
- retains all Phase 5A concurrency, depth, recursion, queue, message, output,
  timeout, and budget ceilings.

Phase 5C does not add slash commands, a new TUI page, or new ACP methods.

## 3. Upstream baseline and porting policy

The sole behavioral baseline is Grok Build commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138` under Apache-2.0, especially:

- `xai-grok-tools/.../grok_build/task/backend.rs`;
- `xai-grok-tools/.../grok_build/task/types.rs`;
- `xai-grok-tools/.../grok_build/task/coordinator.rs` and its submodules;
- `xai-grok-tools/.../grok_build/task/active_message.rs`;
- the Task, task-output, kill-task, and active-message tool implementations;
- `xai-grok-shell` child-runner, session fork/context, worktree, and teardown
  paths used by those task components.

Substantially derived production files carry the repository's required source
header and are added to `docs/superpowers/reference/lato-upstream-sources.md`.

The port excludes Grok branding, cloud-account state, billing, telemetry,
remote workspace services, product notifications, automatic merge, push,
publication, and unrelated goal/workflow UI. When an upstream dependency has no
Lato equivalent, the port preserves the upstream contract and implements it
over a small Lato adapter rather than copying an unrelated xAI subsystem.

## 4. Architecture

### 4.1 Authoritative coordinator stack

The Grok Build coordinator model becomes authoritative:

```text
model-visible task tools
        |
        v
LatoTaskBackendResource
        |
        v
ChannelBackend ---- bounded active-message ingress
        |
        v
SubagentCoordinator (single writer)
        |
        +---- ChildSessionRunner ---- RuntimeSession
        |
        +---- GitWorkspaceAllocator ---- shared root / isolated worktree
        |
        +---- ResultVerifier
```

`lato-runtime` owns the ported coordinator, backend channels, admission,
registry, waiting, cancellation, and snapshots. Existing Phase 5A public types
remain available through conversions so other crates do not need a flag-day
rewrite. The old Phase 5A actor implementation is removed from the production
path after parity tests prove the replacement.

### 4.2 Child session runner

`lato-agent` owns `ChildSessionRunner`. Each admitted task creates a distinct
`RuntimeSession` with a distinct session ID, turn ID, cancellation root, tool
runtime, policy scope, history, and usage accounting.

The runner lifecycle follows Grok Build:

1. Resolve and validate the built-in profile.
2. Allocate the workspace before publishing `Running`.
3. Build the narrowed child tool catalog and policy scope.
4. Build a bounded context package.
5. Create the child `RuntimeSession` and acknowledge promotion.
6. Submit the task prompt and relay progress, usage, tool, and model events.
7. Capture the terminal output and artifacts.
8. Verify the result.
9. Commit one terminal coordinator state.
10. Shut down and join the child before releasing its workspace.

If cancellation wins before promotion acknowledgement, the half-created child
is shut down and the workspace is released without ever becoming visible as
running.

### 4.3 Context package

The child never receives a blind copy of the complete parent conversation.
The package is constructed from:

- the child task description and requested deliverable;
- the built-in profile instructions;
- immutable parent constraints and applicable policy obligations;
- the workspace root and repository identity;
- explicitly selected evidence, references, artifacts, and prior child output;
- a compact parent-state summary when needed;
- remaining task budget and hard execution limits.

The package has byte, message, artifact, and reference-count ceilings. Tool
results are represented by bounded excerpts plus artifact references. Secrets,
approval decisions, transient UI state, and unrelated parent turns are not
copied. Parent context remains authoritative; a child result is an explicit
return value, not an implicit history merge.

### 4.4 Built-in profiles

Only these profiles are accepted:

| Profile | Workspace | Tool capability | Result contract | Verification |
| --- | --- | --- | --- | --- |
| `explorer` | shared read-only | read, list, search, approved network fetch | answer, evidence list, citations, usage | schema plus citation/reference resolution |
| `worker` | isolated worktree | read/search plus bounded file writes and shell | summary, changed files, tests, artifacts, usage | schema plus workspace/path/test invariants |
| `reviewer` | shared read-only | read, list, search, bounded shell inspection | structured findings with severity and locations | schema plus finding/location invariants |

The effective child capability set is always:

```text
parent grant intersection profile allowlist intersection workspace allowance
```

An override may narrow this result but can never add a capability. Every tool
call still passes through `ToolRuntime`, `PolicyEngine`, execution grants, and
sandbox obligations. A read-only profile cannot gain write authority through a
shell command or an absolute path.

Nested spawn is present only when the parent has spawn authority and the child
remains below all depth, child-count, concurrency, and budget ceilings.

## 5. Git workspace allocator

`GitWorkspaceAllocator` implements the existing allocator contract with the
behavioral rules from Grok Build's worktree lifecycle:

- `explorer` and `reviewer` lease the canonical repository root read-only;
- `worker` leases `.lato/worktrees/<task-id>` as an isolated Git worktree;
- branches use `lato/task-<sanitized-task-id>`;
- allocation uses a process-local per-repository lock plus an on-disk ownership
  marker containing task ID, process ID, repository identity, and creation time;
- allocation is transactional: partial branch, registration, directory, and
  marker creation are rolled back in reverse order;
- release is idempotent and bounded;
- release performs `git worktree remove --force` followed by guarded prune;
- a failed cleanup preserves a recovery marker and emits a cleanup-failed event;
- startup scans only the configured `.lato/worktrees` root, reclaims entries
  whose owner process is dead, and never traverses arbitrary user paths;
- live leases are never reclaimed;
- no code path merges, commits, pushes, publishes, or deletes the source branch.

Parallel workers never share a writable root. Shared read-only leases do not
serialize readers. Any future shared-write mode remains disabled for built-in
profiles in this phase.

## 6. Events, progress, usage, and background completion

Child `RuntimeSession` events are translated to bounded coordinator events:

- preparing and running phase changes;
- model request start/end;
- tool start/end/failure;
- incremental token and tool-call usage;
- monotonic progress snapshots;
- verification start/end;
- terminal result and cleanup outcome.

The coordinator remains the sole authority for task state and accounting. Late
events from an obsolete generation are ignored. Usage is settled exactly once;
reported child usage can reduce remaining parent/root budgets but cannot exceed
reserved limits.

In Phase 5C, foreground spawns follow Grok Build's await budget and become
background tasks when that deadline expires. Background completion is buffered
as a bounded session event and is returned by `wait`/`inspect`; it does not
inject an unbounded synthetic transcript and does not alter existing rendered
CLI/TUI/ACP output.

## 7. Cancellation and shutdown

Cancellation authority flows downward:

```text
root/turn/task cancellation token
        -> child RuntimeSession operation token
        -> model stream token and every ToolContext token
        -> subprocess/process-group termination where applicable
```

Cancellation closes new message admission, cancels descendants, and waits for
a configured grace period. Model and tool futures are then aborted and joined.
Shell execution terminates the spawned process group rather than only dropping
the Rust future. Cleanup runs under its own bounded token so cancellation cannot
skip workspace release.

Parent session shutdown follows Grok Build's bounded teardown:

1. close spawn admission;
2. recursively cancel the owned task tree;
3. drain active-message admissions;
4. join child sessions until the shutdown deadline;
5. force-abort remaining child futures and process groups;
6. settle usage and commit terminal states;
7. release workspaces and persist cleanup failures;
8. stop the coordinator only after callback/event queues are boundedly drained.

No terminal state may be replaced by a late completion or late usage report.

## 8. Verification and repair flow

Model output is parsed into the profile's structured result before coordinator
completion. Invalid output is a verification failure, not a successful freeform
answer.

- Explorer citations must reference an item in the supplied or produced
  evidence set. Local paths must remain within the leased workspace; remote
  citations must correspond to a recorded fetch result.
- Worker changed-file and artifact paths must be inside its worktree. Declared
  test commands and outcomes are recorded from actual tool events when
  available; contradictory self-reports fail verification.
- Reviewer findings require severity, message, evidence, and a resolvable file
  location when a file is named. An empty issue list is valid.

Programmatic verification runs before independent-review policy. Reviewer
findings never trigger an implicit mutation. The parent may explicitly spawn a
new `worker` task with those findings in its context package. That task consumes
normal child-count, recursion, worktree, and budget quotas.

## 9. Phase 5C model-facing tools

The compatibility `spawn_subagent(session_id)` schema is removed from the v1
tool registry and replaced atomically with:

- `spawn`: task, description, built-in profile, bounded context references,
  background/await behavior, and narrowing overrides;
- `send`: owned task ID, `queue` or `steer`, and bounded message text;
- `wait`: one or more owned task IDs and a bounded wait deadline;
- `cancel`: owned task ID;
- `inspect`: owned task ID or owned-running-task listing.

The backend binds every call to the current parent session/root. Unknown and
foreign task IDs share one not-found response. Tool schemas do not expose
arbitrary profile definitions, unrestricted cwd selection, raw policy grants,
or unbounded time/budget parameters.

The old direct worktree helper is removed after the new spawn tool passes the
compatibility and end-to-end suite. There is no interval where two production
spawn implementations are registered under different names.

## 10. Failure behavior

Failures are typed and mapped to stable task error codes:

- invalid profile or authority expansion: reject before reservation;
- queue/concurrency/depth/budget saturation: retained rejected task result;
- workspace allocation failure: rollback and fail before running;
- model failure or block: timeout/cancel, join, then cleanup;
- tool failure: report event and allow the child turn to handle it within its
  remaining bounded turn budget;
- verification failure: terminal failed result with the invalid output retained
  only under configured output caps;
- cleanup failure: task result remains terminal, but inspection exposes cleanup
  failure and recovery-marker identity;
- coordinator or parent shutdown: bounded tree cancellation and resource drain.

Panics in validation, runner, verifier, callback, or cleanup adapters are
contained and converted to failures. No panic may terminate the coordinator
actor or strand a known live lease without a recovery marker.

## 11. Testing and acceptance

### 11.1 Port parity tests

Carry over and adapt Grok Build tests for:

- backend channel binding and foreign-session denial;
- admission, queue fairness, foreground/background handoff;
- active-message permit accounting and finalization races;
- query, multi-waiter, cancellation, late-event, and retention behavior;
- root/turn/task cancellation and bounded teardown.

Existing Phase 5A tests remain and are pointed at the ported coordinator through
the compatibility layer until equivalent coverage is proven.

### 11.2 Real Phase 5B end-to-end tests

Tests use deterministic fake model streams but real `RuntimeSession`, tool
runtime, policy evaluation, filesystem operations, subprocess cancellation, and
Git repositories. Required cases are:

1. parent session to child session to read tool to verified result;
2. two workers mutate the same relative file concurrently in distinct
   worktrees without cross-observation;
3. explorer and reviewer cannot write through file or shell tools;
4. child permissions are strict subsets of parent permissions;
5. child usage and progress reach coordinator inspection monotonically;
6. queued, preparing, model-running, and tool-running cancellation;
7. task deadline and budget exhaustion;
8. blocked model future is aborted and joined;
9. shell process group is terminated;
10. tool and verification failures produce one terminal state;
11. worktree allocation rollback and release retry;
12. stale dead-owner cleanup and live-owner preservation;
13. parent shutdown closes the full task tree within a fixed deadline;
14. reviewer findings can be passed only through an explicit worker spawn;
15. current CLI, TUI, headless, and ACP golden/output tests remain unchanged.

### 11.3 Phase 5C acceptance

- v1 tools expose exactly the new lifecycle operations and built-in profiles;
- compatibility spawn no longer creates a worktree directly;
- model tool loop can spawn, inspect, send, wait, and cancel;
- background events and results are observable without presentation changes;
- foreign-task access, arbitrary profile injection, and authority widening fail;
- every configured hard limit is covered by at least one test.

The final repository gate is `cargo test --workspace`, `cargo clippy --workspace
--all-targets -- -D warnings`, and `cargo install --path .`.

## 12. Architecture derivation and limits

The cognitive jobs are coordination, exploration, implementation, independent
review, and explicit repair. Exploration and review may run independently;
repair depends on a reviewer signal and an explicit parent decision. This is a
bounded factory-style control loop with conditional repair, not an automatic
self-healing loop.

The autonomy boundary is the child `RuntimeSession`; therefore verification is
performed at its result membrane. Schema validation is the floor, programmatic
invariants are used wherever possible, and reviewer independence is reserved
for code-review output. Dynamic width comes from explicit parent spawns and is
bounded by the Phase 5A coordinator limits. Recursive depth, active messages,
and repair iterations remain integer-capped.

AgentField remains a later adapter rather than a Phase 5B/5C dependency. Its
live contract version reviewed for this design is `2026-03-24-v1`.

## 13. Migration sequence

1. Import upstream types/backend/coordinator behind an internal feature path.
2. Implement Lato conversions and run both coordinator contract suites against
   the new path.
3. Add the real child runner and Git allocator.
4. Add profile context, tool narrowing, event/usage relay, and verification.
5. Pass the complete Phase 5B end-to-end and compatibility suite.
6. Switch the production coordinator construction to the ported stack and
   delete the superseded Phase 5A actor implementation.
7. Replace compatibility `spawn_subagent` with the Phase 5C task tools.
8. Pass workspace tests, clippy, install locally, and run command-level smoke
   tests.

Each switch is atomic at the public registry boundary. No migration step changes
CLI/TUI/ACP output or performs Git merge, commit, push, or publication on behalf
of a child task.
