# Lato Phase 3A Policy and Public Beta Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put every Lato tool call behind a capability-based, exact-call approval and sandbox policy membrane, then add a secret-safe `lato doctor` command that establishes public Beta readiness.

**Architecture:** `lato-core` owns stable policy contracts, the new `lato-policy` crate owns deterministic decisions, fingerprints, grants, and redacted events, and `lato-tools::ToolRuntime` owns the only prepare-authorize-execute path. `SessionActor` becomes a generic approval presenter, existing dispatch and `SessionTrust` remain behind a narrowly scoped compatibility bridge, and Doctor consumes public diagnostic APIs without invoking a model unless `--live` is explicit.

**Tech Stack:** Rust 2024, Tokio, async-trait, serde/serde_json, SHA-256, std synchronization primitives, existing Lato workspace/tool/agent crates.

## Global Constraints

- Follow `docs/superpowers/specs/2026-09-01-lato-phase-3a-policy-beta-hardening-design.md`.
- Policy evaluation, fingerprinting, redaction, and Doctor's default checks must be deterministic and require no AgentField provider key.
- A policy may narrow capabilities but must never add a capability absent from the registered descriptor.
- Missing policy, matching grant, or required sandbox backend must deny dangerous execution.
- Raw prompts, complete tool arguments, file contents, credentials, authorization headers, and secret environment values must not enter policy events or Doctor output.
- Preserve model-facing built-in names, `Lato:<name>`, and the legacy `write` alias.
- Preserve the existing legacy dispatcher until an equivalent native implementation replaces it.
- The pre-existing modified files `crates/lato-agent/src/actor.rs`, `crates/lato-ai/src/api.rs`, `crates/lato-ai/src/models_file.rs`, `crates/lato-ai/src/stream.rs`, `crates/lato-tools/src/dispatch.rs`, `crates/lato-tools/src/edit.rs`, `crates/lato-tools/src/registry.rs`, `src/cli.rs`, and `tests/cli_headless.rs` are user-owned; inspect the live diff before editing, use narrow patches, and stage only task-specific hunks/files.
- After each task run its focused tests and commit only that task. At the end run full tests, strict Clippy, `cargo install --path .`, and installed-binary smoke tests.
- Do not add remote telemetry or make a network request from default `lato doctor`.

---

## File Map

| Path | Responsibility |
|---|---|
| `crates/lato-core/src/policy.rs` | Stable IDs, modes, requests, decisions, grants, denials, and sandbox obligation contracts |
| `crates/lato-core/src/tool.rs` | Capability vocabulary, side-effect compatibility, and grant-bearing `ToolContext` |
| `crates/lato-policy/src/capability.rs` | Descriptor consistency and capability narrowing checks |
| `crates/lato-policy/src/fingerprint.rs` | Canonical JSON and domain-separated exact-call SHA-256 fingerprint |
| `crates/lato-policy/src/approval.rs` | Expiring, atomic, single-use approval ledger |
| `crates/lato-policy/src/engine.rs` | Deterministic Allow / RequireApproval / Deny policy matrix |
| `crates/lato-policy/src/sandbox.rs` | Obligation derivation and satisfaction validation |
| `crates/lato-policy/src/event.rs` | Redacted policy event schema and sink interface |
| `crates/lato-tools/src/runtime.rs` | Mandatory prepare-authorize-execute membrane |
| `crates/lato-tools/src/builtin_adapter.rs` | Exact post-grant bridge into legacy `allow_once` and dispatch |
| `crates/lato-workspace/src/sandbox.rs` | Existing OS backend adapted to typed obligations and readiness probes |
| `crates/lato-agent/src/actor.rs` | Generic structured approval interaction |
| `src/doctor.rs` | Offline checks, optional live checks, report rendering, and exit policy |
| `src/cli.rs` | Thin `doctor` command routing and help text |
| `tests/policy_runtime_wiring.rs` | Repository-level proof that actor and runtime cannot bypass policy |
| `tests/doctor_cli.rs` | Installed-shape CLI contract, JSON, exit status, offline behavior, and redaction |

---

### Task 1: Stable Policy Contracts and Capability Validation

