# Lato Phase 2B-1 ModelPort Adapter Design

**Date:** 2026-09-01  
**Status:** Design sections approved; written-spec review pending  
**Scope:** Route the existing built-in and custom HTTP model providers through the Phase 2A `ModelPort` contract while retaining the legacy `ModelStream` consumer temporarily.

## Context

Phase 2A introduced a provider-independent, object-safe `ModelPort` in `lato-core`. The current production path still uses `lato_ai::ModelStream`, passes an untyped JSON context, and receives `StreamPiece` values over a Tokio channel. `SessionActor` owns the sampling loop and currently depends on that legacy interface.

Phase 2B-1 migrates the provider boundary first. It does not rewrite the actor loop or the existing provider protocol implementations. The result must make `ModelPort` part of the real CLI and ACP execution path, preserve current OpenAI Chat, OpenAI Responses, Anthropic, custom-provider, authentication, retry, and SSE behavior, and leave the later actor-native migration straightforward.

## Goals

- Make every configured HTTP provider execute through `Arc<dyn ModelPort>`.
- Preserve the current `SessionActor` behavior through a narrow compatibility adapter.
- Reuse the existing request builders, authentication, bounded retry policy, and SSE parsers.
- Convert provider failures and stream interruptions into stable `ModelError` values at the new boundary.
- Propagate cancellation into the provider task without introducing detached work.
- Keep existing user modifications intact, including controlled preservation when the CLI wiring file must receive a small additional patch.

## Non-goals

- Migrating `SessionActor` to construct `ModelRequest` directly.
- Replacing the legacy tool registry with `ToolCatalog`.
- Adding new providers, protocols, authentication methods, or model-discovery behavior.
- Inventing token usage when the legacy parser does not expose it.
- Removing `ModelStream`, `StreamPiece`, `HttpModelStream`, or `CustomHttpModelStream`.
- Adding AgentField reasoners or making an AgentField model call. This phase is deterministic Rust transport adaptation.

## Alternatives Considered

### A. Bidirectional compatibility adapter — selected

Add a provider-facing adapter that implements `ModelPort` over the existing `ModelStream`, plus an actor-facing adapter that implements `ModelStream` over a `ModelPort`. Provider construction composes the two boundaries, so production traffic crosses `ModelPort` without rewriting the actor or provider protocol code.

This gives the smallest reversible migration, isolates translation logic in one module, and allows the actor-facing half to be deleted later.

### B. Rewrite the actor and provider stream directly

Change `SessionActor`, `stream.rs`, and the request code to use `ModelRequest` and `ModelStreamEvent` natively. This removes compatibility code sooner, but couples several high-risk changes and overlaps heavily with existing user modifications.

### C. Duplicate the HTTP provider in a new crate

Build a separate provider crate around `ModelPort`. This has strong isolation, but creates parallel authentication, request, retry, and protocol parsing implementations that can drift.

## Architecture

```text
SessionActor
    │ legacy JSON context + mpsc<StreamPiece>
    ▼
ModelPortStreamAdapter
    │ ModelRequest + CancellationToken
    ▼
LegacyModelPort
    │ existing ModelStream call
    ▼
HttpModelStream / CustomHttpModelStream
    │ existing request, auth, retry, HTTP and SSE implementation
    ▼
LegacyModelPort
    │ ModelEventStream
    ▼
ModelPortStreamAdapter
    │ mpsc<StreamPiece>
    ▼
SessionActor
```

The round trip is intentional during migration. The canonical request and event contracts become observable and testable in production without forcing the actor and provider implementations to move in the same commit. A later phase replaces `ModelPortStreamAdapter` with native actor use and removes the redundant legacy encoding step.

## Components

### `LegacyModelPort`

`LegacyModelPort` lives in a new `crates/lato-ai/src/model_port_adapter.rs` module and implements `lato_core::ModelPort`.

It owns:

- the bound `ModelSelection`;
- `Arc<dyn ModelStream>` containing either the built-in or custom HTTP stream;
- declared `ModelCapabilities`.

Capabilities are conservative during compatibility: tool use is enabled because the current actor supplies tools; parallel tool calls, reasoning, vision, and structured output remain false unless the existing provider metadata proves support. Unknown context and output limits remain `None`.

Its `stream` method:

1. rejects a pre-cancelled request;
2. rejects a provider/model selection that differs from the bound selection;
3. converts `ModelRequest` into the existing JSON context;
4. starts the legacy stream with a bounded Tokio channel;
5. maps each `StreamPiece` to a canonical `ModelStreamEvent`;
6. publishes legacy task failure as an item in the established stream;
7. emits one terminal `Completed` event when the legacy stream ends normally;
8. aborts or joins the producer when cancellation or consumer closure makes further work useless.

