# OpenAI Codex Provider Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Lato's native `openai-codex` provider match Pi's OAuth, request, WebSocket, SSE, compression, and session-affinity behavior while preserving a standalone Rust binary.

**Architecture:** A new focused `lato-ai::codex` module owns Codex request construction, event mapping, and WS-first/SSE-fallback transport. Existing credential storage gains an optional ChatGPT account ID, while `SessionActor` adds provider-neutral session metadata to its legacy context. Other provider paths continue through the existing generic request and stream code.

**Tech Stack:** Rust 2024, Tokio, Reqwest, Serde JSON, tokio-tungstenite with rustls, zstd, existing Lato `ModelStream` and `ModelPort` adapters.

## Global Constraints

- Do not require Node.js, Pi, or the official Codex CLI at runtime.
- Do not read or mutate `~/.codex/auth.json` or `~/.pi/agent/auth.json`.
- Never expose access tokens, refresh tokens, authorization headers, or raw JWT payloads in diagnostics.
- Preserve the current behavior and request snapshots of every non-Codex provider.
- WebSocket-to-SSE fallback is allowed only before any model event is accepted.
- OAuth refresh failure must not fall back to environment credentials.
- Browser callbacks bind only to a loopback interface.
- Complete implementation verification with `cargo test --workspace`, strict Clippy, and `cargo install --path .`.

---

## File Structure

- Create `crates/lato-ai/src/codex/mod.rs`: public-in-crate Codex request configuration and high-level WS-first transport orchestration.
- Create `crates/lato-ai/src/codex/events.rs`: stateful Codex Responses event mapper producing `StreamPiece` values.
- Create `crates/lato-ai/src/codex/sse.rs`: zstd request encoding, SSE submission, and incremental parsing.
- Create `crates/lato-ai/src/codex/websocket.rs`: WebSocket handshake, connection reuse, bounded affinity pool, retries, and frame processing.
- Modify `crates/lato-ai/src/lib.rs`: register the Codex module.
- Modify `crates/lato-ai/src/store.rs`: persist optional ChatGPT account IDs with Pi-compatible aliases.
- Modify `crates/lato-ai/src/auth.rs`: carry the account ID into resolved auth and preserve it through refresh.
- Modify `crates/lato-ai/src/oauth.rs`: extract account identity, add callback and device-code login, and return account-aware tokens.
- Modify `src/args.rs`: represent browser and device-code OAuth invocation explicitly.
- Modify `crates/lato-ai/src/api.rs`: stop sending `OpenaiCodexResponses` through generic Responses construction.
- Modify `crates/lato-ai/src/stream.rs`: delegate Codex models to the dedicated transport.
- Modify `crates/lato-agent/src/actor.rs`: add stable session/turn metadata to model context.
- Modify `crates/lato-agent/src/host.rs`: accept and persist account identity supplied through ACP OAuth login.
- Modify `crates/lato-ai/Cargo.toml` and `Cargo.lock`: add WebSocket and zstd dependencies.
- Modify `tests/cli_headless.rs`: verify account-aware mocked Codex login persistence without exposing secrets.
- Modify `README.md`: document Codex transport and login behavior.

---

### Task 1: Account-aware OAuth credentials

**Files:**
- Modify: `crates/lato-ai/src/store.rs`
- Modify: `crates/lato-ai/src/auth.rs`
- Modify: `crates/lato-ai/src/oauth.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Test: inline tests in the three files above

**Interfaces:**
- Produces: `OAuthTokens { access, refresh, expires, account_id }`.
- Produces: `Auth.account_id: Option<String>`.
- Produces: `extract_chatgpt_account_id(access_token: &str) -> Result<String, String>`.
- Produces: `store_oauth(..., account_id: Option<&str>)` as the single OAuth persistence path.
- Consumes: existing file locking and atomic replacement in `CredentialStore`.

- [ ] **Step 1: Write failing compatibility and JWT tests**

Add tests that construct a JWT from a base64url-encoded payload and verify both Lato and Pi field spellings:

```rust
#[test]
fn extracts_chatgpt_account_id_from_access_jwt() {
    let payload = URL_SAFE_NO_PAD.encode(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-7"}}"#,
    );
    let token = format!("header.{payload}.signature");
    assert_eq!(extract_chatgpt_account_id(&token).unwrap(), "acct-7");
}

