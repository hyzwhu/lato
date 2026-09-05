# Lato Phase 4C3 Grok-Style Context Overflow Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Lato recover once from provider context overflow, preflight oversized tool results, compact through a bounded input ladder, suppress repeated deterministic failures, and use Grok Build-style asynchronous two-pass prefire.

**Architecture:** Preserve Lato's ownership split: `lato-ai` emits typed provider failures, `lato-agent` owns context decisions and speculative state, and `lato-runtime` remains the only component allowed to install compacted history. Prefire produces an ephemeral NOTE1 through a non-installing driver request; every final summary still passes through the existing checkpoint-first replacement path.

**Tech Stack:** Rust 2024, Tokio, async-trait, futures, serde/serde_json, reqwest, existing ACP/TUI protocol and JSONL session store.

## Global Constraints

- Grok Build reference is commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` under `/Users/huangyongzhao/Documents/work/grok-build`.
- Default automatic compaction threshold remains 85 percent.
- Default prefire lead is 10 percentage points, so prefire starts at 75 percent.
- Pass one covers 95 percent of estimated token weight and NOTE1 is capped at 12,000 characters.
- Final compaction input degradation is monotonic: `Prepared -> Fitted -> Lossy -> terminal`.
- One final compaction may make at most three total summary-model attempts across pass two and all single-pass input stages; speculative pass one has its own single attempt before the final compaction operation exists.
- A provider-rejected sampling step may be rebuilt and resubmitted at most once and only before any response event.
- Manual `/compact` ignores automatic-compaction suppression.
- Speculative prefire never replaces history, writes a compaction checkpoint, or appears as assistant output.
- Every installed replacement uses Phase 4C1 checkpoint-first persistence and reconciliation.
- Preserve all unrelated user work; stage and commit only files named by the current task.
- Run relevant tests before deploying and finish with `cargo install --path .` from the repository root.

---

## File structure

- `crates/lato-core/src/model.rs`: typed model failure kind and optional provider metadata.
- `crates/lato-core/src/compaction.rs`: provider-overflow trigger and public compaction policy constants.
- `crates/lato-core/tests/model_port.rs`: serde and constructor compatibility tests for typed failures.
- `crates/lato-core/tests/compaction_contract.rs`: stable trigger/policy contracts.
- `crates/lato-ai/src/provider_error.rs`: provider-neutral HTTP/body context-overflow classification.
- `crates/lato-ai/src/api.rs`: preserve structured HTTP failure data for sampling calls.
- `crates/lato-ai/src/stream.rs`: propagate typed failures and response-start state through legacy streaming.
- `crates/lato-ai/src/models_file.rs`: propagate the same typed failure contract for configured models.
- `crates/lato-ai/src/codex/{mod.rs,sse.rs,websocket.rs,events.rs}`: Codex transport classification and output-start propagation.
- `crates/lato-ai/src/model_port_adapter/{legacy_port.rs,stream_adapter.rs}`: keep typed failure metadata across old/new model boundaries.
- `crates/lato-ai/src/lib.rs`: export the new classifier/types.
- `crates/lato-ai/tests/provider_error.rs`: structured and compatibility classifier fixtures.
- `crates/lato-agent/src/context_recovery.rs`: four-state automatic suppression and one-credit overflow recovery policy.
- `crates/lato-agent/src/compaction_input.rs`: Prepared/Fitted/Lossy construction and UTF-8-safe fitting.
- `crates/lato-agent/src/two_pass.rs`: token-weighted split, tool-boundary snapping, NOTE1 handling, fingerprints, and pass histories.
- `crates/lato-agent/src/context_usage.rs`: own recovery/suppression state beside the existing ledger.
- `crates/lato-agent/src/actor.rs`: sampling-boundary orchestration, preflight, prefire, and compact-and-resubmit.
- `crates/lato-agent/src/legacy_driver.rs`: bounded compaction sampler and prefire/pass-two driver implementation.
- `crates/lato-agent/src/lib.rs`: crate exports for new focused modules.
- `crates/lato-agent/tests/{context_recovery.rs,compaction_runtime.rs}`: policy, ladder, prefire, and agent/runtime integration.
- `crates/lato-runtime/src/driver.rs`: non-installing prefire protocol and optional two-pass input on actual compaction.
- `crates/lato-runtime/src/session.rs`: service prefire requests during an active turn without entering compaction state.
- `crates/lato-runtime/tests/session_runtime.rs`: runtime isolation, cancellation, and persistence tests.
- `src/client.rs`, `src/tui/{state.rs,widgets.rs}`: new trigger labels and suppression diagnostics.
- `tests/session_compaction_cli.rs`: ACP/restart/duplicate-output end-to-end tests.
- `README.md`, `docs/superpowers/reference/lato-upstream-sources.md`: delivered behavior and source attribution.

---

### Task 1: Carry typed provider failures through every model boundary

**Files:**

- Modify: `crates/lato-core/src/model.rs`
- Modify: `crates/lato-core/tests/model_port.rs`
- Modify: `crates/lato-ai/src/stream.rs`
- Modify: `crates/lato-ai/src/models_file.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/legacy_port.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`
- Modify: `crates/lato-agent/tests/legacy_driver.rs`
- Modify: `crates/lato-agent/tests/runtime_session.rs`
- Modify: `src/client.rs`
- Modify: `tests/session_compaction_cli.rs`

**Interfaces:**

- Produces: `ModelErrorKind`, `ModelError::{with_status,with_context_window,with_output_started}`, and `ModelStream` methods returning `ModelError` instead of `String`.
- Consumes: existing `ModelError`, `ModelCallReport`, `ModelStreamEvent`, and `Retryability`.

- [ ] **Step 1: Write failing core contract tests**

Add tests that prove defaults preserve old call sites and metadata survives serde:

```rust
#[test]
fn model_error_metadata_is_optional_and_round_trips() {
    let old = ModelError::new("model.failed", "failed", Retryability::Never);
    assert_eq!(old.kind, ModelErrorKind::Other);
    assert_eq!(old.status_code, None);
    assert_eq!(old.context_window, None);
    assert!(!old.output_started);

    let typed = old
        .with_kind(ModelErrorKind::ContextOverflow)
        .with_status(400)
        .with_context_window(128_000)
        .with_output_started(true);
    let json = serde_json::to_value(&typed).unwrap();
    assert_eq!(serde_json::from_value::<ModelError>(json).unwrap(), typed);
}
```

- [ ] **Step 2: Run the failing core test**

Run: `cargo test -p lato-core --test model_port model_error_metadata_is_optional_and_round_trips -- --nocapture`

Expected: compilation fails because `ModelErrorKind` and metadata builders do not exist.

- [ ] **Step 3: Extend the canonical failure contract**

Add this stable enum and backward-compatible fields:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    ContextOverflow,
    Authentication,
    Credit,
    RateLimited,
    InvalidRequest,
    Transport,
    Cancelled,
    #[default]
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ModelError {
    pub code: String,
    pub message: String,
    pub retryability: Retryability,
    #[serde(default)]
    pub kind: ModelErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub output_started: bool,
}
```