**Files:**
- Modify: `crates/lato-core/src/lib.rs`
- Modify: `crates/lato-core/src/tool.rs`
- Create: `crates/lato-core/src/policy.rs`
- Create: `crates/lato-core/tests/policy_contract.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `crates/lato-tools/tests/tool_catalog.rs`
- Modify: `crates/lato-tools/tests/tool_runtime.rs`
- Modify: `crates/lato-agent/src/actor.rs`

**Interfaces:**
- Produces: `PolicyMode`, `PolicyRequest`, `PolicyDecision`, `ApprovalFingerprint`, `ApprovalRequest`, `GrantId`, `ExecutionGrant`, `PolicyDenial`, `SandboxProfile`, `SandboxObligation`, `NetworkPolicy`, and `EnvironmentPolicy`.
- Produces: `ToolDescriptor::validate_policy_metadata()` and `ToolContext::with_execution_grant(ExecutionGrant)`.
- Consumes: existing typed IDs and `ToolDescriptor` metadata.

- [ ] **Step 1: Write failing serialization and descriptor consistency tests**

Create `crates/lato-core/tests/policy_contract.rs` with concrete assertions:

```rust
use lato_core::{
    DescriptorError, SideEffect, ToolCapability, ToolDescriptor, ToolName, ToolSource,
    ToolLayer, ToolConcurrency, ToolIdempotency, ToolCancellation,
};
use semver::Version;

fn descriptor(capabilities: Vec<ToolCapability>, side_effect: SideEffect) -> ToolDescriptor {
    ToolDescriptor {
        name: ToolName::parse("test:tool").unwrap(),
        version: Version::new(1, 0, 0),
        description: "test tool".into(),
        input_schema: serde_json::json!({"type":"object"}),
        capabilities,
        side_effect,
        concurrency: ToolConcurrency::Serial,
        idempotency: ToolIdempotency::NonIdempotent,
        timeout_ms: 1_000,
        max_output_bytes: 1_024,
        cancellation: ToolCancellation::Cooperative,
        source: ToolSource { layer: ToolLayer::User, id: "test".into(), replacement: None },
    }
}

#[test]
fn file_write_cannot_claim_read_only_side_effect() {
    let error = descriptor(vec![ToolCapability::FileWrite], SideEffect::ReadOnly)
        .validate()
        .unwrap_err();
    assert_eq!(error, DescriptorError::CapabilitySideEffectMismatch);
}

#[test]
fn network_write_requires_external_mutation() {
    assert!(descriptor(
        vec![ToolCapability::NetworkWrite],
        SideEffect::WorkspaceMutation,
    ).validate().is_err());
}

#[test]
fn policy_decision_has_stable_tagged_json() {
    let value = serde_json::to_value(lato_core::PolicyDecision::Deny(
        lato_core::PolicyDenial::new("policy.denied", "denied"),
    )).unwrap();
    assert_eq!(value["kind"], "deny");
    assert_eq!(value["value"]["code"], "policy.denied");
}
```

- [ ] **Step 2: Run the focused test and verify the contract is absent**

Run: `cargo test -p lato-core --test policy_contract`

Expected: compilation fails because the policy types, new variants, and mismatch error do not exist.

- [ ] **Step 3: Add the policy contract module and migrate capability names**

Implement `crates/lato-core/src/policy.rs` with the exact public shapes used by later tasks:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode { Ask, Auto, Always }

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ApprovalFingerprint(pub String);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct GrantId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SandboxObligation {
    pub profile: SandboxProfile,
    pub workspace_root: std::path::PathBuf,
    pub writable_roots: Vec<std::path::PathBuf>,
    pub network: NetworkPolicy,
    pub environment: EnvironmentPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PolicyRequest {
    pub session_id: crate::SessionId,
    pub turn_id: crate::TurnId,
    pub call_id: crate::ToolCallId,
    pub tool_name: crate::ToolName,
    pub arguments_digest: String,
    pub capabilities: Vec<crate::ToolCapability>,
    pub side_effect: crate::SideEffect,
    pub mode: PolicyMode,
    pub project_trusted: bool,
    pub sandbox: SandboxObligation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile { Off, Workspace, ReadOnly }

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy { Deny, PublicHttpsRead }

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentPolicy { pub allowed_keys: Vec<String> }

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionGrant {
    pub id: GrantId,
    pub fingerprint: ApprovalFingerprint,
    pub sandbox: SandboxObligation,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRequest {
    pub request: PolicyRequest,
    pub fingerprint: ApprovalFingerprint,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PolicyDenial { pub code: String, pub message: String }

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow(ExecutionGrant),
    RequireApproval(ApprovalRequest),
    Deny(PolicyDenial),
}
```

Add `PolicyDenial::new(code, message)` and constructors for the obligation profiles. No policy type contains a raw argument field. In `tool.rs`, migrate capabilities to `FileRead`, `FileWrite`, `ProcessSpawn`, `NetworkRead`, `NetworkWrite`, `TaskControl`, and `ExtensionInvoke`; migrate side effects to `None`, `ReadOnly`, `WorkspaceMutation`, and `ExternalMutation`; add serde aliases for the old wire spellings where serde supports them. Add `DescriptorError::CapabilitySideEffectMismatch` and enforce the two tests plus `ProcessSpawn != None`.

