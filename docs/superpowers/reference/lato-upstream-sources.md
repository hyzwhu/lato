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

## Required source header

Copied or substantially derived Rust files begin with an exact project, commit, and path. For example:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values
```

Do not copy product-specific account, cloud-task, billing, UI, branding, or unrelated telemetry code.
