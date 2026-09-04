# Lato Phase 4C1 Grok-Style Manual Compaction Design

## 1. Status and decision

Phase 4C1 turns the durable history-replacement foundation from Phase 4B into
a user-facing manual context-compaction operation. The design follows the
semantic structure of Grok Build at commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`: compaction is a session operation,
not an ordinary conversational turn; the active model produces a continuation
summary; the resulting conversation is validated and replaced through a
checkpoint; cancellation and failures leave a deterministic recoverable state.

The implementation will align Grok Build's state machine, data flow, bounded
retry policy, summary validation, and checkpoint ordering with Lato's existing
typed runtime and append-only canonical journal. It will not copy product-only
memory, telemetry, hook, transcript-segment, or multi-agent facilities that do
not yet exist in Lato.

Phase 4C is delivered in three independently testable layers:

1. Phase 4C1: manual `/compact [context]`, one-pass summarization, validation,
   durable replacement, cancellation, and recovery;
2. Phase 4C2: canonical context usage, threshold compaction, and model-switch
   checks;
3. Phase 4C3: context-overflow recovery, bounded resubmission, failure
   suppression, and optional two-pass/prefire optimization.

This document specifies Phase 4C1 and freezes the interfaces needed by the two
later layers.

## 2. Upstream behavior being followed

The relevant Grok Build components are:

- `xai-grok-shell/src/session/compaction.rs`, where manual and automatic
  compaction are session operations with independent cancellation, bounded
  attempts, candidate validation, checkpoint persistence, and history
  replacement;
- `xai-grok-shell/src/session/helpers/session_compact.rs`, which constructs a
  continuation-oriented summary request and classifies cancellation,
  deterministic, transient, overflow, and degenerate outcomes;
- `xai-chat-state/src/compaction_utils.rs`, which prepares model-safe summary
  input, carries an earlier summary forward, extracts the latest real user
  query, formats summary output, and validates the replacement conversation;
- `xai-chat-state/src/actor/queries.rs` and `actor/mutations.rs`, where history
  snapshots, compaction state, replacement, and derived token state have clear
  actor boundaries;
- `xai-grok-shell/src/session/persistence.rs`, where checkpoint and replacement
  writes are serialized through the session persistence actor.

Phase 4B already adapted the last persistence property to Lato. Phase 4C1 must
call that boundary rather than create another writer.

## 3. Goals

Phase 4C1 must:

1. expose `/compact` and `/compact <user context>` in the TUI;
2. represent compaction as a typed session command with a distinct operation
   identity and lifecycle events;
3. use the session's currently selected model to summarize a frozen history
   snapshot without enabling tools;
4. produce a continuation-oriented summary that preserves user intent,
   technical state, files, decisions, errors, completed work, and pending work;
5. validate and durably install the replacement using Phase 4B's
   checkpoint-first transaction;
6. keep the old logical history unchanged when sampling, cancellation, or
   candidate validation fails before the replacement marker commits;
7. reconcile from the canonical journal if a marker commits but derived
   publication or in-memory installation is interrupted;
8. preserve Phase 4A's unresolved-side-effect and canonical-corruption
   guarantees;
9. freeze trigger, policy, context-usage, and error contracts that later
   automatic compaction will reuse.

## 4. Non-goals

Phase 4C1 does not add:

- automatic threshold compaction;
- model-switch compaction;
- automatic recovery from a provider context-length error;
- automatic continuation or resubmission of the interrupted user turn;
- two-pass summarization or background prefire;
- transcript segments or out-of-band memory artifacts;
- pre-compact or post-compact extension hooks;
- subagent, task, todo, or MCP state injection;
- a separate compaction model or provider configuration;
- destructive canonical journal truncation, rotation, or archival;
- a second persistence actor or any direct write from the TUI or compactor.

## 5. Considered approaches

### 5.1 Chosen: semantic parity delivered in layers

Build the complete Grok-style compaction lifecycle behind stable Lato
interfaces, but deliver manual compaction first. Later automatic triggers reuse
the same state machine, compactor, validation, and persistence transaction.
This preserves architectural parity without introducing dependencies on Lato
phases that have not been implemented.

### 5.2 Rejected: port the complete current Grok Build module at once

This would combine manual and automatic compaction with two-pass prefire,
memory recovery, transcript segments, hooks, telemetry, todo state, and
subagent state. Those facilities cross Phase 5 and Phase 6 boundaries and
would create placeholder abstractions with no current consumer.

### 5.3 Rejected: expose only the existing in-memory helper

`SessionActor::compact_explicit` can replace an in-memory vector, but it does
not generate a summary, coordinate with the runtime, persist a checkpoint, or
survive restart. Exposing it as `/compact` would violate the journal and
recovery contracts.

## 6. Architecture and ownership

### 6.1 Core contracts

`lato-core` adds a checked `CompactionId` and the following provider-neutral
contracts:

- `Command::CompactSession(CompactSession)` with optional `user_context`;
- `Command::CancelCompaction { compaction_id }`;
- `CompactionTrigger::{Manual, Threshold, PreflightOverflow, ModelSwitch}`;
- `CompactionPolicy` with threshold percentage, maximum attempts, and summary
  output reserve;
- `ContextUsage` with estimated input tokens, context window, and utilization;
- compaction lifecycle event payloads and stable compaction errors.

Only `Manual` is accepted from a user command in Phase 4C1. The other trigger
variants are serialized and tested now so Phase 4C2 and Phase 4C3 do not need
to change persisted or client-visible wire shapes.

### 6.2 Runtime coordinator

`lato-runtime` is the only coordinator allowed to start or cancel a
compaction. It owns:

- the compaction state transition and operation ID;
- the compaction cancellation token;
- serialization against foreground turns, shutdown, and other compactions;
- canonical lifecycle commits before related visible events;
- invocation of the compactor on a frozen history snapshot;
- invocation of the Phase 4B history-replacement store;
- reconciliation and in-memory installation after persistence.

The runtime does not construct prompts, call provider HTTP APIs, interpret
provider-specific errors, or write JSONL files.

### 6.3 Agent compactor

`lato-agent` adds an object-safe `SessionCompactor` boundary. Its request
contains the compaction ID, trigger, active model selection, model
capabilities, immutable model-message history, optional user context, policy,
and cancellation token. Its successful result contains a validated candidate
history plus before/after size estimates and summary metadata.

The implementation uses the same active model as the session. It reaches the
model through the existing model abstraction and supplies no tool descriptors
with `ToolChoice::None`. It does not invoke tools, mutate session history, or
write storage. Provider adapters remain responsible for request encoding and
typed stream errors.

The existing `compact_explicit` helper is removed or reduced to a private pure
assembly helper. It cannot remain an alternate mutable compaction path.

### 6.4 Storage boundary

The runtime receives a session store capable of both canonical `EventStore`
operations and `HistoryProjectionStore::replace_history`. File and memory
stores continue to implement the same behavior. No caller receives direct
access to checkpoint paths.

The file store retains Phase 4B ordering:

```text
checkpoint temp write and sync
→ checkpoint rename and directory sync
→ HistoryProjectionReplaced(context_compaction) with SyncData
→ history projection publication
→ metadata publication
→ session directory sync
```

### 6.5 TUI boundary

The TUI parses `/compact` and `/compact <text>` into a backend command. It does
not snapshot history, call the model, or replace history itself. It renders
runtime lifecycle events and uses the active-operation identity to route
Ctrl-C to `CancelCompaction`.

## 7. Session state machine

Compaction is independent from a turn and never receives a synthetic
`TurnId`:

```text
Idle
 └─ CompactSession
      → Compacting(compaction_id)
          ├─ completed → Idle
          ├─ failed    → Idle
          ├─ cancelled → Idle
          └─ shutdown  → Cancelling → Stopped