Extend `ToolContext`:

```rust
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
    pub execution_grant: Option<ExecutionGrant>,
}

impl ToolContext {
    pub fn with_execution_grant(mut self, grant: ExecutionGrant) -> Self {
        self.execution_grant = Some(grant);
        self
    }
}
```

Update all existing `ToolContext` literals to set `execution_grant: None`, and re-export the policy types from `lato-core/src/lib.rs`.

- [ ] **Step 4: Run core and affected workspace tests**

Run: `cargo test -p lato-core && cargo test -p lato-tools --no-run && cargo test -p lato-agent --no-run`

Expected: all `lato-core` tests pass and dependent crates compile after their built-in metadata uses the new enum names.

- [ ] **Step 5: Commit the stable contract**

```bash
git add crates/lato-core crates/lato-tools/src/builtin_adapter.rs crates/lato-tools/tests/tool_catalog.rs crates/lato-tools/tests/tool_runtime.rs
git add -p crates/lato-agent/src/actor.rs
git commit -m "feat: add policy and capability contracts"
```

Before committing, use `git diff --cached --name-only` and remove any unrelated staged file.

---

### Task 2: Policy Crate, Canonical Fingerprints, and Descriptor Checks

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/lato-policy/Cargo.toml`
- Create: `crates/lato-policy/src/lib.rs`
- Create: `crates/lato-policy/src/capability.rs`
- Create: `crates/lato-policy/src/fingerprint.rs`
- Create: `crates/lato-policy/tests/fingerprint.rs`
- Create: `crates/lato-policy/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 policy and tool contracts.
- Produces: `validate_capability_subset`, `canonical_arguments`, and `approval_fingerprint(&PolicyRequest) -> ApprovalFingerprint`.

- [ ] **Step 1: Add failing canonicalization and narrowing tests**

```rust
use lato_policy::{approval_fingerprint, canonical_arguments, validate_capability_subset};

#[test]
fn object_key_order_does_not_change_canonical_arguments() {
    let a = serde_json::json!({"path":"a", "options":{"b":2,"a":1}});
    let b = serde_json::json!({"options":{"a":1,"b":2}, "path":"a"});
    assert_eq!(canonical_arguments(&a).unwrap(), canonical_arguments(&b).unwrap());
}

#[test]
fn policy_cannot_add_network_write() {
    let declared = vec![lato_core::ToolCapability::NetworkRead];
    let effective = vec![lato_core::ToolCapability::NetworkRead, lato_core::ToolCapability::NetworkWrite];
    assert!(validate_capability_subset(&declared, &effective).is_err());
}
```

The fingerprint test must clone one valid request, change each of session, turn, call, tool, arguments digest, capabilities, side effect, and sandbox profile independently, and assert every digest differs.

- [ ] **Step 2: Run tests and verify the new crate is missing**

Run: `cargo test -p lato-policy`

Expected: Cargo reports that package `lato-policy` does not exist.

- [ ] **Step 3: Create the crate and deterministic canonicalization**

Add `lato-policy` to workspace members. Its dependencies are `lato-core`, `serde`, `serde_json`, `sha2 = "0.10"`, and `thiserror = "2"`.

Implement recursive canonical JSON sorting without altering array order or numeric/string values:

```rust
pub fn canonical_arguments(value: &serde_json::Value) -> Result<Vec<u8>, FingerprintError> {
    fn normalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let sorted = map.iter().map(|(key, value)| (key.clone(), normalize(value))).collect();
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(normalize).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_vec(&normalize(value)).map_err(FingerprintError::Serialize)
}
```

Hash the canonical request using SHA-256 with the domain prefix `b"lato.policy.approval.v1\0"`. Sort and deduplicate capabilities before serialization. Encode the digest as lowercase hexadecimal using a small local formatter so no additional encoding dependency is needed.

- [ ] **Step 4: Implement capability narrowing**