Keep `ModelError::new` source-compatible by initializing the new fields, add the four chainable builders, and make `cancelled()` use `ModelErrorKind::Cancelled`.

- [ ] **Step 4: Write failing adapter propagation tests**

In the adapter tests, create a stream that emits one text piece and then returns a typed overflow. Assert the error emerging from the canonical adapter has `kind == ContextOverflow` and `output_started == true`. Add a second fixture that fails before any piece and preserves `output_started == false`.

```rust
assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
assert_eq!(error.status_code, Some(400));
assert_eq!(error.context_window, Some(128_000));
assert_eq!(error.output_started, saw_text);
```

- [ ] **Step 5: Change `ModelStream` to return `ModelError`**

Use this exact trait shape:

```rust
#[async_trait]
pub trait ModelStream: Send + Sync {
    fn active_model_port(&self) -> Option<ActiveModelPort> { None }

    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError>;

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError>;
}
```

Convert legacy string failures at their origin. In both direction adapters,
track whether any piece/event was forwarded and overwrite only
`error.output_started` with `error.output_started || observed_output`; never
flatten a typed error through `to_string()`.

- [ ] **Step 6: Run model boundary tests**

Run: `cargo test -p lato-core --test model_port && cargo test -p lato-ai model_port_adapter -- --nocapture && cargo test -p lato-ai stream -- --nocapture`

Expected: all tests pass and no `ModelStream` implementation returns `String`.

- [ ] **Step 7: Commit Task 1**

```bash
git add crates/lato-core/src/model.rs crates/lato-core/tests/model_port.rs crates/lato-ai/src/stream.rs crates/lato-ai/src/models_file.rs crates/lato-ai/src/model_port_adapter
git add crates/lato-agent/tests/compaction_runtime.rs crates/lato-agent/tests/legacy_driver.rs crates/lato-agent/tests/runtime_session.rs src/client.rs tests/session_compaction_cli.rs
git commit -m "feat: preserve typed model sampling failures"
```

