# Lato Phase 4C2 Grok-Style Automatic Compaction Design

**Date:** 2026-09-05  
**Status:** Approved for planning  
**Reference:** Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## 1. Purpose

Phase 4C1 added a durable, manually triggered compaction operation. Phase 4C2
extends that boundary with Grok Build-style context accounting and automatic
compaction:

- track the model-visible context footprint using provider usage plus local
  estimates;
- check the configured threshold before every model sample, including samples
  after tool calls in the same turn;
- compact automatically when the active context reaches the threshold;
- compact on model switches when a smaller context window or a different model
  family makes the existing history unsafe;
- route all automatic replacements through Phase 4C1's state machine,
  checkpoint-first persistence, validation, cancellation, and reconciliation.

The default automatic compaction threshold is 85 percent, matching Grok Build.

## 2. Source behavior

The design follows these behaviors in the pinned Grok Build source:

- `xai-chat-state/src/actor/state.rs` estimates message additions as UTF-8
  bytes divided by four and keeps an exact provider-confirmed baseline;
- `xai-chat-state/src/actor/mutations.rs` reseeds usage after a conversation
  replacement by scaling the new local estimate with the provider-to-estimate
  ratio, capped at the pre-replacement total;
- `xai-chat-state/src/actor/queries.rs` triggers at the configured percentage of
  the active model's context window;
- `xai-grok-shell/src/session/compaction.rs` checks immediately before sampling,
  evaluates smaller-window model switches, and continues after non-auth
  automatic compaction failures;
- `xai-grok-shell/src/session/acp_session_impl/model_switch.rs` applies the new
  sampling configuration before model-switch compaction and forces compaction
  for incompatible model families when model-authored history exists;
- `xai-grok-shell/src/util/config/resolve/compaction.rs` defaults the threshold
  to 85 percent.

Lato reproduces these semantics through its provider-neutral model contracts
and typed runtime rather than copying Grok Build's actor implementation.

## 3. Scope

### 3.1 Included

- provider-confirmed context usage baselines;
- deterministic bytes-per-token estimates for content added after the most
  recent provider response;
- context usage status updates;
- an 85 percent default threshold;
- a pre-sampling threshold check at every sampling step;
- automatic compaction without ending the enclosing user turn;
- smaller-window model-switch compaction;
- cross-family model-switch compaction;
- ACP and TUI model switching through one serialized session transaction;
- cancellation and durable compaction lifecycle visibility;
- backward-compatible model configuration fields;
- tests from token arithmetic through CLI/TUI integration.

### 3.2 Deferred to Phase 4C3

- classifying provider context-overflow errors and compacting after rejection;
- preflight hard-overflow recovery after large tool outputs;
- rebuilding and resubmitting a provider-rejected request;
- sticky suppression for repeated deterministic automatic failures;
- the bounded lossy compaction input ladder;
- two-pass summarization and asynchronous prefire.

Phase 4C2 may detect that context use exceeds 100 percent, but it does not claim
to recover from a provider overflow. That recovery belongs to Phase 4C3.

## 4. Chosen architecture

Context accounting belongs beside the model/history loop in `lato-agent`, while
serialization and durable replacement remain owned by `lato-runtime`. Model
construction remains in the protocol host, but the final swap becomes a
session operation.

This preserves four invariants:

1. usage is measured against the exact history and model call that produced it;
2. every automatic replacement uses the same validation and persistence path
   as `/compact`;
3. no provider request observes a transient mix of old and compacted history;
4. model switching cannot bypass turn/compaction mutual exclusion.

The alternatives were rejected as follows:

- checking only at the beginning of the next user turn misses growth caused by
  tool results within the current turn;
- triggering compaction in ACP or TUI duplicates policy across clients and
  bypasses the model sampling boundary;
- replacing history directly in `SessionActor` bypasses Phase 4C1's durable
  checkpoint and reconciliation protocol.

## 5. Context accounting

### 5.1 State

The driver maintains a context ledger with:

- `confirmed_total_tokens`: the most recent trustworthy model-visible total;
- `estimate_at_confirmation`: the local estimate of the history represented by
  that confirmation;
