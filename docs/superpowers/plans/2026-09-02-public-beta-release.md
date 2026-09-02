# Lato Public Beta Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a self-service Lato Public Beta with release-grade CLI parsing, session listing and resume, accurate offline Doctor checks, five verified release archives, optional LIVE smoke evidence, and user-first documentation.

**Architecture:** Add a typed `clap` boundary in front of the existing handlers, route persisted-session operations through `AcpHost`, and factor interactive client initialization into new/resume paths without bypassing journal recovery. Keep ordinary CI and release builds offline; isolate real-provider checks in a manually triggered workflow.

**Tech Stack:** Rust 2024, Tokio, clap 4 derive, serde/serde_json, existing ACP host and canonical event store, GitHub Actions, GitHub CLI.

## Global Constraints

- Preserve `lato`, `lato -p ...`, `lato login`, `lato doctor`, and `lato acp` behavior and exit codes.
- Set the root binary package version to `0.1.0-beta.1`; release tag, archive names, and `lato --version` must agree with it.
- `lato sessions` and `lato resume` must use ACP methods; CLI code must not parse journal JSONL directly.
- Resume must fail closed for missing, corrupt, or unresolved-side-effect sessions and must never create a replacement session.
- Ordinary CI and release workflows must not receive real-provider credentials or make provider requests.
- Release output is five archives: macOS Intel, macOS Apple Silicon, Linux x86-64, Linux ARM64, and Windows x86-64, plus one `SHA256SUMS` file.
- Successful JSON commands write only JSON to stdout; diagnostics go to stderr.
- TUI, package-manager distribution, session mutation, and session summaries remain out of scope.
- Preserve user-owned untracked files, especially `AGENTS.md`.

---

## File Map

- Create `src/args.rs`: typed public CLI grammar and parse-to-invocation mapping.
- Create `src/sessions.rs`: ACP-backed session listing and stable output rendering.
- Modify `src/main.rs`: register new modules and continue calling `cli::run`.
- Modify `src/cli.rs`: dispatch typed invocations and parameterize interactive startup.
- Modify `src/client.rs`: shared host initialization, create/resume constructors, and ACP session listing.
- Modify `crates/lato-agent/src/host.rs`: reject resume for sessions absent from active memory and persisted stores.
- Modify `tests/cli_headless.rs`: command, session-listing, and failure-path integration coverage.
- Modify `tests/doctor_cli.rs`: model-source resolution and redaction coverage.
- Modify `src/doctor.rs`: offline model resolution aligned with runtime startup.
- Create `.github/workflows/release.yml`: native five-target build, smoke, archive, checksum, and publish.
- Create `.github/workflows/live-smoke.yml`: manual secret-aware provider smoke and summary.
- Modify `README.md`: Beta positioning, binary install, Quick Start, sessions, diagnosis, and limits.
- Modify `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`: record the final Beta gate evidence.

### Task 1: Typed CLI grammar and compatibility

**Files:**
- Create: `src/args.rs`
- Modify: `src/main.rs`
- Modify: `src/cli.rs`
- Modify: `Cargo.toml`
- Test: `src/args.rs`
- Test: `tests/cli_headless.rs`

**Interfaces:**
- Produces: `args::Invocation`, `args::PromptArgs`, `args::DoctorArgs`, and `args::LoginMethod`.
- Consumes: `cli::run(Vec<String>)` remains the binary entry point.

- [ ] **Step 1: Add failing CLI contract tests**

Add integration tests that invoke the compiled binary and assert generated help and version behavior:

```rust
#[test]
fn public_beta_help_lists_session_commands_and_headless_compatibility() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for required in ["sessions", "resume", "doctor", "login", "acp", "-p"] {
        assert!(help.contains(required), "missing {required}: {help}");
    }
}

#[test]
fn public_beta_version_matches_cargo_package() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("lato ", env!("CARGO_PKG_VERSION"))
    );
}
```

- [ ] **Step 2: Run the new tests and verify failure**

Run: `cargo test --test cli_headless public_beta_ -- --nocapture`

Expected: FAIL because current help omits `sessions` and `resume`, and `--version` is not implemented.

