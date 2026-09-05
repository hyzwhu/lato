# Lato Phase 4C3 Grok-Style Context Overflow Recovery Design

**Date:** 2026-09-05

**Status:** Approved for planning

**Reference:** Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## 1. Purpose

Phase 4C1 made model-generated compaction a durable, manually callable
operation. Phase 4C2 added provider-aware context accounting, threshold
compaction, and model-switch compaction. Phase 4C3 closes the remaining
failure modes around a full context window:

- recover from a provider-rejected oversized request without ending the turn;
- detect hard overflow after tool output before sending a doomed request;
- suppress deterministic automatic-compaction loops until their cause can
  plausibly change;
- degrade the summarizer input through a fixed, observable ladder rather than
  leaving a conversation permanently incompactable;
- hide most compaction latency by generating a first-pass summary before the
  85-percent line and merging it with the recent tail when compaction fires.

The behavior follows the pinned Grok Build source while preserving Lato's
typed model boundary, single session runtime, append-only canonical journal,
and checkpoint-first history replacement.

## 2. Source behavior

The design derives from these pinned Grok Build components:

- `xai-grok-shell/src/session/compaction.rs` owns the 10-percentage-point
  prefire lead, 95/5 token-weighted split, cached pass-one validation,
  provider-overflow recovery, tool-output preflight, input degradation, and
  automatic-compaction suppression decisions;
- `xai-grok-shell/src/session/compaction_config.rs` defines the single in-flight
  prefire slot, shared cancellation generation, cache fields, and four
  suppression lifetimes;
- `xai-grok-shell/src/session/two_pass.rs` keeps tool calls with their results,
  caps the first note, and builds the hierarchical pass-two request;
- `xai-chat-state/src/compaction_utils.rs` fits a conversation to a fixed token
  budget by dropping oldest complete units and truncating the last unit only
  when no complete unit fits;
- `xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs` converts a
  probable provider overflow into compact-and-resubmit and lets the outer
  sampling loop rebuild the request from current history;
- `xai-grok-shell/src/session/acp_session_impl/turn.rs` starts prefire before
  the ordinary threshold check and checks hard overflow after tool output;
- `xai-grok-shell/src/session/acp_session_impl/model_switch.rs` starts a
  family-switch compaction from the lossy-safe input path;
- `xai-grok-shell/src/session/helpers/session_compact.rs` classifies context
  length, authentication, credit, schema, cancellation, deterministic, and
  transient failures.

Lato reproduces the observable state transitions and safety bounds. It does
not import Grok Build's unrelated memory, segment archive, fork-prefix,
workflow-child, or telemetry subsystems.

## 3. Scope

### 3.1 Included

- typed sampling failures with context-overflow evidence;
- provider-specific context-overflow parsing and a conservative compatibility
  classifier;
- output-start tracking for safe resubmission decisions;
- one compact-and-resubmit recovery per sampling step;
- a hard-overflow preflight after tool results and before the next sample;
- `Prepared -> Fitted -> Lossy` compaction input degradation;
- turn, sticky, credit-until-success, and authentication suppression modes;
- 75-percent two-pass prefire for the default 85-percent threshold;
- a 95/5 token-weighted split that never separates a tool call from its
  results;
- bounded and validated pass-one cache state;
- fallback from unusable two-pass work to the existing single-pass path;
- cancellation, model-switch invalidation, durable replacement, ACP, TUI,
  restart, and regression tests;
- source attribution and user documentation updates.

### 3.2 Excluded

- compaction transcript or segment archives;
- memory flushes before compaction;
- forked-session inherited-prefix pinning and release;
- max-output-token salvage and continuation joining;
- workflow-child or subagent-specific token budgets;
- provider-side compaction headers;
- a new telemetry backend;
- background installation of a speculative summary before the real threshold.

Prefire is model work only. It never mutates visible history and never creates
a compaction checkpoint on its own.

## 4. Chosen architecture

Use a behavior-equivalent port rather than copying Grok Build's module graph.
Lato retains the following ownership boundaries:

1. `lato-core` defines stable errors, triggers, policy values, and internal
   recovery state values.
2. `lato-ai` turns transport/provider responses into typed sampling failures
   and records whether a response event was observed.
3. `lato-agent` decides when to prefire, compact, suppress, and resubmit. It
   owns ephemeral two-pass cache state because validity depends on the exact
   live history and active model generation.
4. `lato-runtime` serializes actual replacements and persists them through the
   Phase 4C1 checkpoint contract. It also services a non-installing prefire
   request while the enclosing turn remains active.
5. ACP and TUI render only actual compaction lifecycle events. Speculative
   pass one remains quiet.

