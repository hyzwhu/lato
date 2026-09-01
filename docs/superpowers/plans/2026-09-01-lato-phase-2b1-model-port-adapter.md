# Lato Phase 2B-1 Model Port Adapter Implementation Plan

> **For Codex:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task.

**Goal:** Route every current CLI and ACP provider through the canonical `ModelPort` boundary without changing user-visible model behavior, while preserving all pre-existing uncommitted work.

**Architecture:** Add a bidirectional compatibility layer in `lato-ai`. `LegacyModelPort` converts canonical requests into the existing JSON/`ModelStream` protocol and exposes canonical events; `ModelPortStreamAdapter` converts the actor's legacy request and channel contract back through `ModelPort`. Existing HTTP request construction, authentication, and SSE parsers remain the protocol authority.

**Tech Stack:** Rust 2021, Tokio, tokio-util cancellation tokens, futures streams, serde_json, semver, existing `lato-core` and `lato-ai` contracts.

---

## Working-tree safety

The repository starts with user-owned changes. Before implementation, capture:

```bash
git status --short > /tmp/lato-phase2b1-status-before.txt
git diff --binary > /tmp/lato-phase2b1-user-before.patch
shasum crates/lato-agent/src/actor.rs crates/lato-ai/src/api.rs crates/lato-ai/src/models_file.rs crates/lato-ai/src/stream.rs crates/lato-tools/src/dispatch.rs crates/lato-tools/src/edit.rs crates/lato-tools/src/registry.rs src/cli.rs tests/cli_headless.rs > /tmp/lato-phase2b1-user-before.sha256
git diff -- src/cli.rs > /tmp/lato-phase2b1-cli-before.patch
git diff -- crates/lato-ai/src/stream.rs > /tmp/lato-phase2b1-stream-before.patch
```

Never stage a protected dirty file wholesale. For `src/cli.rs` and `crates/lato-ai/src/stream.rs`, construct a patch against `HEAD` containing only the approved new hunk and apply it with `git apply --cached`. Do not edit the other protected files.

### Task 1: Add the legacy request codec and public adapter facade

**Files:**
- Modify: `Cargo.lock`
- Modify: `crates/lato-ai/Cargo.toml`
- Modify: `crates/lato-ai/src/lib.rs`
- Create: `crates/lato-ai/src/model_port_adapter.rs`
- Create: `crates/lato-ai/src/model_port_adapter/codec.rs`

**Step 1: Write failing codec tests**

Cover exact conversion of system, user, assistant, and tool messages; assistant text plus tool calls; tool-call ID preservation; tool definitions mapped to `legacy:<wire-name>`; model selection preservation; and malformed legacy payload rejection as `model.invalid_request`.

**Step 2: Run the focused tests and confirm failure**

```bash
cargo test -p lato-ai model_port_adapter::codec::tests -- --nocapture
```

Expected: compilation or assertion failure because the codec does not exist.

**Step 3: Add dependencies and implement the minimum codec**

Add `lato-core`, `futures-util`, `semver`, `tokio-stream`, and `tokio-util` with the `rt` feature. Add Tokio's `time` feature for dev tests.

Expose `model_port_adapter` from `lib.rs`. Define a facade that will ultimately export `LegacyModelPort`, `ModelPortStreamAdapter`, and:

```rust
pub fn adapt_model_stream(
    provider: &str,
    model: &str,
    stream: Arc<dyn ModelStream>,
) -> Result<Arc<dyn ModelStream>, ModelSelectionError>
```

In `codec.rs`, decode the existing actor JSON format into `ModelRequest` and encode canonical requests back to the exact existing JSON format with `stream: true`. Use conservative legacy tool descriptors:

- version `1.0.0`
- side effect `None`
- parallelism `Parallel`
- idempotent
- timeout 30 seconds
- output limit 65,536 bytes
- cooperative cancellation
- built-in source `legacy-model-context`

Only strip the `legacy:` namespace when restoring the provider wire name. Reject malformed JSON and malformed tool arguments; never silently substitute `{}`.

**Step 4: Run tests and quality checks**

```bash
cargo test -p lato-ai model_port_adapter::codec::tests -- --nocapture
cargo fmt --all -- --check
cargo clippy -p lato-ai --all-targets -- -D warnings
```

**Step 5: Commit**

```bash
git add Cargo.lock crates/lato-ai/Cargo.toml crates/lato-ai/src/lib.rs crates/lato-ai/src/model_port_adapter.rs crates/lato-ai/src/model_port_adapter/codec.rs
git commit -m "feat: add lato legacy model request codec"
```

