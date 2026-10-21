# Agent HTTP API v1

Lato can host an application agent behind an authenticated local HTTP service.
Use a dedicated `LATO_HOME` for each application. Its `agent.json`, credentials,
plugins, and durable sessions are the same ones used by the CLI and ACP host.

## Start

Select and log in to a real model using the existing Lato model configuration.
The service requires an explicit `--model provider/model` selection and never
falls back to the built-in test model.

```bash
export LATO_SERVICE_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
export LATO_MODEL="your-provider/your-model"
lato serve --model "$LATO_MODEL" --listen 127.0.0.1:8765
```

The token must contain 32–1024 bearer-token characters. It is read from the
environment and never printed by the startup message. All API routes require
`Authorization: Bearer <token>`, including health and SSE. The listener accepts
loopback addresses only. To expose a product API remotely, put authentication,
user/tenant authorization, TLS, and application routing in your gateway. One
Lato service credential represents one trusted application principal, not an
end-user identity. Browser apps can use a same-origin backend proxy; this service
does not enable cross-origin browser access.

The default sandbox is `workspace`, and writes require explicit approvals through
the API. `--sandbox read-only` or `--sandbox off` selects another existing scope.
`--auto-approve` explicitly selects automatic approval; deny rules and scope
checks still apply. Extensions are denied in an untrusted workspace. Add
`--trust-workspace` when enabling trusted application skills/MCP servers, and
use `--plugin-dir PATH` for trusted plugin roots. Plugins must also be enabled
in the normal configuration. Folder trust and tool approval remain separate.
`--agent-config PATH` selects a process-specific application configuration without
editing `$LATO_HOME/agent.json`. It can reuse existing provider login credentials;
use a dedicated home when separate applications also need separate journals/plugins.

A general-mode application can begin with this `$LATO_HOME/agent.json`:

```json
{
  "mode": "general",
  "instructions": "Use the configured business modules to complete user tasks. Ask for missing required information and report tool failures honestly.",
  "tools": ["builtin:search_tool", "builtin:use_tool", "builtin:skill", "builtin:workflow"],
  "workspaceInstructions": false
}
```

Configure the MCP business servers and skills separately. `tools: []` creates a
conversation-only application. Rust applications can supply `ServiceConfig.agent`
and register `Arc<dyn Tool>` modules through `ServiceConfig.tools` before calling
`AgentService::new(config, model_stream)` and `serve(listener, shutdown_token)`.

## Routes

| Method and path | Request | Behavior |
|---|---|---|
| `GET /v1/health` | — | Process readiness |
| `GET /v1/info` | — | Application mode, selected tool names and admission limits; omits application instructions |
| `GET /v1/sessions` | — | Durable session listing |
| `POST /v1/sessions` | `{}` | Creates an open session, returns 201 and `sessionId` |
| `POST /v1/sessions/resume` | `{"sessionId":"..."}` | Explicitly reopens a durable session with the same application configuration |
| `GET /v1/sessions/{id}` | — | Open-session state and active operation ID |
| `POST /v1/sessions/{id}/operations` | `{"text":"..."}` | Admits a natural-language task and returns 202 with `operationId` |
| `POST /v1/sessions/{id}/operations` | `{"skill":{"name":"plugin:skill","args":"...","context":"..."}}` | Runs one explicit user-invocable skill through the same agent loop |
| `GET /v1/operations/{id}` | — | Accepted/running/cancelling/completed/cancelled/failed status and terminal result |
| `POST /v1/sessions/{id}/cancel` | `{}` | Requests cancellation without waiting for model completion |
| `POST /v1/sessions/{id}/close` | `{}` | Cancels and drains the live session; retains its durable journal |
| `GET /v1/sessions/{id}/skills` | — | Lists the current user-invocable plugin skills |
| `GET /v1/sessions/{id}/approvals` | — | Pending exact-call approvals |
| `POST /v1/sessions/{id}/approvals/{approvalId}` | `{"allow":true}` or `{"allow":false}` | Consumes one pending decision; stale or repeated decisions return 409 |
| `GET /v1/sessions/{id}/events` | Optional `?after=<sequence>` or `Last-Event-ID` | Replays retained session events and follows live SSE updates |

JSON request bodies reject unknown fields and are limited to 64 KiB. An operation
must supply exactly one of `text` or `skill`. Skill names are limited to 256 bytes,
arguments to 8 KiB, and optional context to 32 KiB. HTTP errors are JSON:

```json
{"apiVersion":"v1","error":{"code":"service.session_busy","message":"session already has an active operation or is closing"}}
```

## Submit and observe

```bash
curl -sS -H "Authorization: Bearer $LATO_SERVICE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{}' http://127.0.0.1:8765/v1/sessions
```

Use the returned `sessionId` to submit a task. Store the `operationId` before
polling its status. The response is an admission receipt; it does not assert that
the task finished. Operations execute independently of the request connection.
Closing an SSE tab or disconnecting an HTTP client does not cancel admitted work.

```json
{
  "apiVersion": "v1",
  "operationId": "op-...",
  "sessionId": "s...",
  "status": "completed",
  "result": {"text": "Report ready.", "truncated": false, "originalBytes": 13},
  "error": null
}
```

Only one operation runs per session. A second submission returns 409 rather than
silently queuing. Up to 16 operations run across 64 open sessions. The most recent
128 operation records are retained in memory. Completed text is bounded to
256 KiB and reports `truncated` and `originalBytes` when it exceeds that limit.
The canonical session journal retains the complete model-visible conversation.

For a minimal working client with polling and tool approvals, run:

```bash
python3 examples/general-agent/http_client.py \
  --url http://127.0.0.1:8765 '查询客户并生成报告'
```

The client reads `LATO_SERVICE_TOKEN`, creates a session, and sends one operation.
It asks about each pending approval on an interactive terminal and denies it when
stdin is non-interactive. `--allow-writes` explicitly approves pending exact calls
for that client session. The example never automatically retries a submission.

## Approvals and cancellation

A pending approval includes its ID, session/call IDs, tool name, summary, declared
capabilities, side effect, exact-call fingerprint, and the validated arguments
after any pre-tool hook rewrite. Arguments above 32 KiB are omitted with explicit
`argumentsTruncated` and `argumentsBytes` fields. The tool remains blocked
until the client answers. Approvals expire after 120 seconds by default; the Rust
service configuration can select a positive timeout up to 300 seconds. Timeout,
session cancellation, session close, or service shutdown denies pending approval.
An approval never widens the sandbox or bypasses existing policy checks.

Cancellation is cooperative at the existing runtime boundary. A call that already
finished can still produce a completed result when cancellation arrives late.
For uncertain interrupted side effects, the journal's existing recovery checks
remain authoritative. Do not automatically resubmit a write after a connection
error or interrupted operation.

## SSE and reconnection

Each SSE frame uses `event: update`, a monotone `id`, and JSON data containing
`sequence`, `sessionId`, `kind`, and `data`. Kinds include existing runtime deltas
and tool events, plus `operation_accepted`, `operation_cancelling`,
`operation_finished`, `approval_requested`, and `approval_closed`. Runtime updates
are flushed to the event stream before the operation's terminal event. The
terminal result contains the full retained answer, independently of deltas.

```bash
curl -N -H "Authorization: Bearer $LATO_SERVICE_TOKEN" \
  http://127.0.0.1:8765/v1/sessions/SESSION_ID/events
```

Keep the last SSE ID and reconnect with `Last-Event-ID` or `after`. IDs belong to
this service process and are global across its sessions; events are filtered by
the selected session. The ring retains at most 512 events and 2 MiB. Without a
cursor, a connection replays the retained portion. A cursor older than the ring
receives 409 `service.event_gap`; a future or invalid cursor receives 400. A slow
live subscriber receives `event: gap` and disconnects. Inspect operation status
and reconnect without a cursor to resynchronize. Events larger than 64 KiB contain
an explicit `eventTruncated` notice instead of a partial JSON object.

## Restart and lifecycle

SIGINT/SIGTERM stops admission, closes event streams, denies approvals, cancels
active operations, and drains sessions with bounded shutdown waits. Journals are
durable; operations, pending approvals, and event replay are process-local. After
restart, explicitly resume the session. Old operation IDs return 404 and no model
or tool request is automatically replayed. The service does not promise submission
idempotency across request retries or restarts; a client must not retry a write
submission without reconciling the session journal/application state.

General applications may explicitly select `builtin:workflow`. Its natural-language
starts use the ordinary tool approval membrane and bind the listed workflow ID,
content revision, arguments and agent budget. There is no HTTP shortcut to start
an arbitrary script. Generic child-task and AgentField delegation remain excluded
from custom configurations. The service represents one application; tenant
isolation and a desktop workbench belong to the integrating product.