`validate_capability_subset` must compare sets and return `CapabilityError::Expansion(ToolCapability)` for the first effective capability absent from the descriptor. Reuse `ToolDescriptor::validate()` for side-effect consistency; do not maintain a second contradictory matrix.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test -p lato-policy && cargo test -p lato-core`

Expected: all tests pass.

```bash
git add Cargo.toml Cargo.lock crates/lato-policy
git commit -m "feat: add deterministic policy fingerprints"
```

---

### Task 3: Atomic Approval Ledger and Deterministic Policy Engine

**Files:**
- Create: `crates/lato-policy/src/approval.rs`
- Create: `crates/lato-policy/src/engine.rs`
- Create: `crates/lato-policy/tests/approval.rs`
- Create: `crates/lato-policy/tests/policy_matrix.rs`
- Modify: `crates/lato-policy/src/lib.rs`

**Interfaces:**
- Consumes: `approval_fingerprint`, `PolicyRequest`, `PolicyMode`, and grant contracts.
- Produces: `ApprovalLedger::issue`, `ApprovalLedger::consume`, `PolicyEngine::evaluate`, and `PolicyEngine::approve`.

- [ ] **Step 1: Write failing expiry, replay, concurrency, and policy-matrix tests**

The concurrency test must issue one grant, place the ledger and grant in `Arc`, release eight threads with a `Barrier`, and assert exactly one `consume` returns `Ok(())`:

```rust
let successes = handles.into_iter()
    .filter(|handle| handle.join().unwrap().is_ok())
    .count();
assert_eq!(successes, 1);
```

The policy matrix must assert:

```rust
assert!(matches!(engine.evaluate(&read_request(PolicyMode::Ask)), PolicyDecision::Allow(_)));
assert!(matches!(engine.evaluate(&write_request(PolicyMode::Ask)), PolicyDecision::RequireApproval(_)));
assert!(matches!(engine.evaluate(&write_request(PolicyMode::Always)), PolicyDecision::Allow(_)));
assert!(matches!(engine.evaluate(&untrusted_extension()), PolicyDecision::Deny(_)));
```

- [ ] **Step 2: Run the tests and verify missing symbols**

Run: `cargo test -p lato-policy --test approval --test policy_matrix`

Expected: compilation fails for `ApprovalLedger` and `PolicyEngine`.

- [ ] **Step 3: Implement a lock-protected single-use ledger**

Use `Mutex<HashMap<GrantId, GrantRecord>>`, `AtomicU64` for unique process-local grant IDs, and `std::time::Instant` for expiry. Do not serialize ledger records.

```rust
pub struct ApprovalLedger {
    next_id: AtomicU64,
    grants: Mutex<HashMap<GrantId, GrantRecord>>,
    ttl: Duration,
}

pub fn consume(
    &self,
    grant: &ExecutionGrant,
    expected: &ApprovalFingerprint,
) -> Result<(), ApprovalError> {
    let mut grants = self.grants.lock().map_err(|_| ApprovalError::Unavailable)?;
    let record = grants.remove(&grant.id).ok_or(ApprovalError::ConsumedOrMissing)?;
    if Instant::now() > record.expires_at { return Err(ApprovalError::Expired); }
    if &record.fingerprint != expected { return Err(ApprovalError::Mismatch); }
    Ok(())
}
```

Remove the record on every consumption attempt so a mismatched or expired token cannot be retried.

- [ ] **Step 4: Implement the policy engine**

`PolicyEngine` owns `Arc<ApprovalLedger>`. `evaluate` validates the request, derives its fingerprint, denies untrusted `ExtensionInvoke`, returns `RequireApproval` for Ask-mode workspace/external mutation, and otherwise issues an internal grant and returns `Allow(grant)`. `approve` accepts an `ApprovalRequest`, recomputes its fingerprint inputs, and issues a matching grant; it never accepts a caller-supplied digest without recomputation.

Public functions:

```rust
impl PolicyEngine {
    pub fn new(ledger: Arc<ApprovalLedger>) -> Self;
    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision;
    pub fn approve(&self, request: &ApprovalRequest) -> Result<ExecutionGrant, PolicyError>;
    pub fn consume(&self, grant: &ExecutionGrant, expected: &ApprovalFingerprint) -> Result<(), PolicyError>;
}
```

- [ ] **Step 5: Run all policy tests and commit**

Run: `cargo test -p lato-policy`

Expected: all fingerprint, capability, ledger, and matrix tests pass.

```bash
git add crates/lato-policy
git commit -m "feat: add exact-call approval policy engine"
```

---

### Task 4: Make ToolRuntime the Mandatory Policy Membrane

**Files:**
- Modify: `crates/lato-tools/Cargo.toml`
- Modify: `crates/lato-tools/src/runtime.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Modify: `crates/lato-tools/tests/tool_runtime.rs`
- Create: `crates/lato-tools/tests/policy_runtime.rs`

**Interfaces:**
- Consumes: `PolicyEngine`, `PolicyRequest`, `PolicyDecision`, and exact-call grants.
- Produces: `PolicyScope`, opaque `PreparedToolCall`, `ToolRuntime::prepare`, `ToolRuntime::decision`, `ToolRuntime::approve`, and `ToolRuntime::execute`.

