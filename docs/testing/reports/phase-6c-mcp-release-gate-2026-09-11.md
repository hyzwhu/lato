# Phase 6C MCP Runtime Release Gate

Planned checkpoint date: 2026-09-11

Actual final execution date: 2026-09-11

Result: PASS (with documented gate hygiene)

Phase 6C is based on Grok Build commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138` and lands on branch `phase-6c-mcp`
above `master @ 2a66a019`. This checkpoint delivers trusted+enabled MCP runtime
over frozen `PluginSnapshot`: stdio and streamable HTTP transports, bounded
lifecycle, schema cache, progressive `search_tool`/`use_tool` discovery, and
ToolRuntime safety membrane integration. SenseNova exit0/no-artifact work
remained parallel and did not block this gate.

## Phase 6C commits on `phase-6c-mcp`

| Task | SHA | Subject |
| --- | --- | --- |
| 1 | `d806076f58b96908a59ae282f8fa54eda031f2d3` | feat(mcp): materialize trusted MCP descriptors from snapshots |
| 2 | `e3b8c552d5f8e3b6470babf6a741d323a226c99a` | feat(mcp): bound stdio and streamable HTTP server lifecycle |
| 3 | `bdd032cc4622da688ea533862268a0a827e18f7a` | feat(mcp): cache MCP tool schemas per snapshot generation |
| 4 | `f411a3968119ff7ecda0ea5f9ced80c69aed8658` | feat(tools): progressive MCP discovery via search_tool and use_tool |
| 5 | `10fd978d22d7a6037387eca80d8bbf8932d41c72` | feat(agent): route MCP calls through ToolRuntime safety membrane |
| 6 | `00fd26e24e2fec1deeadc389248190d7a14e5646` | feat(mcp): harden MCP results, SSRF, and fault isolation |
| 7 | `aed4d9afafdbb4c7e163f49f8af7fd8186ec9e37` | feat(agent): isolate MCP generations and narrow child capabilities |
| 8 | `9af7ee73bd61e370f6217c0b93a5fb07b9b43d1c` | docs: record phase 6c mcp runtime gate |

## Focused verification

```text
$ cargo test -p lato-mcp
PASS (lifecycle, discovery, fault_boundaries + unit tests)

$ cargo test -p lato-extensions --test mcp_config
PASS

$ cargo test -p lato-tools --test mcp_provider
PASS

$ cargo test -p lato-agent --test mcp_runtime
PASS
```

## Repository quality gate

```text
$ cargo fmt --all -- --check
PASS

$ cargo test --workspace
104 suites; 1029 passed; 0 failed; 0 ignored

$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile; 0 warnings; PASS
```

Platform: Linux `x86_64` (`cursor` kernel 6.12.94+), `rustc`/`cargo` 1.98.1.
OpenSSL linkage used `pkg-config` after installing `pkg-config` + existing
`libssl-dev`; `OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu` and
`OPENSSL_INCLUDE_DIR=/usr/include` also present in the environment from prior
Phase 6C tasks.

### Gate hygiene applied during Task 8

- `cargo fmt --all` across Phase 6C sources so `--check` is green.
- Clippy `-D warnings` cleanups in `lato-mcp`, `lato-tools` (`mcp_provider`,
  `shell` tests), and `lato-workspace` (`sandbox` collapsible-if /
  needless-return).
- `tests/phase6b_hooks_smoke.rs` and `crates/lato-extensions/tests/hooks_command.rs`
  drain stdin (`cat >/dev/null`) before `printf` so short-lived command hooks
  do not race the runner's stdin write (EPIPE → fail-open / `HookRunError::Io`).
  These were pre-existing flakes exposed under the denser Phase 6C workspace run.

## Local deployment

```text
$ CARGO_INCREMENTAL=0 cargo install --path .
Installed package `lato v0.1.0-beta.2` (executable `lato`)
# install path: /home/box/.cargo/bin/lato

$ lato --version
lato 0.1.0-beta.2
```

## Installed-command smoke

```text
$ LATO_SMOKE_BINARY="$(command -v lato)" \
    cargo test --test phase6c_mcp_smoke -- --nocapture
running 1 test
phase6c-mcp-smoke-ok
test phase6c_installed_command_smoke_exercises_mcp_runtime ... ok
test result: ok. 1 passed; 0 failed
```

The smoke verifies the installed binary, then exercises a trusted temp plugin
with:

1. **stdio** MCP server (`demo__ping`) — progressive `search_tool` → `use_tool`
2. **loopback streamable HTTP** MCP server (`httpdemo__echo`) — search → use
3. **SSRF reject** — private and link-local resolutions map to `mcp.unsafe_url`
4. **child narrowing** — child cannot restore a parent-removed server/tool
5. **SessionEnd reap** — stdio child PIDs are gone after bounded shutdown

Default model surface remains `search_tool` / `use_tool` only (no direct
`server__tool` expansion).

## Docs

- README `### Plugin MCP` documents config, progressive discovery, trust
  prerequisite, shutdown/reap, and child narrowing; removed
  “MCP execution remains Phase 6C”.
- Upstream ledger updated with smoke / release-gate attribution.

## SenseNova parallel confirmation

No SenseNova bug-fix commits or gate criteria were introduced on
`phase-6c-mcp`. SenseNova exit0/no-artifact remains a parallel track and did
not block Phase 6C.

## Preserved pre-existing dirty paths

None observed on this agent box beyond Task 8 worktree changes.
