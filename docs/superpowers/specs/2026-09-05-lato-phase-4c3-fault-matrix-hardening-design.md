# Lato Phase 4C3 Fault-Matrix Hardening Design

## 1. Goal

Harden the delivered Phase 4C3 context-recovery behavior with adversarial,
layered fault injection. The work must prove that recovery remains bounded,
does not duplicate visible or tool output, preserves durable history ordering,
and applies automatic-compaction suppression with the intended lifetime.

This phase is allowed to fix defects exposed by its tests. It does not create a
general-purpose fault-injection framework or redesign unrelated model, tool,
storage, or UI code.

## 2. Reference behavior

The behavioral reference remains Grok Build at commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`, especially
`crates/codegen/xai-grok-shell/src/session/compaction.rs` and
`crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs`.

The implementation under test is Lato's existing provider-neutral adaptation:

- typed model failures and provider classification;
- single-use provider-overflow recovery;
- tool-output preflight compaction;
- Prepared/Fitted/Lossy compaction inputs;
- speculative pass-one prefire and validated pass-two installation;
- session-scoped automatic-compaction suppression;
- checkpoint-before-marker durable replacement.

## 3. Test layers

### 3.1 Provider and adapter layer

Deterministic model fixtures exercise errors before output, after text, after a
tool-call delta, and during transport fallback. Tests assert that
`ModelErrorKind`, HTTP status, provider context window, and `output_started`
survive every adapter boundary. A generic invalid request must never be
reclassified as context overflow.

### 3.2 Actor and runtime layer

Scripted streams and compaction ports cover these cases:

1. first pre-output overflow compacts and resubmits exactly once;
2. a second overflow terminates without a third ordinary request;
3. any overflow after text or tool output terminates without replay;
4. failed recovery preserves the original provider failure as the primary
   failure and attaches compaction context;
5. cancellation between durable checkpoint completion and resubmission stops
   the turn before another provider request;
6. tool output that makes the next request provably oversized triggers
   `PreflightOverflow` before sampling;
7. failed preflight returns `context.preflight_recovery_failed` and does not
   submit a known-oversized request;
8. suppression modes block every automatic entry point while manual
   compaction remains available.

Runtime assertions verify event and journal ordering, operation ownership, and
the absence of speculative prefire records.

### 3.3 Two-pass and cache layer

Tests place utilization immediately below and at the 75% prefire boundary and
at the 85% compaction boundary. They verify one pass-one request, a 95%-weighted
prefix that does not split tool pairs, cache reuse after tail append, and cache
invalidation after prefix mutation or model-generation change.

Pass-one and pass-two failures consume the existing global three-submission
budget and fall back to the single-pass ladder. NOTE1 is never installed,
journaled, or rendered as transcript output.

### 3.4 ACP, CLI, and restart layer

End-to-end fixtures assert stable `provider_overflow` and
`preflight_overflow` lifecycle notifications, one final assistant answer or
one terminal error, and no duplicated text/tool calls. Restart after recovered
compaction must restore only the committed logical history. Existing Phase
4C1/4C2 journal fixtures remain readable without migration.

## 4. Failure semantics

Recovery is permitted only when no output has started, automatic compaction is
not suppressed, and the sampling recovery budget is unused. A provider-supplied
positive smaller context window narrows the recovery decision for that request.
The original request JSON is discarded after a successful replacement.

The recovery credit is consumed before compaction begins and is not restored by
compaction success. It is reset only after a successful ordinary provider
response advances the turn to a new sampling boundary.

Suppression classification uses typed errors first. Size and summary-schema
failures are sticky; credit failures last until a successful ordinary provider
response; authentication failures last until credential refresh; other
deterministic automatic failures last for the user turn. Repeated blocked checks
are silent.

## 5. Durable and visible-state invariants

- No model or tool delta may be replayed after it was visible.
- Prefire never changes `SessionPhase`, installs history, writes a compaction
  journal record, or creates a TUI transcript item.
- Real compaction keeps the enclosing turn ID and receives its own compaction
  ID.
- History changes only after checkpoint publication and the authoritative
  replacement marker.
- A failed or cancelled recovery leaves the previously authoritative history
  intact.
- Restart observes either the old history or the committed replacement, never
  NOTE1 or a partially fitted input.

## 6. Implementation constraints

Tests should extend existing local fixtures where practical. Small dedicated
script types are acceptable when they make request type, emitted pieces,
terminal error, or synchronization barriers explicit. Production sleeps and
network-dependent tests are prohibited; synchronization uses channels,
`Notify`, or bounded timeouts.

Production work is restricted to Phase 4C3 code paths. Existing user-owned
dirty files remain untouched and unstaged. Each defect fix receives focused
regression coverage before implementation changes.

## 7. Acceptance

The hardening is complete when:

- all cases in Sections 3 and 4 have deterministic regression evidence;
- no recovery path can produce more than one ordinary resubmission;
- output-started errors never trigger compaction-and-replay;
- preflight prevents a known-oversized request;
- two-pass work remains speculative until NOTE2 is validated and committed;
- suppression clear conditions and status updates are proven;
- restart and legacy journal tests pass;
- `cargo fmt --all -- --check`, Clippy with warnings denied, the workspace test
  suite, local installation, and installed-command smoke tests all pass.
