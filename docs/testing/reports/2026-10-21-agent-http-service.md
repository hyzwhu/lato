# Agent HTTP service — Verification

Date: 2026-10-21

## Delivered

- `lato-service` library and `lato serve` CLI, using an explicitly selected real provider endpoint rather than a silent test-model fallback.
- Authenticated loopback-only API v1: asynchronous operations, independent status/cancellation, live/replayed SSE, explicit Skill calls, session resume/close, and exact-call approvals.
- Process-specific `--agent-config`, existing credentials, plugin-root discovery, explicit workspace trust, sandbox selection, and opt-in automatic approval.
- Bounded session/operation/event retention, visible replay gaps and truncation, cancellation-safe prompt observation, and bounded shutdown.
- Backward-compatible approval hook carrying the canonical arguments after validation/pre-tool rewrites. Existing approvers retain their behavior; service clients can review actual parameters.
- Thread-safe asynchronous credential lookup callbacks so the existing ACP host can serve concurrent transport requests without a separate agent loop.
- Runnable MCP customer lookup/report plugin, synthetic dataset, report Skill, Python client, and installed-command smoke.

## Results

| Verification | Result |
|---|---|
| `cargo test --workspace --locked --no-run` | Passed |
| `cargo test --workspace --locked -- --test-threads=1` | Passed: 1,578 tests, 0 failures, 0 ignored; 133 suites including doc tests |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed |
| Service HTTP integration tests | 10 passed, plus bounded-event unit coverage |
| CLI service tests | Passed: explicit model/token/bind checks and a real CLI → HTTP provider-protocol request with graceful process termination |
| Python example syntax and direct MCP module smoke | Passed |
| `cargo install --path . --locked` | Passed; replaced `/Users/huangyongzhao/.cargo/bin/lato` |
| Installed `lato serve --help` | Passed; serves model, scope, trust, plugin, and application-configuration options |
| Installed `smoke_http.py` | Passed: 3 provider calls, 2 business module calls, 2 exact-call approvals, verified generated report |
| Installed service + `http_client.py` | Passed: selected-provider transport, returned answer, closed session |

The HTTP suite proves responsiveness while a model is blocked, concurrent session
creation, per-session/global admission, cancellation followed by session reuse,
cancelled/denied/expired approvals, exact arguments and single-use decisions,
cross-session approval rejection, strict/oversized input rejection, authenticated
error responses, event ordering/replay/gaps, disconnected clients, UTF-8 result
truncation, durable session restart, and both explicit/model-selected Skills.

Earlier full runs overlapped compilation with tests and exposed the existing
two-second MCP hook fixture deadlines under machine load. The final run built
all binaries first and ran tests serially; no hook deadline/assertion was weakened.
One earlier doc-test run also mixed updated source with old dependencies while
edits were in progress; the final stable-source run passed. The first runnable
plugin smoke found a timeout-unit mistake in the example: `timeout: 30` meant
30 milliseconds. The example now correctly specifies 30,000 milliseconds.

## Scope of evidence

The installed smoke runs the actual executable, authenticated local HTTP server,
provider-protocol decoder, MCP subprocess, real data lookup/report functions,
tool policy/approvals, canonical session journal, and filesystem artifact creation.
Model decisions are scripted by a local HTTP fixture and customer data is
synthetic. This verifies real execution plumbing without claiming live-model
intent recognition, task accuracy, or integration with the user's production APIs.
The user's provider credentials and default application files were not modified
by these smokes; fixtures use isolated temporary homes/workspaces.

The API represents one trusted application principal. Product gateways still own
end-user/tenant authorization. Operations, approvals and SSE replay are
process-local; session journals are durable. Submission retries are not guaranteed
idempotent and are never performed automatically. Trusted plugin hooks retain
their existing behavior, and the allowed MCP gateway exposes the configured
trusted MCP surface.

## Remaining goal work

Custom application profiles still exclude built-in child-task, Workflow, and
AgentField delegation. Workflow workers currently construct independent catalogs
and coding-oriented context. The next increment must inherit application
instructions, registered business tools, and permission ceilings through those
workers, and verify an actual Workflow business chain. The overall goal remains
active; this report completes the HTTP integration increment only.
