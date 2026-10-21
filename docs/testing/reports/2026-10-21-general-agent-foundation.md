# General Agent Foundation — Verification

Date: 2026-10-21

## Delivered scope

- A typed, bounded `AgentConfiguration` and optional trusted `$LATO_HOME/agent.json`.
- General application instructions and explicit model-call tool catalogs, with existing coding defaults preserved.
- Immutable host configuration and public Rust business-tool registration.
- Actual runtime catalog restriction, schema validation, approval checks, and legacy alias compatibility for business tool names.
- Initial application identity synchronized to the canonical journal before session publication; fingerprint checks on resume, including zero-turn sessions.
- Offline doctor diagnostics, an executable Rust embedding example, and an MCP-oriented configuration template.
- Project instructions record the user's preference to implement reasonable choices directly without routine brainstorming/design approval.

## Checks and results

| Check | Result |
|---|---|
| `cargo test --workspace -- --test-threads=2` | Passed: 1,564 tests, 0 failures, 0 ignored; 129 suites including doc tests |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed |
| `cargo run -p lato-agent --example business_agent --offline` | Passed: real lookup/report tool execution with a scripted model and fixture data |
| `cargo install --path . --locked` | Passed; replaced `/Users/huangyongzhao/.cargo/bin/lato` |
| Installed CLI with an isolated general-agent home | Passed: headless response and persisted application identity |
| Installed CLI `doctor --json` | Passed: configuration status reported without application instructions |
| Installed CLI with malformed application configuration | Passed: rejected with `agent.invalid_configuration` |
| Installed CLI with default configuration | Passed: headless coding-mode smoke |

The initial unrestricted test run exposed load-sensitive failures in existing MCP hook tests with two-second hook deadlines. An isolated rerun and the final full workspace run with two test threads passed all 18 MCP runtime tests. No hook deadlines or assertions were weakened. A new runtime test initially reused replay state captured before shutdown; its fixture was corrected to replay the current journal sequence before restarting.

The ordinary install command began resolving newer dependency versions. That build was interrupted before replacement, and installation was repeated with `--locked` so the deployed executable uses the tested lockfile. `Cargo.lock` was not modified.

## Practical limits and next work

This increment verifies execution plumbing with deterministic or loopback fixtures. It does not measure real-model intent recognition, parameter inference, or task accuracy and does not integrate the user's actual business APIs.

Application-aware built-in child-task, Workflow, and AgentField delegation remains disabled for custom configurations. Explicit host workflow operations remain available. The MCP gateway exposes the full surface of the configured trusted MCP servers; per-user and per-tenant business authorization belongs in adapters/services. Trusted plugin hooks retain their existing behavior. Configured sessions preserve the original system/workspace context, so use the same application workspace when resuming. Older binaries without application-configuration support do not enforce these configuration checks.

Follow-on development includes product-facing service integration, application-aware delegation, module packaging, current-workspace refresh on resume, multi-user isolation, and live evaluation against representative business tasks. The overall development goal remains active; this report covers the first increment only.
