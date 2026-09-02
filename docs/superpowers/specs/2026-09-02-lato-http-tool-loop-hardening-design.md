# Lato HTTP Tool Loop Hardening Design

**Date:** 2026-09-02

## Goal

Finish and harden the current uncommitted HTTP model tool-loop work without broadening the product scope. The resulting CLI must preserve streamed assistant text, execute supported tool calls through the existing policy/runtime membrane, return tool results to the originating model dialect, and terminate predictably.

## Scope

This slice covers the existing changes in:

- `crates/lato-agent/src/actor.rs`
- `crates/lato-ai/src/api.rs`
- `crates/lato-ai/src/models_file.rs`
- `crates/lato-ai/src/stream.rs`
- `crates/lato-tools/src/dispatch.rs`
- `crates/lato-tools/src/edit.rs`
- `crates/lato-tools/src/registry.rs`
- `src/cli.rs`
- `tests/cli_headless.rs`

The untracked repository instruction file `AGENTS.md` is preserved but excluded from the feature commit.

## Design

### 1. Streaming transport and parsing

The HTTP transport will decode streaming bytes without applying lossy UTF-8 conversion independently to each network chunk. A multibyte character split across chunks must be reconstructed exactly. Complete SSE/JSON lines may continue to emit text immediately, while structured tool calls may be assembled from the complete response when required by the provider format.

Malformed structured tool arguments must not be silently converted into an empty object that could change the meaning of a call. They will produce a stable failure path and will not invoke a mutating tool with fabricated arguments.

Regression fixtures will cover split multibyte UTF-8, fragmented tool arguments, malformed arguments, non-stream JSON responses, and the existing OpenAI Chat, OpenAI Responses, and Anthropic event shapes.

### 2. Provider request fidelity

The canonical actor context remains the source of `messages`, `tools`, `tool_choice`, and `stream`. Each provider request builder will translate supported options into that provider's wire shape instead of silently ignoring them.

- OpenAI Chat keeps `auto`/`required` tool choice and streaming selection.
- OpenAI Responses and Azure Responses receive their supported tool-choice and streaming fields.
- Anthropic receives the equivalent native tool-choice shape and streaming selection.
- Providers without an implemented equivalent remain explicit rather than pretending the option was honored.

The forced-tool retry remains bounded to one retry. A provider rejection may fall back only for a failure attributable to the requested tool-choice option; unrelated HTTP 400 responses must remain visible failures.

### 3. Actor tool-loop termination

The actor retains the existing hard limits: at most 50 sampling steps, bounded context, cancellation checks, and policy-gated tool execution. Repeated identical tool calls will not be re-executed indefinitely, but the guard must not manufacture a successful assistant answer. The terminal state will remain explicit: either return a typed/stable stall error, or feed a bounded diagnostic tool result back to the model and allow a final response within the remaining sampling budget.

History must preserve assistant text, tool calls, and tool results in provider-compatible order. Cancellation after persistence must not execute the tool, and authorization must continue through `prepare`, policy decision/approval, and `execute`.

### 4. Local CLI facts and model cache

The local directory/model shortcut remains, but its matcher will be narrowed to unambiguous fact requests such as `pwd`, an explicit current working-directory question, or an explicit current-model question. General prompts that merely mention a model, path, workspace, or file must continue to the selected model.

Provider model discovery will continue to persist successful results in `models-store.json` with a current timestamp. Failed discovery must not overwrite a previously usable cache entry.

### 5. Compatibility tools

The compatibility layer keeps the accepted aliases already introduced by the current work (`write`/`write_file`, content field spellings, and edit argument spellings). Aliases must resolve to the same canonical tool descriptor before policy fingerprinting. File creation and replacement remain serialized through `FileLocks`, and workspace/path policy remains authoritative.

## Error Handling

- Invalid UTF-8 or malformed structured tool arguments return stable errors rather than lossy or fabricated values.
- Transport and provider HTTP errors remain distinguishable from policy denial and tool execution errors.
- A denied tool call is recorded as a tool result for the model but is never executed.
- Receiver cancellation stops streaming promptly and does not leave an active tool execution behind.
- Cache writes occur only after successful model discovery.

## Verification

Implementation is complete only when all of the following pass:

1. Focused unit and integration tests for the changed actor, stream, API, tool, and headless CLI paths.
2. `cargo fmt --all -- --check`.
3. `cargo test --workspace --no-fail-fast`.
4. `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
5. `cargo install --path .`.
6. Installed-binary offline smoke tests for `lato doctor`, fake-model headless prompting, local fact queries, and the offline HTTP tool-loop fixture where practical.

The final feature commit will include only the intended implementation, tests, and this design/plan documentation. Existing unrelated user changes remain untouched.

## Non-goals

- Replacing `SessionActor` with a new state-machine architecture.
- Adding new providers, tools, or interactive commands.
- Running live vendor tests without credentials.
- Changing the policy engine, approval fingerprint contract, or sandbox model.
- Committing the untracked `AGENTS.md` file as part of this feature.
