# Lato Phase 4C3 Fault-Matrix Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden Phase 4C3 with deterministic provider, actor, runtime, persistence, and restart fault injection so context recovery never replays visible output, exceeds its retry budget, submits known-oversized requests, leaks speculative NOTE1 state, or loses a committed checkpoint.

**Architecture:** Keep fault scripts local to the test layer and exercise the real `lato-ai -> lato-agent -> lato-runtime -> ACP` path. Production changes are limited to defects demonstrated by a failing invariant test; the expected fix is an explicit cancellation checkpoint at the actor's sampling boundary after automatic compaction and before rebuilding/resubmitting a provider request.

**Tech Stack:** Rust 2024, Tokio (`Notify`, channels, `CancellationToken`, bounded `timeout`), async-trait, futures, serde/serde_json, existing in-memory/file event stores, ACP host, and Cargo workspace tests.

## Global Constraints

- The approved design is `docs/superpowers/specs/2026-09-05-lato-phase-4c3-fault-matrix-hardening-design.md`.
- Grok Build reference remains commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` under `/Users/huangyongzhao/Documents/work/grok-build`.
- Do not create a generic fault-injection framework. Fixtures belong beside the tests that consume them.
- Use channels, `Notify`, cancellation tokens, and bounded timeouts for synchronization. Do not add production sleeps or network calls.
- A provider-overflow retry is allowed once per rejected sampling step, only before any visible text, reasoning, usage, or tool-call delta.
- Cancellation is checked before every ordinary provider request, including the request rebuilt after a successful compaction checkpoint.
- A known preflight overflow must compact before sampling or fail as `context.preflight_recovery_failed`; it must never call the ordinary model with the oversized input.
- The 75/85/95 percentages remain unchanged: prefire lead at 75%, final threshold at 85%, and pass-one split at 95% of estimated token weight.
- Final compaction has one global three-attempt ceiling across pass two and the Prepared/Fitted/Lossy fallback ladder.
- Speculative NOTE1 is ephemeral: it is never installed, journaled, emitted as an assistant delta, or replayed after restart.
- Manual compaction continues to bypass automatic-compaction suppression.
- Preserve unrelated user-owned changes. Before each commit, inspect `git diff --cached --name-only` and stage only files named by that task.
- Run relevant tests before deployment. After the final feature/fix, run `cargo install --path .` from the repository root.

---

## File Structure

- `crates/lato-ai/src/model_port_adapter/legacy_port.rs`: legacy-to-canonical adapter fault matrix.
- `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`: canonical-to-legacy adapter fault matrix.
- `crates/lato-ai/tests/provider_error.rs`: structured provider classification and generic-400 negative cases.
- `crates/lato-agent/tests/context_recovery_faults.rs`: focused scripted-stream integration tests for overflow, replay, cancellation, and preflight behavior.
- `crates/lato-agent/src/actor.rs`: sampling-boundary cancellation guard and private prefire-cache invariant tests.
- `crates/lato-agent/tests/context_recovery.rs`: suppression lifecycle and manual bypass policy.
- `crates/lato-agent/tests/legacy_driver.rs`: two-pass attempt-budget and degradation tests through the real compaction driver.
- `crates/lato-runtime/tests/session_runtime.rs`: runtime event ordering, speculative isolation, cancellation, and checkpoint fault tests.
- `tests/session_compaction_cli.rs`: ACP/restart end-to-end recovery and compatibility tests.
- `docs/testing/lato-agent-test-cases.md`: append the verified Phase 4C3 hardening matrix without replacing user-authored material.

---

### Task 1: Complete the provider and adapter failure matrix

**Files:**

- Modify: `crates/lato-ai/src/model_port_adapter/legacy_port.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`
- Modify: `crates/lato-ai/tests/provider_error.rs`

- [ ] **Step 1: Add legacy adapter tests for every output boundary**

Extend `Behavior::PiecesThenFail` coverage with text and tool-call cases. Use one helper to construct the typed error:

```rust
fn overflow() -> ModelError {
    ModelError::new(
        "model.context_overflow",
        "maximum context length is 128000 tokens",
        Retryability::Never,
    )
    .with_kind(ModelErrorKind::ContextOverflow)
    .with_status(400)
    .with_context_window(128_000)
}
```

Assert all four fields after a pre-output failure, after `StreamPiece::Text`, and after `StreamPiece::ToolCall`. The pre-output case must keep `output_started == false`; both emitted-piece cases must set it to true. Assert the adapter produces one terminal error and no completion after the error.

- [ ] **Step 2: Add canonical adapter tests for partial tool deltas and fallback metadata**

Feed `ScriptedPort` a `ToolCallDelta` followed by `Err(overflow())`. Assert no completed `StreamPiece::ToolCall` is flushed, but the returned error keeps kind/status/window and has `output_started == true`. Add a pre-output case with the same metadata and `output_started == false`.

Also cover an event stream that ends without `Completed`: before output it returns `model.stream_interrupted` with `output_started == false`; after a reasoning or usage event it returns the same stable code with `output_started == true`.

- [ ] **Step 3: Strengthen provider-classifier negative cases**

Add table-driven fixtures proving that these remain `InvalidRequest`, never `ContextOverflow`:

```rust
for body in [
    r#"{"error":{"message":"invalid tool schema"}}"#,
    r#"{"error":{"message":"max_output_tokens must be positive"}}"#,
    r#"{"error":{"code":"bad_request","message":"invalid request"}}"#,
] {
    let error = classify_provider_failure(400, body);
    assert_eq!(error.kind, ModelErrorKind::InvalidRequest, "{body}");
}
```

Retain positive structured-code and context-window extraction cases.

- [ ] **Step 4: Run the focused adapter suite**

Run:

```bash
cargo test -p lato-ai model_port_adapter -- --nocapture
cargo test -p lato-ai --test provider_error -- --nocapture
```

Expected: all cases pass. If a new case fails, change only the adapter/classifier branch demonstrated by that case and rerun the exact failing test before the full focused suite.

- [ ] **Step 5: Commit Task 1**

```bash
git add crates/lato-ai/src/model_port_adapter/legacy_port.rs crates/lato-ai/src/model_port_adapter/stream_adapter.rs crates/lato-ai/tests/provider_error.rs
git diff --cached --name-only
git commit -m "test: harden model overflow boundaries"
```

---

### Task 2: Prove one-shot overflow recovery and no visible-output replay

**Files:**

- Create: `crates/lato-agent/tests/context_recovery_faults.rs`

- [ ] **Step 1: Add a local scripted fault stream**

Keep the fixture in the new integration test file:

```rust
#[derive(Clone)]
struct FaultStep {
    pieces: Vec<StreamPiece>,
    terminal: Result<(), ModelError>,
}