### Task 2: Adapt existing providers to `ModelPort`

**Files:**
- Create: `crates/lato-ai/src/model_port_adapter/legacy_port.rs`
- Modify: `crates/lato-ai/src/model_port_adapter.rs`

**Step 1: Write failing port tests**

Use a scripted legacy `ModelStream` to assert:

- text events retain order;
- tool calls receive a monotonic index and preserve ID, name, and arguments;
- completion is emitted exactly once;
- pre-cancelled requests never call the provider;
- selection mismatch returns a typed stable error;
- provider send failure appears as a `model.stream_interrupted` stream item;
- downstream cancellation or stream drop terminates the producer within 250 ms.

**Step 2: Run and confirm failure**

```bash
cargo test -p lato-ai model_port_adapter::legacy_port::tests -- --nocapture
```

**Step 3: Implement `LegacyModelPort`**

Store the bound `ModelSelection` and `Arc<dyn ModelStream>`. Implement `ModelPort` using bounded channels and a cancellation-aware producer task. Validate cancellation and selection before invoking the provider. Convert legacy `StreamPiece` values into canonical text and tool-call deltas, then a single completion event.

Return setup failures directly. Once streaming starts, deliver provider failures as typed stream items. Aborting the consumer must cancel and abort the producer; no detached tasks are allowed. Advertise conservative capabilities, including tool use, without claiming unavailable usage or reasoning support.

**Step 4: Run focused checks**

```bash
cargo test -p lato-ai model_port_adapter::legacy_port::tests -- --nocapture
cargo clippy -p lato-ai --all-targets -- -D warnings
```

**Step 5: Commit**

```bash
git add crates/lato-ai/src/model_port_adapter.rs crates/lato-ai/src/model_port_adapter/legacy_port.rs
git commit -m "feat: adapt legacy providers to lato model port"
```

### Task 3: Bridge `ModelPort` back to the actor stream contract

**Files:**
- Create: `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`
- Modify: `crates/lato-ai/src/model_port_adapter.rs`

**Step 1: Write failing reverse-adapter tests**

Test full text ordering, interleaved indexed tool deltas, strict tool-argument completion parsing, unique generated call IDs, completed-event handling, canonical error formatting, cancellation, downstream receiver drop, and suppression of unsupported reasoning and usage.

**Step 2: Run and confirm failure**

```bash
cargo test -p lato-ai model_port_adapter::stream_adapter::tests -- --nocapture
```

**Step 3: Implement `ModelPortStreamAdapter` and the factory**

Implement the existing `ModelStream` trait. Decode the actor JSON context with the codec, create a `CancellationToken`, invoke `ModelPort::stream`, and translate canonical events into `StreamPiece`.

Accumulate `ToolCallDelta` values by index in a `BTreeMap`. On completion, parse every accumulated argument string as complete JSON and emit the legacy tool-call piece. Incomplete or malformed arguments must fail with the stable code prefix instead of being repaired. Make cancellation bidirectional and ensure receiver drop tears down every producer task.

Finish `adapt_model_stream` by constructing:

```text
existing provider ModelStream
  -> LegacyModelPort
  -> ModelPortStreamAdapter
  -> Arc<dyn ModelStream>
```

**Step 4: Run adapter checks repeatedly**

```bash
for i in 1 2 3 4 5; do cargo test -p lato-ai model_port_adapter -- --nocapture || exit 1; done
cargo clippy -p lato-ai --all-targets -- -D warnings
```

**Step 5: Commit**

```bash
git add crates/lato-ai/src/model_port_adapter.rs crates/lato-ai/src/model_port_adapter/stream_adapter.rs
git commit -m "feat: bridge lato model port to legacy actor stream"
```

### Task 4: Route ACP provider construction through the adapter

**Files:**
- Modify: `crates/lato-acp/src/host.rs`
- Add or modify ACP tests colocated with `host.rs`

**Step 1: Add a failing source-boundary test**

Assert both ACP `session/set_model` provider constructor branches call the shared adapter factory and no raw provider is returned directly.

**Step 2: Run and confirm failure**

```bash
cargo test -p lato-acp host -- --nocapture
```

**Step 3: Wire both ACP constructors**

Keep provider selection and configuration unchanged. Wrap each existing provider stream with `adapt_model_stream` at the construction boundary. Map model-selection failures through the existing ACP error path.

**Step 4: Run focused and integration tests**

```bash
cargo test -p lato-acp -- --nocapture
cargo test -p lato-agent -- --nocapture
```

