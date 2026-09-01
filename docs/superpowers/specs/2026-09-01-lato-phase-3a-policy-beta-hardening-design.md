# Lato Phase 3A Policy and Public Beta Hardening Design

**Date:** 2026-09-01

**Status:** Design approved; implementation pending

**Scope:** Establish a non-bypassable policy membrane around every tool invocation, bind approvals to exact calls, fail closed when sandbox obligations cannot be satisfied, and add an offline-first `lato doctor` command for public Beta readiness.

## Context

Phase 2B-2 made `ToolRuntime` and `ToolCatalog` the authoritative tool-definition and execution boundary. The actor no longer calls the legacy dispatcher directly, and built-in tools are independently registered behind compatibility adapters. This creates the correct place to enforce one security policy for built-in, replacement, extension, and future MCP tools.

The remaining policy is fragmented. `SessionActor` decides interactive approval from tool names, `SessionTrust` tracks a coarse one-shot approval counter, and the legacy dispatcher repeats mutation and sandbox checks. An approval is not cryptographically bound to the normalized tool name, arguments, capabilities, call identity, or sandbox obligation. A custom tool invoked outside the actor lacks the actor's interactive layer, and new extension types would otherwise need to reproduce security behavior.

Public Beta requires a deterministic, auditable boundary before architectural cleanup. Phase 3A therefore adds a policy membrane while preserving the existing dispatcher and sandbox implementation behind explicit compatibility bridges.

## Goals

- Route every built-in and extension tool invocation through one mandatory `PolicyEngine` owned by `ToolRuntime`.
- Evaluate risk from declared capabilities and side effects rather than string-matching tool names.
- Bind user approval to the exact session, turn, call, canonical tool, normalized arguments, capabilities, and sandbox obligation.
- Make approval grants short-lived, single-use, atomic, and non-replayable.
- Ensure policies can narrow requested permissions but can never expand them.
- Turn sandbox requirements into typed execution obligations that tools cannot discard or weaken.
- Fail closed when policy, approval state, capability validation, or sandbox preparation is unavailable.
- Preserve existing behavior through a narrow `SessionTrust` and dispatcher compatibility bridge.
- Add an offline-first, secret-safe `lato doctor` command suitable for local support and CI.
- Emit lightweight, redacted diagnostic events without committing to a telemetry backend.

## Non-goals

- Removing `dispatch.rs`, `SessionTrust`, or every legacy sandbox helper in this phase.
- Rewriting all built-in tools as native implementations.
- Building full OpenTelemetry export, remote telemetry ingestion, or prompt/content logging.
- Adding a dynamic AgentField reasoner graph. Authorization is deterministic program logic; dangerous actions terminate at explicit human approval.
- Implementing workflow orchestration, multi-agent scheduling, or remote MCP transport.
- Making live provider calls during normal `lato doctor` execution.
- Expanding a tool's permissions based on model arguments or tool implementation behavior.

## Alternatives Considered

### A. Policy membrane first — selected

Add an independent policy crate and make `ToolRuntime` enforce it before invoking any catalog entry. Bridge approved built-in calls into the existing one-shot approval mechanism only at the final execution boundary. Add Doctor alongside the membrane so operators can diagnose the boundary.

This fixes the highest-risk Beta issues without combining them with a complete tool rewrite.

### B. Doctor and observability first

Improve troubleshooting before changing policy. This would make failures easier to see but would leave approval replay, name-based risk inference, and inconsistent enforcement unresolved. It is insufficient for public Beta.

### C. Rewrite policy, actor, tools, dispatcher, and sandbox together

Replace every compatibility layer in one release. This produces a cleaner end state but substantially increases regression risk and overlaps heavily with active files. The membrane design deliberately supports this migration later without changing its external contracts.

## Security Architecture

```text
model tool call
    │ wire name + JSON arguments
    ▼
ToolRuntime
    │ resolve alias and canonical identity
    │ normalize and validate arguments
    │ validate descriptor capabilities
    ▼
PolicyEngine
    ├── Allow
    ├── RequireApproval
    └── Deny
          │
          ▼
ApprovalLedger
    │ issue / verify / atomically consume exact-call grant
    ▼
SandboxObligation
    │ prepare required backend or fail closed
    ▼
Tool::invoke
    │ native tool or legacy dispatch adapter
    ▼
redacted result and diagnostic event
```

`ToolRuntime` is the mandatory execution membrane. A runtime cannot be constructed in production mode without a policy engine and approval ledger. Direct runtime callers receive the same `RequireApproval` or `Deny` result as actor callers. The actor supplies presentation and user interaction, but does not decide which named tools are dangerous.

The policy system follows four invariants:

1. A descriptor requests capabilities; policy may only retain or narrow them.
2. An approved call is immutable. Any relevant change produces a different fingerprint.
3. Sandbox requirements are execution obligations, not advice.
4. Missing enforcement infrastructure denies dangerous execution.

## Crate Boundaries

### `lato-core`

`lato-core` contains stable, serializable contracts shared across the workspace:

- `ToolCapability`
- `SideEffect`
- `PolicyRequest`
- `PolicyDecision`
- `ApprovalFingerprint`
- `ApprovalRequest`
- `ExecutionGrant`
- `PolicyDenial`
- `SandboxObligation`
- stable policy and sandbox error codes

It contains no project trust storage, UI logic, sandbox implementation, or approval database.

### `lato-policy`

A new `lato-policy` crate implements deterministic enforcement:

```text
crates/lato-policy/src/
    lib.rs
    engine.rs
    capability.rs
    fingerprint.rs
    approval.rs
    sandbox.rs
    event.rs
```

The crate owns capability consistency checks, policy evaluation, canonical fingerprint construction, the in-memory approval ledger, sandbox obligation validation, and redacted policy events. It depends on `lato-core` contracts but not on the actor or CLI.

### `lato-tools`

`ToolRuntime` receives the policy services during construction and enforces the full prepare-authorize-execute sequence. `ToolCatalog` rejects inconsistent descriptors during registration. Built-in adapters consume an approved legacy bridge only immediately before entering the old dispatcher.

### `lato-agent`

`SessionActor` becomes a generic approval broker. It renders the structured approval request, receives the user's decision, asks the ledger to issue a grant, and retries the prepared call. It does not contain a list of mutating tool names.

### CLI

The root crate adds a self-contained `doctor` module and thin CLI routing for `lato doctor`. Doctor uses public diagnostic interfaces instead of reaching into actor internals.

## Capability Model

The initial capability vocabulary is intentionally small:

```rust
pub enum ToolCapability {
    FileRead,
    FileWrite,
    ProcessSpawn,
    NetworkRead,
    NetworkWrite,
    TaskControl,
    ExtensionInvoke,
}

pub enum SideEffect {
    None,
    ReadOnly,
    WorkspaceMutation,
    ExternalMutation,
}
```

Capabilities describe what an implementation can access. Side effects describe the maximum externally visible mutation. Both are required because either dimension alone is incomplete.

Catalog registration validates consistency. Examples:

- `FileWrite` cannot claim `None` or `ReadOnly` side effects.
- `NetworkWrite` requires `ExternalMutation`.
- `ProcessSpawn` cannot claim `None`.
- `FileRead` without mutation may claim `ReadOnly`.
- `ExtensionInvoke` must also declare the underlying effective capabilities when known.

Invalid or contradictory descriptors fail catalog construction. Extensions cannot make themselves safe by selecting a benign tool name or omitting a side effect.

## Policy Input and Decisions

A policy request includes:

- session, turn, and tool-call identifiers;
- canonical tool identity after alias resolution;
- normalized arguments or their canonical digest;
- declared capabilities and side effect;
- workspace root and normalized target resources when available;
- project trust and configured execution mode;
- requested sandbox profile and derived obligation;
- descriptor source and extension trust state.

The result is one of:

```rust
pub enum PolicyDecision {
    Allow(ExecutionGrant),
    RequireApproval(ApprovalRequest),
    Deny(PolicyDenial),
}
```

`Allow` still creates an internal execution grant so every invocation follows one execution path. `RequireApproval` contains a redacted, human-readable summary plus the fingerprint inputs required for issuance. `Deny` contains a stable code, safe message, and non-sensitive diagnostic metadata.

Initial compatibility policy:

| Capability or effect | Default behavior |
|---|---|
| workspace-bounded `FileRead` | allow |
| `FileWrite` / workspace mutation | require approval in Ask mode |
| `ProcessSpawn` | require approval in Ask mode and require sandbox obligation |
| public HTTPS `NetworkRead` | allow after SSRF, DNS, redirect, and destination validation |
| `NetworkWrite` / external mutation | require approval |
| `TaskControl` | evaluate from effective side effect |
| untrusted `ExtensionInvoke` | deny until explicitly trusted |

Always/automatic modes suppress the human prompt only where configuration permits; they do not bypass capability validation, grant creation, sandbox preparation, or audit events.

## Canonical Arguments and Approval Fingerprint

The runtime resolves aliases before authorization and uses the canonical tool identity for policy. Arguments are parsed as JSON, recursively canonicalized with stable object-key order, and serialized without insignificant formatting. Normalization that changes execution semantics belongs to the tool's preparation step and must happen before fingerprinting.

The approval fingerprint commits to:

```text
schema_version
+ session_id
+ turn_id
+ call_id
+ canonical_tool_name
+ canonical_arguments_hash
+ sorted_capabilities
+ side_effect
+ sandbox_obligation_hash
```

A domain-separated cryptographic digest produces the opaque fingerprint. Raw arguments are not stored in the ledger or diagnostic events.

Changing an alias to a different canonical tool, any argument, capability, side-effect declaration, sandbox profile, session, turn, or call ID creates a different fingerprint. Semantically identical JSON object key order does not.

## Approval Lifecycle

The authorization lifecycle is:

1. Runtime resolves the tool, prepares normalized arguments, validates its descriptor, and computes the fingerprint.
2. Policy returns `Allow`, `RequireApproval`, or `Deny`.
3. For `RequireApproval`, the actor displays the structured risk summary.
4. On consent, `ApprovalLedger` issues a short-lived grant bound to the fingerprint.
5. Runtime recomputes and verifies the fingerprint immediately before execution.
6. The ledger atomically consumes the grant.
7. Sandbox preparation succeeds, then the tool is invoked.

Grants are single-use, expire after a bounded duration, and cannot cross session, turn, or call boundaries. Concurrent attempts to consume one grant result in exactly one success. Denied, expired, mismatched, or already consumed grants never enter tool code.

Automatic `Allow` decisions use runtime-owned ephemeral grants. This preserves one execution state machine and ensures internal code cannot accidentally create an approval-only fast path.

## Legacy Approval Bridge

Existing built-ins still call dispatcher-level mutation checks through `SessionTrust`. During Phase 3A, runtime authorization remains authoritative while the old check is retained as defense in depth.

Only after a matching new grant has been atomically consumed may the built-in adapter issue the legacy `allow_once` immediately before calling the dispatcher. No unused legacy approval is created when policy denies, sandbox preparation fails, or the call fingerprint changes. Custom and native tools rely on the new grant directly and cannot acquire a legacy counter.

This bridge is deliberately private to the built-in adapter and is removed when each implementation migrates away from `dispatch`.

## Sandbox Obligations

Policy returns a typed obligation such as:

```rust
pub struct SandboxObligation {
    pub profile: SandboxProfile,
    pub workspace_root: PathBuf,
    pub writable_roots: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub environment: EnvironmentPolicy,
}
```

Before tool execution, the runtime or capability-specific adapter must prepare a backend that proves it can satisfy the obligation. A tool may accept stricter constraints but cannot weaken them.

Required behavior:

- File writes are limited to explicitly authorized canonical roots.
- Path authorization occurs after lexical normalization and symlink-aware resolution.
- Process execution always passes through the selected sandbox backend.
- Child environments are assembled from an allowlist; secrets are not inherited by default.
- Network access enforces protocol, DNS resolution, private-address, redirect, and destination rules.
- Backend absence, initialization failure, or unsupported obligations deny execution.
- A model or tool call cannot select `sandbox=off`.

An operator may explicitly configure sandbox-off behavior at session startup where the product permits it. That configuration does not suppress approval or other policy checks and is surfaced prominently by Doctor.

The first implementation wraps the current terminal and network safety functions behind the obligation contract. It does not rewrite proven OS-specific sandbox code.

## Diagnostic Events

Phase 3A defines a small event interface and emits:

- `policy.evaluated`
- `policy.approval_requested`
- `policy.approval_consumed`
- `policy.denied`
- `sandbox.prepared`
- `sandbox.denied`
- `tool.started`
- `tool.completed`
- `tool.failed`
- `doctor.check_completed`

Events may include typed session, turn, and call IDs, canonical tool identity, capabilities, decision, stable error code, elapsed time, result size, and argument digest. They must not include raw prompts, complete arguments, file contents, authorization headers, tokens, or secret environment values.

The default sink is local and lightweight. A future journal or OpenTelemetry adapter can implement the same interface without changing policy evaluation.

## `lato doctor`

The CLI adds:

```text
lato doctor
lato doctor --json
lato doctor --strict
lato doctor --live
```

Default execution is deterministic and offline. It checks:

- binary version, platform, current workspace, and effective Lato home;
- configuration parsing and selected model availability;
- whether credential sources exist, reporting only source and presence;
- home and workspace access needed by configured modes;
- ToolCatalog construction and descriptor consistency;
- PolicyEngine construction and a fixed self-test decision matrix;
- sandbox backend availability for configured profiles;
- project trust, plugin, and extension status.

`--json` emits a versioned stable object containing overall status and individual check records. Normal warnings keep exit status zero; errors return one. `--strict` upgrades warnings to failure. `--live` adds bounded provider connectivity checks and is the only mode allowed to use the network. It must not submit an ordinary prompt completion as part of the default health check.

Doctor output passes through the same redaction utilities as policy events. Tests seed recognizable fake secrets and assert they never appear in human, JSON, error, or debug output.