- [ ] **Step 3: Add clap and define the typed invocation boundary**

Set the root package version to `0.1.0-beta.1` and add `clap = { version = "4", features = ["derive"] }` to root dependencies. Define parser types in `src/args.rs` and expose a parse function that never terminates the process itself:

```rust
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SandboxArg { Off, Workspace, ReadOnly }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptArgs {
    pub text: String,
    pub ask: bool,
    pub sandbox: SandboxArg,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginMethod { ApiKey(String), Oauth }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorArgs { pub json: bool, pub strict: bool, pub live: bool }

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invocation {
    InteractiveNew,
    Prompt(PromptArgs),
    Sessions { json: bool },
    Resume { session_id: String },
    Login { provider: String, method: LoginMethod },
    Doctor(DoctorArgs),
    Acp,
}

pub fn parse(args: Vec<String>) -> Result<Invocation, clap::Error> {
    let cli = Cli::try_parse_from(std::iter::once("lato".to_string()).chain(args))?;
    cli.into_invocation()
}
```

Use this private grammar and convert it to the public `Invocation` types with an `into_invocation` method:

```rust
#[derive(Debug, Parser)]
#[command(name = "lato", version, about = "Public Beta coding agent")]
struct Cli {
    #[arg(short = 'p', action = ArgAction::SetTrue)]
    prompt: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    ask: bool,
    #[arg(long, value_enum, default_value_t = SandboxArg::Off)]
    sandbox: SandboxArg,
    #[arg(long)]
    model: Option<String>,
    #[arg(value_name = "TEXT", num_args = 0..)]
    text: Vec<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Sessions { #[arg(long, action = ArgAction::SetTrue)] json: bool },
    Resume { session_id: String },
    Login {
        provider: String,
        #[arg(long, conflicts_with = "oauth")]
        api_key: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        oauth: bool,
    },
    Doctor {
        #[arg(long, action = ArgAction::SetTrue)] json: bool,
        #[arg(long, action = ArgAction::SetTrue)] strict: bool,
        #[arg(long, action = ArgAction::SetTrue)] live: bool,
    },
    Acp,
}
```

`into_invocation` joins all headless text operands with spaces, rejects empty `-p`, rejects `--ask`, `--sandbox`, `--model`, or free text without `-p`, makes login require exactly one method, and returns `clap::Error::raw(ErrorKind::MissingRequiredArgument | ErrorKind::ArgumentConflict, ...)` for semantic validation. `ArgAction::SetTrue` rejects duplicate occurrences. Subcommands conflict with all headless fields.

- [ ] **Step 4: Route parse results without changing handlers**

Register `mod args; mod sessions;` in `src/main.rs`. Replace string-position dispatch in `cli::run` with:

```rust
pub async fn run(args: Vec<String>) -> i32 {
    match crate::args::parse(args) {
        Ok(Invocation::InteractiveNew) => interactive(InteractiveStartup::New).await,
        Ok(Invocation::Prompt(args)) => prompt(args).await,
        Ok(Invocation::Sessions { json }) => crate::sessions::list(json).await,
        Ok(Invocation::Resume { session_id }) => {
            interactive(InteractiveStartup::Resume(session_id)).await
        }
        Ok(Invocation::Login { provider, method }) => login(provider, method).await,
        Ok(Invocation::Doctor(args)) => doctor_cmd(args).await,
        Ok(Invocation::Acp) => crate::stdio::run().await,
        Err(error) => {
            let code = if error.use_stderr() { 2 } else { 0 };
            let _ = error.print();
            code
        }
    }
}
```

Change the handler signatures to `prompt(PromptArgs)`, `doctor_cmd(DoctorArgs)`, and `login(String, LoginMethod)`. Convert `SandboxArg` to `SandboxProfile` in `prompt`; pass the typed booleans directly to Doctor; match `LoginMethod` to reuse the existing API-key and OAuth branches. Preserve secure interactive key entry and the existing credential store.

- [ ] **Step 5: Add parser unit coverage for invalid combinations**