```

Rules:

1. compaction starts only from an idle, non-stopped session;
2. a turn, another compaction, rename-sensitive session replacement, and model
   switching are rejected while compaction is active;
3. `StartTurn` during compaction returns `compaction.already_active`;
4. a second compact request returns `compaction.already_active`;
5. `CancelCompaction` is idempotent for the active ID and rejects a different
   ID;
6. shutdown cancels compaction, waits for the model task to exit, commits the
   terminal state when possible, and then stops the session;
7. a completed, failed, or cancelled operation always releases the state back
   to idle unless shutdown owns the transition.

The submission gate in `RuntimeSession` covers both turns and compactions so a
command cannot be accepted in the interval before its started event is
observed.

## 8. Canonical lifecycle and visible events

The journal adds:

- `CompactionRequested { compaction_id, trigger, user_context }`;
- `CompactionFailed { compaction_id, error_code }`;
- `CompactionCancelled { compaction_id }`.

Successful logical replacement remains represented by
`HistoryProjectionReplaced` with reason `context_compaction`; it includes the
checkpoint identity and replacement anchors. No duplicate success record is
needed.

User context is stored because it affects the generated summary and is part of
the operation's authoritative input. Provider error bodies and model drafting
text are not stored in canonical records. Failures retain only a stable,
bounded error code and safe message where existing redaction policy permits.

Visible events are:

- `CompactionStarted { compaction_id, trigger }`;
- `CompactionCompleted { compaction_id, before, after, checkpoint_id }`;
- `CompactionFailed { compaction_id, error }`;
- `CompactionCancelled { compaction_id }`.

`CompactionStarted` is broadcast only after `CompactionRequested` is durable.
A terminal failure or cancellation is broadcast only after its canonical
record is durable. `CompactionCompleted` is broadcast only after the runtime
has reconciled the committed replacement and installed the corresponding
history in memory.

## 9. Compaction algorithm

### 9.1 Preconditions and snapshot

The runtime rejects compaction when:

- a turn, approval, or compaction is active;
- replay reports an unresolved prepared tool call;
- the history has no system message or no real user turn;
- the source, excluding the system prompt and separately re-injected project
  instructions, is shorter than 2,000 Unicode scalar values;
- the store cannot establish a valid canonical replay boundary.

After committing `CompactionRequested`, the runtime freezes the complete
provider-independent history and its journal cursor. The compactor never reads
live mutable history after that point.

### 9.2 Summary input preparation

Following Grok Build's default summary preparation:

- keep ordinary system, user, and assistant text;
- strip reasoning blocks;
- replace images with the literal placeholder `[image]`;
- convert tool calls to bounded textual annotations containing call identity
  and tool name;
- omit raw tool-result payloads, retaining bounded status and outcome context
  sufficient for the summary model to describe what occurred;
- strip incomplete trailing tool-call fragments;
- include an earlier compaction summary as authoritative early-history input;
- append the manual `/compact <text>` value as explicitly labeled
  user-provided compaction context.

The preparation function is pure and bounded. It never edits the original
snapshot. Phase 4C1 does not use a lossy multi-stage input ladder; if the
prepared request still exceeds the active model's safe request budget, it
returns `compaction.input_too_large` and preserves the old history. Phase 4C3
adds Grok Build's degraded input ladder and overflow recovery.

### 9.3 Summary request

The prompt asks the active model for one continuation summary with these nine
required sections:

1. primary request and intent;
2. key technical concepts;
3. files and important code sections;
4. errors and fixes;
5. solved and in-progress problem solving;
6. user-message evolution;
7. explicitly pending tasks;
8. exact current work position;
9. one optional next step strictly derived from the latest user request.

The prompt states that earlier compaction summaries are authoritative, asks
the model not to call tools, and requires one summary payload without a
separate reasoning block. User-provided compaction context must be incorporated
but cannot override system policy or manufacture new pending work.

### 9.4 Output normalization and validation

The compactor:

1. collects only text deltas;
2. rejects any tool-call event;
3. rejects a non-completed, length-truncated, content-filtered, or cancelled
   terminal reason;
4. removes leading drafting analysis and unwraps one recognized summary
   wrapper;
5. neutralizes echoed compaction-control tags;
6. collapses excessive blank lines and trims the result;
7. verifies all nine required sections;
8. rejects a cleaned summary shorter than 500 Unicode scalar values as
   degenerate;
9. rejects a cleaned summary larger than 32 KiB or the configured summary
   output-token reserve, whichever bound is reached first;
10. verifies that the serialized candidate is at least 20 percent smaller
    than the serialized source history.

A short source conversation is classified as `compaction.nothing_to_compact`
before sampling, rather than weakening the degenerate-summary floor.

### 9.5 Replacement history shape

Phase 4C1 uses this stable order:

```text
original system message
→ current project instructions, when separately represented
→ latest real user objective, wrapped as continuation context
→ one synthetic user-role compaction-summary message
```

Raw assistant/tool working tails are not retained by default. The latest real
user objective is retained separately so the continuation remains anchored to
human intent. Required file, command, error, decision, and pending-work detail
belongs in the summary. This matches Grok Build's current compaction view,
which drops its recent assistant/tool working tail to maximize reclaimed
context.

`HistoryItem::CompactionSummary` gains a stable conversion to and from a
synthetic user-role `ModelMessage`. The representation uses an explicit,
versioned wrapper so it cannot be confused with a real human message during
future compaction or replay.

Before persistence, the candidate must contain exactly one system head, no
orphan tool results, no dangling tool calls, no transient reasoning, and no
entry above configured bounds.

## 10. Retry and error classification

One manual operation performs at most `CompactionPolicy.max_attempts`, with a
Phase 4C1 default of three.

- cancellation: never retry;
- authentication, authorization, invalid model configuration, and malformed
  deterministic requests: never retry;
- transient transport errors, rate limits, and server failures: retry within
  the attempt and wall-clock budgets;
- degenerate but otherwise completed summaries: retry within the same bounds;
- input or provider context overflow: do not retry in Phase 4C1;
- checkpoint, journal, or projection errors: do not resample the summary.

Backoff is cancellation-aware and bounded. The operation publishes a single
terminal event regardless of how many internal attempts occurred.

Stable errors live under `compaction.*`, including:

- `compaction.nothing_to_compact`;
- `compaction.active_turn`;
- `compaction.already_active`;
- `compaction.input_too_large`;
- `compaction.degenerate_summary`;
- `compaction.invalid_summary`;
- `compaction.model_failed`;
- `compaction.cancelled`;
- `compaction.persistence_failed`;
- `compaction.reconciliation_failed`.

Existing model, journal, and projection errors remain typed causes and are not
classified by matching rendered error strings.

## 11. Failure and crash recovery

### 11.1 Before the replacement marker

Sampling failure, cancellation, invalid output, checkpoint-temp failure, or a
failure before the marker commit leaves the previous logical history active.
The runtime commits the corresponding failure or cancellation lifecycle record
when the canonical store remains writable, broadcasts the terminal event, and
returns to idle.

### 11.2 After the replacement marker

Once `HistoryProjectionReplaced` commits, the new checkpoint history is the
canonical logical history even if derived publication fails. The runtime must
not continue with the old in-memory history. It immediately replays the
canonical journal:

- if replay identifies the new checkpoint, install the reconstructed history
  and complete the operation with a recoverable projection warning;
- if replay cannot establish one authoritative history, transition the session
  to a failed/stopped state and return `compaction.reconciliation_failed`;
- never resample or append a second replacement marker automatically.

### 11.3 Process restart

On restart, Phase 4B replay handles any committed checkpoint marker. A durable
`CompactionRequested` without a replacement or terminal record is reported as
an interrupted compaction but is not automatically rerun. Model inference has
no external tool side effect, but repeating it could produce a different
summary and is not required for safe recovery.

## 12. TUI behavior

The command registry adds:

- `/compact` — compact using the default continuation prompt;
- `/compact <text>` — emphasize the supplied context in the summary.

While compacting, the TUI remains visible and renders a compacting status. The
composer may retain typed text but cannot submit another turn. Ctrl-C cancels
the active compaction. Model and session switching are disabled until the
operation reaches a terminal state.

Success reports before/after message counts and estimated size. Failure states
that the previous history was retained when no marker committed. When
reconciliation recovered a committed replacement, the UI reports success with
a storage-rebuild warning instead of claiming that the old history remains.
`/status` exposes the current compaction state and most recent terminal result.

The TUI never renders the model's summary drafting stream as a normal assistant
answer. Only the final installed summary becomes part of model history.

## 13. Interfaces reserved for Phase 4C2 and Phase 4C3

`CompactionTrigger`, `CompactionPolicy`, and `ContextUsage` are complete wire
contracts in 4C1. The runtime compaction entry point accepts a trigger even
though the TUI can submit only `Manual`.

Phase 4C2 will:

- source context windows from `ModelPort::capabilities`;
- combine exact provider usage with a conservative estimate for messages added
  since the last response;
- check thresholds before model sampling and after model switching;
- invoke the same runtime command internally with `Threshold` or
  `ModelSwitch`.

Phase 4C3 will:

- classify typed context-overflow responses;
- check tool-result growth before the next sampling call;
- run compaction and rebuild the interrupted request once;
- suppress repeated deterministic auto-compaction failures;
- add a bounded lossy input ladder, then optional two-pass/prefire sampling.

Neither phase may bypass Phase 4C1's mutual exclusion, validation, replacement,
or reconciliation rules.

## 14. Testing strategy

### 14.1 Core contract tests

- checked, stable, distinct `CompactionId` values;
- command, trigger, policy, usage, lifecycle event, and error serialization;
- backward-compatible deserialization of pre-4C journals and events;
- projection of compaction lifecycle records without changing conversation
  messages.

### 14.2 Runtime state-machine tests

- compaction starts only from idle;
- turn and compaction submissions are mutually exclusive under concurrency;
- duplicate compaction and mismatched cancellation IDs are rejected;
- cancellation and shutdown stop a blocked model call promptly;
- started and terminal visibility follows durable journal acknowledgement;
- a successful marker is reconciled before the completed event;
- no command observes a transient mix of old and new history.

### 14.3 Agent and model tests

- summary preparation strips reasoning, images, and unbounded tool payloads;
- earlier summaries and the latest real user objective survive;
- optional user context is labeled and cannot alter system instructions;
- valid output produces the exact replacement shape;
- missing sections, short output, tool calls, and truncated completion fail;
- transient and degenerate outcomes retry at most three times;
- deterministic failure and cancellation do not retry;
- a fake current model is used and no tool descriptor is advertised.

### 14.4 Store and recovery tests

- a manual replacement writes a `context_compaction` checkpoint and marker;
- pre-marker faults retain old logical history;
- post-marker derived faults replay the new checkpoint;
- checkpoint absence or mismatch fails closed;
- derived corruption after compaction rebuilds from the checkpoint;
- unresolved tool outcomes reject compaction without writing a checkpoint;
- resume installs the same compressed history in headless, TUI, and ACP-backed
  runtime paths.

### 14.5 TUI and end-to-end tests

- slash registry and completion include `/compact`;
- optional context is passed unchanged to the typed backend request;
- busy state, Ctrl-C, success, failure, and rebuild-warning rendering;
- a real runtime session can compact, stop, resume, and continue from the same
  summary;
- a failed manual compact can be followed by an ordinary turn using the
  original history;
- existing workspace, Clippy, formatting, installation, and offline smoke
  gates remain green.

## 15. Acceptance criteria

Phase 4C1 is complete when:

1. `/compact [context]` executes as a typed non-turn session operation;
2. the current session model produces a bounded, structurally valid
   continuation summary without tools;
3. successful compaction materially reduces model-visible history;
4. the replacement uses Phase 4B checkpoint-first persistence and survives
   restart;
5. every visible lifecycle transition follows its required durable boundary;
6. cancellation and every pre-marker failure preserve the previous history;
7. every post-marker failure either reconciles to the new canonical history or
   stops fail-closed;
8. unresolved side effects can never be hidden, completed, or retried by
   compaction;
9. automatic-trigger interfaces are fixed without enabling automatic behavior;
10. all relevant tests, Clippy with warnings denied, local installation, and
    installed-binary smoke tests pass.