Before committing, inspect `git diff --cached --name-only` and unstage any unrelated user-owned file.

---

### Task 2: Classify provider context overflow without generic-400 false positives

**Files:**

- Create: `crates/lato-ai/src/provider_error.rs`
- Create: `crates/lato-ai/tests/provider_error.rs`
- Modify: `crates/lato-ai/src/lib.rs`
- Modify: `crates/lato-ai/src/api.rs`
- Modify: `crates/lato-ai/src/stream.rs`
- Modify: `crates/lato-ai/src/codex/mod.rs`
- Modify: `crates/lato-ai/src/codex/sse.rs`
- Modify: `crates/lato-ai/src/codex/websocket.rs`
- Modify: `crates/lato-ai/src/codex/events.rs`

**Interfaces:**

- Consumes: Task 1 `ModelErrorKind` and metadata builders.
- Produces: `classify_provider_failure(status, body) -> ModelError` and transport-specific output-start decoration.

- [ ] **Step 1: Add failing structured and compatibility fixtures**

Cover these cases in `crates/lato-ai/tests/provider_error.rs`:

```rust
#[test]
fn structured_context_length_error_is_typed() {
    let error = classify_provider_failure(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"maximum context length is 128000 tokens"}}"#,
    );
    assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
    assert_eq!(error.status_code, Some(400));
    assert_eq!(error.context_window, Some(128_000));
}

#[test]
fn generic_bad_request_is_not_context_overflow() {
    let error = classify_provider_failure(400, r#"{"error":{"message":"invalid tool schema"}}"#);
    assert_eq!(error.kind, ModelErrorKind::InvalidRequest);
}
```

Also test 401 authentication, 402/credit phrases, 429 rate limiting, 413 with a context-length code, known plain-text compatibility phrases, malformed JSON, and a 400 mentioning only `max_output_tokens`.

- [ ] **Step 2: Run the failing classifier tests**

Run: `cargo test -p lato-ai --test provider_error -- --nocapture`

Expected: compilation fails because `provider_error` and its export do not exist.

- [ ] **Step 3: Implement a bounded classifier**

Create a pure classifier with this public entry point:

```rust
pub fn classify_provider_failure(status: u16, body: &str) -> ModelError
```

Inspect JSON pointers `/error/code`, `/error/type`, `/error/message`,
`/error/context_window`, and `/context_window`. Accept context-overflow codes
such as `context_length_exceeded`, `context_window_exceeded`, and
`prompt_too_long`. The compatibility text list must require a context noun and
an exceeded/too-long predicate. Extract a positive token limit from structured
numeric fields first and from a bounded decimal adjacent to `context` only as
fallback. Cap the stored message at 4,096 safe characters.

- [ ] **Step 4: Route HTTP and Codex failures through the classifier**

Keep catalog/discovery APIs source-compatible, but add a typed sampling helper:

```rust
pub async fn send_sampling_response(
    client: &reqwest::Client,
    spec: &HttpRequestSpec,
) -> Result<reqwest::Response, ModelError>
```

Use it from ordinary streaming. Map Codex SSE and WebSocket setup/event
failures through the same classifier when an HTTP status/body exists. Preserve
the existing WebSocket-to-SSE fallback rule: fallback is allowed only before
any response event, and a failed fallback reports the last typed failure.

- [ ] **Step 5: Run provider and transport tests**

Run: `cargo test -p lato-ai --test provider_error && cargo test -p lato-ai codex -- --nocapture && cargo test -p lato-ai stream_repair -- --nocapture`

Expected: all tests pass; a generic 400 remains `InvalidRequest` and overflow metadata survives both transports.

- [ ] **Step 6: Commit Task 2**

```bash
git add crates/lato-ai/src/provider_error.rs crates/lato-ai/tests/provider_error.rs crates/lato-ai/src/lib.rs crates/lato-ai/src/api.rs crates/lato-ai/src/stream.rs crates/lato-ai/src/codex
git commit -m "feat: classify provider context overflow"
```

---

### Task 3: Add bounded automatic-recovery and suppression policy

**Files:**

- Modify: `crates/lato-core/src/compaction.rs`
- Modify: `crates/lato-core/tests/compaction_contract.rs`
- Create: `crates/lato-agent/src/context_recovery.rs`
- Create: `crates/lato-agent/tests/context_recovery.rs`
- Modify: `crates/lato-agent/src/context_usage.rs`
- Modify: `crates/lato-agent/src/lib.rs`