## Business Workflows

| Method and path | Request | Behavior |
|---|---|---|
| `GET /v1/sessions/{id}/workflows` | — | Discover trusted named definitions |
| `GET /v1/sessions/{id}/workflow-runs` | — | Retained runs including status, phase, budget and completed result |
| `POST /v1/sessions/{id}/workflow-runs/pause` | `{"name":"display-name"}` | Request pause at the next host boundary; poll until paused |
| `POST /v1/sessions/{id}/workflow-runs/resume` | `{"name":"display-name","agentBudget":4}` | Resume an eligible run; optional budget 1–1024, must increase after budget exhaustion |
| `POST /v1/sessions/{id}/workflow-runs/stop` | `{"name":"display-name"}` | Cancel and drain a run |

The model-visible Workflow tool's `list` action includes content revisions needed
for `start`. Workflow scripts are trusted application code. General-mode workers
receive the same application identity and an intersection of its exact tool list
with their role's capabilities. They cannot obtain the Workflow manager themselves.
Worker `read-write` mode includes selected external business modules; read-only
roles exclude mutation tools, including generic MCP invocation brokers. Use direct
read-only SDK tools for such roles. General workers share the inherited workspace;
`isolation_worktree` is reserved for coding mode. Parent read-only sandboxes remain
read-only in children. Optional `output_schema` contracts validate business JSON
with the existing bounded correction retry; no coding-specific output fields are
required. Validation proves output structure, not the truth of business claims.

A main operation can finish after launching a background run. Keep the session
open, poll Workflow runs and continue handling approvals until it finishes or
requires attention. `cancel` cancels the main operation; use the Workflow `stop`
route to cancel a particular background run. Closing the session drains its runs.
There are at most four active runs per session; each uses its explicit agent budget
(maximum 1024) and the existing bounded child concurrency. These are separate from
the main-operation admission limit. The sample Python client waits for active runs
and leaves paused sessions open for follow-up.

Delegated events use `lato/delegated/update` with the parent `sessionId` and the
original child update nested in `update`. Pending approvals appear under that same
parent; `executionSessionId` identifies the actual child, while the fingerprint
still binds its original exact policy request. A different session cannot answer
that approval. Pause/stop cancels pending child approvals; resuming requires fresh
decisions. Completed `result` values are retained up to 256 KiB; larger values
include an explicit notice and `resultTruncated: true`. Return artifact references
for larger outputs.

Configured Workflow persistence uses version 2 records bound to the application
configuration fingerprint. Only the same configuration can restore them; legacy
managers and other application identities skip them. Existing legacy version 1
coding runs retain their behavior. Active-at-restart runs become interrupted;
paused runs can resume through the same session's Workflow routes.

## Runnable business module example

`examples/general-agent/business-plugin` is a complete stdlib-only Python MCP
plugin with a customer lookup module, a report module that writes a Markdown
artifact, and a customer-report skill. Its customer records are synthetic.

```bash
lato serve --model "$LATO_MODEL" --agent-config examples/general-agent/agent.json \
  --plugin-dir examples/general-agent/business-plugin --trust-workspace
python3 examples/general-agent/http_client.py '查询 Acme 并生成客户报告'
```

The module writes under its own `outputs/` directory. Review and approve the
report call through the client/API. The resulting report states its synthetic
data source. Replace these module bodies with your own authenticated business
API adapters when integrating real functions.

To verify the entire executable → HTTP provider → MCP → two business modules →
approval → artifact chain without credentials or inference charges:

```bash
python3 examples/general-agent/smoke_http.py --binary "$HOME/.cargo/bin/lato"
```

This smoke uses the installed Lato executable, a real local HTTP connection, and
a real MCP subprocess. Model decisions are scripted, so this verifies execution
plumbing rather than live-model task accuracy. It uses a temporary workspace and
removes only its own fixtures afterward.

To verify a background Workflow that delegates lookup and report to two business
workers using the same real HTTP/MCP transport and synthetic data:

```bash
python3 examples/general-agent/smoke_http.py --binary "$HOME/.cargo/bin/lato" --workflow
```

For live use, copy `examples/general-agent/customer-report.rhai` into
`$LATO_HOME/workflows/customer-report.rhai` and ask the client to run it with
`customer` set to `Acme`. The example requires no coding tools.
