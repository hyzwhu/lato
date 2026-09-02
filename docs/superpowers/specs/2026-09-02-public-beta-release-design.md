# Lato Public Beta Release Design

## Status

- Date: 2026-09-02
- Target: GitHub Public Beta for developers who discover Lato without prior project context
- Distribution: prebuilt GitHub Release artifacts for macOS, Linux, and Windows
- UI scope: line-oriented interactive CLI; TUI is explicitly deferred

## Goal

Turn the existing tested Lato runtime into a self-service Public Beta that an unfamiliar developer can download, configure, use, diagnose, and resume without building the repository. The release must preserve the existing headless and ACP interfaces while exposing persisted sessions through the CLI and attaching reproducible evidence to each release.

## Non-goals

This slice does not add a TUI, a graphical installer, Homebrew, Scoop, session deletion, session renaming, generated session summaries, or a curl-piped installer. It does not redesign the model/tool runtime, ACP protocol, journal format, approval policy, or sandbox implementation.

## User-facing command contract

Lato uses `clap` for release-grade argument parsing and generated help while retaining the current command handlers and runtime boundaries.

```text
lato
lato -p [--ask] [--sandbox off|workspace|read-only] [--model provider/model] TEXT
lato sessions [--json]
lato resume <SESSION_ID>
lato login PROVIDER (--api-key KEY|--oauth)
lato doctor [--json] [--strict] [--live]
lato acp
lato --help
lato --version
```

Running `lato` without arguments creates a new interactive session. Existing `-p` invocations retain their current behavior and exit-code contract. `clap` usage and validation errors return 2. Configuration, journal, model, and runtime failures return 1.

`lato sessions` obtains session IDs through the existing ACP `session/list` method and prints them newest-first. Since the current persistence contract exposes IDs rather than descriptive metadata, the human format remains intentionally minimal. An empty store prints `No saved sessions.` and exits successfully.

`lato sessions --json` writes only machine-readable data to stdout using this versioned shape:

```json
{
  "schema_version": 1,
  "sessions": ["s1788336000000-1"]
}
```

Errors go to stderr so successful JSON output is safe to pipe. The command does not contact model providers.

`lato resume <SESSION_ID>` enters the existing interactive experience using the configured default model, current working directory, and the same folder-trust prompt and approval policy as a new session. Initialization calls ACP `session/resume` instead of `session/new`. A missing session, invalid journal, or unresolved prepared side effect fails closed. Resume never falls back to creating a new session.

Interactive slash commands do not gain `/resume` in this slice. Session switching inside a live interactive client would introduce lifecycle and approval-state questions that are not needed for the Beta release.

## Code boundaries

The change is a focused vertical slice rather than a full CLI rewrite.

- `src/args.rs` owns `clap` types, help text, version output, and argument-level validation.
- `src/sessions.rs` owns the CLI adapters for ACP session listing and resumption plus human and JSON rendering.
- `src/cli.rs` retains model selection, authentication, trust prompting, and the interactive event loop. Its interactive entry accepts an explicit new-or-resume startup mode.
- `src/client.rs` provides explicit constructors for creating and resuming an `InteractiveAcpClient`. Both constructors share host initialization and differ only in the ACP session method and response validation.
- `src/doctor.rs` resolves the selected model from the same offline sources used by normal startup.
- `src/main.rs` remains a thin async entry point.

No CLI code reads journal files directly. Listing and resumption continue through `AcpHost`, preserving migration, validation, fail-closed recovery, and runtime hydration behavior.

## Doctor model resolution

The offline Doctor check recognizes a configured model from these sources, in startup precedence order where applicable:

1. the built-in catalog;
2. `$LATO_HOME/models.json`;
3. the provider entries in `$LATO_HOME/models-store.json`;
4. the compatibility cache in `$LATO_HOME/model-cache.json`.

An identified model reports `ok` and names its source without printing credentials, headers, model responses, or file contents. A model absent from every valid source reports a warning with an actionable instruction to run `lato`, select the model again, or repair the named configuration file.

Malformed source files are reported separately from an unknown model. Doctor remains offline unless `--live` is explicitly supplied. Existing human and `schema_version: 1` JSON contracts remain compatible; new detail text may identify the model source.

## Release artifacts