- [ ] **Step 1: Write failing runtime bypass and alias tests**

Create tests using a counting fake `Tool`:

```rust
let prepared = runtime.prepare(context("call-1"), "write", json!({"contents":"x","path":"a"})).unwrap();
assert!(matches!(runtime.decision(&prepared), PolicyDecision::RequireApproval(_)));
let error = runtime.execute_without_approval_for_test(prepared).await.unwrap_err();
assert_eq!(error.code, "policy.grant_missing");
assert_eq!(calls.load(Ordering::Acquire), 0);
```

Also prove `write` and `write_file` normalize to `builtin:write_file` before fingerprinting and that changing arguments after preparation is impossible because `PreparedToolCall` exposes no mutable argument field.

- [ ] **Step 2: Run the focused test and verify old runtime bypasses policy**

Run: `cargo test -p lato-tools --test policy_runtime`

Expected: compilation fails because the prepare/decision/execute API does not exist.

- [ ] **Step 3: Add policy services to the builder**

Add `lato-policy` dependency. Replace the zero-argument production builder with:

```rust
pub struct PolicyScope {
    pub workspace_root: PathBuf,
    pub mode: PolicyMode,
    pub project_trusted: bool,
    pub sandbox_profile: SandboxProfile,
}

pub struct ToolRuntimeBuilder {
    catalog: ToolCatalog,
    policy: Arc<PolicyEngine>,
    scope: PolicyScope,
}

impl ToolRuntimeBuilder {
    pub fn new(policy: Arc<PolicyEngine>, scope: PolicyScope) -> Self;
}
```

Update every builder call site explicitly; do not add a permissive `Default` implementation.

- [ ] **Step 4: Implement opaque preparation and exact execution**

`prepare` must resolve the alias, resolve the catalog entry, validate cancellation, canonicalize arguments, create `PolicyRequest`, evaluate it, and retain the immutable descriptor/tool/arguments/fingerprint in a private struct. `execute` must require the grant carried by `Allow` or returned by `approve`, atomically consume it, add it to `ToolContext`, recheck cancellation, and only then call `Tool::invoke`.

Use this public flow:

```rust
let prepared = runtime.prepare(context, wire_name, arguments)?;
match runtime.decision(&prepared) {
    PolicyDecision::Allow(grant) => runtime.execute(prepared, grant.clone()).await,
    PolicyDecision::RequireApproval(_) => Err(policy_error("policy.approval_required")),
    PolicyDecision::Deny(denial) => Err(denial.clone().into()),
}
```

Keep `invoke` as this fail-closed convenience path. It may execute automatic Allow decisions but must never synthesize human approval.

- [ ] **Step 5: Run runtime, catalog, and wiring tests**

Run: `cargo test -p lato-tools && cargo test --test tool_runtime_wiring`

Expected: all tests pass, including direct-invoke denial for approval-required tools.

- [ ] **Step 6: Commit the runtime membrane**

```bash
git add crates/lato-tools Cargo.lock
git commit -m "feat: enforce policy in tool runtime"
```

---

### Task 5: Generic Actor Approval and Exact Legacy Bridge

**Files:**
- Modify: `crates/lato-agent/Cargo.toml`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `crates/lato-tools/src/dispatch.rs`
- Modify: `src/cli.rs`
- Create: `tests/policy_runtime_wiring.rs`

**Interfaces:**
- Consumes: Task 4 prepared-call API and `ApprovalRequest`.
- Produces: `ToolApproval::approve(&ApprovalRequest)`, actor prepare/prompt/approve/execute flow, and post-grant-only legacy allowance.

- [ ] **Step 1: Write failing actor and compatibility tests**

Add tests proving a renamed custom write-capable tool triggers generic approval, a denial does not invoke it, and one approval cannot authorize a second call with different arguments. Add a source-wiring assertion that `actor.rs` no longer references `requires_approval`, `has_allow_once`, or calls `allow_once`.

```rust
let actor = source("crates/lato-agent/src/actor.rs");
for forbidden in ["requires_approval", "has_allow_once", ".allow_once()"] {
    assert!(!actor.contains(forbidden), "actor bypass remains: {forbidden}");
}
```

- [ ] **Step 2: Run focused tests and observe name-based approval**

Run: `cargo test -p lato-agent interactive_streams_deltas_and_approves_at_tool_boundary && cargo test --test policy_runtime_wiring`

Expected: new tests fail because `ToolApproval` still receives name and arguments and actor checks names.

- [ ] **Step 3: Convert the approval trait and console renderer**

Change the trait to:

```rust
#[async_trait]
pub trait ToolApproval: Send + Sync {
    async fn approve(&self, request: &lato_core::ApprovalRequest) -> bool;
}
```

The console renderer prints canonical tool, capabilities, side effect, and the request's redacted summary. It must not print raw arguments. Update actor test fakes to accept the structured request.

- [ ] **Step 4: Replace actor name checks with the two-phase runtime flow**

In `process_tool_call`, create `ToolContext { execution_grant: None, ... }`, call `prepare`, inspect the returned decision, prompt only for `RequireApproval`, call `runtime.approve`, and call `runtime.execute`. Convert denial and grant errors to the existing bounded tool-result format so the model sees a stable failure instead of terminating the session.

Do not change repetition detection, persist-before-execute order, output bounding, cancellation, history, or event ordering.

- [ ] **Step 5: Add the exact built-in compatibility bridge**

In `LegacyDispatchTool::invoke`, require `context.execution_grant.is_some()` for every invocation. Only descriptors with `WorkspaceMutation` or `ExternalMutation` issue `environment.trust.allow_once()` immediately before `dispatch`. Remove actor use of the legacy counter. Keep dispatcher `require_mutating_approval` as the final compatibility check.

This bridge code must be adjacent:

```rust
if matches!(self.descriptor.side_effect, SideEffect::WorkspaceMutation | SideEffect::ExternalMutation) {
    if context.execution_grant.is_none() {
        return Err(tool_error("policy.grant_missing", "tool execution grant is missing"));
    }
    self.environment.trust.allow_once();
}
```

- [ ] **Step 6: Run actor, tool, CLI compilation, and integration tests**

Run: `cargo test -p lato-agent && cargo test -p lato-tools && cargo test --test policy_runtime_wiring && cargo test --test cli_headless`

Expected: all pass; existing Ask approval remains interactive and custom mutating tools are covered without actor edits.

- [ ] **Step 7: Commit only intentional hunks**

Because several files are pre-modified, inspect `git diff` and use `git add -p` for `actor.rs`, `dispatch.rs`, and `cli.rs`.

```bash
git add -p crates/lato-agent/src/actor.rs crates/lato-tools/src/dispatch.rs src/cli.rs
git add crates/lato-agent/Cargo.toml crates/lato-tools/src/builtin_adapter.rs tests/policy_runtime_wiring.rs Cargo.lock
git commit -m "feat: bind actor approvals to exact tool calls"
```

---

### Task 6: Typed Sandbox Obligations and Fail-Closed Preparation

**Files:**
- Modify: `crates/lato-workspace/Cargo.toml`
- Modify: `crates/lato-workspace/src/sandbox.rs`
- Modify: `crates/lato-workspace/src/lib.rs`
- Modify: `crates/lato-tools/src/shell.rs`
- Modify: `crates/lato-policy/src/sandbox.rs`
- Create: `crates/lato-workspace/tests/sandbox_obligation.rs`
- Create: `crates/lato-tools/tests/sandbox_policy.rs`

**Interfaces:**
- Consumes: `SandboxObligation` from Task 1.
- Produces: `SandboxBackend` trait, `HostSandboxBackend::readiness`, `HostSandboxBackend::prepare`, and `validate_sandbox_obligation`.

- [ ] **Step 1: Write failing backend readiness and no-fallback tests**

Use a backend constructed with an explicit missing wrapper path and assert:

```rust
let error = backend.prepare(&workspace_obligation(temp.path()), "echo forbidden").unwrap_err();
assert_eq!(error.code(), "sandbox.unavailable");
assert!(!temp.path().join("forbidden").exists());
```

Add tests for a read-only obligation rejecting writable roots, a workspace obligation rejecting a root outside the workspace, and Off being accepted only when the session-level obligation explicitly contains Off.

- [ ] **Step 2: Run focused tests and verify typed APIs are absent**

Run: `cargo test -p lato-workspace --test sandbox_obligation && cargo test -p lato-tools --test sandbox_policy`

Expected: compilation fails for `SandboxBackend` and typed errors.

- [ ] **Step 3: Move `SandboxProfile` ownership to core and re-export it**

Add `lato-core` to `lato-workspace` dependencies. Remove the local enum definition and `pub use lato_core::SandboxProfile` from the sandbox module so existing `lato_workspace::SandboxProfile` imports keep compiling.

- [ ] **Step 4: Implement backend contract over the existing wrapper**

```rust
pub trait SandboxBackend: Send + Sync {
    fn readiness(&self, profile: SandboxProfile) -> SandboxReadiness;
    fn prepare(&self, obligation: &SandboxObligation, command: &str)
        -> Result<SandboxCommand, SandboxError>;
}
```