Two rejected alternatives are:

- copying Grok Build's session actor and sampler hierarchy, which would pull
  unrelated product state into Lato and bypass its runtime protocol;
- shipping overflow recovery first and prefire later, which leaves the named
  Phase 4C3 behavior incomplete and duplicates intermediate protocol work.

## 5. Typed sampling failures

`ModelError` gains backward-compatible optional metadata:

- a stable failure kind including `context_overflow`, `authentication`,
  `credit`, `rate_limited`, `invalid_request`, `transport`, `cancelled`, and
  `other`;
- an HTTP status when one exists;
- the provider-reported context window when one exists;
- whether any response event was observed before failure.

Provider adapters must parse structured JSON fields before considering text.
The text compatibility classifier is a bounded list of known context-length
signals and never treats a generic HTTP 400 as overflow. Transport adapters
set `output_started` after the first model event, including a text delta,
reasoning delta, tool-call delta, usage event, or terminal event.

The actor considers a failed request recoverable through compaction only when:

1. no response event was observed;
2. the error is explicitly `context_overflow`, or the error supplies a
   positive context window smaller than the tracked request estimate;
3. automatic compaction is not suppressed;
4. the sampling step has not already consumed its overflow resubmission.

This prevents duplicate user-visible text and duplicate tool side effects.

## 6. Sampling-boundary order

Every non-salvage sampling step uses this order:

1. measure current context against the active model generation;
2. if two-pass is enabled, usage is at least `threshold - prefire_lead`, no
   valid cache exists, and no prefire is running, snapshot the history and
   start pass one;
3. apply the ordinary Phase 4C2 threshold or deferred model-switch check;
4. if compaction installed a replacement, restart the loop from step 1;
5. enforce the local serialized-byte guard;
6. build and submit the provider request;
7. on a recoverable pre-output provider overflow, run compaction, rebuild from
   installed history, and resubmit once;
8. process output and tools normally;
9. after tool results are committed and added to history, perform hard
   preflight before the next sample.

The preflight compares the provider-aware token estimate with the active
context window. If it is over 100 percent, it invokes compaction with
`PreflightOverflow`, then restarts the loop. Provider rejection uses
`ProviderOverflow`. Both remain part of the enclosing user turn.

If threshold compaction fails but the request remains under all known hard
limits, Lato warns and continues unchanged as in Phase 4C2. A failed preflight
means the request is already known not to fit, so Lato returns an explicit
context recovery error rather than submitting or looping.

## 7. Bounded provider resubmission

Each sampling step starts with an unused overflow-recovery credit. Recovery
compaction does not replenish that credit. A successful provider response that
advances the conversation, or completion of the tool calls produced by that
response, starts the next logical sampling step with a fresh credit. The
rejected request may therefore be rebuilt only once. The recovery sequence is:

1. retain the original typed provider error;
2. run automatic compaction against the authoritative current history;
3. wait for checkpoint persistence and driver history installation;
4. discard the rejected request body;
5. remeasure and rebuild a new request with a new model-call ID;
6. submit once using the same turn ID;
7. return the second failure without another overflow recovery.

If compaction fails, the provider overflow remains the primary error and the
compaction failure is attached as recovery context. Authentication failures
during recovery remain actionable authentication errors. Uncertain durable
state or failed reconciliation retains the existing fail-closed session stop.

## 8. Compaction input degradation

The summarizer uses a monotonic three-stage input ladder:

### 8.1 Prepared

This is Lato's existing safe preparation: keep ordinary conversation text,
replace images with `[image]`, keep only complete tool-call/result pairs,
flatten tool calls to annotations, and cap each included tool result at 1,024
characters. It remains the normal starting point for manual, threshold,
preflight, and provider-overflow compaction.

### 8.2 Fitted

If the provider rejects Prepared input for context length, rebuild preparation
from the authoritative history and fit it to:

`context_window - summary_reserve_tokens`

The fitter preserves the system head, newest complete conversation units, and
the latest real user objective. It drops oldest whole units first and never
begins with an orphaned tool result. If no complete tail unit fits, it keeps
the owning tool call when present and truncates its results on UTF-8
boundaries. Truncation adds a deterministic byte-count marker.

### 8.3 Lossy

If Fitted input also overflows, rebuild a more aggressive representation that
removes old tool-result bodies and nonessential assistant payload while
preserving tool names, outcomes, prior compaction summaries, concrete user
requirements, system policy, and the latest real user objective. Fit this
representation to:

`floor(context_window * 0.70) - compaction_protocol_overhead`