**Interfaces:**

- Consumes: Task 1 `ModelErrorKind` and Phase 4C2 `ContextTracker`.
- Produces: `CompactionTrigger::ProviderOverflow`, `AutoCompactionSuppression`, `SuppressionReason`, `SamplingRecoveryBudget`, and clear-condition methods.

- [ ] **Step 1: Write failing policy tests**

Test exact scope and reset rules:

```rust
#[test]
fn suppression_clear_conditions_match_grok_build() {
    let mut state = AutomaticRecoveryState::default();
    state.suppress(SuppressionReason::Size);
    state.on_new_turn();
    assert_eq!(state.suppression(), AutoCompactionSuppression::Sticky);
    state.on_context_budget_changed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);

    state.suppress(SuppressionReason::Credit);
    state.on_context_budget_changed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::UntilSuccess);
    state.on_provider_success();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);

    state.suppress(SuppressionReason::Auth);
    state.on_provider_success();
    assert_eq!(state.suppression(), AutoCompactionSuppression::Auth);
    state.on_auth_refreshed();
    assert_eq!(state.suppression(), AutoCompactionSuppression::None);
}

#[test]
fn overflow_recovery_credit_is_single_use() {
    let mut budget = SamplingRecoveryBudget::default();
    assert!(budget.try_use_overflow_recovery());
    assert!(!budget.try_use_overflow_recovery());
}
```

Also assert manual compaction bypasses suppression and all automatic triggers do not.

- [ ] **Step 2: Run the failing policy tests**

Run: `cargo test -p lato-agent --test context_recovery -- --nocapture`

Expected: compilation fails because the recovery types do not exist.

- [ ] **Step 3: Add stable trigger and state types**

