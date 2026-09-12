# Lato

Lato is a Public Beta coding agent for terminal-based development workflows. It provides an interactive coding CLI, a headless prompt mode, and an ACP host backed by the same runtime, tool policy, and durable session journal.

> **Beta software:** interfaces and stored formats are versioned and tested, but may still change before a stable release. Back up important work and review tool approvals carefully.

Prebuilt binaries are available for macOS Intel, macOS Apple Silicon, Linux x86-64, Linux ARM64, and Windows x86-64.

## Install a prebuilt binary

Download the archive for your platform and `SHA256SUMS` from the [v0.1.0-beta.2 release](../../releases/tag/v0.1.0-beta.2). Verify the archive before extracting it.

macOS example for Apple Silicon:

```bash
shasum -a 256 -c SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.2-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.2-aarch64-apple-darwin/lato "$HOME/.local/bin/lato"
```

Linux x86-64 example:

```bash
sha256sum --check SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.2-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.2-x86_64-unknown-linux-gnu/lato "$HOME/.local/bin/lato"
```

Ensure `$HOME/.local/bin` is on `PATH` before running Lato.

Windows PowerShell example:

```powershell
$archive = "lato-0.1.0-beta.2-x86_64-pc-windows-msvc.zip"
Get-FileHash $archive -Algorithm SHA256
# Compare the printed hash with this archive's entry in SHA256SUMS.
Expand-Archive $archive -DestinationPath .
New-Item -ItemType Directory -Force "$HOME\bin" | Out-Null
Copy-Item ".\lato-0.1.0-beta.2-x86_64-pc-windows-msvc\lato.exe" "$HOME\bin\lato.exe"
```

Add `%USERPROFILE%\bin` to the user `PATH`, then verify the installation:

```text
lato --version
```

## Quick Start

Run `lato` without arguments:

```bash
lato
```

On first launch Lato opens its full-screen interface and guides you through provider/model selection, OAuth or a masked API-key input, folder trust, and sandbox scope. The selected model is saved in `~/.lato/config.json`; credentials are kept separately in the locked credential store. The Ratatui interface includes session navigation, streaming reasoning and responses, live tool-call cards, and approval dialogs.

The interface follows `LC_ALL`, `LC_MESSAGES`, or `LANG` on first launch and supports Chinese and English. Override and persist the language with either form below; `/lang` switches it from inside the TUI.

```bash
lato --lang zh-CN
lato --lang en
```

Interactive commands:

```text
/help  /new  /clear  /compact  /sessions  /rename  /delete  /model  /login  /doctor  /search  /lang  /approve  /status  /permissions  /exit
```

Tool calls start collapsed, showing their name, status, and duration. Focus the Tool calls panel with Tab, select calls with Up/Down or j/k, and use Enter/Space to toggle details (Left collapses, Right expands). PgUp/PgDn scroll through full arguments and results; Home/End select the first/last call. A scrollbar shows the current position. Incoming calls preserve your place while you are inspecting the panel.

Use Tab/Shift-Tab to move between panels, Up/Down to scroll the focused panel, Enter on a selected session to resume it, Cmd/Ctrl-K to open the command palette, Ctrl-F or `/search` to search, and Ctrl-C to cancel a streaming turn or exit while idle. In the session panel, press `r` to rename the selected session and press `d` twice within three seconds to delete it permanently. `/rename [title]` renames the current session, while `/delete` opens an explicit permanent-delete confirmation. `/model` and `/login` open dialogs inside the TUI. Type to filter providers and models, use arrow keys and Enter to choose, and press Esc to cancel. API keys are masked; OAuth URLs and device codes appear in the dialog. Successful model changes preserve the current session and conversation. `/sessions` opens a filterable session picker that matches titles and IDs; `/doctor` shows the offline diagnostic report in the conversation. Finish or cancel a response before changing models or sessions. The input cursor follows Unicode text and scrolls long input horizontally. The workspace starts untrusted. Mutating tool calls display their name and arguments and request approval exactly at the execution boundary. You can instead trust the workspace for the process or use `/approve` to pre-authorize one call. On smaller terminals, side panels collapse automatically so the conversation remains usable.

Sandbox scope and tool approval are separate. The startup sandbox picker defaults to
`workspace`; select `off` explicitly to allow writes outside the current workspace,
including creating sibling projects such as `../abc`. `read-only` denies file writes.
Folder trust enables automatic approval within the selected scope; approving a tool
call does not expand that scope. These choices apply only to the current invocation.
You can also select the scope directly, including when resuming:

```bash
lato --sandbox off
lato --sandbox workspace
lato --sandbox read-only
lato resume s1788336000000-1 --sandbox off
```

`/permissions` and `/status` display the active scope and approval mode. To change
scope, exit and resume with the desired `--sandbox` value. `off` removes Lato's OS
sandbox, while operating-system permissions and existing tool policy checks still
apply. A failed sandboxed command never automatically retries with broader access.

For a one-shot, script-friendly prompt:

```bash
lato -p --model openai/gpt-4.1 --sandbox workspace "inspect this repository and report failing tests"
```

## Sessions and resume

Lato persists accepted turns and complete conversation state under `~/.lato/sessions`. List saved sessions in human or stable JSON form, then resume one interactively:

```bash
lato sessions
lato sessions --json
lato sessions rename s1788336000000-1 "Investigate parser regression"
lato sessions delete s1788336000000-1
# Scripts and other non-interactive callers must confirm explicitly:
lato sessions delete s1788336000000-1 --yes
lato resume s1788336000000-1
```

The first accepted prompt supplies a deterministic local title; manual renames always take priority over automatic titles. Human-readable listing prints the title and durable ID. JSON listing uses schema version 2 and returns `sessionId`, `title`, `titleSource`, `createdAtMs`, and `updatedAtMs` for each session. Delete is permanent: interactive callers must confirm it, while non-interactive callers must pass `--yes`.

Resume uses the currently configured default model and current working directory. It repeats the folder-trust prompt and, unless `--sandbox` is supplied, the sandbox picker. Permissions come from this invocation, not the saved journal. It fails closed instead of creating a replacement when a journal is missing, corrupt, or contains an unresolved side-effect outcome.

In the interactive TUI, `/compact` summarizes a long current conversation and
`/compact <context>` asks the summary to emphasize the supplied context. It uses the
current session model, may make one to three model calls, and never exposes or runs
tools. Ctrl-C cancels active sampling. A successful operation replaces only the
model-visible history through a durable checkpoint; the append-only canonical journal
is retained. Failures before the replacement marker keep the previous history active.
Automatic compaction now starts at 85% context utilization, with a speculative,
non-installing first summary pass beginning at 75%. Tool output that crosses the hard
context boundary is compacted before another provider request. A provider-reported
context overflow may compact and rebuild the interrupted request exactly once, but only
if no output was observed. Oversized compaction input degrades monotonically through
Prepared, Fitted, and Lossy stages under the same three-attempt ceiling; canonical
conversation history is never truncated in place. Deterministic failures suppress
repeated automatic work according to their turn, context, credit, or authentication
lifetime. Manual `/compact` remains available while automatic compaction is suppressed,
and `/status` reports the active suppression mode.

## Plugins

Lato discovers plugin roots from repeatable `--plugin-dir` arguments,
`.lato/plugins/*` in the project, and `$LATO_HOME/plugins/*`, in that priority
order. CLI and user plugins are trusted; project plugins activate only for a
trusted folder. A plugin must also be enabled before it is active.

The canonical manifest is `plugin.json`. Component paths are confined to the
plugin root. `lato/plugins/reload` atomically rebuilds the registry and updates
live sessions at the next safe turn boundary.

An enabled, trusted plugin may provide skills under the directory named by its
manifest's `skills` field. Each skill is a directory containing `SKILL.md`.
Only skills with an authored `description` or `when-to-use` are listed to the
model; invoke one by its stable qualified identity, such as
`example-plugin:code-audit`, through the built-in `skill` tool. Bare names work
only when unique. `disable-model-invocation: true` hides and blocks model calls,
while `user-invocable: false` blocks explicit user-origin calls.

Skill bodies are loaded from the frozen plugin snapshot only after invocation.
They may use `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N`, `${SKILL_DIR}`,
`${SESSION_ID}`, and `${LATO_PLUGIN_ROOT}` tokens. An `allowed-tools` list can
only narrow the tools available to the next model step; policy, approval,
sandbox, and child-profile checks still apply, and the enclosing tool catalog
is restored afterward. Omitting `allowed-tools` (or writing it as an empty
list) is treated as "no narrowing": the next step sees the full registered
tool set rather than a tightened surface, so authors who want to restrict
tools must enumerate at least one entry. A reload received during a turn is
staged for the next turn, so descriptions, bodies, and tool scopes never mix
generations.

