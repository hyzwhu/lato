# Application-aware business Workflows

Date: 2026-10-21

## Delivered behavior

General application configurations may select `builtin:workflow`. Immutable
application instructions and registered business modules flow into child runners.
Each child catalog intersects the parent's exact allowed tool names with its role
capabilities; the parent-only Workflow manager is absent. Read-only role filtering
also excludes mutation tools with empty declared capability lists, and inherited
read-only filesystem sandboxes are preserved. General workers share the inherited
workspace and use domain-neutral result verification; explicit JSON output
contracts still run through schema validation and the existing correction budget.
Coding profiles retain their existing formats and worktree behavior.

Delegated progress is owner-tagged and child approvals appear under the original
HTTP session. The exact policy fingerprint and execution session identity remain
unchanged. Main-operation completion does not cancel background Workflows. Pause,
resume and stop remain available through authenticated HTTP routes. Cancelling a
worker cleans up its pending approval. Workflow resume enforces the active-run and
agent-budget ceilings, and shutdown closes admission before draining children.

Completed values are retained in run status/persistence up to 256 KiB, with an
explicit truncation flag/notice for larger outputs. Configured run records use
version 2 and bind the application configuration fingerprint; legacy or different
application managers cannot restore them. Standalone `lato workflow run` also
loads the trusted application configuration and fails on invalid configuration.

## Automated verification

- `cargo test --workspace --locked --no-run`: passed before running shell-hook tests.
- `cargo test --workspace --locked -- --test-threads=1`: **1,587 passed**, zero failed/ignored, 133 test/doc-test suites.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Python example scripts compile and the example application JSON parses.
- A local HTTP client fixture verified that the sample client continues processing
  child approvals after main-operation completion and closes only after the
  background Workflow finishes.

Added regression cases cover owner-routed exact approvals and cross-session
rejection; business JSON results; Workflow discovery and HTTP pause/resume/stop;
stale approval cleanup; forged coding-tool calls; inherited sandbox and role
restrictions; schema correction without an additional logical agent charge;
application-bound persistence; result truncation; invalid configuration without
coding fallback; resume capacity/budget/closed-admission checks.

## Execution example

`examples/general-agent/customer-report.rhai` delegates lookup and report creation
to two general application workers. `smoke_http.py --workflow` starts a real Lato
process with a local scripted HTTP model provider and an actual Python MCP server.
The pre-install debug-binary smoke passed: seven provider calls, four approved
calls, two business modules, two workers, a completed business result and a verified
report artifact containing the synthetic revenue.

Local deployment passed with `cargo install --path . --locked`; the installed
`/Users/huangyongzhao/.cargo/bin/lato` reports `0.1.0-beta.2`. Both installed-binary
smokes passed:

- Direct business chain: three HTTP provider calls, two approvals, verified artifact.
- Workflow chain (`--workflow`): seven HTTP provider calls, four approvals, two
  delegated workers, completed business result, verified artifact.

Cargo emitted an existing-lockfile warning that `chacha20 0.10.1` is yanked.
Installation succeeded using the tested lockfile; this change did not alter that
package version or perform an unrelated dependency upgrade.

## Scope

Model decisions and customer records in these checks are scripted/synthetic. They
verify real execution plumbing and policy boundaries, not live-model business-task
accuracy. No real business API, production data or user model credential is used.
General read-only roles omit generic mutation-capable MCP brokers; direct read-only
SDK tools can be used for those roles. Trusted Workflow scripts and plugin hooks
retain their existing host privileges. General-mode isolated coding worktrees are
unsupported. A service represents one trusted application principal; multi-tenant
authorization, a desktop workbench, generic task/AgentField delegation for custom
applications, and representative live-model evaluations are separate product work.
