# Lato Phase 8B: Plan mode design

**Status:** proposed for design acceptance
**Date:** 2026-09-15
**Product baseline:** Grok Build CLI planning workflow (user guide), reviewed 2026-09-15; WIN-23 gap item 1

## 1. Decision

Phase 8B adds Plan mode: a session-scoped read-only mode in which the model may
investigate the repository and draft an implementation plan, but cannot mutate
anything. The plan is written to one well-known Markdown file (`plan.md`).
The user reviews and approves it; approval is a trusted external gate that
unlocks the normal tool set for subsequent turns. Rejection or revision
requests keep the session read-only.

Plan mode is a capability trim on the existing policy layer, not a new
execution engine. The invariant "approval does not expand sandbox scope"
(`src/permissions.rs`) is preserved: plan approval unlocks ordinary tools
subject to the session's existing trust and sandbox configuration, never more.

## 2. User contract

Enter and exit only through explicit commands; the model cannot toggle the
mode. There is no config file entry and no environment variable: Plan mode is
always available, defaults to off, and is per-session.

TUI commands:

- `/plan` — enter Plan mode. Shows the state machine state, the plan file
  path, and the trimmed tool list. Entering mid-session is allowed; it takes
  effect for the next turn and is refused while a turn is in flight.
- `/plan exit` — leave Plan mode without approving. The plan file is kept on
  disk. Confirm when a draft exists.
- `/plan status` — show state, plan file path, last draft hash, and approval
  record.
- `/plan approve` — human-only approval. Available only in
  `AwaitingApproval`. Shows the plan file for review first in the TUI.

Headless: `lato -p --plan "task"` runs the single turn in Plan mode, prints
the drafted plan path, and exits with a distinct code (`3`) when a plan was
produced but not approved. Headless never auto-approves; approval is reserved
for interactive sessions and ACP.

Resume: Plan mode is session-scoped and not persisted in trust. `lato resume
<id> --plan` re-enters Plan mode; the previous `plan.md` is loaded as the
starting draft if present and unchanged. Without `--plan`, a resumed session
starts in its normal configured trust mode; the model is reminded that a
plan file exists only if the user asks.

Subagents and workflow runs spawned while in Plan mode inherit the plan-mode
tool trim; a child session cannot grant capabilities the parent lacks.

## 3. Plan file

Exactly one plan file per workspace: `<workspace-root>/plan.md`. Not
configurable in Phase 8B; multiple concurrent plans are out of scope. The
file is ordinary UTF-8 Markdown, user-editable at any time, and is expected
to be committed or ignored by the user's own `.gitignore` choice — Lato does
not touch `.gitignore`.

The model writes the plan only through a dedicated internal capability,
not through `write_file`: while in Plan mode, `Lato:write_file`,
`Lato:search_replace`, and `Lato:run_terminal_command` are denied at the
policy layer, and a bounded `plan_draft` write path (target locked to
`plan.md`, hard size cap 128 KiB, atomic same-directory replace) is the only
mutation permitted. This keeps the plan writable while the rest of the
workspace stays read-only, and prevents the model from smuggling edits
through the plan path.

`/plan approve` records the approved content hash. If `plan.md` changes on
disk after approval and before execution begins, the approval is stale and
the gate must be re-confirmed.

## 4. Capability trimming

While Plan mode is active, `PolicyEngine::evaluate()` is consulted with a
plan-mode overlay applied before the normal decision:

Allowed (no approval needed, sandbox obligations unchanged):

- `Lato:read_file`, `Lato:grep`, `Lato:list_dir` — read-only inspection;
- `Lato:todo_write` — planning aid, no workspace effect;
- read-only web tools (`web_search`, `web_fetch`) — no local side effects.

Denied with a structured `PlanModeDenial` (policy code `plan.mode.readonly`):

- `Lato:write_file`, `Lato:search_replace`, `Lato:run_terminal_command`;
- subagent/task spawn tools (children may execute; the trim is inherited
  anyway, but spawning from a plan turn is denied to keep plan turns cheap);
- MCP and plugin tools — any tool whose `ToolDescriptor` reports a mutation
  `SideEffect` or a capability outside the read-only allowlist. Read-only MCP
  tools are denied in Phase 8B as well: allowlisting third-party tools is
  deferred until their descriptors can be trusted.

