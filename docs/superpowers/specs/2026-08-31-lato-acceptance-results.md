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

## Final commands

```text
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
PATH="$HOME/.cargo/bin:$PATH" cargo check --workspace --target x86_64-pc-windows-gnu
LATO_HOME=$(mktemp -d) cargo run -q -- -p "reply with hi only"
```
