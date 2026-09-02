# OpenAI Codex Provider Parity Design

## Goal

Bring Lato's native `openai-codex` provider to behavioral parity with Pi's Codex provider while preserving Lato as a standalone Rust binary. Lato will own its OAuth credentials, refresh them, and call the ChatGPT Codex backend directly without requiring a Pi or Node.js sidecar.

This work includes browser and device-code authentication, account-aware credentials, the Codex request dialect, WebSocket-first streaming, SSE fallback, zstd request compression, and session-scoped prompt-cache affinity. It does not change the behavior of other model providers.

## Current State

Lato already has a distinct `openai-codex` provider, performs the OpenAI PKCE authorization-code flow, persists OAuth access and refresh tokens in `$LATO_HOME/auth.json`, and refreshes credentials with less than five minutes remaining. The implementation is incomplete in two important ways:

- `OpenaiCodexResponses` is routed through the generic OpenAI Responses request builder, producing `https://chatgpt.com/backend-api/responses` instead of the Codex endpoint.
- Lato does not preserve the ChatGPT account ID or emit the account-aware headers and Codex-specific request body required by the subscription backend.

Pi treats Codex as a distinct provider dialect. It calls `/codex/responses`, sends the OAuth bearer token together with `chatgpt-account-id`, prefers a reusable WebSocket transport, falls back to SSE before any output is emitted, supports zstd-compressed SSE requests, and derives stable per-session cache keys.

## Chosen Architecture

Implement the provider natively in Rust. Add a focused Codex module inside `lato-ai` and delegate `ModelApi::OpenaiCodexResponses` to it from `HttpModelStream`. Generic OpenAI Responses, chat completions, Anthropic, Google, Azure, Bedrock, Mistral, and custom endpoints continue through their existing paths.

The Codex module owns:

- Codex URL resolution and request-body construction.
- ChatGPT account-ID extraction and request headers.
- WebSocket connection establishment, reuse, framing, and event processing.
- SSE submission, optional zstd compression, and event processing.
- Transport fallback classification and bounded session-affinity state.

The existing provider-independent stream boundary remains unchanged. Codex events are translated into the current `StreamPiece::Text` and `StreamPiece::ToolCall` values so the session and tool loops do not acquire provider-specific branches.

## Credential Model and Authentication

Extend OAuth credentials with an optional serialized `account_id` field. The field remains optional for compatibility with existing Lato credentials and Pi-compatible JSON entries.

When resolving an `openai-codex` credential:

1. Read the stored access token.
2. Use the stored `account_id` when present.
3. Otherwise decode the JWT payload and read `https://api.openai.com/auth.chatgpt_account_id`.
4. Reject the credential if the claim is absent or invalid.
5. Persist the recovered account ID the next time the credential is refreshed or otherwise modified.

Every successful authorization-code exchange and refresh extracts the account ID from the new access token. Refresh-token rotation is preserved: when a refresh response omits a replacement refresh token, Lato retains the previous value.

Browser login starts a one-shot loopback HTTP callback listener on port 1455. It validates OAuth state, accepts only the expected callback path, returns a small success or failure page, and closes after completion or cancellation. Manual redirect URL entry remains available as a fallback.

Headless login implements OpenAI's Codex device-code flow. It requests a user code, reports the verification URL through `AuthInteraction`, polls within the server-provided interval and fixed timeout budget, then exchanges the returned authorization code with its verifier. Cancellation stops polling immediately.

Credentials remain in `$LATO_HOME/auth.json`, protected by the existing file permissions and locking rules. Tokens and authorization headers must never appear in errors, logs, test snapshots, or diagnostics.

## Request Dialect

For the built-in Codex provider, resolve the endpoint to:

```text
https://chatgpt.com/backend-api/codex/responses
```

Custom bases follow the same normalization rules: a base ending in `/codex/responses` is used directly, `/codex` receives `/responses`, and other bases receive `/codex/responses`.

Every request includes:

- `Authorization: Bearer <access token>`
- `chatgpt-account-id: <account ID>`
- `originator: lato`
- a Lato `User-Agent`
- the transport-specific `OpenAI-Beta` value
- transport-specific content negotiation and request/session IDs

The request body follows the Codex Responses shape:

- `model`
- `store: false`
- `stream: true`
- `instructions`, separated from conversation messages
- converted `input`
- `text.verbosity`
- `include: ["reasoning.encrypted_content"]`
- `tool_choice`
- `parallel_tool_calls: true`
- converted tools when present
- reasoning effort and summary when configured
- `prompt_cache_key` when a stable session ID is available

Existing Lato messages and tools are converted without changing tool-call identity. System messages become `instructions` rather than ordinary input items. Tool calls and tool results preserve their call IDs.

## Session Affinity

Add `session_id` and `turn_id` metadata to the legacy provider context constructed by `SessionActor`. These fields are provider-neutral execution metadata. Existing codecs and providers continue to ignore unknown context fields.

