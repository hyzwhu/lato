# Agent HTTP service implementation

The previous goal turn made verified progress: immutable application configuration, real tool registration, runtime catalog restrictions, and durable application identity are implemented, tested, and locally installed. The full goal remains active.

This increment gives application developers a usable HTTP integration boundary over those sessions. The service accepts work asynchronously and keeps cancellation and approvals available while a model/tool call is running. A host-wide mutex must never be held for the duration of a prompt.

## Contract

- `lato serve --model provider/model --listen 127.0.0.1:8765` uses the selected real provider and existing credentials. `LATO_SERVICE_TOKEN` is required, with at least 32 printable bearer-token characters. Binding is loopback-only; remote TLS and multi-user authentication belong at a product gateway.
- HTTP `/v1` routes create/resume/close/list sessions, submit prompt or explicit Skill operations, inspect operation results, cancel active operations, inspect/answer pending tool approvals, and stream session events through SSE. Requests have strict JSON bodies and bounded sizes. One active operation is admitted per session; bounded global operation/session history prevents unbounded accumulation.
- All routes, including health, require `Authorization: Bearer ...`. Unknown routes and errors use versioned JSON error objects. No browser CORS expansion or credential-bearing query parameters.
- Operation submission returns 202 before model completion. Running work survives a dropped HTTP response/SSE connection. Cancellation races with queued work are recorded, and cancellation is checked before submission. Shutdown denies approvals, cancels operations, closes runtime sessions, and bounds drainage.
- SSE has a cursor and bounded replay. Too-old/future cursors receive an explicit error; a slow subscriber receives a gap event and disconnects, rather than silently losing state. Oversized event payloads are explicitly identified. Terminal operation results are available independently of SSE.
- The existing policy/approval membrane remains authoritative. A remote approval bridge waits for a single decision, denies on timeout/disconnect/cancellation, and never changes sandbox scope. No blanket automatic approval unless the application explicitly selects it.
- Service jobs and SSE replay are process-local; the session journal remains durable. Restart requires explicit session resume; operations are never silently retried.

## Implementation and verification

- [x] Add a `lato-service` library crate with authentication, event replay, approval bridge, operation/session admission, HTTP routes, and bounded shutdown.
- [x] Expose safe host accessors for cloned runtime-session handles and application configuration, enabling concurrent requests without duplicating the agent loop.
- [x] Wire `serve` CLI parsing, provider selection, plugin roots, sandbox, and termination. Add `--agent-config` for process-specific application configuration and `--trust-workspace` for explicit extension trust.
- [x] Exercise real loopback HTTP with a controllable model, two business tools, approval allow/deny/timeout, concurrent cancel/status/event requests, request rejection, replay gaps, reconnect, dropped clients, and shutdown.
- [x] Add a minimal client example and endpoint documentation; run relevant/workspace tests and lint, install with the tested lockfile, and smoke the installed command.
- [x] Add a runnable MCP business plugin and verify the installed executable → local HTTP model protocol → MCP subprocess → lookup/report modules → approval → generated artifact chain.

## Verified result

On 2026-10-21, all 1,578 workspace tests passed across 133 suites. Clippy with all targets/features and denied warnings, formatting, and diff checks passed. `cargo install --path . --locked` replaced the local executable. The installed business smoke completed three provider requests, two business module calls, two exact-call approvals, and verified the Markdown artifact. The example Python client also passed against the installed service. See `docs/testing/reports/2026-10-21-agent-http-service.md`.

Implementation uses the official Axum SSE and middleware interfaces: https://docs.rs/axum/latest/axum/response/sse/ and https://docs.rs/axum/latest/axum/middleware/. No separate design-approval gate is required under the user's current project instructions.

Application-aware Workflow delegation is still required by the overall goal. Inspection confirms that workflow workers currently construct their own catalogs and coding-oriented child instructions. That needs explicit inheritance of application identity, custom tools, and policy ceilings; simply removing the validation guard would lose application scope. It will be implemented as a subsequent increment with its own real execution tests.