#[test]
fn oauth_credential_accepts_pi_account_id_alias() {
    let value = serde_json::json!({
        "type": "oauth",
        "access": "a",
        "refresh": "r",
        "expires": 42,
        "accountId": "acct-pi"
    });
    let credential: Credential = serde_json::from_value(value).unwrap();
    let Credential::Oauth { account_id, .. } = credential else { panic!() };
    assert_eq!(account_id.as_deref(), Some("acct-pi"));
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

Run:

```bash
cargo test -p lato-ai extracts_chatgpt_account_id_from_access_jwt
cargo test -p lato-ai oauth_credential_accepts_pi_account_id_alias
```

Expected: compilation fails because the account-ID field and extractor do not exist.

- [ ] **Step 3: Extend the credential and resolved-auth types**

Use an optional canonical snake-case field with a Pi alias:

```rust
Credential::Oauth {
    access: String,
    refresh: String,
    expires: i64,
    #[serde(default, alias = "accountId", skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}
```

Add `account_id: Option<String>` to `Auth` and `OAuthTokens`. Decode the middle JWT segment with `URL_SAFE_NO_PAD`, parse JSON, and require a non-empty string at `/https:~1~1api.openai.com~1auth/chatgpt_account_id` using correct JSON-pointer escaping.

- [ ] **Step 4: Preserve account identity through resolution and refresh**

Update all OAuth destructuring sites, including ACP login, and change persistence to:

```rust
pub fn store_oauth(
    store: &mut CredentialStore,
    provider_id: &str,
    access: &str,
    refresh: &str,
    expires: i64,
    account_id: Option<&str>,
) -> std::io::Result<()>;
```

For `openai-codex`, resolve a missing stored account ID from the access JWT. Every authorization exchange and refresh returns a freshly extracted ID. Kimi continues to store `None`.

- [ ] **Step 5: Run account and existing auth tests**

Run:

```bash
cargo test -p lato-ai auth::tests
cargo test -p lato-ai oauth::tests
```

Expected: all tests pass; existing Kimi and API-key behavior is unchanged.

- [ ] **Step 6: Commit the credential slice**

```bash
git add crates/lato-ai/src/store.rs crates/lato-ai/src/auth.rs crates/lato-ai/src/oauth.rs crates/lato-agent/src/host.rs
git commit -m "feat: preserve Codex account identity"
```

---

### Task 2: Browser callback and device-code login

**Files:**
- Modify: `crates/lato-ai/src/oauth.rs`
- Modify: `src/args.rs`
- Modify: `src/cli.rs`
- Test: inline OAuth tests and `tests/cli_headless.rs`

**Interfaces:**
- Consumes: `OAuthTokens` and account extraction from Task 1.
- Produces: `login_openai_codex_browser(...)` and `login_openai_codex_device(...)` internal flows.
- Produces: `LoginMethod::Oauth { device_auth: bool }`, selected with `--oauth` and optional `--device-auth`, while preserving manual redirect fallback for browser login.

- [ ] **Step 1: Write failing callback and device polling tests**

Use a local Tokio TCP fixture and injectable endpoint configuration. Add argument parsing tests that require `--oauth` when `--device-auth` is present. Assert:

```rust
assert_eq!(callback.path(), "/auth/callback");
assert_eq!(callback.code(), "authorization-code");
assert!(receive_callback(expected_state, wrong_state_request).await.is_err());
assert_eq!(device_poll.pending_count(), 2);
assert_eq!(tokens.account_id.as_deref(), Some("acct-device"));
```

Cover browser success, wrong state, cancellation, `authorization_pending`, `slow_down`, device expiry, and successful exchange.

- [ ] **Step 2: Run the OAuth tests and verify failure**

```bash
cargo test -p lato-ai oauth::tests -- --nocapture
```

Expected: new callback/device tests fail because only manual browser redirect input exists.

- [ ] **Step 3: Implement the one-shot loopback callback**

Bind `127.0.0.1:1455`, parse a bounded HTTP request, require `GET /auth/callback`, validate `state`, write a small static HTML response, and close. Race callback completion against manual input and cancellation; the first valid result wins and all other work is aborted.

- [ ] **Step 4: Implement device-code login**

Use the Pi-aligned endpoints and forms:

```text
POST https://auth.openai.com/api/accounts/deviceauth/usercode
GET  https://auth.openai.com/codex/device
POST https://auth.openai.com/api/accounts/deviceauth/token
POST https://auth.openai.com/oauth/token
```

Poll with a bounded deadline, honor `authorization_pending` and `slow_down`, and exchange the returned authorization code with the provided verifier and device redirect URI.

- [ ] **Step 5: Update CLI interaction and mocked persistence tests**

Keep `lato login openai-codex --oauth` as browser-default behavior and add `lato login openai-codex --oauth --device-auth` for headless operation. Reject `--device-auth` for API-key login or providers other than `openai-codex`. Keep ACP-compatible notices. Update `LATO_MOCK_OAUTH` assertions so the stored record contains a non-secret test account ID.

- [ ] **Step 6: Run CLI and OAuth tests**

```bash
cargo test -p lato-ai oauth::tests
cargo test args::tests
cargo test --test cli_headless openai_codex
```

Expected: all focused tests pass and no test output contains access or refresh token values.

- [ ] **Step 7: Commit login parity**

```bash
git add crates/lato-ai/src/oauth.rs src/args.rs src/cli.rs tests/cli_headless.rs
git commit -m "feat: add complete Codex OAuth login flows"
```

---

### Task 3: Codex request builder and event mapper

**Files:**
- Create: `crates/lato-ai/src/codex/mod.rs`
- Create: `crates/lato-ai/src/codex/events.rs`
- Modify: `crates/lato-ai/src/lib.rs`
- Modify: `crates/lato-ai/src/api.rs`
- Test: inline tests in the new modules and existing `api.rs` snapshots

**Interfaces:**
- Produces: `CodexRequest { url, headers, body, session_key }`.
- Produces: `build_codex_request(model: &Model, auth: &Auth, context: &Value) -> Result<CodexRequest, String>`.
- Produces: `CodexEventMapper::accept(&mut self, value: Value) -> Result<Vec<StreamPiece>, String>`.
- Consumes: existing message/tool conversion behavior and `StreamPiece`.

- [ ] **Step 1: Write failing request-contract tests**

Assert the exact endpoint and mandatory semantics:

```rust
assert_eq!(request.url, "https://chatgpt.com/backend-api/codex/responses");
assert_eq!(request.header("chatgpt-account-id"), Some("acct-7"));
assert_eq!(request.header("originator"), Some("lato"));
assert_eq!(request.body["store"], false);
assert_eq!(request.body["stream"], true);
assert_eq!(request.body["instructions"], "system text");
assert_eq!(request.body["prompt_cache_key"], "session-7");
assert_eq!(request.body["include"], serde_json::json!(["reasoning.encrypted_content"]));
```

Also snapshot generic OpenAI Responses and verify it still resolves to `/v1/responses` with no ChatGPT account header.

- [ ] **Step 2: Write failing event-mapper tests**

Feed output-text deltas, reasoning deltas, `response.output_item.added`, function argument deltas/done, completion usage, explicit error, and malformed required fields. Assert that a tool call is emitted exactly once with its original `call_id`, name, and parsed JSON arguments.

- [ ] **Step 3: Run focused tests and verify failure**

```bash
cargo test -p lato-ai codex::
cargo test -p lato-ai b1_3_openai_responses_shape
```

Expected: Codex module tests do not compile; the existing generic OpenAI test still passes.

- [ ] **Step 4: Implement URL, headers, and body construction**

Split system messages into `instructions`; convert the remaining messages and tools into Responses items; set `store`, `stream`, `text.verbosity`, encrypted reasoning inclusion, tool choice, parallel tool calls, reasoning options, and prompt cache key. Reject missing bearer token or account ID before network I/O.

- [ ] **Step 5: Implement the stateful event mapper**

Track pending function calls by item ID, append argument fragments, emit only on completion, and reject invalid completed JSON. Record whether any semantic event has started so transport fallback can be decided without inspecting emitted channel state.

- [ ] **Step 6: Remove Codex from generic request construction**

Change the generic branch to only `ModelApi::OpenaiResponses`; make direct generic construction of `OpenaiCodexResponses` return a clear internal routing error so it cannot silently regress to `/responses`.

- [ ] **Step 7: Run all `lato-ai` unit tests**

```bash
cargo test -p lato-ai
```

Expected: all tests pass, including unchanged non-Codex request tests.

- [ ] **Step 8: Commit the dialect**

```bash
git add crates/lato-ai/src/codex crates/lato-ai/src/lib.rs crates/lato-ai/src/api.rs
git commit -m "feat: implement Codex request dialect"
```

---

### Task 4: Stable session metadata

**Files:**
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/codec.rs`
- Test: inline actor and codec tests

**Interfaces:**
- Produces context keys `session_id: String` and `turn_id: String`.
- Consumes: the existing `SessionActor.session_id` and `SessionActor.turn_id` values.
- Preserves: all existing decoded canonical model request fields.

- [ ] **Step 1: Write a failing actor-context test**

Capture the context delivered to a recording `ModelStream` and assert:

```rust
assert_eq!(context["session_id"], "session-cache-7");
assert_eq!(context["turn_id"], "turn-cache-3");
assert!(context["messages"].is_array());
assert!(context["tools"].is_array());
```

- [ ] **Step 2: Run the test and verify failure**

```bash
cargo test -p lato-agent model_context_contains_stable_session_metadata
```

Expected: assertions fail because the metadata keys are absent.

- [ ] **Step 3: Add metadata to the provider context**

Construct the context with the existing messages/tools plus the two IDs. Do not add provider-specific options to `lato-core::ModelRequest`. Confirm the legacy codec ignores the extra keys and produces the same canonical request.

- [ ] **Step 4: Run actor and adapter tests**

```bash
cargo test -p lato-agent
cargo test -p lato-ai model_port_adapter
```

Expected: all tests pass.

- [ ] **Step 5: Commit session affinity metadata**

```bash
git add crates/lato-agent/src/actor.rs crates/lato-ai/src/model_port_adapter/codec.rs
git commit -m "feat: pass session affinity to model providers"
```

---

### Task 5: SSE transport and zstd encoding

**Files:**
- Create: `crates/lato-ai/src/codex/sse.rs`
- Modify: `crates/lato-ai/src/codex/mod.rs`
- Modify: `crates/lato-ai/Cargo.toml`
- Modify: `Cargo.lock`
- Test: inline SSE tests using a local TCP fixture

**Interfaces:**
- Produces: `encode_sse_body(body: &Value) -> Result<EncodedBody, String>`.
- Produces: `stream_sse(client, request, mapper, tx) -> Result<TransportOutcome, CodexTransportError>`.
- Consumes: `CodexRequest` and `CodexEventMapper` from Task 3.

- [ ] **Step 1: Add zstd and write a failing round-trip test**

Add `zstd = "0.13"` and assert:

```rust
let encoded = encode_sse_body(&serde_json::json!({"model":"gpt-test"})).unwrap();
let decoded = zstd::stream::decode_all(encoded.bytes.as_slice()).unwrap();
assert_eq!(serde_json::from_slice::<Value>(&decoded).unwrap()["model"], "gpt-test");
assert_eq!(encoded.content_encoding.as_deref(), Some("zstd"));
```

- [ ] **Step 2: Run the test and verify failure**

```bash
cargo test -p lato-ai codex::sse::tests::zstd_body_round_trips
```

Expected: test does not compile because the SSE encoder is absent.

- [ ] **Step 3: Implement bounded zstd encoding and plain fallback**

Serialize once, compress in `spawn_blocking`, and return ordinary JSON bytes when compression returns an error. Set `content-encoding: zstd` only for compressed bytes.

- [ ] **Step 4: Implement incremental SSE submission**

Send Codex SSE headers, read arbitrary byte chunks, reconstruct UTF-8 lines without lossy conversion, parse `data:` records, and pass values to the shared event mapper. Limit error response excerpts and strip control characters.

- [ ] **Step 5: Test chunk boundaries, tools, errors, and cancellation**

The local fixture must split a multi-byte character and a JSON event across chunks, verify compressed request bytes, return a tool call, and hold one request open until cancellation. Assert exact text/tool output and bounded completion time.

- [ ] **Step 6: Run focused and crate tests**

```bash
cargo test -p lato-ai codex::sse
cargo test -p lato-ai
```

Expected: all tests pass.

- [ ] **Step 7: Commit SSE support**

```bash
git add crates/lato-ai/src/codex/sse.rs crates/lato-ai/src/codex/mod.rs crates/lato-ai/Cargo.toml Cargo.lock
git commit -m "feat: add compressed Codex SSE transport"
```

---

### Task 6: Reusable WebSocket transport

**Files:**
- Create: `crates/lato-ai/src/codex/websocket.rs`
- Modify: `crates/lato-ai/src/codex/mod.rs`
- Modify: `crates/lato-ai/Cargo.toml`
- Modify: `Cargo.lock`
- Test: inline tests using a local WebSocket server

**Interfaces:**
- Produces: `CodexWebSocketPool` keyed by the clamped session key.
- Produces: `stream_websocket(request, mapper, tx) -> Result<TransportOutcome, CodexTransportError>`.
- Consumes: `CodexRequest`, `CodexEventMapper`, and the caller cancellation lifetime.

- [ ] **Step 1: Add the WebSocket dependency and failing handshake test**

Add Tokio/rustls WebSocket support without native OpenSSL. The local server records upgrade headers and asserts bearer, account, beta, session, request ID, originator, and user agent values.

- [ ] **Step 2: Run the handshake test and verify failure**

```bash
cargo test -p lato-ai codex::websocket::tests::sends_codex_upgrade_headers
```

Expected: test does not compile because the WebSocket transport is absent.

- [ ] **Step 3: Implement connection establishment and frame processing**

Build an authenticated WebSocket request, apply connect/read timeouts, send one serialized Codex request frame, accept text and binary JSON frames, respond to ping, treat close frames explicitly, and feed events through the shared mapper.

- [ ] **Step 4: Implement bounded per-session reuse**

Store only healthy idle connections. Use a fixed capacity of 32 sessions and a 15-minute idle lifetime. Remove failed and closed entries before reuse. Never share an entry across different session keys.

- [ ] **Step 5: Implement bounded continuation retries**

Allow one reconnect for a connection-limit error before stream start and one clean reconnect when the backend reports missing continuation state. Do not retry either condition twice.

- [ ] **Step 6: Test reuse, isolation, eviction, retry, and cancellation**

Run two requests with one session and assert one upgrade; run another session and assert a separate upgrade; advance the test clock or inject an old idle timestamp and assert eviction. Verify retry counts and that cancellation closes the active request promptly.

- [ ] **Step 7: Run focused and crate tests**

```bash
cargo test -p lato-ai codex::websocket
cargo test -p lato-ai
```

Expected: all tests pass without network access outside loopback.

- [ ] **Step 8: Commit WebSocket support**

```bash
git add crates/lato-ai/src/codex/websocket.rs crates/lato-ai/src/codex/mod.rs crates/lato-ai/Cargo.toml Cargo.lock
git commit -m "feat: add reusable Codex WebSocket transport"
```

---

### Task 7: WS-first orchestration and safe fallback

**Files:**
- Modify: `crates/lato-ai/src/codex/mod.rs`
- Modify: `crates/lato-ai/src/stream.rs`
- Test: inline Codex integration tests

**Interfaces:**
- Produces: `CodexTransport::stream(model, auth, context, tx)`.
- Consumes: SSE and WebSocket transport outcomes.
- Preserves: `HttpModelStream::stream(...) -> Result<(), String>`.

- [ ] **Step 1: Write failing fallback-safety tests**

Use paired local WS/SSE fixtures:

```rust
assert_eq!(pre_stream_failure.sse_requests(), 1);
assert_eq!(pre_stream_failure.output_text(), "fallback-ok");
assert_eq!(post_stream_failure.sse_requests(), 0);
assert!(post_stream_failure.error().contains("after stream start"));
```

- [ ] **Step 2: Run fallback tests and verify failure**

```bash
cargo test -p lato-ai codex_falls_back_only_before_stream_start
```

Expected: fails because `HttpModelStream` still uses generic HTTP streaming.

- [ ] **Step 3: Delegate Codex models from `HttpModelStream`**

Give each `HttpModelStream` a lazily initialized `CodexTransport`. For `OpenaiCodexResponses`, build the Codex request and use WS-first orchestration; for all other APIs, retain the current generic path byte-for-byte.

- [ ] **Step 4: Implement fallback classification**

Fallback only when the WebSocket error is classified as transport-level and the mapper reports no accepted semantic event. Preserve the original WS diagnostic if SSE also fails, while keeping both response excerpts bounded and secret-free.

- [ ] **Step 5: Run transport and regression tests**

```bash
cargo test -p lato-ai codex
cargo test -p lato-ai stream
```

Expected: fallback tests pass and existing generic stream tests remain green.

- [ ] **Step 6: Commit orchestration**

```bash
git add crates/lato-ai/src/codex/mod.rs crates/lato-ai/src/stream.rs
git commit -m "feat: route Codex through resilient transports"
```

---

### Task 8: Documentation, full verification, and local deployment

**Files:**
- Modify: `README.md`
- Modify only if tests expose a defect: files already listed in Tasks 1-7

**Interfaces:**
- Documents: credential ownership, login modes, transport order, and the distinction between ChatGPT subscription access and Platform API keys.
- Verifies: the complete workspace and installed `lato` command.

- [ ] **Step 1: Update user documentation**

Document:

```bash
lato login openai-codex --oauth
lato -p --model openai-codex/codex-mini-latest "reply with hi only"
```

Explain that Lato stores and refreshes its own credential in `$LATO_HOME/auth.json`, does not reuse Pi/Codex CLI stores, prefers WebSocket, safely falls back to compressed SSE before output begins, and requires a ChatGPT plan with Codex access.

- [ ] **Step 2: Run formatting and focused tests**

```bash
cargo fmt --all -- --check
cargo test -p lato-ai
cargo test -p lato-agent
cargo test --test cli_headless
```

Expected: every command exits 0.

- [ ] **Step 3: Run the full workspace tests**

```bash
cargo test --workspace
```

Expected: every workspace test passes.

- [ ] **Step 4: Run strict Clippy**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: exit 0 with no warnings.

- [ ] **Step 5: Inspect the final diff and credential hygiene**

```bash
git diff --check
rg -n "Bearer [A-Za-z0-9_-]{20,}|refresh_token.*[A-Za-z0-9_-]{20,}" crates tests README.md
git status --short
```

Expected: no whitespace errors, no embedded token-like fixtures outside deliberately short synthetic values, and the pre-existing untracked `AGENTS.md` remains untouched.

- [ ] **Step 6: Install locally as required by repository policy**

```bash
cargo install --path .
lato --version
```

Expected: install exits 0 and the installed command reports `lato 0.1.0-beta.1`.

- [ ] **Step 7: Commit documentation and final repairs**

```bash
git add README.md crates/lato-ai crates/lato-agent/src/actor.rs tests/cli_headless.rs Cargo.lock
git commit -m "docs: document native Codex subscription support"
```

- [ ] **Step 8: Report live-validation status accurately**

If no real ChatGPT credential was used, report offline protocol validation as passed and live subscription validation as not run. If an already-configured local Lato credential is explicitly used, run one bounded prompt and report its model, transport, and result without printing credential material.
