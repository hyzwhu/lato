# Phase 7B3 Workflow Rhai Host Release Gate

Planned checkpoint date: 2026-09-12

Result: PASS (focused crates + install)

Phase 7B3 ports Grok Build's Rhai workflow engine and journal, runs
`agent()` through `ChildSessionRunner`, and compiles 7B2 JSON steps into
sequential `agent()` scripts. `lato workflow run` uses the configured
model or `default_fake_stream()`. `--validate-only` stays canned. No TUI
runner, no AgentField.

## Commits

On `phase-7b3-workflow-rhai-host` from `d3db617`:

- `feat(workflow): port Grok script host, journal, and meta`
- `feat(workflow): port Rhai engine and canned validate`
- `feat(workflow): compile JSON steps into sequential agent scripts`
- `feat(agent): resolve user, project, and plugin workflow scripts`
- `feat(agent): run workflow agents through ChildSessionRunner`
- `fix(agent): keep schema retries within agent_budget`
- `feat(cli): run workflows through the Rhai host`
- `test(cli): isolate workflow CLI tests from LATO_MODEL`
- docs / clippy follow-ups in this gate commit

## Verification

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy -p lato-workflow -p lato-agent --all-targets -- -D warnings` | PASS |
| `cargo test -p lato-workflow` | PASS (71 unit + 5 engine + 1 inert + 2 script_runtime) |
| `cargo test -p lato-agent --test workflow_host --test subagent_runner` | PASS (6 + 7) |
| `cargo test --test workflow_cli` | PASS (4) |
| `cargo install --path .` | PASS (see this commit message / install log) |

Workspace-wide `cargo test --workspace` was not required to block 7B3 after
focused crates and install succeeded; re-run before a public tag if needed.

## Invariants

- Live `agent()` counts 1 against `agent_budget`; schema retries do not.
- `parallel` over cap launches zero children.
- `fork_context` is `Unsupported`.
- Untrusted project `.lato/workflows` and untrusted plugins are invisible.
- Unknown id → `workflow.not_found` with no host spawn.
- JSON plugin CLI fixture completes on the fake stream when `LATO_MODEL` is unset.