The protocol overhead includes the compaction instruction and reserved output
budget. The final budget is saturating and cannot underflow.

The ladder never moves backward. An overflow at Lossy is terminal and receives
sticky size suppression. It never truncates the canonical conversation in
place. Only a validated summary may replace model-visible history.

The existing three-attempt summary budget remains the global sampling limit
for one compaction. Input-stage transitions consume attempts; they do not
create fresh retry budgets.

## 9. Automatic-compaction suppression

All automatic entry points consult one session-scoped suppression gate:

| Mode | Causes | Cleared by |
|---|---|---|
| `None` | no active suppression | not applicable |
| `Turn` | other deterministic or ordinary automatic failure | next real user turn |
| `Sticky` | terminal input size, summary schema, or insufficient-reduction failure | successful compaction, rewind/context shrink, or a model context-budget change |
| `UntilSuccess` | credit exhaustion or spending limit | next successful ordinary provider response |
| `Auth` | 401, expired, or rejected credentials | successful login or token refresh |

The gate covers threshold, model-switch, preflight, provider-overflow recovery,
and prefire. Manual `/compact` ignores it so a user can retry deliberately.
Model switching does not clear account-state suppression. A context-budget
change clears only `Turn` or `Sticky`; a successful provider response does not
clear `Auth`.

Only the first transition from `None` emits the automatic-compaction failure
notification. Repeated checks stay quiet. Suppression is ephemeral runtime
state and is reconstructed as `None` after process restart; the bounded ladder
and one-resubmission limit still prevent a tight loop after restart.

## 10. Two-pass prefire

Two-pass prefire is enabled for ordinary sessions by policy. Defaults match
the pinned Grok behavior:

- automatic compaction threshold: 85 percent;
- prefire lead: 10 percentage points;
- prefire line: 75 percent;
- pass-one coverage: 95 percent of estimated token weight;
- pass-one NOTE limit: 12,000 characters;
- a closed `<summary>` body must exceed 1,000 characters before it is preferred
  over the whole pass-one response.

The split is by estimated token weight rather than message count. The boundary
snaps so an assistant tool call and its following tool results remain on the
same side. Pass one receives the prepared prefix plus the normal compaction
instruction. Its result becomes `NOTE1`.

Pass two receives:

1. system messages from the prefix;
2. a bounded NOTE1 carrier;
3. the prepared live tail;
4. a final instruction requiring one self-contained summary that incorporates
   both NOTE1 and the recent tail.

Only pass two's validated output can become the installed summary.

## 11. Prefire cache and concurrency

One session may have at most one pass-one job. Its result contains:

- NOTE1;
- live-history prefix length;
- fingerprint of that exact prefix;
- active model generation;
- estimated prefix tokens;
- pass-one latency for diagnostics.

When actual compaction fires, it awaits an in-flight pass one rather than
starting an immediately redundant single-pass request. The result is used only
if the current history still has the recorded prefix, the fingerprint matches,
and the model generation is unchanged. Otherwise it is stale and discarded.

Cache invalidation occurs on model switch, rewind, explicit history edit,
successful application, or any mismatching prefix. A pass-one error, empty
NOTE1, stale cache, failed pass two, or degenerate pass-two output falls back
to the ordinary single-pass ladder.

The foreground turn, prefire, and actual compaction share one cancellation
generation with a holder count. Ctrl-C cancels every holder in that generation.
The token is replaced only after all holders drain, so overlapping prefire and
foreground compaction cannot escape the same stop request.

Prefire uses a snapshot and never holds the mutable actor/history lock across
model I/O. It reports its result through an internal runtime request or join
handle. It does not emit visible compaction-started state, write journal
records, or change the usage ledger.

## 12. Runtime and persistence

The existing runtime automatic-compaction request remains the only route to a
history replacement. Add a distinct internal request for pass-one generation;
it returns a candidate note and metadata but has no installation capability.

Actual threshold, model-switch, preflight, and provider-overflow operations
retain the Phase 4C1 order:

1. request and start lifecycle records;
2. model generation and validation;
3. checkpoint file publication;
4. authoritative replacement marker;
5. driver history installation;
6. completion record and live event;
7. usage-ledger reseed.

Prefire failure cannot enter reconciliation because it writes no replacement.
Pass-two or single-pass failure before a checkpoint leaves old history active.
A failure after an uncertain persistence boundary invokes the existing replay
and reconciliation logic.

Provider-overflow recovery keeps the enclosing turn ID and allocates a new
model-call ID. No rejected provider request body is journaled as conversation.

## 13. ACP and TUI behavior