struct FaultStream {
    steps: tokio::sync::Mutex<VecDeque<FaultStep>>,
    contexts: Mutex<Vec<serde_json::Value>>,
    calls: AtomicUsize,
}
```

`stream` must record the request context, increment `calls`, emit every scripted piece in order, and then return the scripted terminal result. Fail with a clear test-fixture error if a request consumes more steps than provided. Do not add this fixture to production code or a shared crate.

- [ ] **Step 2: Add the first-overflow recovery case**

Script `seed success -> pre-output overflow -> valid compaction summary -> ordinary success`. Through `AcpHost`, assert:

- the prompt completes;
- exactly four model contexts exist;
- the compaction context has no tools;
- the rebuilt ordinary request has tools and the installed conversation summary;
- the rejected request is not counted as visible assistant output.

This supersedes the narrower copy in `compaction_runtime.rs`; remove that old test only after the new test passes so coverage never drops.

- [ ] **Step 3: Add the second-overflow terminal case**

Script `seed success -> overflow -> summary -> overflow` and provide no fifth step. Assert the prompt fails with the original `model.context_overflow` code/message and `calls == 4`. This proves there is no second compaction and no third ordinary submission.

- [ ] **Step 4: Add text/tool output replay guards**

Run two independent cases:

1. emit `Text("partial")`, then overflow;
2. emit one `ToolCall`, then overflow.

Assert each prompt terminates, the stream call count stays at one for that prompt, and no compaction request is made. For text, assert the ACP update contains `partial` exactly once. For the tool case, use a harmless deterministic test tool or an invalid/nonexistent tool call that still proves the actor observed the call boundary; assert there is no replay request.

- [ ] **Step 5: Add recovery-failure causality cases**

Inject both runtime outcomes:

- compaction returns `ContinueUnchanged` for a deterministic size/schema failure;
- compaction itself returns a fatal error.

Assert the first path reports the original overflow as the primary terminal failure, while the second includes `context recovery failed:` after the original provider failure. Never replace the provider error with only the compaction error.

- [ ] **Step 6: Run and commit Task 2**

Run:

```bash
cargo test -p lato-agent --test context_recovery_faults -- --nocapture
cargo test -p lato-agent --test compaction_runtime -- --nocapture
```

Expected: all cases pass, and the scripted stream never exhausts unexpectedly.

Commit:

```bash
git add crates/lato-agent/tests/context_recovery_faults.rs crates/lato-agent/tests/compaction_runtime.rs
git diff --cached --name-only
git commit -m "test: cover bounded overflow recovery"
```

---

### Task 3: Close the cancellation gap between checkpoint and resubmission

**Files:**

- Modify: `crates/lato-agent/tests/context_recovery_faults.rs`
- Modify: `crates/lato-agent/src/actor.rs`

- [ ] **Step 1: Write a deterministic failing cancellation test**

Use a runtime-backed `LegacyTurnDriver`, `MemoryEventStore`, and the Task 2 fault stream. Coordinate with `Notify`/channels so the test:

1. receives the provider overflow;
2. allows automatic compaction to finish and observes `CompactionCompleted`;
3. sends `Command::CancelTurn` before the next ordinary provider request;
4. releases the actor to continue.

The model fixture must expose a `resubmit_started` notification and count ordinary calls. Wrap every await in a one-second `tokio::time::timeout`.

Expected invariant:

```rust
assert_eq!(ordinary_calls.load(Ordering::SeqCst), 1);
assert!(matches!(terminal.payload, EventPayload::TurnCancelled { .. }));
```

- [ ] **Step 2: Run the single test and observe RED**

Run:

```bash
cargo test -p lato-agent --test context_recovery_faults cancellation_after_compaction_never_resubmits -- --nocapture
```

Expected before the fix: the fixture observes a rebuilt provider request, or the call-count assertion fails.

- [ ] **Step 3: Add one sampling-boundary cancellation guard**

In `SessionActor::prompt_with_context`, check cancellation at the beginning of every sampling-loop iteration, before usage/preflight work can lead to a provider submission:

```rust
if self.cancelled || self.turn_cancellation.is_cancelled() {
    self.active = false;
    return Ok(TurnOutcome::Cancelled);
}
```

Keep the existing in-stream cancellation check. Do not clear or roll back a compaction checkpoint that runtime has already committed; cancellation prevents only the next model request.

- [ ] **Step 4: Run cancellation and runtime regression tests**

Run:

```bash
cargo test -p lato-agent --test context_recovery_faults cancellation_after_compaction_never_resubmits -- --nocapture
cargo test -p lato-runtime automatic_compaction_is_cancelled_with_its_turn_and_rejects_manual_overlap -- --nocapture
cargo test -p lato-agent --test journal_runtime -- --nocapture
```

Expected: the new test passes, the committed compacted history remains replayable, and no ordinary resubmit begins.

- [ ] **Step 5: Commit Task 3**

```bash
git add crates/lato-agent/src/actor.rs crates/lato-agent/tests/context_recovery_faults.rs
git diff --cached --name-only
git commit -m "fix: stop recovery resubmit after cancellation"
```

---

### Task 4: Cover preflight overflow and suppression lifetimes

**Files:**

- Modify: `crates/lato-agent/tests/context_recovery_faults.rs`
- Modify: `crates/lato-agent/tests/context_recovery.rs`

- [ ] **Step 1: Add oversized tool-output preflight coverage**

Construct an actor with a deterministic test tool whose bounded output still pushes measured input above a deliberately small active model window. Script the first ordinary response as the tool call, then provide a valid summary and a final ordinary response.

Assert request order is `ordinary tool call -> tool-free compaction -> rebuilt ordinary request`; no known-oversized ordinary context appears between the tool result and compaction.

- [ ] **Step 2: Add terminal preflight-failure coverage**

Make compaction fail or continue unchanged while the request remains oversized. Assert:

```rust
assert!(error.to_string().contains("context.preflight_recovery_failed"));
assert_eq!(ordinary_calls_after_tool, 0);
```

The test must prove the model fixture did not receive the known-oversized request.

- [ ] **Step 3: Expand the suppression lifecycle table**

In `context_recovery.rs`, table-test every trigger against each scope:

- `Turn`: cleared only by `on_new_turn`;
- `Sticky`: survives a new turn, cleared by budget change or successful compaction;
- `UntilSuccess`: cleared only by provider success;
- `Auth`: cleared only by auth refresh;
- `Manual`: always allowed regardless of the current scope.

Also assert repeated suppression does not replace the original scope or emit a second state transition.

- [ ] **Step 4: Prove suppression blocks all automatic entry points**

For a sticky deterministic failure, exercise threshold, model-switch, preflight-overflow, and provider-overflow entry points. Assert no automatic compaction model request is issued after suppression. Then invoke `lato/session/compact` and assert manual compaction still starts.

- [ ] **Step 5: Run and commit Task 4**

Run:

```bash
cargo test -p lato-agent --test context_recovery -- --nocapture
cargo test -p lato-agent --test context_recovery_faults preflight -- --nocapture
cargo test -p lato-agent --test context_recovery_faults suppression -- --nocapture
```

Expected: all automatic paths are blocked under suppression, while the manual path is unaffected.

Commit:

```bash
git add crates/lato-agent/tests/context_recovery.rs crates/lato-agent/tests/context_recovery_faults.rs
git diff --cached --name-only
git commit -m "test: harden preflight and suppression policy"
```

---

### Task 5: Exercise two-pass boundaries, cache invalidation, and the global attempt ceiling

**Files:**

- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/two_pass.rs`
- Modify: `crates/lato-agent/tests/legacy_driver.rs`