Use `lato doctor` (or `lato doctor --json`) to inspect project/plugin trust and
configuration. ACP clients can call `lato/plugins/reload`; its response includes
the new generation, active/discovered counts, and bounded diagnostics. The
published snapshot is live in the shared registry by the time the call
returns: sessions that successfully adopted the new generation see it on
their next turn, and newly created sessions bind to it. If one or more live
sessions reject adoption, the response carries `failedSessionIds` alongside
the new generation; those sessions are not rolled back and silently retain
their previous generation until a subsequent `lato/plugins/reload` (or
session restart) advances them, so consumers can rely on a follow-up reload
to converge.

### Plugin hooks

Trusted, enabled plugins may declare `hooks/hooks.json` or an inline `hooks`
object. Phase 6B supports `SessionStart`, `SessionEnd`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `Stop`, `PreCompact`, and `PostCompact`, including
case/separator compatibility aliases. A canonical configuration is:

```json
{"hooks":{"PreToolUse":[{"matcher":"read_file|search","hooks":[{"type":"command","command":"./check","timeout":5},{"type":"http","url":"https://hooks.example.invalid/check"}]}]}}
```

Handlers execute sequentially in stable plugin/group order. Missing, empty, or
`*` matchers match all; simple `a|b` forms are exact alternatives and other
forms are regular expressions. Command hooks receive bounded JSON on stdin,
authentic `LATO_*` identity variables, a one-MiB combined output limit, and
process-tree cleanup on timeout or cancellation. HTTP hooks accept HTTPS only,
disable redirects, resolve and reject private/link-local/CGNAT/unspecified
destinations, and intentionally allow loopback for trusted local hooks. As in
the pinned upstream baseline, DNS is checked before the request; connection-time
address pinning is not yet provided.

Failures and timeouts fail open; healthy explicit block decisions are enforced.
Defaults are 5s for observers and `PreToolUse`, 30s for `UserPromptSubmit`, 600s
for `PostToolUse`/`Stop`, and 1500ms for `SessionEnd` (explicit SessionEnd values
cap at 60s). `PreToolUse` may replace the complete argument object, but never
the resolved tool name. Every replacement is passed through schema validation,
PolicyEngine, approval, scope, and sandbox planning again; a hook `allow` grants
no authority. Stop continuation is capped at eight. Reloads apply at turn
boundaries, child sessions materialize hooks only after capability narrowing,
and lifecycle/compaction observer mutations are ignored. Audit records store
hashes and bounded metadata rather than prompt, arguments, output, environment,
or URL credentials.

### Plugin MCP

Trusted, enabled plugins may declare MCP servers in `.mcp.json` or an inline
`mcpServers` object. Phase 6C supports **stdio** and **streamable HTTP**
transports only. A canonical configuration is:

```json
{
  "mcpServers": {
    "demo-stdio": {
      "command": "python3",
      "args": ["server.py"],
      "env": { "DEMO": "1" },
      "cwd": "."
    },
    "demo-http": {
      "url": "http://127.0.0.1:9443/mcp",
      "headers": { "Authorization": "Bearer …" },
      "transport": "streamable-http"
    }
  }
}
```

Only descriptors from the turn's frozen `PluginSnapshot` of **trusted and
enabled** plugins are materialized. Untrusted project plugins never start MCP
child processes or HTTP client sessions. Command/`cwd` paths must stay inside
the plugin root; reserved identity environment variables cannot be forged by
server config.

By default the model sees only progressive discovery tools — `search_tool` and
`use_tool` — plus ordinary non-MCP builtins. `search_tool` retrieves qualified
`server__tool` names from the generation-scoped schema cache; `use_tool`
invokes one discovered tool. Optional direct expansion of a small allowlisted
server set into model tool definitions is opt-in and still passes the same
ToolRuntime membrane. Colliding qualified names do not silently overwrite.