`HostSandboxBackend::prepare` validates the obligation, then delegates to `wrap_shell_command_with`. Map missing wrappers to `sandbox.unavailable`, unsupported platform behavior to `sandbox.unsupported`, and other preparation failures to `sandbox.preparation_failed`. Never call `native_shell` after an error for Workspace or ReadOnly.

- [ ] **Step 5: Make process tools consume the typed obligation**

Change terminal execution to receive the grant's sandbox obligation rather than reading only `SessionTrust.sandbox`. Build the command with the backend before spawning. Assemble child environment with `env_clear()` plus the explicit safe keys needed for shell execution; do not forward variables whose names contain `TOKEN`, `SECRET`, `PASSWORD`, `API_KEY`, or `AUTHORIZATION`.

- [ ] **Step 6: Run sandbox and regression tests**

Run: `cargo test -p lato-workspace && cargo test -p lato-policy && cargo test -p lato-tools`

Expected: all tests pass on the host platform; unavailable non-Off sandbox tests prove fail-closed behavior.

- [ ] **Step 7: Commit**

```bash
git add crates/lato-workspace crates/lato-policy/src/sandbox.rs crates/lato-tools/src/shell.rs crates/lato-tools/tests/sandbox_policy.rs Cargo.lock
git commit -m "feat: enforce typed sandbox obligations"
```

---

### Task 7: Redacted Policy Events and Offline Doctor Service

**Files:**
- Create: `crates/lato-policy/src/event.rs`
- Create: `crates/lato-policy/src/redaction.rs`
- Modify: `crates/lato-policy/src/lib.rs`
- Modify: `crates/lato-policy/src/engine.rs`
- Modify: `crates/lato-tools/src/runtime.rs`
- Create: `src/doctor.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Create: `tests/doctor_cli.rs`

**Interfaces:**
- Produces: `PolicyEvent`, `PolicyEventKind`, `PolicyEventSink`, `NoopPolicyEventSink`, and `redact_text`.
- Produces: `DoctorOptions`, `DoctorReport`, `DoctorCheck`, `DoctorStatus`, and `doctor::run`.

- [ ] **Step 1: Write failing redaction and offline Doctor tests**

Seed `LATO_TEST_SECRET=doctor-super-secret-7319`, write the same marker into a temporary credential fixture, run Doctor through an injectable network probe that panics if called, and assert:

```rust
assert!(!human.contains("doctor-super-secret-7319"));
assert!(!json.contains("doctor-super-secret-7319"));
assert_eq!(probe.calls(), 0);
assert_eq!(report.schema_version, 1);
```

Add event tests that inspect serialized policy denied/completed events and prove arguments and prompt text are absent while IDs, canonical tool name, decision, elapsed milliseconds, and output byte count remain.

- [ ] **Step 2: Run tests and verify Doctor/event modules are absent**

Run: `cargo test -p lato-policy event && cargo test --test doctor_cli`

Expected: compilation fails for event and Doctor APIs.

- [ ] **Step 3: Implement redacted event contracts**

Use a closed event payload rather than arbitrary JSON:

```rust
pub struct PolicyEvent {
    pub kind: PolicyEventKind,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub call_id: Option<ToolCallId>,
    pub tool_name: Option<ToolName>,
    pub argument_digest: Option<String>,
    pub code: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub output_bytes: Option<usize>,
}

pub trait PolicyEventSink: Send + Sync { fn emit(&self, event: PolicyEvent); }
```

Emit evaluated/requested/consumed/denied from the engine and started/completed/failed from runtime. The no-op sink is the default. `redact_text` masks credential-like `key=value`, bearer headers, and known secret values supplied by the caller.

- [ ] **Step 4: Implement Doctor as an offline service**

`DoctorReport` contains `schema_version: 1`, `status`, and ordered checks. Default checks cover version/platform, Lato home accessibility, settings parsing, configured model lookup, credential presence without value access, ToolCatalog construction/count, fixed policy self-test matrix, sandbox readiness, and project/plugin trust summaries.

Use this call boundary:

```rust
pub async fn run(options: DoctorOptions, deps: &DoctorDependencies) -> DoctorReport;
pub fn render_human(report: &DoctorReport) -> String;
pub fn exit_code(report: &DoctorReport, strict: bool) -> i32;
```

`DoctorDependencies` includes an injectable live probe. Invoke it only when `options.live` is true and wrap it in `tokio::time::timeout(Duration::from_secs(5), ...)`.

- [ ] **Step 5: Run redaction and service tests**

Run: `cargo test -p lato-policy && cargo test --test doctor_cli`

Expected: all tests pass; default Doctor has zero probe calls and no seeded secret appears.

- [ ] **Step 6: Commit**

```bash
git add crates/lato-policy src/doctor.rs src/main.rs tests/doctor_cli.rs Cargo.toml Cargo.lock
git commit -m "feat: add policy diagnostics and doctor service"
```

---

### Task 8: CLI Contract, Beta Gate, Installation, and Smoke Tests

**Files:**
- Modify: `src/cli.rs`
- Modify: `tests/cli_headless.rs`
- Modify: `tests/doctor_cli.rs`
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`

