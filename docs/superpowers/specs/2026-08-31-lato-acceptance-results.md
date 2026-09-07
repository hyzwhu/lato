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

## HTTP model tool-loop hardening gate (2026-09-02)

Measured on macOS arm64, rustc 1.98.0, from branch `codex/http-tool-loop-hardening`.

| Gate | Result | Evidence |
|---|---|---|
| Focused regressions | PASS | Lossless split-UTF-8 streaming, malformed tool arguments, OpenAI Responses/Anthropic request options, unrelated HTTP 400 classification, repeated-call termination, local-fact false positives, compatibility aliases, model cache, and offline HTTP edit-loop fixtures all pass. |
| Workspace tests | PASS | `cargo test --workspace --no-fail-fast`: **314 passed, 0 failed, 0 ignored**; all doc-tests pass. |
| Formatting | PASS | `cargo fmt --all -- --check` exits 0 with no output. |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` exits 0 with no warnings. |
| Install | PASS | `cargo install --path .` replaced `/Users/huangyongzhao/.cargo/bin/lato`. |
| Installed Doctor | PASS | Empty temporary `LATO_HOME`: human Doctor reports `warn` and exits 0; JSON reports `schema_version: 1`, nine registered tools, and exits 0. |
| Installed headless smoke | PASS | Fake-model prompt prints `hi`; `lato -p "pwd"` prints the repository working directory. |
| LIVE vendors | SKIPPED | No live provider credentials were used; no live-provider claim is made by this gate. |

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

## Phase 4A canonical event journal gate (2026-09-02)

Measured on macOS arm64 with Homebrew `rustc 1.98.0` from branch
`codex/phase-4a-event-journal`.

| Gate | Result | Evidence |
|---|---|---|
| Formatting | PASS | `cargo fmt --all -- --check` exited 0. |
| Focused journal suites | PASS | `lato-core` journal contract: 4 passed; `lato-store`: 14 passed; `lato-runtime`: 15 passed; `lato-agent`: 64 passed. |
| Workspace tests | PASS | `cargo test --workspace --no-fail-fast`: **348 passed, 0 failed, 0 ignored** including doc-tests. The runtime wiring assertion was updated to the new `authorize` / `execute_authorized` membrane before the final green run. |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` exited 0 with no warnings. |
| Crash-boundary repetition | PASS | `lato-store/tests/file_recovery.rs` ran 3 times with 12/12 passing each time; `lato-agent/tests/journal_runtime.rs` ran 3 times with 5/5 passing each time. Covered write/flush/sync boundaries, import publication, no duplicate acknowledgement, prepared-before-invoke, and unknown completion outcome. |
| Legacy migration | PASS | Atomic/idempotent import, retained legacy source, journal precedence, malformed-source no-publication, list deduplication, and unresolved prepared-call refusal all pass. |
| Source headless journal smoke | PASS | Fresh temporary `LATO_HOME`; `cargo run -q -- -p "reply with hi only"` printed `hi` and created `sessions/s<timestamp>-1/events.jsonl`; offline Doctor JSON parsed with `schema_version == 1`. |
| Local install | PASS | `cargo install --path .` completed in release mode and replaced `/Users/huangyongzhao/.cargo/bin/lato`. |
| Installed binary smoke | PASS | Fresh temporary `LATO_HOME`; installed `lato doctor --json` exited 0 with `schema_version == 1`; installed `lato -p "reply with hi only"` printed `hi`, exited 0, and created `sessions/s<timestamp>-1/events.jsonl`. |
| Windows cross-check | UNAVAILABLE optional gate | `rustup target list --installed` listed `x86_64-pc-windows-gnu`, but the active Homebrew Rust toolchain could not find that target's `core` crate (`E0463`). No Windows compile claim is made for this checkpoint. Unix-only code remains platform-gated and non-Unix fallbacks compile structurally in the host build. |
| LIVE vendors | SKIPPED | No live provider credentials or subscription accounts were used; no live-provider claim is made. |

The canonical journal stores no model/reasoning deltas, repairs only a torn final record, and rejects
complete corruption. Mutating/non-idempotent tools are synchronously prepared before invocation and
synchronously completed afterward. A prepared call without completion is surfaced as
`journal.incomplete_side_effect` and is never automatically replayed. Phase 4B snapshotting and
compaction were intentionally not included in this gate.