- `estimated_tokens_since_confirmation`: estimated content added afterward;
- active model selection and model-call generation;
- active context window;
- current `ContextUsage`.

The current estimated total is:

```text
confirmed_total_tokens + estimated_tokens_since_confirmation
```

Before the first provider usage event, the full local estimate is used as the
total.

### 5.2 Provider usage

For a completed model response, the provider-confirmed context total is:

```text
input_tokens + output_tokens
```

Missing values contribute only when present. A partial or absent usage event
must not replace a more trustworthy baseline with zero. `reasoning_tokens` is
not added separately because provider APIs normally report it as a subset of
output tokens. `cached_input_tokens` is reporting metadata and is not added to
input tokens a second time.

The usage event carries the model-call generation that opened the request. A
late event from a replaced model is ignored for the active ledger and may still
be retained as diagnostic call metadata.

### 5.3 Local estimation

Lato uses the same conservative seam as Grok Build:

- textual content and JSON-serialized tool arguments/results: UTF-8 bytes / 4;
- images: a fixed per-image estimate defined in one shared token-estimation
  module;
- structured message overhead: encoded content only in Phase 4C2; provider
  overhead is preserved by ratio calibration after the first confirmed usage.

All arithmetic is saturating. Context windows of zero or unknown size disable
automatic triggering without disabling usage reporting.

### 5.4 Replacement reseed

After compaction, let:

- `P` be the pre-replacement confirmed or estimated total;
- `E_old` be the local estimate at the last provider confirmation;
- `E_new` be the local estimate of compacted history.

When `P > 0` and `E_old > 0`, the new total is:

```text
min(P, round(E_new * P / E_old))
```

Otherwise it is `E_new`. This carries provider-side overhead without allowing
a replacement to increase the tracked total. The next provider response
self-corrects the estimate.

## 6. Model metadata

`ActiveModelPort` exposes an immutable snapshot containing:

- `ModelSelection`;
- `ModelCapabilities`, including `context_window`;
- a stable optional model-family identifier;
- the selected `ModelPort`;
- a monotonically increasing switch generation.

Built-in catalog entries declare context windows and family identifiers.
Custom model entries accept optional `context_window` and `model_family`
fields. Existing `models.json` files remain valid because both fields default
to unknown.

When a custom model omits its family, Lato derives a conservative fallback from
provider plus API dialect. If either side is still unknown, Lato performs only
the smaller-window rule and does not infer a cross-family switch from model
names.

## 7. Usage transport

`ModelPortStreamAdapter` currently discards `ModelStreamEvent::Usage`. Phase
4C2 adds an internal usage channel associated with the model-call generation.
The legacy `StreamPiece` public shape remains compatible; usage is delivered to
`SessionActor` through a separate observer rather than rendered as content.

For each sample:

1. snapshot the active model, capabilities, and generation;
2. build the provider request from the current history;
3. receive text/tool deltas normally;
4. retain the latest usage event for that call;
5. after successful completion, update the context ledger only if the call
   generation still matches;
6. emit an updated context-usage notification.

An interrupted, cancelled, or failed stream does not install a new confirmed
baseline.

## 8. Automatic threshold flow

The threshold check occurs inside the model/tool loop immediately before every
provider sample:

1. incorporate all messages and tool results added since the last response;
2. snapshot the active context window;
3. compute and publish `ContextUsage`;
4. if usage is below 85 percent, sample normally;
5. at or above 85 percent, request `Threshold` compaction through the runtime;
6. await the terminal compaction result without completing the enclosing turn;
7. on success, rebuild the request from compacted history and sample;
8. on a continuable failure, keep the old history and sample once;
9. on cancellation, authentication failure, or uncertain durable state, end the
   turn according to the failure rules below.

The original user turn keeps its `TurnId`. Automatic compaction receives a
separate `CompactionId` and normal compaction lifecycle events.

The runtime must support a nested maintenance phase owned by the active turn.
It is externally visible as compacting, but no second user command can enter
between history snapshot, replacement, and request rebuild. Manual compaction
continues to require an idle session.