**Interfaces:**
- Consumes: Task 7 Doctor service.
- Produces: `lato doctor [--json] [--strict] [--live]` and documented public Beta gate status.

- [ ] **Step 1: Add failing command-line contract tests**

Run the test binary with temporary `LATO_HOME` and assert:

```rust
let output = lato(&["doctor", "--json"], &home);
assert!(output.status.success());
let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
assert_eq!(report["schema_version"], 1);
assert!(report["checks"].as_array().unwrap().iter().any(|c| c["id"] == "tool_catalog"));
```

Add cases for unknown flags returning `2`, ordinary warnings returning `0`, `--strict` warnings returning `1`, and help listing all four Doctor forms.

- [ ] **Step 2: Run CLI tests and observe missing routing**

Run: `cargo test --test doctor_cli --test cli_headless`

Expected: the new command tests fail with `error: unknown command`.

- [ ] **Step 3: Add thin CLI parsing and rendering**

Before `acp`, `login`, and `-p` routing, recognize `doctor`. Accept only `--json`, `--strict`, and `--live`; reject duplicate or unknown flags with exit `2`. Call the service, print JSON to stdout for `--json`, otherwise print the human report, then return `doctor::exit_code`.

Update help text exactly to include:

```text
lato doctor [--json] [--strict] [--live]
```

Use narrow patching because `src/cli.rs` and `tests/cli_headless.rs` contain pre-existing user changes.

- [ ] **Step 4: Run focused and full verification**

Run:

```bash
cargo test --test doctor_cli --test policy_runtime_wiring --test tool_runtime_wiring
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: every test passes and Clippy reports no warnings.

- [ ] **Step 5: Update Beta-facing documentation with measured results**

In `README.md`, document Doctor usage, offline default, opt-in live probe, approval fingerprinting, and sandbox fail-closed behavior. In the acceptance results document, record the actual test count, Clippy result, install result, and smoke commands from this task; do not claim public Beta ready if any required gate failed.

- [ ] **Step 6: Install the exact workspace build**

Run: `cargo install --path .`

Expected: Cargo installs or replaces the `lato` executable successfully under the active Cargo bin directory.

- [ ] **Step 7: Smoke-test the installed command**

Run:

```bash
lato doctor
lato doctor --json
```

Expected: both commands complete without network access, reveal no secret values, and return an exit status matching the report.

Run the existing deterministic model fixture smoke for a read tool, then an Ask-mode write approval test and a sandbox-unavailable denial fixture. Expected: read succeeds, exact approved write succeeds once, replay is denied, and sandbox failure does not execute the command.

- [ ] **Step 8: Verify protected work and commit the release gate**

Compare `git status --short` and the saved pre-task dirty-file list. Confirm every pre-existing user change remains present. Stage the CLI hunks interactively and the clean documentation/test files explicitly:

```bash
git add -p src/cli.rs tests/cli_headless.rs README.md
git add tests/doctor_cli.rs docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
git commit -m "feat: expose public beta doctor and policy gate"
```

Record the final commit IDs, test totals, installed binary path, Doctor status, and any remaining Beta blocker in the handoff.

---

## Self-Review Checklist

- Spec coverage: Tasks 1–6 cover capability policy, exact approvals, legacy compatibility, and sandbox obligations; Task 7 covers redacted diagnostics and offline Doctor; Task 8 covers CLI, release verification, installation, and Beta status.
- Bypass coverage: runtime direct invocation, aliases, replacement tools, legacy allowance timing, grant replay, concurrent consumption, sandbox absence, and untrusted extensions each have an explicit test.
- Type consistency: `PolicyRequest` feeds `PolicyEngine`; `PreparedToolCall` retains its fingerprint and decision; `ExecutionGrant` is issued and consumed by the same engine; `ToolContext` receives the consumed grant; Sandbox derives from the grant-bound obligation.
- Secret handling: neither policy contracts nor event structs contain raw prompts or arguments, and Doctor redaction tests seed recognizable secrets.
- Deployment: the final task includes full tests, strict Clippy, `cargo install --path .`, installed-binary Doctor checks, and representative tool smokes.
