# Lato Session Metadata and Management Design

## Goal

Replace opaque session-ID-only navigation with useful local titles, manual rename, and confirmed permanent deletion across both the CLI and TUI. Preserve the existing journal format and ACP `session/list` response so current clients and persisted sessions remain compatible.

## Scope

This increment includes:

- deterministic titles derived locally from the first accepted user prompt;
- manual titles that override the derived title;
- structured session summaries for CLI and TUI consumers;
- CLI rename and delete commands;
- TUI rename and two-step permanent-delete controls;
- safe coordination with live session writers;
- lazy compatibility for sessions created before metadata existed.

It does not include model-generated titles, archive/restore, content search, bulk mutation, or generated summaries.

## Reference Architecture

Follow Grok Build's per-session summary pattern rather than Codex's SQLite plus append-only compatibility index. Each Lato session directory gains a small mutable metadata sidecar while `events.jsonl` remains the append-only source of runtime truth.

```text
$LATO_HOME/sessions/<session-id>/
  events.jsonl
  metadata.json
  metadata.json.lock
```

The lock file has a stable path and is never renamed. Metadata updates take an exclusive lock across read-modify-write, write a sibling temporary file with restrictive permissions, flush and sync it, atomically replace `metadata.json`, and sync the session directory. This mirrors the concurrency and durability properties of Grok Build's `summary.json` updates.

## Metadata Contract

`metadata.json` uses a versioned schema:

```json
{
  "schema_version": 1,
  "session_id": "s...",
  "title": "Add session management",
  "title_source": "automatic",
  "created_at_ms": 1788451200000,
  "updated_at_ms": 1788451212345
}
```

`title_source` is `automatic` or `manual`. Manual rename always wins and later automatic initialization must not overwrite it. Metadata parsing fails closed for mutation: corrupt or mismatched metadata is reported and is never silently replaced. Listing remains resilient by falling back to journal-derived display data while surfacing no secret content.

Title normalization is shared by automatic and manual paths:

- remove C0/C1 control characters;
- collapse whitespace and newlines to single spaces;
- trim surrounding whitespace and common quote characters;
- cap at 60 Unicode scalar values without splitting UTF-8;
- reject an empty manual title;
- use `New session` when automatic derivation has no usable text.

Automatic titles are derived from the first `TurnInputAccepted` record. For ordinary prose, the title is the first 10 whitespace-delimited words within the 60-character cap. This is deterministic, offline, and compatible with legacy journals.

## Store API

Extend `lato-store` with a metadata service colocated with `FileEventStore`. Its public operations are:

- `list_session_summaries()` — returns summaries ordered by most recently updated, then session ID;
- `ensure_automatic_title(session_id, first_prompt)` — atomically creates or fills only an absent automatic title;
- `rename_session(session_id, title)` — validates and persists a manual title;
- `delete_session(session_id)` — permanently removes one validated session directory after its writer is shut down.

The service validates `SessionId`, rejects symlinked metadata and session paths, and confines every operation beneath the configured sessions directory. Deletion is idempotent at the storage layer. It must never accept a raw path from an RPC or CLI caller.

Legacy sessions have no eager migration. Listing replays only enough bounded journal data to locate the first accepted input and timestamps, then returns a derived summary. The first successful rename or automatic-title persistence creates `metadata.json` atomically.

## Runtime and Protocol

Keep ACP `session/list` unchanged (`{"sessions":["id", ...]}`) for compatibility. Add Lato extension methods:

- `lato/session/list` → `{"sessions":[SessionSummary, ...]}`;
- `lato/session/rename` with `{sessionId, title}`;
- `lato/session/delete` with `{sessionId}`.

The host owns mutation coordination:

1. Rename refuses while the target session has an active turn, then applies the metadata patch.
2. Delete refuses while a turn is active, removes the session from the resident map, shuts down its event writer, and only then deletes the directory.
3. Failures leave the session visible and return a stable error rather than optimistically claiming success.

An automatic metadata initialization is attempted after the first user input has been durably accepted. Failure is non-fatal to the turn because the journal remains authoritative and the title can be derived during listing.

## CLI

Change `sessions` into a command with optional nested actions while preserving plain `lato sessions` and `lato sessions --json`:

```text
lato sessions
lato sessions --json
lato sessions rename <SESSION_ID> <TITLE>
lato sessions delete <SESSION_ID>
lato sessions delete <SESSION_ID> --yes
```

Human listing prints title first, then the durable ID and relative/absolute update time as space permits. JSON advances to schema version 2 and returns structured entries. Rename prints the normalized saved title.

Deletion is permanent. Without `--yes`, the CLI requires an interactive terminal and asks the user to type an explicit confirmation. Non-interactive deletion fails with guidance to pass `--yes`; `--yes` is the scripting opt-in and does not bypass runtime safety checks.

## TUI

The persistent session side panel and `/sessions` picker show `title` as the primary label and the durable ID as secondary text. Filtering matches both fields.

When the sessions panel is focused:

- `r` opens a rename dialog prefilled with the current title;
- `d` arms deletion for the selected row;
- a second `d` within a short confirmation window permanently deletes that same row;
- selection changes, Escape, timeout, or any unrelated key cancels the armed deletion.

The footer and dialog explicitly say that deletion is permanent. Mutation is unavailable during a running turn. On success, rename updates the visible row and delete removes it without restarting the TUI. On failure, local state is retained and the error appears in the conversation/status area.

Slash commands `/rename <title>` and `/delete` operate on the current session. `/delete` uses a confirmation dialog and returns to a new session after success. Help text and Chinese/English strings are updated together.

## Error Handling and Safety

- Metadata schema versions newer than supported produce a stable unsupported-schema error.
- Empty, oversized, or control-only manual titles are rejected before I/O.
- Session IDs are parsed through `SessionId`; callers cannot select arbitrary paths.
- Symlinked/non-directory session roots and symlinked/non-regular metadata files are rejected.
- Rename uses lock plus atomic replacement, so crashes retain either the old or new complete file.
- Delete shuts down and removes resident writers before filesystem removal.
- TUI confirmation is bound to the exact selected session ID and expires; it is rechecked at execution time.
- The store treats a missing directory as an idempotent delete, while user-facing commands report an unknown session when it was not present at request validation.

## Testing

`lato-store` tests cover metadata round trips, Unicode normalization, manual-title precedence, lazy legacy derivation, atomic replacement, concurrent updates, symlink rejection, wrong session IDs, corrupt/newer schemas, idempotent deletion, and deletion after writer shutdown.

Agent/host tests cover the three extension methods, unchanged `session/list`, unknown sessions, active-turn refusal, resident shutdown, and mutation failure propagation.

CLI tests cover parsing, human and schema-v2 JSON output, rename validation, interactive delete refusal without a TTY, `--yes`, and exit codes.

TUI state/widget tests cover title rendering, ID fallback, filtering by either field, rename success/failure, delete arming and expiry, selection-bound confirmation, running-turn refusal, and bilingual help. Existing PTY smoke tests verify the primary session-management flow.

## Compatibility and Rollout

No journal schema changes are required. Old sessions remain resumable and acquire a derived title on read. Existing ACP clients continue using `session/list`; new Lato clients prefer `lato/session/list`. The public README limitation about ID-only sessions is removed after tests pass.

Implementation is complete only after targeted tests, the workspace test suite, formatting and lint checks pass, followed by `cargo install --path .` so the local `lato` command contains the feature.