## 9. Model-switch transaction

TUI and ACP model changes use one session-scoped transaction:

1. resolve credentials and construct the candidate model port;
2. validate selection, capabilities, and model configuration;
3. acquire the session submission gate and reject a concurrent turn,
   compaction, or second switch;
4. commit the candidate as the active sampling configuration;
5. update the threshold and context-window view;
6. for a cross-family switch, compact immediately with the new model when
   model-authored history exists;
7. for a same-family switch, retain the prior model/window marker for the next
   pre-sampling check;
8. publish the final model-changed and context-usage updates;
9. return the selected model to the caller.

The candidate is not exposed before step 4. Once step 4 succeeds, an ordinary
compaction failure does not roll the switch back, matching Grok Build's
"switching anyway" behavior.

### 9.1 Smaller-window switch

For a same-family switch:

- no compaction is required when the new context window is equal to or larger
  than the previous window;
- when the new window is smaller, recompute utilization using the new window;
- compact only when the current total reaches the new model's threshold;
- perform the actual compact at the next safe pre-sampling boundary so the
  switch transaction stays short and the new model is used for summarization;
- `session/set_model` therefore completes after installing the new same-family
  model; its pending smaller-window check is consumed before that session's
  next provider request.

### 9.2 Cross-family switch

When both family identifiers are known and differ, compact whenever history
contains at least one model-authored item. The compaction uses the new model and
the normal Phase 4C1 safe input preparation. Phase 4C2 does not add Grok Build's
lossy input ladder; if safe preparation cannot fit, the failure is reported and
the switch still completes. Phase 4C3 will add the bounded lossy fallback.

Empty or system/user-only history does not require family-switch compaction.

## 10. Runtime commands and events

The runtime adds a typed model-switch command and internal automatic
maintenance request. Their wire behavior is fixed:

- a model switch carries old and candidate model metadata, not credentials;
- an automatic compaction carries `Threshold` or `ModelSwitch` and the measured
  `ContextUsage` that caused it;
- `ContextUsageUpdated` is a live status event and is not written to the
  canonical journal;
- compaction requested/started/completed/failed/cancelled records remain
  durable exactly as in 4C1;
- the model-changed notification is emitted only after the switch transaction
  reaches its terminal state.

The journal remains the authority for history replacement. Context usage is
reconstructed from the restored history until a new provider confirmation
arrives, so old journals need no migration.

## 11. Cancellation and concurrency

- Ctrl-C during automatic compaction cancels both the compaction and its
  waiting user turn.
- A cancelled automatic compaction never starts the deferred provider request.
- Manual compaction, model switching, session switching, and new turns are
  rejected while automatic compaction is active.
- A model switch is rejected while an existing turn is running; Lato does not
  mutate the active provider beneath an in-flight request.
- Shutdown cancels and awaits the nested compaction task before closing the
  session.
- Late usage events are generation-checked and cannot modify the new model's
  context ledger.

## 12. Failure handling

### 12.1 Automatic compaction

- `compaction.nothing_to_compact`: treat as no maintenance needed and continue;
- ordinary model, summary-validation, or safe-input failures: emit the real
  compaction failure, preserve old history, and continue to the provider call;
- authentication failure: surface the authentication error and abort the turn;
- checkpoint or projection failure before a committed marker: preserve old
  history and apply the existing 4C1 error semantics;
- uncertain or irreconcilable durable state: fail closed and stop the session;
- user cancellation: emit cancellation and do not sample.

Repeated-failure suppression is intentionally absent in 4C2. Phase 4C3 adds
Grok Build's turn-scoped, sticky, credit, and authentication suppression modes
together with overflow recovery.

### 12.2 Model switching

- candidate construction or validation failure leaves the old model active;
- after candidate commit, an immediate cross-family non-auth compaction failure
  leaves the new model active and is returned as a warning/status update;
- after candidate commit, an immediate cross-family auth failure leaves the new
  selection active but returns the actionable auth error;
- persistence of selected-model metadata is ordered before the final
  model-changed notification;
- if metadata persistence has an uncertain outcome, reconcile before reporting
  success.