- [ ] **Step 1: Add exact split-boundary unit tests**

In `two_pass.rs`, build weighted messages immediately below, at, and above the 95% target. Include an assistant tool call followed by its tool result. Assert `split_for_two_pass` never separates the pair and always returns an interior index when two-pass input is viable.

In actor tests, drive utilization at 74%, 75%, 84%, and 85%. Assert pass one starts exactly once in `[75, 85)`, is not started below 75%, and final compaction is selected at 85%.

- [ ] **Step 2: Add prefire cache reuse/invalidation unit tests**

Construct `PrefireCache` values directly inside the private actor test module. Assert:

- appending messages after the cached prefix preserves reuse;
- modifying any prefix message invalidates the cache;
- changing model generation invalidates the cache;
- a cached prefix longer than current history invalidates the cache.

Use a completed `Ready` slot for deterministic tests; do not sleep waiting for a spawned task.

- [ ] **Step 3: Add a scripted canonical compaction port**

In `legacy_driver.rs` integration tests, record each `ModelRequest` and return scripted results for pass two and fallback stages. Identify pass-two requests by the NOTE1 marker and identify Prepared/Fitted/Lossy requests by serialized message size and truncation markers.

- [ ] **Step 4: Prove one global three-attempt budget**

Cover at least these scripts:

1. pass two overflows, Prepared overflows, Fitted succeeds;
2. pass two returns an invalid summary, Prepared overflows, Fitted overflows;
3. no prefire cache: Prepared overflows, Fitted overflows, Lossy fails.

Assert each operation makes no more than `CompactionPolicy::default().max_attempts` model requests. The successful path installs only the validated final summary. The exhausted paths report the final bounded compaction error without a fourth request.

- [ ] **Step 5: Prove NOTE1 never becomes durable or visible**

Use a unique sentinel such as `SECRET_SPECULATIVE_NOTE1`. Assert it appears only in the pass-two model request, not in:

- the replacement history;
- `ConversationItemCommitted` journal records;
- ACP `session/update` deltas;
- runtime `ModelDelta` events.

- [ ] **Step 6: Run and commit Task 5**

Run:

```bash
cargo test -p lato-agent two_pass -- --nocapture
cargo test -p lato-agent actor::tests -- --nocapture
cargo test -p lato-agent --test legacy_driver compaction -- --nocapture
```

Expected: exact 75/85/95 boundaries, prefix/generation invalidation, three-attempt ceiling, and NOTE1 isolation all pass.

Commit:

```bash
git add crates/lato-agent/src/actor.rs crates/lato-agent/src/two_pass.rs crates/lato-agent/tests/legacy_driver.rs
git diff --cached --name-only
git commit -m "test: stress two-pass compaction bounds"
```

---

### Task 6: Extend runtime checkpoint and speculative-isolation fault injection

**Files:**

- Modify: `crates/lato-runtime/tests/session_runtime.rs`

- [ ] **Step 1: Make the prefire isolation test inspect persistence**

Run `BlockingPrefireDriver` with `MemoryEventStore`, cancel the turn while prefire is blocked, and replay the store. Assert there is no `CompactionRequested`, `HistoryProjectionReplaced`, or assistant conversation record containing the NOTE1 sentinel. Retain the existing assertion that `install_history` is never called.

- [ ] **Step 2: Add completed-prefire isolation**

Create a driver whose prefire completes with a sentinel NOTE1 but whose ordinary turn then completes without requesting final compaction. Assert the event stream contains one turn completion, no compaction lifecycle event, and replay contains no NOTE1.

- [ ] **Step 3: Extend checkpoint failure ordering assertions**

For `BeforeCheckpointPublish`, assert the runtime emits one `CompactionFailed` followed by one `TurnFailed`, retains old history, and never calls in-turn installation.

