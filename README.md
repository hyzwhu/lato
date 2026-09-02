# Lato

Lato is a Public Beta coding agent for terminal-based development workflows. It provides an interactive coding CLI, a headless prompt mode, and an ACP host backed by the same runtime, tool policy, and durable session journal.

> **Beta software:** interfaces and stored formats are versioned and tested, but may still change before a stable release. Back up important work and review tool approvals carefully.

Prebuilt binaries are available for macOS Intel, macOS Apple Silicon, Linux x86-64, Linux ARM64, and Windows x86-64.

## Install a prebuilt binary

Download the archive for your platform and `SHA256SUMS` from the [v0.1.0-beta.1 release](../../releases/tag/v0.1.0-beta.1). Verify the archive before extracting it.

macOS example for Apple Silicon:

```bash
shasum -a 256 -c SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.1-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.1-aarch64-apple-darwin/lato "$HOME/.local/bin/lato"
```

Linux x86-64 example:

```bash
sha256sum --check SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.1-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.1-x86_64-unknown-linux-gnu/lato "$HOME/.local/bin/lato"
```

Ensure `$HOME/.local/bin` is on `PATH` before running Lato.

Windows PowerShell example:

```powershell
$archive = "lato-0.1.0-beta.1-x86_64-pc-windows-msvc.zip"
Get-FileHash $archive -Algorithm SHA256
# Compare the printed hash with this archive's entry in SHA256SUMS.
Expand-Archive $archive -DestinationPath .
New-Item -ItemType Directory -Force "$HOME\bin" | Out-Null
Copy-Item ".\lato-0.1.0-beta.1-x86_64-pc-windows-msvc\lato.exe" "$HOME\bin\lato.exe"
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

On first launch Lato asks you to choose a model, then configures OAuth or securely reads an API key without echoing it. The selected model is saved in `~/.lato/config.json`; credentials are kept separately in the locked credential store. After setup, Lato opens a full-screen Ratatui interface with session navigation, streaming reasoning and responses, live tool-call cards, and approval dialogs.

The interface follows `LC_ALL`, `LC_MESSAGES`, or `LANG` on first launch and supports Chinese and English. Override and persist the language with either form below; `/lang` switches it from inside the TUI.

```bash
lato --lang zh-CN
lato --lang en
```

Interactive commands:

```text
/help     /clear     /model     /login     /lang     /approve     /status     /exit
```

Use Tab/Shift-Tab to move between panels, Up/Down to scroll the focused panel, Cmd/Ctrl-K to open the command palette, `/` on an empty composer to search, and Ctrl-C to cancel a streaming turn or exit while idle. `/model` and `/login` temporarily leave the full-screen view for secure terminal prompts, then return in a fresh conversation. The workspace starts untrusted. Mutating tool calls display their name and arguments and request approval exactly at the execution boundary. You can instead trust the workspace for the process or use `/approve` to pre-authorize one call. On smaller terminals, side panels collapse automatically so the conversation remains usable.

For a one-shot, script-friendly prompt:

```bash
lato -p --model openai/gpt-4.1 --sandbox workspace "inspect this repository and report failing tests"
```

## Sessions and resume

Lato persists accepted turns and complete conversation state under `~/.lato/sessions`. List saved sessions in human or stable JSON form, then resume one interactively:

```bash
lato sessions
lato sessions --json
lato resume s1788336000000-1
```

Resume uses the currently configured default model and current working directory. It repeats the folder-trust prompt and fails closed instead of creating a replacement when a journal is missing, corrupt, or contains an unresolved side-effect outcome.

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
```

## Public Beta limitations

- Session entries currently use durable IDs instead of generated titles, and selecting a historical session from the side panel is not yet supported; use `lato resume SESSION_ID`.
- Homebrew, Scoop, and other package-manager channels are not maintained yet.
- Session listing exposes durable IDs, not generated titles, rename, or delete operations.
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
the canonical journal wins. Snapshotting and journal compaction are deferred to Phase 4B.

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