## Public Beta local release gate (2026-09-02)

Measured on macOS arm64 with Homebrew `rustc 1.98.0` from branch
`codex/public-beta-release`. The binary package version is `0.1.0-beta.1`.

| Gate | Result | Evidence |
|---|---|---|
| Formatting | PASS | `cargo fmt --all -- --check` exited 0. |
| Workspace tests | PASS | `cargo test --workspace --no-fail-fast`: **364 passed, 0 failed, 0 ignored** across 52 unit, integration, and doc-test result groups. |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` exited 0 with no warnings. |
| Typed CLI | PASS | Generated help contains `sessions`, `resume`, `login`, `doctor`, `acp`, and `-p`; `lato --version` prints `lato 0.1.0-beta.1`; invalid combinations retain exit 2. |
| Session listing | PASS | Fresh-home human output reports `No saved sessions.`; JSON parses with `schema_version == 1`; two persisted sessions list newest-first. |
| Session resume | PASS | Unknown sessions are rejected without creation; existing legacy and canonical sessions hydrate history; unresolved prepared side effects retain fail-closed journal handling; non-TTY resume returns 2. |
| Doctor model resolution | PASS | Built-in, `models.json`, `models-store.json`, and `model-cache.json` sources are covered offline. The installed local configuration reports `sensenova/glm-5.2 found in models-store.json` and Doctor status `ok`. |
| Source smokes | PASS | Fresh temporary `LATO_HOME`: version matched, Doctor JSON parsed with schema 1, fake-model prompt returned `hi`, and session JSON parsed with one persisted session. |
| Local install | PASS | `cargo install --path .` replaced `/Users/huangyongzhao/.cargo/bin/lato` with `lato 0.1.0-beta.1` (10,546,512 bytes). |
| Installed smokes | PASS | Fresh temporary `LATO_HOME`: installed version matched, Doctor JSON parsed with schema 1, fake-model prompt returned `hi`, and session JSON parsed with one persisted session. |
| Workflow static validation | PASS | Ruby YAML parsing succeeded for `release.yml` and `live-smoke.yml`; release workflow contains five native target entries, an exact archive-count gate, SHA-256 generation, offline smokes, and publish-only `contents: write`; provider secrets occur only in the manual LIVE workflow. |
| Five-platform remote build | PASS | GitHub Actions run `33596316688` completed successfully for macOS x86_64/arm64, Linux x86_64/arm64, and Windows x86_64; every build, offline Doctor/headless smoke, archive upload, checksum, and publish step passed. |
| LIVE providers | SKIPPED | The manual `live-provider-smoke` workflow was not dispatched and no provider credentials were used. Release notes must state that LIVE validation is unavailable until a successful run is linked. |
| GitHub prerelease | PUBLISHED | `v0.1.0-beta.1` was published as a non-draft prerelease at `https://github.com/hyzwhu/lato/releases/tag/v0.1.0-beta.1` with five platform archives plus `SHA256SUMS`. All five downloaded archives passed local `shasum -a 256 -c SHA256SUMS` verification. |
| General branch CI | FOLLOW-UP | Initial `master` CI run `33595874555` passed macOS and the no-LIVE-network guard, but the full Linux and Windows test jobs exited 101. The release workflow's native builds and binary smokes passed on both platforms; the failing full-suite cases still require authenticated log inspection and repair. |

The downloadable Public Beta gate is green and the five archives are checksum-verified. LIVE provider
coverage remains explicitly skipped. General Linux/Windows full-suite CI is a visible follow-up and
must be green before promoting the Beta toward a stable release.

## Phase 5 release baseline (2026-09-07)

Measured on macOS arm64 with Rust 1.98.0 from branch
`codex/phase-6a-plugin-runtime`, before beginning Phase 6A. The binary package
version is `0.1.0-beta.2`.