## 13. ACP and TUI behavior

`session/set_model` waits for the serialized switch transaction and returns the
active model plus an optional immediate cross-family compaction warning. A
same-family smaller-window check remains pending until the next turn's first
sampling boundary. All interactive model changes use this request; the TUI no
longer calls `SwitchableModelStream::set` directly.

The TUI status view displays:

```text
context: <estimated tokens> / <window> (<percent>%)
```

When the window is unknown, it displays the estimated token total without a
percentage. During automatic compaction it displays a distinct compacting
state and trigger. Summary drafting deltas never appear as assistant output.

Automatic compaction completion refreshes the displayed usage from the reseeded
ledger. Failure explains whether the old history was retained or a committed
replacement was recovered, using the 4C1 reconciliation result.

## 14. Compatibility

- existing journal records, history projections, and compaction checkpoints
  require no migration;
- existing `models.json` entries deserialize with unknown context window and
  family;
- legacy `ModelStream` implementations continue to compile and operate without
  usage reporting;
- unknown context windows disable automatic triggering instead of inventing a
  limit;
- manual `/compact` behavior and its public ACP extension remain unchanged;
- the hard byte limit remains a final guard until Phase 4C3 adds overflow
  recovery.

## 15. Testing strategy

### 15.1 Core and accounting tests

- threshold defaults to 85 percent;
- one token below the threshold does not trigger and the threshold does;
- percentage and totals use saturating arithmetic;
- provider input plus output becomes the confirmed baseline;
- cached input and reasoning tokens are not double-counted;
- partial or absent usage cannot erase a trustworthy baseline;
- post-response message growth is estimated and added;
- replacement ratio reseeding is rounded and capped;
- zero and unknown context windows never trigger.

### 15.2 Model adapter tests

- usage survives the canonical-to-legacy adapter boundary;
- the latest usage for one completed call is delivered exactly once;
- cancellation and failed streams do not confirm usage;
- a late old-generation usage event cannot update the active ledger;
- model metadata exposes context window and family without name guessing.

### 15.3 Agent and runtime tests

- threshold checks run before the first sample and again after tool results;
- threshold compaction completes before the original request is sampled;
- successful compaction resumes the same `TurnId` with compacted history;
- ordinary compaction failure retains history and samples once;
- auth failure aborts without sampling;
- cancellation cancels both maintenance and the enclosing turn;
- checkpoint reconciliation behavior is identical to manual compaction;
- nested automatic maintenance excludes concurrent external commands.

### 15.4 Model-switch tests

- a larger or equal same-family window does not compact;
- a smaller same-family window below threshold does not compact;
- a smaller same-family window at threshold compacts under the new model;
- a cross-family switch with model-authored history compacts regardless of
  percentage;
- empty or system/user-only history does not compact on a family switch;
- candidate construction failure preserves the old model;
- a non-auth compaction failure keeps the new model and returns a warning;
- a late usage event from the old model is ignored;
- concurrent turn, compaction, and model switch requests are rejected.

### 15.5 Integration and release checks

- ACP `session/set_model` uses the transaction and reports the final model;
- TUI model configuration no longer swaps the stream directly;
- context status and automatic compaction updates render correctly;
- restart after automatic compaction restores the checkpointed history;
- workspace format, check, tests, and Clippy pass;
- `cargo install --path .` installs the completed feature;
- an installed-binary smoke test exercises threshold and model-switch paths
  with deterministic fake providers where practical.

## 16. Acceptance criteria

Phase 4C2 is complete when:

1. context usage combines provider-confirmed totals with deterministic local
   growth estimates;
2. the default threshold is 85 percent and is checked before every model call;
3. reaching the threshold compacts durably and resumes the same user turn;
4. smaller-window and cross-family switches follow the rules above and use the
   new model for compaction;
5. all automatic replacements pass through 4C1 validation, checkpointing, and
   reconciliation;
6. cancellation and failure never expose mixed history or silently change the
   selected model;
7. old journals, model files, and legacy model streams remain usable;
8. the test suite, lint gates, local installation, and smoke tests pass.