Cover missing prompt text, `--ask` without `-p`, missing resume ID, duplicate Doctor flags, both login methods, and the existing `-p --model ... --sandbox ... TEXT` ordering. Assertions must use `clap::error::ErrorKind` rather than matching rendered English text.

- [ ] **Step 6: Run focused tests**

Run: `cargo test --bin lato && cargo test --test cli_headless && cargo test --test doctor_cli`

Expected: all tests pass; existing `-p`, Doctor, login, and ACP tests remain green.

- [ ] **Step 7: Commit the typed CLI boundary**

```bash
git add Cargo.toml Cargo.lock src/args.rs src/main.rs src/cli.rs tests/cli_headless.rs tests/doctor_cli.rs
git commit -m "feat: add public beta CLI surface"
```

### Task 2: ACP-backed session listing and fail-closed resume

**Files:**
- Create: `src/sessions.rs`
- Modify: `src/client.rs`
- Modify: `src/cli.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Test: `src/client.rs`
- Test: `crates/lato-agent/src/host.rs`
- Test: `tests/cli_headless.rs`

**Interfaces:**
- Consumes: `Invocation::Sessions`, `Invocation::Resume`, `AcpHost::handle`, and `InteractiveStartup`.
- Produces: `client::list_sessions_over_acp(...)`, `InteractiveAcpClient::new_session_with_approval(...)`, and `InteractiveAcpClient::resume_session_with_approval(...)`.

- [ ] **Step 1: Add failing host tests for unknown resume**

Extend host tests so a never-persisted ID is rejected and does not appear in a subsequent list:

```rust
#[tokio::test]
async fn resume_rejects_unknown_session_without_creating_it() {
    let mut host = host();
    let response = host.handle(req(
        1,
        "session/resume",
        serde_json::json!({"sessionId": "s1700000000000-404"}),
    )).await.unwrap();
    assert_eq!(response["error"]["code"], -32000);
    assert_eq!(response["error"]["message"], "unknown session");

    let listed = host.handle(req(2, "session/list", serde_json::json!({})))
        .await.unwrap();
    assert!(!listed["result"]["sessions"].as_array().unwrap()
        .iter().any(|id| id == "s1700000000000-404"));
}
```

- [ ] **Step 2: Verify the resume test fails**

Run: `cargo test -p lato-agent resume_rejects_unknown_session_without_creating_it -- --nocapture`

Expected: FAIL because `session/resume` currently constructs an empty session for an unknown ID.

- [ ] **Step 3: Enforce persisted existence in AcpHost**

Add a private async helper that checks active sessions, legacy transcript IDs, and canonical event-store IDs, then guard `session/resume` before import/replay:

```rust
async fn session_exists(&self, sid: &str) -> bool {
    if self.sessions.contains_key(sid) { return true; }
    if self.transcripts.as_ref().and_then(|store| store.list().ok())
        .is_some_and(|ids| ids.iter().any(|id| id == sid)) {
        return true;
    }
    if let Some(store) = &self.events
        && let Ok(ids) = store.list_sessions().await
        && ids.iter().any(|id| id.as_str() == sid) {
        return true;
    }
    false
}
```

Return JSON-RPC `-32000 / unknown session` before invoking migration or creating a runtime session when the helper is false.

- [ ] **Step 4: Add failing client tests for create, list, and resume response handling**

Tests must prove that constructors send `session/new` or `session/resume`, propagate ACP errors, and preserve the requested resumed ID. Add a read-only accessor used only for assertions:

```rust
pub(crate) fn session_id(&self) -> &str { &self.session_id }
```

- [ ] **Step 5: Factor shared client initialization**

Implement a private start enum and one initializer:

```rust
enum SessionStart { New, Resume(String) }

async fn initialize_with_approval(
    cwd: PathBuf,
    trust: SessionTrust,
    stream: Arc<dyn ModelStream>,
    approval: Option<Arc<dyn ToolApproval>>,
    start: SessionStart,
) -> Result<Self, String>
```

The initializer sends `initialize`, then either `session/new` or `session/resume`; it must check the JSON-RPC `error` member before extracting `result.sessionId`. Keep `new_with_approval` as a compatibility wrapper if existing tests or callers still use it.

Expose the exact wrappers:

```rust
pub async fn new_session_with_approval(
    cwd: PathBuf,
    trust: SessionTrust,
    stream: Arc<dyn ModelStream>,
    approval: Option<Arc<dyn ToolApproval>>,
) -> Result<Self, String>