For `BeforeMetadataPublish`, assert one `CompactionCompleted` with `projection.write_failed`, one `TurnCompleted`, committed replacement replay, and no duplicated lifecycle event.

- [ ] **Step 4: Add old-journal compatibility replay**

Seed an event stream containing only records from before Phase 4C3 (session start, turn records, and optionally a Phase 4C1 compaction checkpoint). Replay and start another turn. Assert absence of new recovery metadata is treated as defaults, not corruption.

- [ ] **Step 5: Run and commit Task 6**

Run:

```bash
cargo test -p lato-runtime --test session_runtime prefire -- --nocapture
cargo test -p lato-runtime --test session_runtime automatic_compaction -- --nocapture
cargo test -p lato-runtime --test session_runtime old_journal -- --nocapture
```

Expected: bounded completion, exactly-once terminal/lifecycle events, checkpoint reconciliation, and backward-compatible replay.

Commit:

```bash
git add crates/lato-runtime/tests/session_runtime.rs
git diff --cached --name-only
git commit -m "test: inject compaction runtime faults"
```

---

### Task 7: Add ACP/restart end-to-end coverage

**Files:**

- Modify: `tests/session_compaction_cli.rs`

- [ ] **Step 1: Add a successful recovery lifecycle E2E test**

Use `AcpHost::new_with_home` and a scripted endpoint to produce `overflow -> compact -> resubmit`. Drain updates and assert exactly one compaction `started`, exactly one compaction terminal event, exactly one final assistant answer, and one prompt response with `status == "complete"`.

- [ ] **Step 2: Add terminal recovery E2E tests**

Cover second overflow and post-output overflow. Assert each prompt produces one ACP error response, never both a success and error, and no assistant delta is duplicated. Verify the post-output case preserves the one already-emitted partial delta without replay.

- [ ] **Step 3: Add restart-after-recovery coverage**

Close the host after a successful automatic checkpoint, reopen with the same Lato home, call `session/resume`, and prompt again. Assert the resumed ordinary request contains the committed `<conversation_summary version="1">`, excludes raw pre-compaction payload and NOTE1 sentinel, and contains the new user input exactly once.

- [ ] **Step 4: Run and commit Task 7**

Run:

```bash
cargo test --test session_compaction_cli -- --nocapture
```

Expected: all ACP lifecycle, duplicate-output, restart, and compatibility assertions pass.

Commit:

```bash
git add tests/session_compaction_cli.rs
git diff --cached --name-only
git commit -m "test: cover context recovery end to end"
```

---

### Task 8: Document the matrix, run release gates, and deploy locally

**Files:**

- Modify: `docs/testing/lato-agent-test-cases.md`

- [ ] **Step 1: Append the verified Phase 4C3 matrix**

Add a concise section that maps the delivered tests to these guarantees: typed provider metadata, no post-output replay, single recovery credit, cancellation before resubmit, preflight refusal, suppression lifetimes, two-pass cache invalidation, global three-attempt ceiling, NOTE1 isolation, checkpoint reconciliation, ACP exactly-once behavior, and restart compatibility.

Because this file already has user-owned modifications, inspect its current diff first and append without rewriting or discarding existing material:

```bash
git diff -- docs/testing/lato-agent-test-cases.md
```

- [ ] **Step 2: Format and run focused packages**

Run:

```bash
cargo fmt --all -- --check
cargo test -p lato-ai --all-targets
cargo test -p lato-agent --all-targets
cargo test -p lato-runtime --all-targets
cargo test --test session_compaction_cli
```

Expected: all commands exit 0.

- [ ] **Step 3: Run workspace release gates**

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Expected: zero warnings and all tests pass.

- [ ] **Step 4: Install and smoke-test the local command**

Run:

```bash
cargo install --path .
lato --version
lato --help >/dev/null
```

Expected: installation succeeds, version reports `lato 0.1.0-beta.2`, and help exits 0.

- [ ] **Step 5: Commit documentation and any release-only lint repair**

Stage only the documentation lines added for this work plus any narrowly required lint repair:

```bash
git add -p docs/testing/lato-agent-test-cases.md
git diff --cached --name-only
git commit -m "docs: record phase 4c3 fault coverage"
```

Do not add `.lato/`, `.DS_Store`, `docs/testing/reports/`, or unrelated evaluation documents.

- [ ] **Step 6: Final audit**

Run:

```bash
git status --short
git log --oneline -10
```

Expected: only pre-existing user-owned changes/untracked files remain, and every task commit is visible in order.
