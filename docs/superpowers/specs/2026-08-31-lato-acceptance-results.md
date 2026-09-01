# Lato v1 Acceptance Results

- Acceptance source: `2026-08-31-lato-acceptance.md`
- Host OS: macOS arm64
- Rust: stable, edition 2024
- LIVE vendor tests: skipped because no vendor API keys or subscription accounts are available in the test environment

## Phase 0 / A

| Cases | Result | Evidence |
|---|---|---|
| A0-1..A0-4 | PASS | `cargo test --workspace a0_` and CLI integration tests |
| A1-1..A1-8 | PASS | protocol, host, stdio ACP, persistence, model switch, and architecture tests |
| A2-1..A2-7 | PASS | actor/tool tests; phase-0 router snapshot remains separately testable |
| A3-1, A3-3, A3-4, A3-5 | PASS | path/shell tests; Windows spellings are tested by platform-neutral helper |
| A3-2 OS:windows | N/A on macOS | Windows target compiles; runtime case is in Windows CI |
| A4-1..A4-6 | PASS | store/env/refresh/login/status tests |
| A5-1..A5-2 | PASS | `cli_headless` integration tests |

## Phase 1 / B

| Case | Result | Evidence |
|---|---|---|
| B1-1 | PASS | protocol+host request routing test |
| B1-2 | PASS | mixed supported catalog test |
| B1-3 | PASS | three-dialect request shape, SSE parser fixtures, and real localhost HTTP roundtrip |
| B1-4 | PASS at phase-1 checkpoint | Google was rejected until E3 connected the dialect; final v1 now advertises it supported |
| B1-5 | PASS | CLI and auth registry reject cloud-special API-key login |
| B1-6 | LIVE-SKIPPED | Offline equivalent `b1_6_headless_http_model_tool_loop_edits_workspace_offline_fixture` passes end-to-end |
| B1-7 | PASS | raw Anthropic key and bearer token tests |

## Phase 2 / C

| Case | Result | Evidence |
|---|---|---|
| C1-1..C1-5 | PASS | persisted mock tokens, provider boundaries, PKCE, refresh/no-fallback tests |
| C1-6 | LIVE-SKIPPED | Requires real Kimi Code and ChatGPT subscriptions |

## Sandbox / D

| Case | Result | Evidence |
|---|---|---|
| D1-1 | PASS | macOS Seatbelt workspace write test |
| D1-2 | PASS | macOS outside-write denial test |
| D1-3 OS:linux | N/A on macOS | bwrap test and Linux CI job are configured |
| D1-4 OS:windows | N/A on macOS | Restricted Token + ACL + Job Object backend, Windows test, and CI job are configured; Windows target compiles |
| D1-5 | PASS | missing wrapper/backend failure is fail-closed |
| D1-6 | PASS | host file read/edit test under shell sandbox policy |

## Phase 3–5 / E

| Case | Result | Evidence |
|---|---|---|
| E3-1 | PASS | Azure, Google, Vertex, Bedrock, Mistral, and Cloudflare request fixtures |
| E4-1 | PASS | models.json loader, llama.cpp refresh fixture, and CLI→ACP→HTTP→SSE end-to-end test |
| E5-1 | PASS | plugin trust/reload, hooks, MCP stdio transport, progressive discovery, SSRF controls, and subagent worktree isolation tests |

## Global / G and CI / F

- G1: PASS — OAuth outside `kimi-coding` and `openai-codex` is rejected.
- G2: PASS — CLI architecture test proves headless enters through `AcpHost`; no direct actor call in CLI.
- G3: PASS — interactive default is `ask`.
- G4: PASS — hard-limit failure and explicit compaction tests; no silent truncation.
- F1/F3: implemented in `.github/workflows/ci.yml`; PR tests are offline.
- F2: Windows path, shell, headless, Restricted Token, fail-closed, and full workspace jobs configured.
- F4: LIVE cases are explicitly listed as skipped above rather than reported as passed.

## Phase 3A / Doctor and policy Beta gate (2026-09-01)

Measured on macOS arm64, rustc 1.98.0, from workspace HEAD plus this Doctor CLI slice.

| Gate | Result | Evidence |
|---|---|---|
| Focused tests | PASS | `cargo test --test doctor_cli --test policy_runtime_wiring --test tool_runtime_wiring`: doctor_cli 6 passed; policy_runtime_wiring 2 passed; tool_runtime_wiring 2 passed (wiring now requires `tool_runtime.prepare(` / `tool_runtime.execute(`) |
| Workspace tests | PASS | `cargo test --workspace --no-fail-fast`: **304 passed, 0 failed, 0 ignored** (304). Stale `invoke(` assertion updated to prepare/execute |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` after collapsing nested `if` in `crates/lato-workspace/src/sandbox.rs` and removing a needless struct update in `tests/doctor_cli.rs` |
| Install | PASS | `cargo install --path .` replaced `/Users/huangyongzhao/.cargo/bin/lato` (8,927,072 bytes, 2026-09-01) |
| Installed Doctor | PASS | `LATO_HOME=$(mktemp -d) lato doctor` → status `warn`, exit 0; `lato doctor --json` → `schema_version` 1, `tool_catalog` present, exit 0; `lato doctor --strict` → exit 1; seeded `LATO_TEST_SECRET` not present in output; no `--live` so no provider completion |
| Headless fake-model smoke | PASS | `LATO_HOME=$(mktemp -d) lato -p "reply with hi only"` printed `hi`, exit 0 |
| Ask-mode write (installed CLI) | N/A (no tty) | `lato -p --ask "write hello" </dev/null` → `error: --ask requires a tty`, exit 2 |
| Read / exact write / replay / sandbox | PASS (unit/integration fixtures) | `runtime_advertises_and_executes_the_same_tools` ok; `write_alias_consumes_allow_once_exactly_once` ok; `grant_is_consumed_before_tool_invocation` ok; `consumed_grant_cannot_be_replayed` ok; `missing_wrapper_does_not_execute_workspace_command` ok; `missing_wrapper_is_unavailable_and_does_not_run` ok |

**Public Beta ready:** workspace test gate is now green after updating the stale `tool_runtime.invoke(` wiring assertion to `prepare`/`execute`. Doctor CLI, offline default, Clippy, install, and installed-binary Doctor smokes previously passed; re-measure Clippy/install/smokes before a public claim if those artifacts drift.

## Final commands

```text
cargo fmt --all -- --check
cargo test --test doctor_cli --test policy_runtime_wiring --test tool_runtime_wiring
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo install --path .
LATO_HOME=$(mktemp -d) lato doctor
LATO_HOME=$(mktemp -d) lato doctor --json
LATO_HOME=$(mktemp -d) lato -p "reply with hi only"
PATH="$HOME/.cargo/bin:$PATH" cargo check --workspace --target x86_64-pc-windows-gnu
```