pub async fn resume_session_with_approval(
    cwd: PathBuf,
    trust: SessionTrust,
    stream: Arc<dyn ModelStream>,
    approval: Option<Arc<dyn ToolApproval>>,
    session_id: String,
) -> Result<Self, String>
```

The existing `new_with_approval` delegates to `new_session_with_approval` until all internal callers have migrated.

Add `list_sessions_over_acp(cwd: PathBuf) -> Result<Vec<String>, String>` using an `AcpHost` with `default_fake_stream()` and process-local headless trust. It sends `initialize` and `session/list`, validates the response, and returns IDs without inspecting disk.

- [ ] **Step 6: Implement stable session rendering**

Create `src/sessions.rs`:

```rust
#[derive(serde::Serialize)]
struct SessionListOutput {
    schema_version: u32,
    sessions: Vec<String>,
}

pub async fn list(json: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match crate::client::list_sessions_over_acp(cwd).await {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| b.cmp(a));
            if json {
                println!("{}", serde_json::to_string_pretty(&SessionListOutput {
                    schema_version: 1,
                    sessions,
                }).expect("session list serialization is infallible"));
            } else if sessions.is_empty() {
                println!("No saved sessions.");
            } else {
                for session in sessions { println!("{session}"); }
            }
            0
        }
        Err(error) => { eprintln!("error: {error}"); 1 }
    }
}
```

- [ ] **Step 7: Parameterize the interactive startup**

Define:

```rust
enum InteractiveStartup { New, Resume(String) }
```

Perform the TTY check for both variants before model configuration. After model and trust setup, call the matching client constructor. Print `Resumed session: <id>` only after ACP confirms success. Keep `/clear` as a new-session operation.

- [ ] **Step 8: Add CLI session integration tests**

Use temporary `LATO_HOME` fixtures created through ACP or existing journal APIs. Assert empty human output, descending human IDs, JSON schema/order, non-TTY resume exit 2, unknown resume failure without a new journal directory, and valid ACP resume history hydration. Do not handcraft malformed journal records except in the existing journal-recovery test helpers.

- [ ] **Step 9: Run focused and workspace session tests**

Run:

```text
cargo test -p lato-agent resume -- --nocapture
cargo test --test cli_headless session -- --nocapture
cargo test -p lato-store
```

Expected: all pass, including legacy migration and incomplete-side-effect refusal coverage.

- [ ] **Step 10: Commit session UX**

```bash
git add src/sessions.rs src/client.rs src/cli.rs crates/lato-agent/src/host.rs tests/cli_headless.rs
git commit -m "feat: expose persisted sessions in CLI"
```

### Task 3: Align Doctor with runtime model resolution

**Files:**
- Modify: `src/doctor.rs`
- Test: `tests/doctor_cli.rs`

**Interfaces:**
- Consumes: `lookup_model`, `load_models_json`, and `ProviderModelsStore`.
- Produces: offline `model` Doctor check with source-aware messages and existing `DoctorReport` schema version 1.

- [ ] **Step 1: Add failing source-resolution tests**

Create one fixture per supported source. For example, a custom model fixture must assert:

```rust
#[tokio::test]
async fn doctor_recognizes_models_json_selection_offline() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.json"),
        r#"{"default_model":"local/qwen"}"#).unwrap();
    std::fs::write(home.path().join("models.json"), r#"{
        "models":[{"provider":"local","id":"qwen","api":"openai-completions",
        "base_url":"http://127.0.0.1:8080/v1","env":"LOCAL_KEY"}]
    }"#).unwrap();
    let deps = DoctorDependencies {
        home: home.path().to_path_buf(),
        workspace: workspace.path().to_path_buf(),
        live_probe: CountingProbe::panic_if_called(),
    };
    let report = run(DoctorOptions { live: false }, &deps).await;
    let model = report.checks.iter().find(|check| check.id == "model").unwrap();
    assert_eq!(model.status, DoctorStatus::Ok);
    assert!(model.message.contains("models.json"));
}
```

Add `doctor_recognizes_provider_store_selection_offline`, `doctor_recognizes_compatibility_cache_selection_offline`, `doctor_reports_malformed_models_json_separately`, and `doctor_model_source_messages_remain_redacted`. Build provider-store data through `ProviderModelsStore::open(...).write(...)` using a `ProviderModelsEntry`; write compatibility cache using the same `{"models":[...]}` fixture shape already used by `discovered_provider_model_cache_runs_with_persisted_provider_credential`.

- [ ] **Step 2: Verify focused failures**

Run: `cargo test --test doctor_cli doctor_ -- --nocapture`

Expected: source-resolution cases fail because `model_check` currently checks only the built-in catalog.

- [ ] **Step 3: Implement source-aware offline resolution**

Replace `model_check(configured)` with `model_check(home, configured)`. Use a small internal enum:

```rust
enum ModelSource { BuiltIn, ModelsJson, ProviderStore, CompatibilityCache }

