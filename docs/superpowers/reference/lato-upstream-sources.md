# Lato Upstream Source Ledger

This file records code copied or structurally derived from upstream agent implementations.
Every copied production file and its tests must add a row before merge.

## Pinned baselines

| Project | Repository path | Commit | License |
|---|---|---|---|
| Codex | `/Users/huangyongzhao/Documents/work/rustproject/codex` | `633ab199cfd724aa78013c006b27a2b3d049fc3b` | Apache-2.0 |
| Grok Build | `/Users/huangyongzhao/Documents/work/grok-build` | `bb7f39d5858cbf5e00de639367f59debbdcb0138` | Apache-2.0 |

## Reuse records

| Lato target | Upstream source | Reuse mode | Tests carried over | Lato changes |
|---|---|---|---|---|
| `crates/lato-core/src/state.rs` | Codex `codex-rs/core/src/session/session.rs` and `codex-rs/core/src/session/handlers.rs` | Structural derivation | Single-active-turn, replace, cancel, shutdown state tests | Reduced to a provider- and transport-independent state machine |
| `crates/lato-runtime/src/session.rs` | Codex `codex-rs/core/src/session/handlers.rs::submission_loop` | Structural derivation | Submission ordering, cancellation, replacement, shutdown tests | Tokio channels expose typed Lato Command/Event values |
| `crates/lato-runtime/src/driver.rs` | Grok Build `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs` | Structural derivation | Child/event channel completion and cancellation tests | Generalized from child tasks to a foreground turn driver |
| `crates/lato-agent/src/legacy_driver.rs` | Lato `crates/lato-agent/src/actor.rs` plus Codex `codex-rs/core/src/session/handlers.rs` | Compatibility adapter | Delta forwarding, cancellation, steering, history hydration | Runs the existing Lato turn loop behind the typed `TurnDriver` membrane |
| `crates/lato-core/src/tool.rs` | Grok Build `crates/common/xai-tool-runtime/src/tool.rs` and `dispatch.rs` | Structural derivation | Object safety, JSON membrane, typed errors | Reduced to Lato core descriptor/invoke contracts; streaming tool output deferred |
| `crates/lato-tools/src/catalog.rs` | Codex `codex-rs/core/src/tools/registry.rs` | Structural derivation | Duplicate rejection, deterministic registration, runtime/spec separation | Added explicit layer and semver replacement rules |
| `crates/lato-core/src/model.rs` | Codex `codex-rs/model-provider/src/provider.rs` | Structural derivation | Object-safe provider boundary and capabilities | Added normalized Lato request and stream event types |
| `crates/lato-store/src/file.rs` | Grok Build `crates/codegen/xai-workflow/src/journal.rs` | Structural derivation | Bounded replay, torn-tail recovery, malformed-record rejection | Generalized to canonical per-session journals with strict projection and atomic legacy import |
| `crates/lato-store/src/writer.rs` | Codex `codex-rs/rollout/src/recorder.rs` | Structural derivation | Ordered writes, acknowledgement barriers, shutdown draining | Reduced to one bounded writer queue per session with durability levels and reopen/replay retry |
| `crates/lato-store/src/projection.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs`, `session/persistence.rs`, `session/compaction.rs`, and `session/helpers/replay.rs` | Structural derivation | Replaceable chat history, damaged-history quarantine, checkpoint-before-marker ordering, rebuild from authoritative updates | Uses Lato's canonical journal, strict hashes and bounded recovery; excludes Grok product/cloud state |
| `crates/lato-store/src/writer.rs` (Phase 4B additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/persistence.rs` and `crates/codegen/xai-chat-state/src/persistence.rs` | Structural derivation | Single persistence actor, source/derived write ordering, full-history replacement acknowledgement | Keeps canonical append acknowledgements authoritative and serializes derived history/checkpoint work on the existing bounded writer |
| `crates/lato-agent/src/compaction.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` and `crates/codegen/xai-chat-state/src/compaction_utils.rs` | Structural derivation | Bounded tool-result preparation, prior-summary carry-forward, required-section validation, replacement ordering | Reduced to one tool-free manual operation with provider-neutral messages and fixed safety bounds |
| `crates/lato-agent/src/legacy_driver.rs` (Phase 4C1 additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Current-model snapshot, cancellation, typed retry classification, three-attempt ceiling | Uses Lato's canonical `ModelPort`; tools and automatic triggers remain disabled |
| `crates/lato-runtime/src/session.rs` (Phase 4C1 additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Independent compaction lifecycle, mutual exclusion, cancellation, marker reconciliation | Coordinates typed operations without treating compaction as a turn or model message |
| `crates/lato-store/src/writer.rs` and `projection.rs` (Phase 4C1 recovery) | Grok Build `crates/codegen/xai-grok-shell/src/session/persistence.rs` and `session/helpers/replay.rs` | Structural derivation | Checkpoint-before-marker faults, post-marker history/metadata rebuild, dense continuation sequence | Keeps `events.jsonl` append-only and treats a committed replacement marker as authoritative |
| `crates/lato-core/src/compaction.rs` (Phase 4C2 additions) | Grok Build `crates/codegen/xai-chat-state/src/actor/state.rs`, `mutations.rs`, and `queries.rs` | Structural derivation | 85-percent threshold, provider baseline, saturating reseed | Generalized the accounting into provider-neutral values and retained Lato's durable compaction contracts |
| `crates/lato-agent/src/context_usage.rs` and `actor.rs` (Phase 4C2) | Grok Build `crates/codegen/xai-chat-state/src/actor/state.rs`, `mutations.rs`, `queries.rs`, and `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Pre-sample threshold checks, usage confirmation, model-switch trigger, post-replacement reseed | Measures Lato history without provider-specific messages and delegates every replacement to the checkpoint-first runtime |
| `crates/lato-runtime/src/session.rs` (Phase 4C2 additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Nested same-turn maintenance, cancellation, ordinary-failure continuation, checkpoint reconciliation | Keeps the enclosing Lato turn ID while assigning a distinct compaction ID and preserving the append-only journal |
| `crates/lato-agent/src/runtime_session.rs` and `host.rs` (Phase 4C2) | Grok Build `crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs` and `session/acp_session_impl/model_switch.rs` | Structural derivation | Cross-family immediate compaction, same-family shrink deferral, per-session isolation, failure semantics | Makes selection a serialized Lato runtime transaction and publishes ACP changes only after durable selection succeeds |
| `crates/lato-ai/src/provider_error.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs` | Behavioral derivation | Context-overflow recognition and bounded compatibility parsing | Prefers provider-neutral structured error metadata and never treats a generic HTTP 400 as overflow |
| `crates/lato-agent/src/context_recovery.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Automatic-compaction suppression scopes and bounded resubmission | Encodes turn, sticky, credit-until-success, and authentication lifetimes as ephemeral Lato session state |
| `crates/lato-agent/src/compaction_input.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs` | Structural derivation | Prepared/Fitted/Lossy ladder, whole tool-unit retention, safe truncation | Rebuilds every stage from canonical provider-neutral messages and shares one three-attempt ceiling |
| `crates/lato-agent/src/two_pass.rs` | Grok Build `crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs` | Structural derivation | 95-percent prefix split, bounded NOTE1, deterministic invalidation | Uses a fixed repository-owned fingerprint and keeps speculative output out of durable history |
| `crates/lato-agent/src/actor.rs` (Phase 4C3 additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` and `session/helpers/session_compact.rs` | Structural derivation | 75-percent prefire, tool-output preflight, provider-overflow recovery, suppression gate | Preserves the enclosing turn, refuses replay after output, and delegates every replacement to Lato runtime |
| `crates/lato-runtime/src/session.rs` (Phase 4C3 additions) | Grok Build `crates/codegen/xai-grok-shell/src/session/compaction.rs` | Structural derivation | Non-installing prefire and cancellation ownership | Runs pass one under the active turn token without changing session phase, journaling, or installing history |

## Required source header

Copied or substantially derived Rust files begin with an exact project, commit, and path. For example:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values
```

Do not copy product-specific account, cloud-task, billing, UI, branding, or unrelated telemetry code.