`LegacyModelPort` does not perform provider-specific protocol parsing. It delegates that work to the existing stream implementation.

### `ModelPortStreamAdapter`

`ModelPortStreamAdapter` implements the legacy `ModelStream` trait over an `Arc<dyn ModelPort>`. It owns the same bound `ModelSelection` and a call-ID generator.

It converts the actor's legacy context into `ModelRequest`, invokes `ModelPort::stream`, and forwards canonical events into the legacy channel:

- `TextDelta` becomes `StreamPiece::Text`;
- tool-call deltas are assembled by call index and emitted once as a complete `StreamPiece::ToolCall`;
- reasoning deltas and usage remain non-user-visible at the legacy boundary;
- a normal `Completed` event closes the producer;
- a stream item error terminates the old call with a string containing the stable model error code.

The adapter must not emit an incomplete tool call or silently replace malformed arguments with `{}`.

### `LegacyRequestCodec`

Conversion helpers remain private to the adapter module. The codec supports the exact JSON shapes currently emitted by `history_to_messages` and `v1_tool_definitions`.

Legacy tool definitions receive temporary canonical names in the `legacy` namespace, such as `legacy:read_file`. When the request is encoded for the old provider builder, the namespace is removed so the wire name stays unchanged. Temporary descriptors use conservative metadata: version `1.0.0`, the original description and input schema, no claimed side effect beyond what the model request needs, and fixed nonzero limits. These descriptors are transport metadata only; they do not register tools and do not replace Phase 2B-2 tool adaptation.

Malformed roles, message content, tool definitions, or tool results fail closed with a typed conversion error. Known OpenAI/Anthropic tool-history shapes round-trip without changing IDs, names, or JSON arguments.

### Provider factory helper

A public helper in `lato-ai` composes a bound legacy stream into the production-compatible shape:

```text
Arc<dyn ModelStream>
    = ModelPortStreamAdapter(
        Arc<dyn ModelPort>
            = LegacyModelPort(existing_stream)
      )
```

Both ACP `session/set_model` construction in `crates/lato-agent/src/host.rs` and CLI construction in `src/cli.rs` use this helper for built-in and custom models.

## Request Semantics

- Each legacy sampling call receives a fresh `ModelCallId`.
- The bound provider/model is authoritative and must match the canonical request.
- Existing message order is preserved exactly.
- `tool_choice`, temperature, maximum output tokens, and response schema map only when represented by the old context.
- Unknown legacy context keys are rejected if they affect sampling semantics; they are not silently forwarded as provider extensions.
- `prompt_bytes` remains available only to the legacy actor-side interface. It is not treated as model token usage.

## Stream Semantics

- Text event order is preserved.
- A legacy complete tool call becomes one canonical `ToolCallDelta` carrying a full arguments JSON string.
- The reverse adapter supports one or more deltas per call and assembles them by `index`.
- A completed stream emits exactly one `Completed` event: `ToolCalls` when at least one tool call was emitted, otherwise `Completed`.
- No usage event is emitted unless usage is actually known.
- Provider reasoning content remains hidden in the compatibility path. Native providers can expose it later as `ReasoningDelta` without changing the contract.
- Backpressure is bounded by channel capacity; no unbounded event buffer is introduced.

## Cancellation and Task Ownership

The compatibility layer has one producer task per request. Cancellation is cooperative at the `ModelPort` boundary and forceful at the legacy task boundary because `ModelStream` has no cancellation parameter.

The producer selects among legacy progress, cancellation, and downstream closure. Cancellation aborts the legacy task and produces `model.cancelled` when the consumer is still present. Dropping the canonical stream must also stop the producer. Every spawned task is either joined or aborted; none may outlive the request indefinitely.

## Error Model

The adapter classifies errors without parsing arbitrary strings in callers:

- pre-call cancellation: `model.cancelled`, never retryable;
- selection mismatch: `model.selection_mismatch`, never retryable;
- invalid compatibility input: `model.invalid_request`, never retryable;
- malformed tool-call completion: `model.invalid_tool_arguments`, never retryable;
- legacy HTTP/auth/protocol failure before stream establishment: `model.provider_request_failed` with conservative retryability;
- failure after stream establishment: a stream item error with `model.stream_interrupted` or the most specific stable code available.

The final legacy-facing error string starts with the stable code so existing string-returning APIs retain diagnostic value. Existing provider retry behavior remains bounded to its current maximum of three attempts; the adapter does not add another retry loop.