Add `ProviderOverflow` to `CompactionTrigger`. Implement:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutoCompactionSuppression {
    #[default]
    None,
    Turn,
    Sticky,
    UntilSuccess,
    Auth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuppressionReason { Size, Schema, Credit, Auth, Other }

#[derive(Debug, Default)]
pub struct SamplingRecoveryBudget { overflow_used: bool }
```

`Size` and `Schema` map to `Sticky`, `Credit` to `UntilSuccess`, `Auth` to
`Auth`, and `Other` to `Turn`. Define the owner explicitly:

```rust
#[derive(Debug, Default)]
pub struct AutomaticRecoveryState {
    suppression: AutoCompactionSuppression,
}
```

Store `AutomaticRecoveryState` inside `ContextTracker` and expose narrow
delegation methods instead of its fields.

- [ ] **Step 4: Add error-to-suppression classification tests and implementation**

Use typed kinds before message compatibility:

```rust
assert_eq!(suppression_reason(&auth_error), SuppressionReason::Auth);
assert_eq!(suppression_reason(&credit_error), SuppressionReason::Credit);
assert_eq!(suppression_reason(&overflow_error), SuppressionReason::Size);
assert_eq!(suppression_reason(&schema_error), SuppressionReason::Schema);
```

Only `ModelErrorKind::Other` may use the bounded legacy phrase list. Do not map
rate limiting or transport interruption to a deterministic sticky state.

- [ ] **Step 5: Run core and policy tests**

Run: `cargo test -p lato-core --test compaction_contract && cargo test -p lato-agent --test context_recovery && cargo test -p lato-agent --test context_usage`

Expected: all tests pass and the old trigger serde fixtures remain readable.

- [ ] **Step 6: Commit Task 3**

```bash
git add crates/lato-core/src/compaction.rs crates/lato-core/tests/compaction_contract.rs crates/lato-agent/src/context_recovery.rs crates/lato-agent/tests/context_recovery.rs crates/lato-agent/src/context_usage.rs crates/lato-agent/src/lib.rs
git commit -m "feat: bound automatic context recovery policy"
```

---

### Task 4: Implement the Prepared, Fitted, and Lossy input ladder

**Files:**

- Create: `crates/lato-agent/src/compaction_input.rs`
- Modify: `crates/lato-agent/src/compaction.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`

**Interfaces:**

- Consumes: existing `prepare_compaction_messages`, `find_compaction_anchors`, and `CompactionPolicy`.
- Produces: `CompactionInputStage`, `CompactionInputStage::next`, `prepare_compaction_input`, and a single three-attempt budget across stages.

- [ ] **Step 1: Write failing pure ladder tests**

Cover stage order and preservation:

```rust
assert_eq!(CompactionInputStage::Prepared.next(), Some(CompactionInputStage::Fitted));
assert_eq!(CompactionInputStage::Fitted.next(), Some(CompactionInputStage::Lossy));
assert_eq!(CompactionInputStage::Lossy.next(), None);

let fitted = prepare_compaction_input(&source, CompactionInputStage::Fitted, 12_000)?;
assert_eq!(fitted.first().unwrap().role, ModelRole::System);
assert!(message_text(fitted.last().unwrap()).contains("latest objective"));
assert!(!begins_with_orphan_tool_result(&fitted));
```

Add a multibyte tool result, a tool call with multiple results, a prior
compaction summary, a budget smaller than the latest unit, and saturating zero-budget cases.

- [ ] **Step 2: Run the failing ladder tests**

Run: `cargo test -p lato-agent compaction_input -- --nocapture`

Expected: compilation fails because `compaction_input` does not exist.

- [ ] **Step 3: Implement whole-unit fitting and safe truncation**

Define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionInputStage { Prepared, Fitted, Lossy }

pub fn prepare_compaction_input(
    source: &[ModelMessage],
    stage: CompactionInputStage,
    input_budget_tokens: u64,
) -> Result<Vec<ModelMessage>, CompactionError>;
```

Use serialized-byte/4 estimates consistently with `ContextTracker`. Keep the
system message outside the body budget, group assistant tool calls with their
following tool results, select newest complete groups backward, and use a
UTF-8 boundary-safe truncator only when no complete final group fits. The
marker is `\n[... truncated {dropped} bytes to fit the compaction window ...]`.

Lossy starts from a representation that removes old tool-result bodies but
retains tool names and outcome markers. It must retain prior compaction summary
text and the latest real user objective.

- [ ] **Step 4: Replace the upfront `InputTooLarge` exit with the ladder**

In `run_compaction`, compute:

```rust
let fitted_budget = context_window.saturating_sub(policy.summary_reserve_tokens);
let lossy_budget = context_window
    .saturating_mul(7)
    .checked_div(10)
    .unwrap_or_default()
    .saturating_sub(policy.summary_reserve_tokens);
```

Start at `Prepared`. Before a model call, advance if the local estimate cannot
fit. After a typed context-overflow error, advance and rebuild from the
original request messages. Increment the same `attempt` counter for every
provider submission. Return `compaction.input_too_large` after Lossy or the
third total attempt.

- [ ] **Step 5: Add driver integration tests**

Script the compaction port to reject Prepared and Fitted with typed overflow,
then accept Lossy. Assert exactly three requests, decreasing serialized sizes,
one final checkpoint candidate, and preservation of the latest objective.
Add a three-overflow case that terminates with `compaction.input_too_large` and
never makes a fourth request.

- [ ] **Step 6: Run compaction tests**

Run: `cargo test -p lato-agent compaction_input -- --nocapture && cargo test -p lato-agent --test compaction_runtime -- --nocapture`

Expected: all ladder and existing manual/automatic compaction tests pass.

- [ ] **Step 7: Commit Task 4**

```bash
git add crates/lato-agent/src/compaction_input.rs crates/lato-agent/src/compaction.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/lib.rs crates/lato-agent/tests/compaction_runtime.rs
git commit -m "feat: degrade compaction input through bounded stages"
```

---

### Task 5: Add two-pass prefire helpers and a non-installing runtime request

**Files:**

- Create: `crates/lato-agent/src/two_pass.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-runtime/src/driver.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`

**Interfaces:**

- Produces: `TwoPassSplit`, `PrefireCompactionRequest`, `PrefireCompactionResult`, `TwoPassCompactionInput`, and `TurnEventEmitter::prefire_compaction`.
- Consumes: Task 4 prepared input and existing `TurnDriver::compact` model sampling path.

- [ ] **Step 1: Write failing pure two-pass tests**

Port the pinned Grok invariants:

```rust
let split = split_for_two_pass(&history, 95);
assert!(split.prefix_tokens * 100 >= split.total_tokens * 95);
assert!(!split_severs_tool_pair(&history, split.index));

let note = note_for_pass_two(&format!("<summary>{}</summary>", "x".repeat(1001)));
assert_eq!(note, "x".repeat(1001));
assert!(note_for_pass_two(&"x".repeat(13_000)).chars().count() < 12_100);
```

Test prefix fingerprint changes on role/text/order mutation and stays stable
when only items after `prefix_len` are appended.

- [ ] **Step 2: Implement pure two-pass builders**

Expose crate-private functions:

```rust
pub(crate) const TWO_PASS_SPLIT_PERCENT: u8 = 95;
pub(crate) const PREFIRE_LEAD_PERCENT: u8 = 10;
pub(crate) const MAX_NOTE1_CHARS: usize = 12_000;

pub(crate) struct TwoPassSplit {
    pub index: usize,
    pub prefix_tokens: u64,
    pub total_tokens: u64,
}

pub(crate) fn split_for_two_pass(history: &[ModelMessage], percent: u8) -> TwoPassSplit;
pub(crate) fn fingerprint_prefix(history: &[ModelMessage], prefix_len: usize) -> u64;
pub(crate) fn note_for_pass_two(raw: &str) -> String;
pub(crate) fn build_pass_one_history(prefix: &[ModelMessage], prompt: &str) -> Vec<ModelMessage>;
pub(crate) fn build_pass_two_history(prefix: &[ModelMessage], tail: &[ModelMessage], note1: &str, prompt: &str) -> Vec<ModelMessage>;
```

Use a deterministic repository-owned hasher implementation rather than
`DefaultHasher`, whose stability is not a persistence contract. The cache is
ephemeral, but deterministic tests make invalidation auditable.

- [ ] **Step 3: Write failing runtime isolation tests**

Create a driver whose prefire blocks on a `Notify`. While it is blocked, assert
the runtime still accepts model deltas and cancellation, emits no
`CompactionStarted`, writes no compaction journal record, and never calls
`install_history`.

- [ ] **Step 4: Add the runtime protocol**

Use these shapes in `lato-runtime/src/driver.rs`:

```rust
#[derive(Clone, Debug)]
pub struct PrefireCompactionRequest {
    pub messages: Vec<ModelMessage>,
    pub prefix_len: usize,
    pub policy: CompactionPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefireCompactionResult { pub note1: String }

#[derive(Clone, Debug)]
pub struct TwoPassCompactionInput {
    pub note1: String,
    pub prefix_len: usize,
}
```

Add `two_pass: Option<TwoPassCompactionInput>` to actual automatic and driver
compaction requests. Add `TurnDriver::prefire_compaction` and
`TurnEventEmitter::prefire_compaction`. The session validates the owning turn,
spawns the driver call with the active turn cancellation child token, and
replies without changing `SessionPhase` or writing journal records.

- [ ] **Step 5: Implement pass-one and pass-two sampling**

`LegacyTurnDriver::prefire_compaction` snapshots the active model once, builds
pass-one input, calls the existing tool-free summary sampler, normalizes NOTE1,
rejects empty output, and returns it without validation as a final summary.

When `CompactionRequest.two_pass` is present, `run_compaction` builds pass-two
history from current system messages, NOTE1 carrier, current tail, and the
special merge instruction. If pass two errors or yields an invalid/degenerate
summary, consume the remaining global attempt budget on the Task 4 single-pass
ladder.

- [ ] **Step 6: Run two-pass and runtime tests**

Run: `cargo test -p lato-agent two_pass -- --nocapture && cargo test -p lato-runtime --test session_runtime prefire -- --nocapture && cargo test -p lato-agent --test compaction_runtime two_pass -- --nocapture`

Expected: pure builders, non-installing runtime behavior, cancellation, and single-pass fallback pass.

- [ ] **Step 7: Commit Task 5**

```bash
git add crates/lato-agent/src/two_pass.rs crates/lato-agent/src/lib.rs crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/tests/compaction_runtime.rs crates/lato-runtime/src/driver.rs crates/lato-runtime/src/session.rs crates/lato-runtime/tests/session_runtime.rs
git commit -m "feat: prefire two-pass compaction summaries"
```

---

### Task 6: Orchestrate preflight and one-shot overflow recovery in the turn loop

**Files:**

- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/context_usage.rs`
- Modify: `crates/lato-agent/src/context_recovery.rs`
- Modify: `crates/lato-agent/tests/context_recovery.rs`
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`
- Modify: `crates/lato-runtime/src/driver.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`

**Interfaces:**

- Consumes: Tasks 1-5 typed failures, suppression state, prefire cache, runtime prefire, two-pass input, and input ladder.
- Produces: exact Phase 4C3 sampling-boundary order and `ProviderOverflow` compact-and-resubmit.

- [ ] **Step 1: Add failing prefire orchestration tests**

Use a scripted model with a 100,000-token window:

```rust
// 74%: no prefire
// 75%: exactly one pass-one request
// appended tail with unchanged prefix: cache remains valid
// edited prefix or model generation change: cache is discarded
// 85%: wait for pass one, send pass two, install only NOTE2
```

Assert prefire creates no journal compaction entries and no UI delta. Assert a
pass-one or pass-two error falls back to one-stage compaction within the same
three-attempt ceiling.

- [ ] **Step 2: Store bounded ephemeral prefire state**

Add to `SessionActor`:

```rust
struct PrefireCache {
    note1: String,
    prefix_len: usize,
    fingerprint: u64,
    model_generation: u64,
    pass1_latency_ms: u64,
}

enum PrefireSlot {
    Empty,
    Running(tokio::task::JoinHandle<Result<PrefireCache, AgentError>>),
    Ready(PrefireCache),
}
```

Start at `threshold_percent.saturating_sub(10)`. Take a history and model
generation snapshot before spawning. At real compaction, await `Running`, then
validate prefix length, fingerprint, and generation. Always consume or clear a
cache after apply/fallback.

- [ ] **Step 3: Add failing tool-output preflight tests**

Script a tool returning enough bounded output to push estimated use above the
active context window. Assert the sequence is:

```text
ordinary sample -> committed tool result -> PreflightOverflow compaction -> checkpoint -> rebuilt sample
```

Assert no second ordinary provider request occurs before the checkpoint. Add a
failed preflight case that returns `context.preflight_recovery_failed` instead
of submitting a known-oversized request.

- [ ] **Step 4: Implement preflight immediately after tool processing**

After every completed tool batch, measure usage. If estimated tokens exceed
the positive active context window, call the common automatic compaction path
with `CompactionTrigger::PreflightOverflow`. On success restart the sampling
loop. On failure above the hard limit, preserve old history and return the
explicit recovery error.

- [ ] **Step 5: Add failing provider-overflow recovery tests**

Cover:

```rust
// first request: typed ContextOverflow, output_started=false
// compaction: succeeds and installs replacement
// rebuilt request: succeeds once
assert_eq!(ordinary_request_count, 2);
assert_eq!(provider_overflow_compaction_count, 1);
```

Also test a second overflow, overflow after one text delta, overflow after a
tool-call delta, suppressed automatic compaction, failed compaction preserving
the original provider error, and cancellation between checkpoint and resubmit.

- [ ] **Step 6: Implement compact-and-resubmit around provider completion**

Keep provider errors typed through the spawned stream task. Before returning an
error, evaluate:

```rust
let recoverable = !error.output_started
    && !context_tracker.auto_compaction_suppressed()
    && recovery_budget.try_use_overflow_recovery()
    && (error.kind == ModelErrorKind::ContextOverflow
        || error.context_window.is_some_and(|window| measured_tokens > window));
```

On `recoverable`, update the active context window if the provider supplied a
positive smaller value, run `ProviderOverflow` compaction, discard the old
request JSON, restart from the top, and preserve the turn ID. Do not reset the
credit after recovery compaction. Reset it only after a successful provider
response advances to the next tool-driven sampling step.

- [ ] **Step 7: Wire suppression transitions and invalidation**

At real user-turn start clear only `Turn`. On normal provider success clear
`UntilSuccess`. On credential refresh clear only `Auth`. On successful
compaction, rewind, or model context-budget change clear `Turn`/`Sticky` and
clear prefire. Every automatic entry point, including prefire, checks the same
gate; `/compact` bypasses it.

Emit one failure notification only when suppression changes from `None`.

- [ ] **Step 8: Run orchestration tests**

Run: `cargo test -p lato-agent --test context_recovery -- --nocapture && cargo test -p lato-agent --test compaction_runtime -- --nocapture && cargo test -p lato-runtime --test session_runtime compaction -- --nocapture`

Expected: all prefire, preflight, single-resubmit, suppression, cancellation, and durability sequences pass.

- [ ] **Step 9: Commit Task 6**

```bash
git add crates/lato-agent/src/actor.rs crates/lato-agent/src/context_usage.rs crates/lato-agent/src/context_recovery.rs crates/lato-agent/tests/context_recovery.rs crates/lato-agent/tests/compaction_runtime.rs crates/lato-runtime/src/driver.rs crates/lato-runtime/tests/session_runtime.rs
git commit -m "feat: recover turns from context overflow"
```

---

### Task 7: Expose recovery state, prove restart safety, document, and deploy

**Files:**

- Modify: `src/client.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/widgets.rs`
- Modify: `tests/session_compaction_cli.rs`
- Modify: `README.md`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**

- Consumes: completed Phase 4C3 runtime lifecycle and trigger values.
- Produces: localized trigger/status rendering, ACP/restart evidence, documentation, and installed `lato` binary.

- [ ] **Step 1: Add failing client/TUI state tests**

Feed lifecycle notifications with both new active triggers:

```rust
assert_eq!(trigger_label(CompactionTrigger::PreflightOverflow, Language::En), "tool-output overflow");
assert_eq!(trigger_label(CompactionTrigger::ProviderOverflow, Language::En), "provider overflow recovery");
assert_eq!(trigger_label(CompactionTrigger::PreflightOverflow, Language::ZhCn), "工具输出溢出");
assert_eq!(trigger_label(CompactionTrigger::ProviderOverflow, Language::ZhCn), "服务端上下文溢出恢复");
```

Assert no prefire-only activity creates a compaction banner or assistant/system transcript item. Add `/status` assertions for `none`, `turn`, `sticky`, `until_success`, and `auth` when the backend reports them.

- [ ] **Step 2: Implement localized rendering**

Keep the current `CompactionUiState` lifecycle. Extend trigger formatting and
the status snapshot only; do not introduce a visible prefire state. Recovery
success returns to the ordinary responding state without an extra terminal
message.

- [ ] **Step 3: Add ACP end-to-end recovery tests**

Extend `tests/session_compaction_cli.rs` with fixtures proving:

1. provider overflow -> `provider_overflow` started/completed -> one final assistant answer;
2. oversized tool output -> `preflight_overflow` before the next model request;
3. second provider overflow is terminal with no third request;
4. output-started overflow returns one error and does not replay text/tool calls;
5. restart after recovered compaction restores only checkpointed logical history;
6. speculative prefire produces no journal record;
7. old Phase 4C1/4C2 journals resume unchanged.

- [ ] **Step 4: Run UI and end-to-end tests**

Run: `cargo test tui::state -- --nocapture && cargo test --test session_compaction_cli -- --nocapture && cargo test -p lato-agent --test acp_runtime -- --nocapture`

Expected: all trigger rendering, no-duplicate-output, ACP sequence, and restart tests pass.

- [ ] **Step 5: Update documentation and attribution**

Replace the README sentence that says Phase 4C3 remains planned with delivered
behavior: 85-percent threshold, 75-percent prefire, hard tool-output preflight,
single provider-overflow resubmission, three-stage input degradation, and
suppression clear conditions.

Add source-ledger rows for:

```text
crates/lato-agent/src/context_recovery.rs
crates/lato-agent/src/compaction_input.rs
crates/lato-agent/src/two_pass.rs
crates/lato-agent/src/actor.rs (Phase 4C3 additions)
crates/lato-ai/src/provider_error.rs
crates/lato-runtime/src/session.rs (Phase 4C3 prefire additions)
```

Each substantially derived file receives a source header naming Grok Build
commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`, the exact upstream file, its
license, and the Lato-specific adaptation.

- [ ] **Step 6: Run formatting, lint, and the full test suite**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit 0. If a pre-existing unrelated test fails, record
the exact command and failure, prove focused Phase 4C3 tests still pass, and do
not alter unrelated user work to hide it.

- [ ] **Step 7: Commit Task 7**

```bash
git add src/client.rs src/tui/state.rs src/tui/widgets.rs tests/session_compaction_cli.rs README.md docs/superpowers/reference/lato-upstream-sources.md
git commit -m "docs: ship grok-style context recovery"
```

- [ ] **Step 8: Install the completed binary locally**

Run: `cargo install --path .`

Expected: exit 0 and output reports installation or replacement of the `lato` executable.

- [ ] **Step 9: Verify the installed command**

Run: `lato --version && lato --help >/dev/null`

Expected: both commands exit 0 and the version corresponds to the current workspace package.

---

## Final acceptance checklist

- [ ] Typed overflow metadata reaches the actor without string flattening.
- [ ] Generic HTTP 400 errors never trigger overflow recovery.
- [ ] Tool-output overflow compacts before another provider request.
- [ ] Provider overflow rebuilds and resubmits once before any output only.
- [ ] Prepared, Fitted, and Lossy share a three-attempt ceiling.
- [ ] Suppression lifetimes and clear conditions match the design.
- [ ] Prefire starts at 75 percent and pass one covers 95 percent by token weight.
- [ ] Prefire cache validation includes prefix length, fingerprint, and model generation.
- [ ] Failed or stale two-pass work falls back without mutating history.
- [ ] Checkpoint ordering, reconciliation, and restart recovery remain intact.
- [ ] ACP/TUI show actual recovery triggers and never show speculative output.
- [ ] Focused tests, workspace tests, Clippy, and local installation pass.
