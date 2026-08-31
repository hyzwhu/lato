# Lato

Lato is a Rust coding-agent harness with a shared ACP host for headless, stdio, and future interactive clients.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Windows compile gate from macOS/Linux with the Rust target installed:

```bash
cargo check --workspace --target x86_64-pc-windows-gnu
```

## Interactive CLI

Install once, then run `lato` without arguments:

```bash
cargo install --path .
lato
```

On first launch Lato asks you to choose a model, then configures OAuth or securely reads an API key without echoing it. The selected model is saved in `~/.lato/config.json`; credentials are kept separately in the locked credential store. After setup, the same process maintains a multi-turn conversation and renders HTTP/SSE model deltas as they arrive.

Interactive commands:

```text
/help     /clear     /model     /approve     /status     /exit
```

The line editor persists history in `~/.lato/history`; Up/Down navigate it and Tab completes slash commands. The workspace starts untrusted. Mutating tool calls display their name and arguments and request approval exactly at the execution boundary. You can instead trust the workspace for the process or use `/approve` to pre-authorize one call.

## Headless CLI smoke test

The default phase fixture is offline and does not require a credential:

```bash
LATO_HOME=$(mktemp -d) cargo run -q -- -p "reply with hi only"
# hi
```

Use a real catalog model by selecting `provider/model`. Credentials resolve in this order: runtime override, persisted OAuth, persisted API key, then provider environment variables.

Built-in China-region OpenAI-compatible providers include:

| Provider | Default model | Credential |
|---|---|---|
| `minimax-cn` | `MiniMax-M2.1` | `MINIMAX_API_KEY` |
| `zhipu` | `glm-4.5` | `ZHIPU_API_KEY` (also accepts `BIGMODEL_API_KEY`/`ZAI_API_KEY`) |
| `sensenova` | `SenseNova-V6-5-Pro` | `SENSENOVA_API_KEY` bearer token |

They also appear in first-launch model selection. SenseNova accounts that expose only AK/SK must generate the platform bearer/JWT token first and supply that token; Lato does not store the secret pair or silently invent a signing scheme.

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

## ACP stdio

```bash
cargo run -q -- acp
```

Each input and output is one JSON-RPC object per line. `session/load` is intentionally unsupported; use `session/resume`.

## OAuth

Only `kimi-coding` and `openai-codex` advertise OAuth. Other providers reject OAuth rather than silently changing credential channels.

```bash
cargo run -q -- login kimi-coding --oauth
cargo run -q -- login openai-codex --oauth
```

## Security

- Headless `-p` uses process-local workspace trust and `approval_mode=always`; deny rules still apply.
- Interactive sessions default to `ask` and mutating tools require an explicit approval token.
- Shell sandbox profiles are `off`, `workspace`, and `read-only`. A missing wrapper fails closed.
- File reads/edits execute in the host; only shell commands enter the OS sandbox.
- Project plugin hooks and MCP processes remain disabled until the project is trusted.
- `web_fetch` rejects loopback, private, link-local, and non-HTTP(S) destinations.

See `docs/superpowers/specs/2026-08-31-lato-acceptance.md` for the authoritative acceptance matrix. LIVE provider and subscription tests require real credentials and are not run in PR CI.