Every MCP invocation is a ToolRegistry provider call: PreToolUse →
`prepare_scoped` → PolicyEngine → approval → execute → PostToolUse. There is no
second execution channel around `McpManager`. Streamable HTTP POSTs send
`Accept: application/json, text/event-stream`, advertise
`MCP-Protocol-Version: 2025-03-26` (and then the negotiated version), echo
`Mcp-Session-Id`, and read a JSON body or an SSE JSON-RPC result without waiting
for the stream to end. After initialize the client opens a GET SSE listener
(`405` is ignored; `ping` is answered; other server requests are rejected;
stream EOF reconnects with `Last-Event-ID`).
Shutdown `DELETE`s a session when one was assigned. A session `404` starts a
new session (initialize without `Mcp-Session-Id`) and retries the RPC once;
a second failure marks the server unhealthy. Endpoints are checked after DNS resolution (SSRF), never
follow redirects, allow plain `http` only on loopback, and redact URL
credentials from user-visible errors and journals. Oversized results are
truncated inline and spilled under `.lato/tool-output/`.

Stdio servers run in a fresh process group. Cancel, timeout, crash isolation,
and SessionEnd shut them down under a bounded deadline with process-tree reap.
A single unhealthy server does not take down peer MCP servers or builtin tools.
Plugin reload is generation-paired: in-flight turns keep generation N MCP
resources; the next turn adopts N+1 and retires the previous manager. Child
sessions may only **narrow** parent MCP server/tool allowlists (or drop
`ExtensionInvoke`); they cannot restore a parent-disabled plugin or a
previously removed tool.

Installed-command MCP smoke must use the cargo-installed binary
(`cargo install --path .` → typically `~/.cargo/bin/lato`). Prefer
`LATO_SMOKE_BINARY="$HOME/.cargo/bin/lato"` (or
`LATO_SMOKE_BINARY="$(command -v lato)"` only after confirming PATH order). An
older `~/.local/bin/lato` may shadow PATH and exercise a stale build.

### Plugin Workflows

Trusted, enabled plugins may declare workflows in `plugin.json` (`workflows`
inline map or a `workflows.json` path under the plugin root). Qualified ids are
`plugin/workflow`. Phase 7A **materializes descriptors only**: `lato` will not
run them, list them in `doctor`, or spawn agents. `agentBudget` is a declared
cap (default 128, max 1024). Execution belongs to a later phase. Untrusted
project plugins contribute no workflow descriptors. Child sessions may only
narrow the parent allowlist.

## Doctor

```bash
lato doctor
lato doctor --json
lato doctor --strict
lato doctor --live
```

Default `lato doctor` is offline: it does not contact providers or submit a prompt completion. It reports binary/platform, Lato home, config parsing, selected-model catalog presence, credential presence (not values), ToolCatalog construction, a fixed PolicyEngine self-test, sandbox readiness, and project/plugin trust. `--json` prints a `schema_version: 1` report on stdout. Warnings keep exit status 0; errors return 1. `--strict` upgrades warnings to failure. `--live` is the only Doctor mode allowed to use the network; it runs a bounded catalog/connectivity probe and does not submit an ordinary prompt completion.

## Headless CLI smoke test

The default phase fixture is offline and does not require a credential:

```bash
LATO_HOME=$(mktemp -d) lato -p "reply with hi only"
# hi
```

The Phase 5 release gate also contains a command-level task smoke that drives the
real `lato` executable through a parent `spawn` → child worker → `inspect` → `wait`
chain, validates the persisted journal, and checks that temporary worktrees are
cleaned up. Run it against the just-installed binary with:

```bash
cargo install --path .
# Prefer ~/.cargo/bin/lato; an older ~/.local/bin/lato may shadow PATH.
LATO_SMOKE_BINARY="${LATO_SMOKE_BINARY:-$HOME/.cargo/bin/lato}" \
  cargo test --test phase5_command_smoke -- --nocapture
# Phase 6C MCP smoke (same binary pin):
# LATO_SMOKE_BINARY="$LATO_SMOKE_BINARY" \
#   cargo test --test phase6c_mcp_smoke -- --nocapture
```

Use a real catalog model by selecting `provider/model`. Credentials resolve in this order: runtime override, persisted OAuth, persisted API key, then provider environment variables.

Built-in China-region providers include:

| Provider | Protocol/base URL | Credential |
|---|---|---|
| `minimax-cn` | Anthropic Messages, `https://api.minimaxi.com/anthropic` | `MINIMAX_CN_API_KEY` |
| `minimax` | Anthropic Messages, `https://api.minimax.io/anthropic` | `MINIMAX_API_KEY` |
| `zai-coding-cn` | OpenAI Completions, `https://open.bigmodel.cn/api/coding/paas/v4` | `ZAI_CODING_CN_API_KEY` |
| `zai` | OpenAI Completions, `https://api.z.ai/api/coding/paas/v4` | `ZAI_API_KEY` |
| `sensenova` | OpenAI-compatible, `https://token.sensenova.cn/v1` | `SENSENOVA_API_KEY` bearer token |