`.github/workflows/release.yml` runs for `v*` tags and supports a manual dry run that builds artifacts without publishing a GitHub Release. Native GitHub-hosted runners build these targets:

| Platform | Rust target | Archive |
|---|---|---|
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `.zip` |

Each archive contains the `lato` executable, `README.md`, `LICENSE`, and `NOTICE`. Asset names include the version and Rust target. Every build runs the produced binary through `--version`, offline `doctor --json`, and the credential-free fake-model headless smoke before upload.

A final release job downloads the five verified artifacts, generates a single `SHA256SUMS`, and publishes them with GitHub CLI using the repository token. Publishing requires every matrix entry to succeed. The workflow grants `contents: write` only to the publishing job; build jobs retain read-only repository permissions. Real provider credentials are unavailable to this workflow.

## LIVE provider smoke

`.github/workflows/live-smoke.yml` is `workflow_dispatch` only. Provider jobs receive separate encrypted repository secrets and execute a bounded, no-tool prompt with a deterministic marker in the expected answer. Each request has a timeout.

A provider without its required secret is recorded as `skipped`, not passed. The workflow summary lists passed, failed, and skipped providers. Secret values are never echoed, passed as CLI arguments, or included in uploaded artifacts.

Normal pull-request CI and the release workflow remain offline. Beta release notes link the most recent LIVE workflow result when one exists. If no usable result exists, the notes state `LIVE provider validation unavailable`; they do not imply successful live verification.

## Documentation and positioning

The README opening describes Lato as a Beta coding agent rather than only as a harness or a future interactive client. Its first sections become:

1. Beta status and supported platforms;
2. binary installation with checksum verification;
3. Quick Start covering first-run model configuration;
4. sessions and resume;
5. Doctor and troubleshooting;
6. known Beta limitations and sensitive local data;
7. source build and contributor information;
8. architecture and security details.

Documentation must not claim that LIVE providers, OAuth subscriptions, or a platform passed unless the corresponding release evidence exists. The canonical journal location and its potentially sensitive contents remain clearly documented.

## Error and safety behavior

- Invalid arguments and unknown commands use generated `clap` diagnostics and exit 2.
- `sessions --json` never mixes successful JSON with progress or warnings on stdout.
- Resume validates the requested session through ACP and never silently creates a replacement.
- Journal corruption and unresolved side effects preserve their existing structured errors.
- Doctor redaction continues to cover stored credentials, environment secrets, bearer headers, and seeded test secrets.
- Release jobs do not receive provider secrets. LIVE jobs receive only the secret required by their provider.
- Archives contain no local configuration, credential store, model cache, session journal, or build-machine paths.

## Verification

Implementation follows test-first changes at each boundary.

### CLI tests

- generated top-level help lists all public commands and `-p` compatibility;
- `--version` includes the Cargo package version;
- malformed flags and missing command operands return 2;
- `sessions` renders newest-first IDs and the explicit empty state;
- `sessions --json` emits the versioned schema and clean stdout;
- a valid persisted session is resumed through ACP and retains conversation history;
- an unknown, malformed, or incomplete-side-effect session fails without creating a session;
- non-TTY use of interactive new and resume modes fails with an actionable message.

### Doctor tests

- built-in, `models.json`, provider-store, and compatibility-cache models are recognized offline;
- malformed model files are distinguished from unknown models;
- output remains redacted in human and JSON modes;
- Doctor does not make a network call without `--live`.

### Repository gates

The release candidate must pass:

```text
cargo fmt --all -- --check
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo install --path .
```

The installed binary then runs `lato --version`, a fresh-home `lato doctor --json`, a fake-model `lato -p` smoke, and session listing/resume smoke coverage. Repository CI continues to test macOS, Linux, and Windows.

The tag workflow must produce all five archives plus `SHA256SUMS`; every extracted binary must pass its platform smoke before the GitHub Release is published.

## Release decision

The repository is ready to tag a Public Beta only when the repository gates, installed-binary smokes, and all five release artifact builds pass. Missing LIVE credentials do not block the Beta, but their skipped status must be visible in the release evidence. TUI work begins only after Beta users demonstrate that long conversations, tool activity, diffs, approvals, or session navigation are materially constrained by the line-oriented interface.