A denial returns the standard denial shape so the model can re-plan; the
system prompt in Plan mode states the restriction once per turn, not per call.

## 5. Exit state machine

States: `Inactive → Drafting → AwaitingApproval → Approved`, plus `Revising`
and `Exited`.

- `Inactive → Drafting`: `/plan` entered; first model turn starts.
- `Drafting`: model investigates and drafts `plan.md`. Each turn ends with a
  proposal marker; the TUI shows the draft summary and offers
  approve/revise/exit.
- `Drafting → Revising`: user replies with change requests (normal chat);
  stays read-only.
- `Revising → AwaitingApproval`: model signals the revised draft is complete.
- `AwaitingApproval → Approved`: `/plan approve` — recorded with content
  hash, timestamp, and approver. The gate is granted through
  `PolicyEngine::approve_external_gate()`, the path built for exactly this
  trusted-gate shape. Approval persists for the session (ledger entry), not
  across sessions.
- `AwaitingApproval → Revising`: user requests edits instead of approving.
- Any state → `Exited`: `/plan exit`. Plan mode does not auto-exit on
  approval: after `Approved`, the session continues in its normal trust mode
  with the approval gate recorded; the transition out of Plan mode is the
  approval itself.
- The model can never transition the machine. Every edge is a user command
  or a user reply. A model attempt to claim approval (via text, tool call, or
  plan content) is inert.

Durable trace: state transitions and the approval record are appended to the
canonical session journal as non-conversation events, so `/plan status` and
post-hoc review are reproducible. `plan.md` itself remains user-owned.

## 6. Architecture boundary

- `lato-core` owns `PlanModeState`, `PlanModeOverlay`, and the denial code —
  pure types, no I/O.
- `lato-policy` applies the overlay in `evaluate()` and exposes the external
  gate grant; no TUI knowledge.
- `lato-agent` owns the per-session state machine, the bounded `plan_draft`
  write path, journal events, and the Plan-mode system-prompt block.
- the root CLI owns `--plan` on `run`, `-p`, and `resume`, and wires state
  into the session host.
- the TUI owns `/plan` dispatch, draft review view, and approval dialog;
  ACP gains optional `lato/plan/status` and `lato/plan/approve` methods
  (approval mirrors the TUI confirmation; write support of the plan file
  over ACP is deferred).

## 7. Explicitly out of scope

- multiple plan files, plan templates, or per-task plan directories;
- auto-approval, timed approval, or model-proposed "self-approval";
- editing files listed in the plan upon approval (approval only unlocks
  normal mode; execution follows normal policy);
- allowing mutation-capable MCP/plugin tools in Plan mode;
- persisting plan-mode state across resume without `--plan`;
- a `/plan file <path>` override or nested plans.

## 8. Failure contract

Plan mode never blocks session startup or normal sessions. A failed
`plan_draft` write is reported to the model as a tool error and leaves the
previous draft intact (atomic replace). A missing or corrupted journal
approval record degrades to `AwaitingApproval`, never to `Approved`.
If the overlay application fails for an unexpected tool, the safe default is
deny with `plan.mode.readonly` — Plan mode fails closed.

Diagnostics may log state, hashes, and denial counts but never plan contents
beyond what the user's own session transcript already shows.

## 9. Acceptance gate

Focused tests must prove:

1. trim correctness: each builtin tool's allow/deny outcome under Plan mode,
   including subagent inheritance and MCP/plugin denial;
2. the only permitted mutation is the locked `plan_draft` path — size cap,
   atomic replace, escape attempts to other paths are denied;
3. state machine edges: every legal transition, every illegal transition
   refused, no model-reachable edge, mid-turn `/plan` refused;
4. headless `--plan` exit code `3` and no auto-approval;
5. approval via `approve_external_gate` unlocks exactly the normal policy
   path and nothing more; stale-hash re-confirmation triggers;
6. journal trace: transitions and approval record are durable and
   reproducible by `/plan status`;
7. resume without `--plan` starts in normal mode; `--plan` reloads an
   unchanged draft;
8. fail-closed behavior on overlay errors and corrupted approval records;
9. README documents enter, draft, revise, approve, exit, and headless paths.

Release acceptance requires focused tests plus a README walkthrough: enter
Plan mode, force a mutation attempt (must be denied with the structured
code), approve a draft, and verify the following turn can write normally.