impl ModelSource {
    fn label(&self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in catalog",
            Self::ModelsJson => "models.json",
            Self::ProviderStore => "models-store.json",
            Self::CompatibilityCache => "model-cache.json",
        }
    }
}
```

Check the built-in catalog first. For each optional file, skip `NotFound`, surface parse/read failures as `DoctorStatus::Error` with `doctor.check_failed`, and find an exact provider/model match. Use `ProviderModelsStore::open(home).read(provider)` for the provider cache rather than duplicating its wire format. Do not resolve credentials or make HTTP requests.

- [ ] **Step 4: Run Doctor regression tests**

Run: `cargo test --test doctor_cli -- --nocapture`

Expected: all Doctor tests pass; the no-live probe count remains zero and seeded secrets remain absent.

- [ ] **Step 5: Verify the reported real local configuration**

Run: `cargo run --quiet -- doctor`

Expected: the configured `sensenova/glm-5.2` reports `ok` from a local model source rather than the previous built-in-catalog warning. Other unrelated warnings are allowed.

- [ ] **Step 6: Commit Doctor resolution**

```bash
git add src/doctor.rs tests/doctor_cli.rs
git commit -m "fix: align doctor model discovery with runtime"
```

### Task 4: Five-target GitHub Release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `v*` tags and manual `workflow_dispatch`.
- Produces: five archives and `SHA256SUMS`; publishes only for tag events.

- [ ] **Step 1: Add the native runner matrix**

Use currently documented standard runner labels and native targets:

```yaml
strategy:
  fail-fast: false
  matrix:
    include:
      - runner: macos-15-intel
        target: x86_64-apple-darwin
        archive: tar.gz
      - runner: macos-15
        target: aarch64-apple-darwin
        archive: tar.gz
      - runner: ubuntu-24.04
        target: x86_64-unknown-linux-gnu
        archive: tar.gz
      - runner: ubuntu-24.04-arm
        target: aarch64-unknown-linux-gnu
        archive: tar.gz
      - runner: windows-2025
        target: x86_64-pc-windows-msvc
        archive: zip
```

Reference: GitHub's official hosted-runner table documents these architectures and labels: `https://docs.github.com/en/actions/reference/runners/github-hosted-runners`.

- [ ] **Step 2: Build and smoke each native binary**

The workflow checks out source, installs stable Rust with the matrix target, caches Cargo, reads `package.version` from `cargo metadata --no-deps`, and, for tag events, rejects a tag other than `v${VERSION}`. It then runs `cargo build --release --locked --target $TARGET` and the produced executable through:

```text
lato --version
LATO_HOME=<temporary-directory> lato doctor --json
LATO_HOME=<temporary-directory> lato -p "reply with hi only"
```