The first four definitions are Rust translations of the reference TypeScript provider factories. Their static catalog is overlaid by the reference-compatible remote catalog endpoint `/api/models/providers/{provider}` and persisted per provider in `~/.lato/models-store.json` with `checked_at`, `last_modified`, and `etag`; fresh cached catalogs are restored without network access. Lato does not blindly append `/models` to these providers. SenseNova is not defined by the reference registry and remains an explicit compatibility provider based on its OpenAI SDK configuration. Lato sends the supplied token to `https://token.sensenova.cn/v1`, discovers models from `/models`, and samples from `/chat/completions`. This token API key is distinct from the legacy platform Access Key ID/Secret pair.

```bash
export LATO_HOME="$HOME/.lato"
lato login openai --api-key "$OPENAI_API_KEY"
lato -p --model openai/gpt-4.1 --sandbox workspace "inspect this repository and fix the tests"
```

Custom OpenAI-compatible endpoints are read from `$LATO_HOME/models.json`:

```json
{
  "models": [
    {
      "provider": "local",
      "id": "qwen",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:8080/v1",
      "env": "LOCAL_API_KEY"
    }
  ]
}
```

## Build and test from source

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo install --path .
```

Windows compile gate from macOS/Linux with the Rust target installed:

```bash
cargo check --workspace --target x86_64-pc-windows-gnu
```

## ACP stdio

```bash
lato acp
```

Each input and output is one JSON-RPC object per line. `session/load` is intentionally unsupported; use `session/resume`.

## OAuth

Only `kimi-coding` and `openai-codex` advertise OAuth. Other providers reject OAuth rather than silently changing credential channels.

```bash
lato login kimi-coding --oauth
lato login openai-codex --oauth
lato login openai-codex --oauth --device-auth
lato -p --model openai-codex/codex-mini-latest "reply with hi only"
```

`openai-codex` uses ChatGPT subscription authentication, not an OpenAI Platform API key. Browser login is the default: Lato listens once on loopback port `1455` for the OAuth callback. Use `--device-auth` on a headless machine and enter the displayed code at the verification URL. A ChatGPT plan with Codex access is required.

Lato owns this login independently. It stores the access token, refresh token, expiry, and ChatGPT account identity in `$LATO_HOME/auth.json` (normally `~/.lato/auth.json`) and refreshes them itself. It does not read or modify Pi's or the Codex CLI's credential files.

Model calls use `https://chatgpt.com/backend-api/codex/responses`, prefer a session-affine WebSocket, and reuse healthy connections. If WebSocket setup fails before any response event, Lato falls back to an incrementally parsed SSE request with a zstd-compressed body. It never replays a request after model output has begun, preventing duplicated text or tool calls.

## Public Beta limitations

- Homebrew, Scoop, and other package-manager channels are not maintained yet.
- LIVE provider validation is reported separately and may be unavailable when repository credentials are not configured. Offline protocol and localhost end-to-end tests still run in ordinary CI.
- `~/.lato` may contain prompts, paths, tool arguments, tool results, credentials, and policy metadata. Treat the entire directory as sensitive local data.

## Runtime architecture

Lato's headless, interactive, and stdio clients share the ACP host. ACP sessions submit typed
commands to `lato-runtime` and consume versioned events from it. `lato-core` owns provider-independent
IDs, errors, commands, events, and session invariants; `lato-runtime` owns cancellation, steering,
replacement, event ordering, and the `TurnDriver` boundary.

The existing model/tool loop currently runs behind `LegacyTurnDriver`, a compatibility adapter around
`SessionActor`. This keeps provider, tool, approval, and transcript behavior stable while Phase 2
moves those capabilities behind dedicated ports.

Phase 2A adds provider-independent `ModelPort` and `Tool` contracts in `lato-core`, plus a layered,
fail-closed `ToolCatalog` in `lato-tools`. Phase 2B-1 now routes every configured CLI and ACP HTTP
provider through `ModelPort`. A bidirectional adapter keeps `SessionActor` on its legacy JSON/channel
contract temporarily, while the existing HTTP authentication, request builders, retry policy, and SSE
parsers remain the protocol authority.