Codex clamps the stable session ID to the backend's prompt-cache key constraints and uses it for:

- `prompt_cache_key`
- `session-id`
- the initial `x-client-request-id`
- selecting a reusable WebSocket connection

Different sessions never share continuation state. The connection map has a fixed maximum size and idle expiry. Closed, failed, or evicted connections are removed before reuse.

## Transport Behavior

### WebSocket

WebSocket is the default transport. A session reuses an open connection when it is healthy. The client sends the Codex request frame, converts response events incrementally, and retains the connection only after a successful terminal event.

A retry is allowed only for recognized pre-stream conditions such as a connection-limit response or missing continuation state. Retries are bounded to one per condition. Cancellation closes or releases the active request promptly.

### SSE

SSE is used when explicitly selected for tests or when WebSocket fails before emitting a model event. The JSON request body is zstd-compressed when compression succeeds; otherwise Lato sends the original JSON body. Compression failure itself is not a provider failure.

SSE parsing remains incremental and UTF-8 safe across arbitrary network chunk boundaries. Codex response events are handled by the dedicated event mapper instead of the generic OpenAI parser.

### Fallback Safety

Automatic WebSocket-to-SSE fallback is permitted only before any model output, reasoning output, tool call, or usage event has been accepted. Once streaming begins, transport failure is returned to the caller and the request is not replayed. This prevents duplicate model work and duplicated tool calls.

## Event Mapping

The event mapper recognizes Codex Responses events for:

- output text deltas and completion
- reasoning summary deltas
- function-call argument deltas and completed calls
- response completion and usage
- structured error and failure events

Text and completed tool calls enter the existing `StreamPiece` channel. Reasoning and usage are retained where the current Lato stream contract can represent them; extending the public stream contract is outside this provider change. Unknown additive events are ignored, while malformed required fields and explicit backend error events fail the request with a bounded diagnostic.

## Error Handling and Security

- Validate OAuth state, callback path, JWT structure, and ChatGPT account ID.
- Bind browser callbacks only to loopback interfaces.
- Do not fall back to environment credentials after OAuth refresh failure.
- Apply finite timeouts to OAuth exchange, device polling, HTTP requests, WebSocket connect, and idle reads.
- Bound provider response excerpts included in errors and strip control characters.
- Never include tokens, complete headers, compressed request bytes, or raw JWT payloads in diagnostics.
- Fall back from WebSocket only before stream start.
- Bound WebSocket retries, session-map capacity, and idle lifetime.
- Preserve cancellation through OAuth, compression, connection, send, and receive phases.

## Compatibility

- Existing OAuth JSON without `account_id` remains readable.
- Pi-style OAuth JSON containing `accountId` may be accepted through a serde alias while Lato writes the canonical `account_id` spelling.
- Existing API-key providers are unaffected.
- Generic OpenAI Responses behavior is unaffected.
- The `ModelStream` and canonical `ModelPort` interfaces remain stable.
- The installed artifact remains one Rust `lato` executable and has no Node.js runtime dependency.

## Dependencies

Add narrowly scoped Rust dependencies to `lato-ai`:

- a maintained Tokio-compatible WebSocket client with TLS support
- zstd compression

Use existing `reqwest`, `tokio`, `serde_json`, `base64`, and hashing dependencies where possible. New dependencies must not introduce a second async runtime or an OpenSSL-only requirement that breaks the existing release targets.

## Verification

Offline tests use local fake OAuth, HTTP, SSE, and WebSocket endpoints and never require a real ChatGPT account.

Required coverage:

- PKCE and OAuth state validation.
- Browser callback success, invalid state, cancellation, and manual fallback.
- Device-code pending, slow-down, success, expiry, and cancellation.
- JWT account-ID extraction and legacy credential compatibility.
- Refresh rotation and locked persistence.
- Exact Codex endpoint, headers, request body, and cache keys.
- zstd request decompression back to the expected JSON.
- incremental SSE parsing across split UTF-8 and event boundaries.
- WebSocket handshake, event mapping, same-session reuse, cross-session isolation, retry limits, and eviction.
- pre-stream WebSocket fallback to SSE.
- no replay after any stream event.
- unchanged request snapshots for non-Codex providers.

Run:

```bash
cargo test -p lato-ai
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo install --path .
```

A real subscription smoke test is optional and explicitly gated by locally available credentials. Offline success must not be reported as live-provider success.

## Out of Scope

- Running Pi as a sidecar or token broker.
- Reading or mutating `~/.codex/auth.json`.
- Sharing credentials automatically between Pi, Codex CLI, and Lato.
- Adding a public token-export endpoint.
- Changing model selection or other provider catalogs beyond Codex models needed for the provider to operate.
- Exposing reasoning or usage through new public Lato protocol events.

