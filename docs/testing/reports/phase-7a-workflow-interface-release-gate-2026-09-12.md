# Phase 7A Workflow Interface Release Gate

Planned checkpoint date: 2026-09-12

Result: PASS (focused crates + install)

Phase 7A freezes trusted+enabled workflow **descriptors** and an inert
`Workflow` trait. No script engine, no AgentField, no doctor listing.

## Commits

See `git log` on `master` for:

- `feat(workflow): add inert workflow types and stable errors`
- subsequent descriptor materialization / docs commits in this slice

## Verification

| Check | Result |
| --- | --- |
| `cargo test -p lato-workflow` | PASS (unit + inert_runtime) |
| `cargo test -p lato-extensions` | PASS (includes `workflow_config`) |
| `cargo clippy -p lato-workflow -p lato-extensions --all-targets -- -D warnings` | PASS |
| `cargo test -p lato-agent --test mcp_runtime --test subagent_runner --test skills_runtime` | PASS |
| `cargo install --path .` | PASS |

Workspace-wide `cargo test --workspace` was not required to block 7A after
focused crates and install succeeded; re-run before a public tag if needed.

## Invariants

- Untrusted/disabled plugins yield no workflow descriptors.
- Ids are `plugin/workflow`; collisions keep-first.
- `agent_budget` default 128, reject 0 and >1024.
- `InertWorkflow::run` → `workflow.not_implemented`; `BudgetAccount` unchanged.
- Child `WorkflowCapabilityCeiling` never widens.