## Wiring and Compatibility

`crates/lato-ai/src/lib.rs` exports the new adapter types and factory. `crates/lato-ai/Cargo.toml` gains a one-way dependency on `lato-core` and only the stream utilities required to expose a boxed stream.

`crates/lato-agent/src/host.rs` changes only the built-in and custom stream construction expressions used by `session/set_model`.

`src/cli.rs` changes only the return expressions inside `configured_stream`. Because this file already contains user modifications, implementation must capture its pre-change diff, apply a narrow patch, and prove every pre-existing hunk remains present. Exact hash preservation applies to every other protected dirty file; for `src/cli.rs`, review is patch-based because the approved adapter wiring intentionally changes it.

No behavior edits are permitted in the existing dirty `actor.rs`, `api.rs`, `models_file.rs`, or `stream.rs` files during this phase. The sole exception is an isolated, mechanical collapse of the currently failing `clippy::collapsible_if` in `stream.rs`; implementation must stage only that formatting hunk and prove all other pre-existing hunks remain present.

## Testing

### Unit tests

- request selection mismatch is rejected;
- pre-cancelled calls return `model.cancelled`;
- current legacy message and tool-definition fixtures decode and re-encode stably;
- text order is preserved through both adapters;
- complete legacy tool calls become canonical events;
- split canonical tool-call arguments assemble once at the legacy boundary;
- malformed or unfinished tool-call arguments fail closed;
- unknown usage remains absent rather than becoming zero;
- producer tasks stop on cancellation and consumer drop;
- `Arc<dyn ModelPort>` and `Arc<dyn ModelStream>` both compile across the adapter membrane.

### Offline integration tests

Local HTTP fixtures cover:

- OpenAI Chat Completions text and tool calls;
- OpenAI Responses function calls;
- Anthropic Messages tool use;
- a custom `models.json` endpoint;
- HTTP failure before stream establishment;
- interruption after the first streamed delta.

At least one source-level or instrumented assertion proves the executed provider path contains `ModelPort`; a test that merely constructs the adapter is insufficient.

### Repository gates

- focused `lato-core`, `lato-ai`, and `lato-agent` tests;
- full workspace tests;
- strict no-dependency Clippy for changed crates;
- formatting and `git diff --check`;
- dependency boundary check: `lato-core` must remain independent of all other Lato crates;
- existing transient local SSE `WouldBlock (os 35)` may be rerun exactly, but its timeout and acceptance criteria may not be weakened;
- `cargo install --path .` and `lato -p "reply with hi only"` smoke test.

## Source Reuse and Provenance

The implementation may copy or structurally derive code under Apache-2.0 with an exact pinned-source header and a ledger entry.

- Codex `codex-rs/model-provider/src/provider.rs`: object-safe provider boundary, explicit capabilities, configured-provider construction.
- Codex `codex-rs/codex-api/src/sse/responses.rs`: normalized stream events and terminal/error handling.
- Grok Build `crates/codegen/xai-grok-sampler/src/client.rs`: stream production and provider request lifecycle.
- Grok Build `crates/codegen/xai-grok-sampler/src/stream/responses.rs`: text/tool/terminal event normalization.
- Grok Build `crates/codegen/xai-grok-sampling-types/src/error.rs` and sampler retry code: explicit stream error retryability.

The pinned upstream commits remain:

- Codex: `633ab199cfd724aa78013c006b27a2b3d049fc3b`
- Grok Build: `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## AgentField Assessment

AgentField live contract `2026-03-24-v1` and `af doctor --json` were checked during design. The local control plane is reachable and coding harnesses are available, but no provider API key is configured. Phase 2B-1 introduces no AgentField node or reasoner: its cognitive-job decomposition contains only deterministic conversion, stream forwarding, and programmatic verification. Creating an artificial multi-reasoner graph would add no intelligence and would violate the leftmost-capable-primitive rule. Provider keys are therefore not required for implementation or offline verification.

## Completion Criteria

- CLI and ACP configured HTTP providers traverse `Arc<dyn ModelPort>` in real execution.
- Existing request/auth/retry/SSE implementations remain the protocol authority.
- Cancellation and downstream drop leave no detached provider task.
- Text and tool-call behavior remains compatible with current actor tests.
- Stable typed model errors exist at the canonical boundary.
- No behavior edits occur in `actor.rs`, `api.rs`, `models_file.rs`, or `stream.rs`; only the explicitly isolated Clippy formatting hunk in `stream.rs` is allowed.
- All repository gates and installed-command smoke tests pass.
- All pre-existing user changes remain intact.