`ModelPort` is the canonical provider extension boundary. New providers should implement it directly;
`LegacyModelPort` and `ModelPortStreamAdapter` exist only to migrate the current providers and actor
independently. A later actor-native phase removes the redundant legacy encode/decode round trip without
changing the core request, event, capability, cancellation, or typed-error contracts.

### Canonical session journal

Sessions backed by `LATO_HOME` persist canonical records at
`$LATO_HOME/sessions/<session-id>/events.jsonl`. The journal contains accepted turn input, complete
conversation items, policy decisions, tool lifecycle boundaries, and turn/session terminals. Model
and reasoning deltas remain live-only events: they are never replayed as authoritative state.

Journal appends are acknowledged before related visible state is broadcast. A mutating or
non-idempotent tool is synchronously recorded as prepared before invocation and synchronously
recorded as completed afterward. If recovery finds a prepared call without a completion, Lato
reports `journal.incomplete_side_effect` (an unknown outcome) and does not automatically invoke the
tool again.

Replay is bounded to 64 MiB and 100,000 records per session. It validates schema, session identity,
dense sequence numbers, unique record IDs, and tool request hashes. Only an interrupted final JSONL
record is repaired; a complete malformed or structurally divergent record fails closed. On Unix,
journal files use mode `0600`, session directories use `0700`, and symlink journal targets are
rejected. Journals may contain prompts, tool arguments/results, paths, and policy metadata, so the
entire Lato home should be treated as sensitive local data.

Legacy `$LATO_HOME/sessions/<session-id>.jsonl` transcripts are imported lazily and atomically on
first resume. The original transcript remains unchanged for compatibility; once both formats exist,
the canonical journal wins.

Phase 4B follows Grok Build's source-stream/derived-history split. The append-only
`events.jsonl` remains the authority, while `history.jsonl` and `history.meta.json`
materialize the current model-visible conversation. Ordinary records reach the canonical
journal before the derived projection. Missing or stale projection files are rebuilt after
the complete journal passes validation; damaged projection files are quarantined before
rebuild. Logical history replacement publishes a private compaction checkpoint before its
marker is synchronized to the canonical journal. A missing or mismatched referenced
checkpoint, canonical corruption, or an unresolved side effect still fails closed. Phase 4B
does not rotate, truncate, archive, or delete canonical journal records, and the Phase 4A
64 MiB/100,000-record bounds remain in force.

Phase 4C1 adds Grok Build-style manual context compaction on top of that storage
boundary. `CompactionRequested` is durable before sampling becomes visible. The
summary request uses the current model with `tools: []` and `tool_choice: none`, has a
three-attempt ceiling, and must satisfy section, size, and reduction checks. A successful
checkpoint marker becomes authoritative even if publishing `history.jsonl` or its
metadata fails; replay rebuilds those derived files before the new history is installed
in memory. If reconciliation cannot prove which checkpoint is authoritative, the
session fails closed.

Phase 4C2/4C3 add provider-informed context accounting and bounded automatic recovery.
Every provider boundary is checked before sampling; speculative prefire output is
ephemeral and never becomes a transcript or journal record. Only a validated final
summary is installed through the same checkpoint-before-marker protocol, so restart
recovery sees either the old logical history or the committed compacted history, never
an intermediate two-pass note.

Copied or structurally derived upstream code is pinned in
[`docs/superpowers/reference/lato-upstream-sources.md`](docs/superpowers/reference/lato-upstream-sources.md).

## Security

- Headless `-p` uses process-local workspace trust and `approval_mode=always`; deny rules still apply.
- Interactive sessions default to `ask` and mutating tools require an explicit approval token.
- Approvals are bound to an exact-call fingerprint (canonical tool identity, arguments, capabilities, side effect, sandbox obligation, and call IDs). Grants are short-lived, single-use, and cannot be replayed.
- Shell sandbox profiles are `off`, `workspace`, and `read-only`. A missing or unusable wrapper fails closed and never degrades into unsandboxed process execution.
- File reads/edits execute in the host; only shell commands enter the OS sandbox.
- Project plugin hooks and MCP processes remain disabled until the project is trusted.
- `web_fetch` rejects loopback, private, link-local, and non-HTTP(S) destinations.
- Doctor output is redacted; credential values and seeded secrets must not appear in human or JSON reports.

See `docs/superpowers/specs/2026-08-31-lato-acceptance.md` for the authoritative acceptance matrix. LIVE provider and subscription tests require real credentials and are not run in PR CI.