| Gate | Result | Evidence |
|---|---|---|
| Formatting | PASS | `cargo fmt --all -- --check` exited 0. |
| Phase 5 focused suites | PASS | Runtime: 187 passed; workspace allocation/sandbox: 24 passed; tool runtime/policy: 25 passed; agent ACP/session/subagent: 26 passed; CLI/session/TUI: 37 passed. |
| Workspace tests | PASS | `cargo test --workspace --no-fail-fast` completed with every unit, integration, and doc-test result group green. |
| Clippy | PASS | `cargo clippy --workspace --all-targets --all-features -- -D warnings` exited 0 with no warnings. |
| Real command task smoke | PASS | `phase5_command_smoke` invoked the real CLI against a bounded localhost SSE fixture and completed parent `spawn` → child worker → `inspect` → `wait` → final response. It also verified the canonical parent journal and empty task-worktree directory. |
| Local install | PASS | `cargo install --path .` completed in release mode and replaced `/Users/huangyongzhao/.cargo/bin/lato`. |
| Installed binary smoke | PASS | `LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase5_command_smoke -- --nocapture`: 1 passed; `lato --version` printed `lato 0.1.0-beta.2`. |
| LIVE providers | SKIPPED | The deterministic command smoke used a localhost provider fixture; no external provider credential or LIVE claim is involved. |

This gate freezes the merged Phase 5B/5C task-runtime behavior as the baseline for
the plugin and extension work. The smoke asserts the public task-tool names
(`spawn`, `send`, `wait`, `cancel`, `inspect`) and rejects the removed
`spawn_subagent` alias.

## Phase 6A plugin runtime foundation gate (2026-09-07)

Measured on macOS arm64 with Rust 1.98.0 from branch
`codex/phase-6a-plugin-runtime`. The binary package version is
`0.1.0-beta.2`.

| Gate | Result | Evidence |
|---|---|---|
| Formatting | PASS | `cargo fmt --all -- --check` exited 0. |
| Plugin runtime | PASS | `cargo test -p lato-extensions`: 23 passed; manifest containment, deterministic CLI/project/user discovery, trust and enablement, immutable snapshots, monotone child narrowing, atomic reload, last-known-good retention, and cross-session CLI-root isolation are covered. |
| Typed contracts and runtime | PASS | `cargo test -p lato-core`: 72 passed; `cargo test -p lato-protocol`: 5 passed; `cargo test -p lato-runtime`: 190 passed including 2 compile-fail doc-tests. Plugin snapshot adoption is journaled before publication. |
| Agent and CLI integration | PASS | Focused agent suites: 36 passed. `plugin_cli`, Phase 5 command smoke, headless, TUI, and sessions suites: 40 passed. Real-process ACP coverage proves two repeatable `--plugin-dir` roots reach a session snapshot; missing roots fail before session creation with exit 2. |
| Workspace tests | PASS | `CARGO_INCREMENTAL=0 cargo test --workspace --no-fail-fast`: **843 passed, 0 failed, 0 ignored**, including doc-tests. Incremental output was disabled after the first build attempt exhausted the isolated worktree's build volume; no test assertion failed in that attempt. |
| Clippy and diff | PASS | `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --all-features -- -D warnings` and `git diff --check` exited 0. |
| Trust and activation | PASS | CLI and user sources are trusted; project sources remain inactive for an untrusted folder. Project/user plugins default disabled, CLI overrides default enabled, and explicit disable wins. Components are cataloged only; no Phase 6B hook/skill or Phase 6C MCP execution claim is made. |
| Reload and session isolation | PASS | Explicit `lato/plugins/reload` forces rediscovery and publishes a higher generation. Idle sessions adopt immediately; active turns retain their immutable generation until the next safe boundary. Child snapshots intersect parent, profile, and workspace `ExtensionInvoke` capability and never read a newer shared snapshot after construction. |
| Local install | PASS | `CARGO_INCREMENTAL=0 cargo install --path .` replaced `/Users/huangyongzhao/.cargo/bin/lato`; `lato --version` printed `lato 0.1.0-beta.2`. |
| Installed binary smoke | PASS | `LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase5_command_smoke -- --nocapture`: 1 passed on unchanged retry after the localhost fixture first encountered the documented transient macOS `WouldBlock (os 35)`. Timeout and assertions were not weakened. |
| LIVE providers and remote platforms | SKIPPED | Tests used local deterministic fixtures. No vendor credentials, external provider, remote CI, Linux, or Windows validation is claimed for this gate. |

Phase 6A freezes the plugin catalog and session-snapshot boundary. Skills and
hooks remain Phase 6B work; MCP server startup and tool execution remain Phase
6C work.