Parse Doctor JSON and assert `schema_version == 1`; assert headless stdout is exactly `hi`. In Bash use `mktemp -d` plus a short `python3 -c` JSON assertion. In PowerShell use `$temp = New-Item -ItemType Directory ...`, `ConvertFrom-Json`, and explicit `if (...) { throw ... }` assertions. No build job receives `contents: write` or repository secrets.

- [ ] **Step 3: Package deterministic asset contents**

Name assets `lato-${VERSION}-${TARGET}.tar.gz` or `.zip`. Stage exactly the executable, `README.md`, `LICENSE`, and `NOTICE` under a single same-named directory. Upload each archive with `actions/upload-artifact`.

- [ ] **Step 4: Add checksum and publish job**

Download all matrix artifacts, generate a sorted `SHA256SUMS`, and assert exactly five archive entries before publishing. Grant only this job:

```yaml
permissions:
  contents: write
```

For `refs/tags/v*`, publish with:

```bash
gh release create "$GITHUB_REF_NAME" --verify-tag --generate-notes \
  artifacts/*.tar.gz artifacts/*.zip artifacts/SHA256SUMS
```

Manual dispatch uploads workflow artifacts but skips `gh release create`.

- [ ] **Step 5: Validate workflow syntax and review permissions**

Run `ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml")'` and inspect `git diff --check`. Confirm build jobs have `contents: read`, publish has `contents: write`, and no `secrets.*` expression exists.

- [ ] **Step 6: Commit release automation**

```bash
git add .github/workflows/release.yml
git commit -m "ci: build verified public beta releases"
```

### Task 5: Manual LIVE provider smoke workflow

**Files:**
- Create: `.github/workflows/live-smoke.yml`

**Interfaces:**
- Consumes: `workflow_dispatch` and provider-specific encrypted secrets.
- Produces: a job summary with explicit passed, failed, and skipped states.

- [ ] **Step 1: Define manual-only inputs and least privilege**

Start with:

```yaml
name: live-provider-smoke
on:
  workflow_dispatch:
    inputs:
      timeout_seconds:
        description: Per-provider timeout
        required: true
        default: "60"
permissions:
  contents: read
```

Use this explicit matrix, covering both OpenAI Responses and Anthropic Messages protocol families:

```yaml
strategy:
  fail-fast: false
  matrix:
    include:
      - provider: openai
        model: openai/gpt-4.1
        credential_env: OPENAI_API_KEY
        secret_name: OPENAI_API_KEY
      - provider: minimax
        model: minimax/MiniMax-M2.1
        credential_env: MINIMAX_API_KEY
        secret_name: MINIMAX_API_KEY
env:
  PROVIDER_SECRET: ${{ secrets[matrix.secret_name] }}
```

Never pass API keys in arguments. Export `PROVIDER_SECRET` only under the matrix entry's `credential_env` name immediately before executing Lato.

- [ ] **Step 2: Implement secret-aware execution**

For each provider, write `skipped` to its result artifact when `PROVIDER_SECRET` is empty. Otherwise install/build Lato, use a temporary `LATO_HOME`, set the exact credential environment variable, and run:

```bash
lato -p --model "$MODEL" "Reply with exactly LATO_LIVE_OK"
```

Assert trimmed stdout equals `LATO_LIVE_OK`; write `passed` or `failed` without printing the secret. Ensure shell tracing is disabled.

- [ ] **Step 3: Aggregate the workflow summary**

Use an `if: always()` summary job that downloads all status artifacts and writes a Markdown table to `$GITHUB_STEP_SUMMARY`. Missing credentials must render `skipped`; failures remain visible and make the workflow fail after the summary is written.

- [ ] **Step 4: Validate isolation from ordinary CI**

Search workflows and assert `live-smoke.yml` is the only new file containing provider secret names, and that it has no `pull_request`, `push`, `schedule`, or `workflow_call` trigger.

- [ ] **Step 5: Commit LIVE smoke automation**

```bash
git add .github/workflows/live-smoke.yml
git commit -m "ci: add manual live provider smoke"
```

### Task 6: User-first Beta documentation

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: final CLI and artifact names.
- Produces: self-service installation and first-run documentation without unsupported claims.

- [ ] **Step 1: Rewrite the README opening and installation path**

