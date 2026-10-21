# General Agent Foundation Implementation Plan

**Goal:** Enable a configured general agent to invoke registered business modules through the existing Lato host.

**Architecture:** Keep the default host behavior; add immutable application configuration, physically restricted tool catalogs, and host-side business-tool registration. Reuse the runtime, policy membrane, model loop, and journal.

**Tech Stack:** Rust, Tokio, serde, existing ACP and tool interfaces.

## Global Constraints

- No new provider or network dependency; no journal schema change.
- No silent fallback after invalid application configuration.
- Preserve default coding behavior, permissions, and sandbox checks.
- Deploy locally using `cargo install --path .` after relevant verification.

## Tasks

- [x] Add `crates/lato-agent/src/configuration.rs`: typed bounded configuration, JSON loader, validation, fingerprint and resume checks. Verify malformed inputs, defaults and changed configuration rejection.
- [x] Add `ToolRuntime::restricted_to` in `crates/lato-tools/src/runtime.rs`, preserving policy and error-code mappings; add restriction support to `SkillRuntimeBinding`. Verify omitted canonical and wire names cannot resolve and unknown allowlist entries fail.
- [x] Integrate configuration and business-tool registration in `host.rs`; parameterize world-state prompt generation in `actor.rs` and configure fresh system history through `legacy_driver.rs` / `runtime_session.rs`. Persist the initial application identity through a host-only `InitializeApplication` runtime command using existing system-message journal records. Verify legacy mode still uses its original prompt and general mode avoids coding instructions.
- [x] Add `crates/lato-agent/tests/general_agent.rs` with deterministic end-to-end business-module, schema, approval, isolation, and journal-resume scenarios. Run `cargo test -p lato-agent --test general_agent`.
- [x] Add an executable embedding example and README instructions. Run affected crate tests and workspace checks, `cargo fmt --all -- --check`, lint, then install and test the installed CLI with an isolated Lato home.

## Validation

Completed on 2026-10-21. Workspace tests passed: 1,564 tests across 129 test/doc-test suites. Clippy with all targets/features and denied warnings passed. The executable business-agent demo passed. Installed using `cargo install --path . --locked` to preserve the tested dependency versions; installed-binary general/default mode, configuration diagnostics, journal identity, and malformed-configuration smoke checks passed. See `docs/testing/reports/2026-10-21-general-agent-foundation.md` for scope and limitations.

Execution remains in this chat under the user's instruction to continue development. The design and implementation use the already approved layered architecture; task-level choices are resolved during inspection and recorded here.
