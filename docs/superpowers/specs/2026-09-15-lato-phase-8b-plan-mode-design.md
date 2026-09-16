# Lato Phase 8B: Plan mode design

**Status:** frozen v1.0, proposed for design re-acceptance
**Date:** 2026-09-15
**Revision:** 2026-09-16 (resolves the five blocking findings on PR #8)
**Product baseline:** Grok Build CLI planning workflow (user guide), reviewed 2026-09-15; WIN-23 gap item 1

## 1. Decision

Phase 8B adds Plan mode: a session-scoped read-only mode in which the model may
investigate the repository and draft an implementation plan, but cannot mutate
anything. The plan is written to one well-known Markdown file (`plan.md`).
The user reviews and approves it; approval records a session-local plan
authorization, then subsequent turns return to the ordinary per-tool policy
path. It is not an execution grant and does not authorize any tool call.
Rejection or revision requests keep the session read-only.

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
- `/plan submit` — the human declares the current draft ready for review and
  moves `Drafting` or `Revising` to `AwaitingApproval`. Model output, a marker,
  or a `plan_draft` call never performs this transition.
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

Phase 8B does not permit creating subagents, tasks, or workflow runs from a
Plan-mode turn. Their spawn descriptors are absent from the model-visible
catalog and the policy overlay also denies forged/direct calls. Sessions and
runs that already existed before `/plan` are independent and receive no new
authority from the parent. Child inheritance is therefore not a Phase 8B
execution path; inherited trimming is deferred until Plan-mode spawning is
introduced by a later design.

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

`/plan approve` records the approved content hash in a session-local
`PlanApproval` record. Every subsequent mutation-capable tool call performs a
fresh bounded read and hash comparison twice: once before ordinary policy
preparation and again immediately before that call's execution grant is
consumed. Both checks run while holding the session Plan-state mutex; a
mismatch atomically clears `PlanApproval`, transitions `Approved → Revising`,
and denies that call with `plan.approval_stale`. This applies to the first and
every later mutation, not only the first mutation after approval. Read-only
calls do not consume or bypass this guard. A new `/plan submit` followed by
`/plan approve` is required; an old approval record can never become valid
again merely because the file is changed back to the same bytes.

The second check closes changes during tool preparation. An external process
can still modify `plan.md` after that check, but this cannot change the already
fingerprinted tool request or grant it additional authority; the next mutation
detects the change and fails closed. The journal records the approval's unique
generation as well as its hash so revocation is monotonic.

### 3.1 `plan_draft` secure publication contract

`plan_draft` accepts Markdown content only; it accepts no caller-supplied path.
The implementation derives the target by joining the immutable, canonical
workspace root with the literal `plan.md`. The UTF-8 byte length is checked
before any filesystem mutation and values greater than 131,072 bytes are
rejected without truncation.

Publication uses the session's shared `FileLocks` instance and this exact
order:

1. acquire the `FileLocks` entry for the derived target and hold it through
   publication and directory sync;
2. re-resolve the target parent and require it to be the canonical workspace
   root; reject a missing/non-directory parent, path escape, or changed parent
   identity;
3. inspect the destination with non-following metadata; reject symlinks and
   any existing non-regular file;
4. create a collision-resistant sibling temporary file with create-new
   semantics (and user-only permissions where supported), write the already
   bounded bytes, flush, and sync the file;
5. repeat the parent-identity and non-following destination checks, then
   atomically rename the sibling over `plan.md` without following the
   destination; and
6. sync the workspace-root directory before releasing the lock. On any error,
   remove only the temporary file created by this call and leave the previous
   `plan.md` intact.

No `create_dir_all`, target canonicalization through a symlink, cross-directory
temporary file, in-place truncate, or silent content truncation is permitted.
All aliases of the workspace-root `plan.md` share the same `FileLocks` key.

## 4. Capability trimming

While Plan mode is active, `PolicyEngine::evaluate()` is consulted with a
plan-mode overlay applied before the normal decision:

Allowed (no approval needed, sandbox obligations unchanged):

- `Lato:read_file`, `Lato:grep`, `Lato:list_dir` — read-only inspection;
- `Lato:todo_write` — planning aid, no workspace effect;
- read-only web tools (`web_search`, `web_fetch`) — no local side effects.

Denied with a structured `PlanModeDenial` (policy code `plan.mode.readonly`):

- `Lato:write_file`, `Lato:search_replace`, `Lato:run_terminal_command`;
- subagent/task/workflow spawn tools (not model-visible in Plan mode and also
  denied at policy evaluation to prevent forged/direct calls);
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
- `Drafting`: model investigates and drafts `plan.md`. The TUI may show a
  draft summary and offer submit/revise/exit, but model text and proposal
  markers are display-only and have no state-machine authority.
- `Drafting → Revising`: user replies with change requests (normal chat);
  stays read-only.
- `Drafting|Revising → AwaitingApproval`: `/plan submit`, issued by the human.
  A model claim that the plan is complete is inert and the state is unchanged.
- `AwaitingApproval → Approved`: `/plan approve` — recorded with content
  hash, timestamp, approver, and unique generation. This creates a
  `PlanApproval`, not a policy `ExecutionGrant`.
- `AwaitingApproval → Revising`: user requests edits instead of approving.
- `Approved → Revising`: a mutation preflight detects a changed or unreadable
  `plan.md`; it revokes the current approval generation and denies the call.
- Any state → `Exited`: `/plan exit`. Plan mode does not auto-exit on
  approval: after `Approved`, the session continues in its normal trust mode
  with the plan approval recorded; approval is the transition out of the
  read-only overlay, not a tool authorization.
- The model can never transition the machine. Lifecycle edges are driven by a
  user command or reply; the sole integrity edge, `Approved → Revising`, is
  caused by the user-owned plan file changing and is only detected/enforced by
  the guard. A model attempt to claim submission or approval (via text, tool
  call, marker, or plan content) is inert.

Durable trace: state transitions and the approval record are appended to the
canonical session journal as non-conversation events, so `/plan status` and
post-hoc review are reproducible. `plan.md` itself remains user-owned.

## 6. Architecture boundary

- `lato-core` owns `PlanModeState`, `PlanModeOverlay`, and the denial code —
  pure types, no I/O.
- `lato-policy` applies the overlay in `evaluate()`; no TUI or plan-approval
  knowledge. `PolicyEngine::approve_external_gate()` remains unchanged and is
  not called by `/plan approve`.
- `lato-agent` owns the per-session state machine, the bounded `plan_draft`
  write path, `PlanApproval` generation/revocation, mutation preflight guard,
  journal events, and the Plan-mode system-prompt block.
- the root CLI owns `--plan` on `run`, `-p`, and `resume`, and wires state
  into the session host.
- the TUI owns `/plan` dispatch, draft review view, and approval dialog;
  ACP gains optional `lato/plan/status` and `lato/plan/approve` methods
  (approval mirrors the TUI confirmation; write support of the plan file
  over ACP is deferred).

After the Plan guard accepts a mutation request, the normal runtime resolves
and prepares that exact `PolicyRequest`. Ordinary policy evaluation still
returns `Allow`, `RequireApproval`, or `Deny`. `Allow` receives the existing
runtime-owned ephemeral grant; `RequireApproval` requires its own human consent
and `approve_external_gate()` issues a short-lived, single-use grant bound to
that one request fingerprint; `Deny` remains denied. Immediately before
execution, the Plan guard rechecks the plan and the runtime independently
recomputes the request fingerprint and atomically consumes only that call's
grant. No grant crosses a call, turn, or session boundary, and Plan approval
cannot be substituted for a policy grant.

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
approval record degrades to `Revising`, never to `Approved`; it requires a new
human submit and approval sequence.
If the overlay application fails for an unexpected tool, the safe default is
deny with `plan.mode.readonly` — Plan mode fails closed.

Diagnostics may log state, hashes, and denial counts but never plan contents
beyond what the user's own session transcript already shows.

## 9. Acceptance gate

Focused tests must prove:

1. trim correctness: each builtin tool's allow/deny and model-visibility
   outcome under Plan mode, including forged subagent/task/workflow spawn and
   MCP/plugin denial;
2. the only permitted mutation is `plan_draft`: no path argument, UTF-8 byte
   limits at 131,072/131,073, shared-target serialization, symlink/non-regular
   destination rejection, parent/path-swap rejection, same-directory
   create-new temporary file, crash/error preservation of the old file,
   atomic replace, file and directory sync, and temporary-file cleanup;
3. state machine edges: every legal transition and every illegal transition
   refused; only `/plan submit` reaches `AwaitingApproval`; model text,
   proposal markers, and tool calls are inert; mid-turn `/plan` is refused;
4. headless `--plan` exit code `3` and no auto-approval;
5. plan approval creates no `ExecutionGrant`; each later tool call traverses
   normal policy and uses only its own fingerprint-bound, single-use grant.
   Tests mutate `plan.md` before preparation and between preparation and grant
   consumption, and on a later mutation after one successful mutation: each
   mismatch denies the call, atomically revokes the approval generation,
   enters `Revising`, and prevents replay of the old approval;
6. journal trace: transitions and approval record are durable and
   reproducible by `/plan status`;
7. resume without `--plan` starts in normal mode; `--plan` reloads an
   unchanged draft;
8. fail-closed behavior on overlay errors, unreadable/oversized plan reads,
   and missing or corrupted approval records;
9. README documents enter, draft, revise, approve, exit, and headless paths.

Release acceptance requires focused tests plus a README walkthrough: enter
Plan mode, force a mutation attempt (must be denied with the structured
code), approve a draft, and verify the following turn can write normally.