Replace the opening with: `Lato is a Public Beta coding agent for terminal-based development workflows.` Put supported platforms, warning, binary download, checksum verification, and PATH installation before source build instructions. Use these literal asset patterns from the release workflow:

```bash
gh release download v0.1.0-beta.1 \
  --pattern 'lato-v0.1.0-beta.1-aarch64-apple-darwin.tar.gz' \
  --pattern SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing
```

Include PowerShell extraction and `Get-FileHash -Algorithm SHA256` instructions for Windows. Do not include `curl | sh`.

- [ ] **Step 2: Add Quick Start and session recovery**

Document:

```text
lato
lato sessions
lato sessions --json
lato resume <SESSION_ID>
lato doctor
```

Explain that resume uses the current default model and current working directory, and that it fails closed for unsafe or corrupt journals.

- [ ] **Step 3: Add Beta limitations and evidence language**

State that Lato is a Public Beta, TUI and package-manager channels are not included, LIVE provider validation may be unavailable, and the local journal may contain prompts, paths, tool arguments, and tool results. Retain architecture, security, provider, custom endpoint, OAuth, and contributor material after the user journey.

- [ ] **Step 4: Check documentation against executable help**

Run `cargo run --quiet -- --help` and `cargo run --quiet -- sessions --help`; compare every documented flag and command. Search README for stale phrases `future interactive clients` and `cargo install --path .` in the primary installation section; source-build instructions may retain the latter.

- [ ] **Step 5: Commit Beta documentation**

```bash
git add README.md
git commit -m "docs: add public beta installation guide"
```

### Task 7: Full validation, local deployment, and acceptance record

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`

**Interfaces:**
- Consumes: all prior tasks.
- Produces: locally installed `lato` and dated, reproducible Beta gate evidence.

- [ ] **Step 1: Run formatting and full repository gates**

```text
cargo fmt --all -- --check
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: every command exits 0 with no failed tests or warnings.

- [ ] **Step 2: Run fresh-home source smokes**

Use a newly created temporary directory, without overwriting user state:

```text
LATO_HOME=<fresh-temp> cargo run --quiet -- --version
LATO_HOME=<fresh-temp> cargo run --quiet -- doctor --json
LATO_HOME=<fresh-temp> cargo run --quiet -- -p "reply with hi only"
LATO_HOME=<fresh-temp> cargo run --quiet -- sessions --json
```

Assert Doctor and sessions JSON parse, both report `schema_version: 1`, and headless stdout is exactly `hi`.

- [ ] **Step 3: Deploy locally as required by AGENTS.md**

Run: `cargo install --path .`

Expected: Cargo replaces or installs the local `lato` executable successfully.

- [ ] **Step 4: Smoke the installed binary**

Repeat `lato --version`, fresh-home `lato doctor --json`, `lato -p`, and `lato sessions --json` using the installed executable. Run the existing ACP fixture or a focused resume integration test to prove installed code and source tests agree.

- [ ] **Step 5: Record exact Beta gate evidence**

Append a dated `Public Beta release gate` section to the acceptance results. Record exact test counts, format/clippy status, installed binary path and version, source/installed smoke results, session list/resume results, release workflow validation, and LIVE status. Use `SKIPPED` for any unexecuted remote workflow or missing credential; never record it as passed.

- [ ] **Step 6: Re-run documentation and diff hygiene checks**

```text
git diff --check
rg -n "TBD|TODO|LIVE.*PASS" README.md docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
git status --short
```

Expected: no whitespace errors, no placeholders or unsupported LIVE claims, and `AGENTS.md` remains untracked and untouched.

- [ ] **Step 7: Commit the acceptance record**

```bash
git add docs/superpowers/specs/2026-08-31-lato-acceptance-results.md
git commit -m "docs: record public beta release gate"
```

- [ ] **Step 8: Final review**

Inspect the complete branch diff and verify every requirement in `docs/superpowers/specs/2026-09-02-public-beta-release-design.md` maps to a passing test, workflow assertion, or explicit documented limitation. Do not create or push a release tag without separate user authorization.