**Step 5: Commit**

```bash
git add crates/lato-acp/src/host.rs
git commit -m "feat: route acp providers through lato model port"
```

### Task 5: Route CLI provider construction through the adapter

**Files:**
- Modify narrowly: `src/cli.rs`
- Modify tests only if needed: `tests/cli_headless.rs`

**Step 1: Add a failing source-boundary test outside the dirty CLI file**

Assert all three `configured_stream` provider branches cross `adapt_model_stream` and none returns a raw provider stream. Preserve the existing headless CLI behavior tests.

**Step 2: Run and confirm failure**

```bash
cargo test --test cli_headless -- --nocapture
```

**Step 3: Apply the narrow CLI wiring**

Wrap only the three existing constructor return expressions. Do not reformat or rewrite unrelated code. Compare against `/tmp/lato-phase2b1-cli-before.patch` to prove all prior user hunks remain.

**Step 4: Stage only the new CLI hunk**

Generate a patch against `HEAD` containing solely the adapter import and constructor changes, then:

```bash
git apply --cached /tmp/lato-phase2b1-cli-ours.patch
git diff --cached -- src/cli.rs
git diff -- src/cli.rs
```

The cached diff must contain only Phase 2B-1 wiring. The working-tree diff must still contain all pre-existing CLI changes.

**Step 5: Test and commit**

```bash
cargo test --test cli_headless -- --nocapture
cargo test -p lato-agent -- --nocapture
git commit -m "feat: route cli providers through lato model port"
```

### Task 6: Harden migration, clear gates, and deploy locally

**Files:**
- Modify narrowly: `crates/lato-ai/src/stream.rs`
- Modify: `docs/architecture.md` or the repository's canonical architecture document
- Add focused tests where the preceding tasks place them

**Step 1: Add final contract tests**

Cover the full facade path for text, tool calls, completion, malformed incomplete tool arguments, typed failures, selection mismatch, pre-cancel, receiver drop, and absence of fabricated usage. Add a source-boundary assertion that ACP has two wrapped constructors and CLI has three.

**Step 2: Apply only the mechanical Clippy fix**

Collapse the already reported nested `if` in `stream.rs` without changing behavior. Build and stage an isolated patch against `HEAD`:

```bash
git apply --cached /tmp/lato-phase2b1-stream-clippy-ours.patch
git diff --cached -- crates/lato-ai/src/stream.rs
git diff -- crates/lato-ai/src/stream.rs
```

The cached diff must contain only the `collapsible_if` rewrite. Confirm the original user diff remains in the worktree.

**Step 3: Document the new extension boundary**

Describe `ModelPort` as the canonical future-facing provider interface, the bidirectional adapter as temporary compatibility infrastructure, and existing HTTP/SSE implementations as protocol authorities during migration. Record that new providers should implement `ModelPort` directly.

**Step 4: Run all gates**

```bash
for i in 1 2 3 4 5; do cargo test -p lato-ai model_port_adapter -- --nocapture || exit 1; done
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo tree -p lato-core
git diff --check
```

If an existing live-provider SSE test fails transiently, rerun that exact test once and record both results; do not weaken or skip it.

**Step 5: Verify protected changes**

Recompute hashes for protected files not intentionally edited and compare them with `/tmp/lato-phase2b1-user-before.sha256`. For `src/cli.rs` and `stream.rs`, compare the original saved patches and inspect both cached and worktree diffs. Confirm `actor.rs`, `api.rs`, `models_file.rs`, tool files, and `cli_headless.rs` retain their original content unless the test file was intentionally extended.

**Step 6: Commit hardening**

```bash
git add docs/architecture.md
git commit -m "test: harden lato model port provider migration"
```

Include the already isolated staged `stream.rs` hunk and any new focused test files in this commit. Never use a broad `git add`.

**Step 7: Install and smoke-test the user command**

```bash
cargo install --path .
lato --help
lato -p "reply with hi only"
```

If provider credentials are unavailable, `lato --help` must pass and the headless prompt result must be reported as credential-blocked rather than treated as an implementation failure.

## Completion evidence

- Every current CLI and ACP provider constructor crosses `ModelPort`.
- Adapter contract tests pass five consecutive times.
- Full workspace tests, formatting, and strict Clippy pass.
- `lato-core` remains independent of provider implementations.
- The installed `lato` command launches.
- Pre-existing user work remains intact, with exact or patch-based evidence.
- Commits are small, task-scoped, and match the messages above.
