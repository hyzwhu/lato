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

## Required source header

Copied or substantially derived Rust files begin with an exact project, commit, and path. For example:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values
```

Do not copy product-specific account, cloud-task, billing, UI, branding, or unrelated telemetry code.