`CompactionTrigger` adds `provider_overflow`; the existing
`preflight_overflow` value becomes active. Existing readers remain compatible
because journal records are tagged values and old records do not contain the
new trigger.

Actual operations render through the current compaction UI:

- `threshold`: context threshold reached;
- `model_switch`: model compatibility or smaller-window maintenance;
- `preflight_overflow`: tool output made the next request too large;
- `provider_overflow`: provider rejected the request and Lato is recovering.

Prefire remains silent. A successful compact-and-resubmit produces no extra
system transcript message and no duplicated assistant delta. A final recovery
failure is rendered once with the primary provider error and concise recovery
detail. `/status` exposes the active suppression mode for diagnostics but never
provider response bodies or credentials.

## 14. Error semantics

- cancellation: cancel the turn and all compaction work; never submit after
  cancellation;
- authentication: abort the oversized turn with actionable login guidance and
  set `Auth` suppression;
- credit: preserve history and set `UntilSuccess` suppression;
- schema/deterministic invalid request: preserve history and set `Sticky` when
  retrying with unchanged context cannot help;
- terminal compaction input overflow: preserve history and set `Sticky`;
- transient compaction failure: preserve history and use `Turn` suppression;
- ordinary threshold failure below a hard limit: warn and continue unchanged;
- preflight failure above the known window: fail without a doomed provider
  call;
- provider overflow plus failed recovery: return the provider error with
  recovery detail;
- provider overflow after output started: do not resubmit;
- checkpoint reconciliation failure: stop the session fail-closed.

## 15. Testing strategy

### 15.1 Core and provider contracts

- backward-compatible `ModelError` serde;
- stable failure-kind and trigger serialization;
- provider JSON fixtures for OpenAI-compatible and Codex responses;
- provider-reported context window extraction;
- compatibility text patterns and generic-400 negative cases;
- output-start propagation for text, reasoning, tool, usage, and terminal
  events.

### 15.2 Pure compaction helpers

- 95/5 token-weighted split;
- small and empty histories;
- tool call/result boundary snapping;
- NOTE1 extraction and 12,000-character cap;
- prefix fingerprint stability and mutation detection;
- Prepared, Fitted, and Lossy ordering;
- UTF-8-safe tail truncation with byte-count marker;
- preservation of system policy, prior summary, and latest user objective;
- saturating budgets and terminal Lossy overflow.

### 15.3 Agent and runtime integration

- prefire starts at 75 percent but not below it;
- a second prefire cannot start while one is active or cached;
- actual compaction awaits an in-flight pass one;
- cache hit performs pass two; stale/model-switched cache falls back;
- pass-one and pass-two failures fall back to single-pass;
- threshold, model-switch, preflight, and provider-overflow triggers use the
  same durable replacement path;
- a large tool output is compacted before another provider request;
- a pre-output provider overflow compacts, rebuilds, and resubmits once;
- an error after a model event never resubmits;
- a second overflow is terminal;
- cancellation stops prefire, compaction, and the turn;
- each suppression cause maps to its exact scope and clear condition;
- manual compaction remains available while automatic work is suppressed;
- checkpoint and install failures retain Phase 4C1 reconciliation behavior.

### 15.4 End-to-end and UI

- ACP emits the new trigger values in actual lifecycle notifications;
- TUI localizes preflight and provider-overflow reasons;
- speculative prefire never appears as assistant output or a running
  compaction banner;
- overflow recovery produces one assistant answer and no duplicate tool call;
- restart after recovered compaction restores the installed history;
- old journals and old model configuration remain readable.

The final verification runs focused crate tests, full workspace tests,
Clippy, and `cargo install --path .` so the installed `lato` command contains
Phase 4C3.

## 16. Acceptance criteria

Phase 4C3 is complete when all of the following hold:

1. Lato recovers once from a pre-output provider context rejection by durably
   compacting, rebuilding, and resubmitting the same turn.
2. A tool result that pushes estimated input beyond the model window is handled
   before another provider request.
3. Automatic compaction cannot form an unbounded retry loop across sampling
   boundaries or deterministic failures.
4. A compaction request can step down only through Prepared, Fitted, and Lossy,
   with at most three total model attempts.
5. Prefire begins ten percentage points before the threshold, caches a valid
   95-percent prefix summary, and reduces foreground work to pass two when the
   cache remains valid.
6. Stale or failed speculative work never changes history and always has a
   safe single-pass fallback.
7. No recovery resubmits after output begins or duplicates a tool side effect.
8. Every installed replacement retains checkpoint-first durability,
   reconciliation, restart recovery, and usage-ledger reseeding.
9. The repository test suite and local `lato` installation succeed.