## Error Model

Initial stable codes include:

| Code | Meaning |
|---|---|
| `policy.invalid_descriptor` | capability and side-effect declaration is inconsistent |
| `policy.approval_required` | exact call requires user approval |
| `policy.denied` | configured policy rejects the request |
| `policy.grant_missing` | execution has no matching grant |
| `policy.grant_mismatch` | grant fingerprint does not match the prepared call |
| `policy.grant_expired` | grant exceeded its validity window |
| `policy.grant_consumed` | grant has already been used |
| `sandbox.unavailable` | required sandbox backend is not available |
| `sandbox.unsupported` | backend cannot satisfy the requested obligation |
| `sandbox.preparation_failed` | backend setup failed before tool execution |
| `doctor.check_failed` | a diagnostic check encountered a concrete error |

User-facing messages explain the next safe action without exposing internal paths or credentials unnecessarily. Errors retain structured details for tests and local diagnostics.

## Implementation Sequence

1. Add `lato-policy` and the stable contracts required by the engine.
2. Implement descriptor consistency validation, canonical fingerprinting, and atomic approval ledger.
3. Extend `ToolRuntime` with prepare, policy evaluation, grant verification, and execute phases.
4. Replace actor name-based approval with generic structured approval handling.
5. Bridge approved built-in calls to existing `SessionTrust`, dispatcher, and sandbox enforcement.
6. Add sandbox obligation adapters and fail-closed backend readiness checks.
7. Add redacted policy events and the offline Doctor service.
8. Wire `lato doctor`, `--json`, `--strict`, and opt-in `--live` into the CLI.
9. Run focused security tests, the full workspace suite, strict Clippy, local installation, and installed-binary smoke tests.

Each step should be independently reviewable and keep the existing test suite green. Compatibility code is removed only after an equivalent non-bypassable native path exists.

## Test Strategy

### Capability and policy matrix

- Validate every built-in descriptor and reject contradictory custom descriptors.
- Cover execution mode, project trust, capability, side effect, and sandbox combinations.
- Prove policy outputs never add capabilities absent from the descriptor.

### Fingerprint and ledger

- Equivalent JSON object key order produces the same digest.
- Changes to tool, arguments, capabilities, side effect, sandbox, session, turn, or call produce a different digest.
- Grants expire, are consumed once, and cannot be replayed.
- Concurrent consumption permits exactly one invocation.
- Aliases resolve before fingerprinting and cannot create an authorization bypass.

### Runtime bypass resistance

- Direct runtime invocation without policy services cannot execute a dangerous tool.
- Direct invocation with no matching grant returns a structured error before tool code runs.
- Replacement and extension tools receive identical policy treatment.
- A denial never increments the legacy approval counter or invokes dispatch.

### Sandbox

- Missing and incapable backends fail closed.
- Path traversal, symlink escape, and writes outside authorized roots are denied.
- Process environments omit seeded secret variables unless explicitly allowed.
- Network checks cover private targets, DNS changes, redirects, and unsupported protocols.

### Doctor and redaction

- Default Doctor performs no network access.
- Human and JSON modes return deterministic statuses and documented exit codes.
- Strict mode upgrades warnings.
- Live checks are opt-in, bounded, and independently mockable.
- Seeded tokens, headers, environment secrets, prompts, and arguments never appear in output.

### Regression and release

- All existing workspace tests remain green.
- Strict workspace Clippy passes with warnings denied.
- `cargo install --path .` succeeds.
- The installed `lato doctor` works in human and JSON modes.
- Installed-binary smoke tests execute representative read, write-with-approval, denied, and sandboxed process paths.

## Public Beta Completion Criteria

Phase 3A is ready for public Beta only when all of the following hold:

- No known built-in, replacement, or extension tool path bypasses policy evaluation.
- File mutation, process launch, and external mutation are covered by unified approval policy.
- Approvals are bound to exact calls and cannot be replayed.
- Sandbox failure never silently degrades into unsandboxed dangerous execution.
- Doctor identifies configuration, credential-presence, catalog, policy, and sandbox failures without leaking secrets.
- Policy and sandbox failures expose stable codes and actionable safe messages.
- Full tests, strict Clippy, installation, and installed-binary smoke tests pass.
- Existing uncommitted user work remains intact throughout the migration.

## AgentField Assessment

The installed AgentField design guidance was applied to classify this work. Policy evaluation, descriptor validation, fingerprint verification, redaction, and Doctor checks use deterministic programmatic verification rather than model judgment. Human approval is the terminal high-assurance rung for dangerous operations. No reasoner graph or provider credential is required for Phase 3A; AgentField integration remains appropriate for later multi-agent and remote-execution phases.
