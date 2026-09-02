# Lato HTTP Tool Loop Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the current HTTP model tool loop so streamed text is lossless, provider options are faithful, malformed or repeated tool calls fail safely, local shortcuts do not steal general prompts, and the installed `lato` binary passes the release gate.

**Architecture:** Keep `SessionActor`, `ModelStream`, and `ToolRuntime` as the existing boundaries. Separate incremental text extraction from whole-response tool-call assembly, translate canonical request options at each provider builder, and make actor termination explicit without introducing a new state-machine layer.

**Tech Stack:** Rust 2024, Tokio, Reqwest, Serde JSON, Cargo workspace tests, Clippy.

## Global Constraints

- Do not replace `SessionActor` with a new state-machine architecture.
- Do not add providers, tools, or interactive commands.
- Do not change the policy engine, approval fingerprint contract, or sandbox model.
- Do not run live vendor tests without credentials.
- Preserve but do not commit the untracked `AGENTS.md` file.
- Run relevant tests before `cargo install --path .`.
- OpenAI Responses `tool_choice` uses `auto` or `required`; Anthropic's equivalent uses `{"type":"auto"}` or `{"type":"any"}`.

---

## File Map

- `crates/lato-ai/src/stream.rs`: lossless byte buffering, incremental text extraction, whole-response tool-call parsing, malformed-argument rejection, and stream fixtures.
- `crates/lato-ai/src/api.rs`: provider-native translation of canonical `tool_choice` and `stream` fields plus request-shape tests.
- `crates/lato-agent/src/actor.rs`: bounded forced-tool retry, repeated-call termination, mutation-intent matching, and actor tests.
- `src/cli.rs`: narrow local-fact recognition and successful-discovery cache persistence.
- `tests/cli_headless.rs`: end-to-end positive and negative CLI fixtures.
- Existing edits in `crates/lato-ai/src/models_file.rs`, `crates/lato-tools/src/dispatch.rs`, `crates/lato-tools/src/edit.rs`, and `crates/lato-tools/src/registry.rs` remain covered by focused tests; change them only if a focused failure proves necessary.

### Task 1: Make streaming decoding lossless and tool arguments fail closed

**Files:**
- Modify: `crates/lato-ai/src/stream.rs`

**Interfaces:**
- Produces: `parse_stream_body(body: &str) -> Result<Vec<StreamPiece>, String>`.
- Produces: `parse_stream_text_line(line: &str) -> Vec<StreamPiece>`.
- Produces: `parse_tool_arguments(raw: &str, call_id: &str) -> Result<serde_json::Value, String>`.
- Consumes: unchanged `StreamPiece::{Text, ToolCall}` and `HttpRequestSpec`.

- [ ] **Step 1: Add failing malformed-argument and split-UTF-8 tests**

Add to `crates/lato-ai/src/stream.rs`:

```rust
#[test]
fn malformed_structured_tool_arguments_are_rejected() {
    let body = r#"data: {"choices":[{"message":{"tool_calls":[{"id":"call-bad","type":"function","function":{"name":"write_file","arguments":"{"}}]},"finish_reason":"tool_calls"}]}"#;
    let error = parse_stream_body(body).unwrap_err();
    assert_eq!(error, "invalid tool arguments for call-bad");
}

#[tokio::test]
async fn http_stream_reassembles_utf8_split_across_network_chunks() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n".as_bytes();
        let split = event.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
        for part in [&event[..split], &event[split..]] {
            socket.write_all(format!("{:x}\r\n", part.len()).as_bytes()).await.unwrap();
            socket.write_all(part).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let request = HttpRequestSpec {
        method: "POST",
        url: format!("http://{address}"),
        headers: vec![],
        body: serde_json::json!({}),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    stream_http_request(&reqwest::Client::new(), &request, tx).await.unwrap();
    assert_eq!(rx.recv().await, Some(StreamPiece::Text("你".into())));
    server.await.unwrap();
}
```

- [ ] **Step 2: Run the tests and verify failure**

```bash
cargo test -p lato-ai malformed_structured_tool_arguments_are_rejected -- --nocapture
cargo test -p lato-ai http_stream_reassembles_utf8_split_across_network_chunks -- --nocapture
```

Expected: the old parser cannot produce the requested error, and the old per-chunk lossy decoder does not preserve `你`.

- [ ] **Step 3: Add fallible whole-body parsing and text-only line parsing**

Replace the argument helper and add the text helper:

```rust
fn parse_tool_arguments(raw: &str, call_id: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|_| format!("invalid tool arguments for {call_id}"))
}

fn parse_stream_text_line(line: &str) -> Vec<StreamPiece> {
    let raw = line.trim();
    let data = raw.strip_prefix("data:").map(str::trim).unwrap_or(raw);
    if data.is_empty() || data == "[DONE]" {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let event_type = value.get("type").and_then(|item| item.as_str()).unwrap_or("");
    let text = json_str_non_empty(&value, "/choices/0/delta/content")
        .or_else(|| json_str_non_empty(&value, "/choices/0/message/content"))
        .or_else(|| json_str_non_empty(&value, "/delta/text"))
        .or_else(|| (event_type == "response.output_text.delta").then(|| value.get("delta").and_then(|item| item.as_str())).flatten())
        .or_else(|| value.get("text").and_then(|item| item.as_str()));
    text.map(|text| vec![StreamPiece::Text(text.into())]).unwrap_or_default()
}
```

Change `drain_complete_tool_calls` to return `Result<(), String>` and assemble each call with:

```rust
if let Some(call) = pending_tools.remove(&key) {
    let call_id = if call.id.is_empty() { key } else { call.id };
    let arguments = parse_tool_arguments(&call.arguments, &call_id)?;
    pieces.push(StreamPiece::ToolCall {
        id: call_id,
        name: call.name,
        arguments,
    });
}
```

Change `parse_stream_body` to return `Result<Vec<StreamPiece>, String>`, use `?` at every drain/argument-parse site, and finish with `Ok(pieces)`. Update parser tests with `.unwrap()` and embedded XML/JSON parsing with `parse_tool_arguments(raw, "embedded").ok()?`.

- [ ] **Step 4: Buffer bytes until a complete line before decoding**

Replace `stream_http_request` body handling with:

```rust
let mut body = Vec::<u8>::new();
let mut line_start = 0usize;
while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
    body.extend_from_slice(&chunk);
    while let Some(relative_end) = body[line_start..].iter().position(|byte| *byte == b'\n') {
        let line_end = line_start + relative_end;
        let line = std::str::from_utf8(&body[line_start..line_end])
            .map_err(|_| "model response was not valid UTF-8".to_string())?;
        for piece in parse_stream_text_line(line) {
            tx.send(piece).await.map_err(|_| "stream receiver closed".to_string())?;
        }
        line_start = line_end + 1;
    }
}
let body = std::str::from_utf8(&body)
    .map_err(|_| "model response was not valid UTF-8".to_string())?;
if line_start < body.len() {
    for piece in parse_stream_text_line(&body[line_start..]) {
        tx.send(piece).await.map_err(|_| "stream receiver closed".to_string())?;
    }
}
for piece in parse_stream_body(body)? {
    if matches!(piece, StreamPiece::ToolCall { .. }) {
        tx.send(piece).await.map_err(|_| "stream receiver closed".to_string())?;
    }
}
```

- [ ] **Step 5: Run and commit the stream slice**

```bash
cargo test -p lato-ai stream::tests -- --nocapture
git add crates/lato-ai/src/stream.rs
git commit -m "fix: harden HTTP stream decoding"
```

Expected: all stream tests pass before the commit.

### Task 2: Preserve provider-native tool-choice and streaming options

**Files:**
- Modify: `crates/lato-ai/src/api.rs`
- Modify: `crates/lato-ai/src/stream.rs`

**Interfaces:**
- Produces: `anthropic_tool_choice(choice: Option<&serde_json::Value>) -> Option<serde_json::Value>`.
- Consumes: canonical `tool_choice: "auto" | "required"` and `stream: bool`.

- [ ] **Step 1: Add failing request-shape and fallback tests**

Add this complete request-shape test to `crates/lato-ai/src/api.rs`:

```rust
#[test]
fn responses_and_anthropic_honor_required_tool_choice_and_non_stream_mode() {
    let context = serde_json::json!({
        "messages":[{"role":"user","content":"write a file"}],
        "tools":[{"type":"function","function":{"name":"write_file","parameters":{"type":"object"}}}],
        "tool_choice":"required",
        "stream":false
    });
    let responses = build_request(
        &lookup_model("openai", "gpt-4.1").unwrap(),
        &auth(),
        context.clone(),
    )
    .unwrap();
    assert_eq!(responses.body["tool_choice"], "required");
    assert_eq!(responses.body["stream"], false);

    let anthropic = build_request(
        &lookup_model("kimi-coding", "kimi-k2").unwrap(),
        &auth(),
        context,
    )
    .unwrap();
    assert_eq!(anthropic.body["tool_choice"], serde_json::json!({"type":"any"}));
    assert_eq!(anthropic.body["stream"], false);
}
```

Add to `crates/lato-ai/src/stream.rs`:

```rust
#[test]
fn unrelated_http_400_does_not_trigger_tool_choice_fallback() {
    let request = HttpRequestSpec {
        method: "POST",
        url: "http://example.invalid".into(),
        headers: vec![],
        body: serde_json::json!({"tool_choice":"required"}),
    };
    assert!(!tool_choice_required_rejected("http 400: invalid model", &request));
    assert!(tool_choice_required_rejected("http 400: unsupported tool_choice required", &request));
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

```bash
cargo test -p lato-ai responses_and_anthropic_honor_required_tool_choice_and_non_stream_mode -- --nocapture
cargo test -p lato-ai unrelated_http_400_does_not_trigger_tool_choice_fallback -- --nocapture
```

Expected: bodies omit/hardcode fields and unrelated HTTP 400 is classified as fallback-worthy.

- [ ] **Step 3: Translate canonical options**

Derive once in `build_request`:

```rust
let tool_choice = context.get("tool_choice").cloned();
let stream = context.get("stream").and_then(|value| value.as_bool()).unwrap_or(true);
```

Build Responses/Azure bodies mutably and insert `tool_choice` only when translated tools are non-empty. Add:

```rust
fn anthropic_tool_choice(choice: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    match choice.and_then(|value| value.as_str()) {
        Some("required") => Some(serde_json::json!({"type":"any"})),
        Some("auto") => Some(serde_json::json!({"type":"auto"})),
        Some("none") => Some(serde_json::json!({"type":"none"})),
        _ => None,
    }
}
```

Set Anthropic `stream` from the canonical value and insert the translated choice only when tools exist.

- [ ] **Step 4: Narrow fallback classification**

```rust
fn tool_choice_required_rejected(error: &str, request: &crate::HttpRequestSpec) -> bool {
    let lower = error.to_ascii_lowercase();
    request.body.get("tool_choice").and_then(|value| value.as_str()) == Some("required")
        && lower.contains("http 400")
        && (lower.contains("tool_choice") || lower.contains("tool choice"))
}
```

- [ ] **Step 5: Run and commit provider fidelity**

```bash
cargo test -p lato-ai api::tests -- --nocapture
cargo test -p lato-ai stream::tests -- --nocapture
git add crates/lato-ai/src/api.rs crates/lato-ai/src/stream.rs
git commit -m "fix: preserve provider tool request options"
```

Expected: all API/stream fixtures pass before the commit.

### Task 3: Make actor retry and repeated-call termination explicit

**Files:**
- Modify: `crates/lato-agent/src/actor.rs`

**Interfaces:**
- Consumes: `ToolRuntime::prepare`, `decision`, `approve`, and `execute` unchanged.
- Produces: stable error `stalled: identical tool call repeated more than 3 times`.
- Produces: `task_requires_workspace_change(text: &str) -> bool` based on action terms rather than bare nouns.

- [ ] **Step 1: Replace repeated-call expectations and add intent coverage**

Change the existing repeated-call test to expect `unwrap_err()` and add:

```rust
#[test]
fn workspace_change_detection_requires_an_action() {
    assert!(task_requires_workspace_change("create a file named hello.txt"));
    assert!(task_requires_workspace_change("修改 src/main.rs"));
    assert!(!task_requires_workspace_change("explain this file format"));
    assert!(!task_requires_workspace_change("what is a program?"));
}
```

The repeated-call assertions must be:

```rust
let error = actor
    .prompt(PromptKind::Headless, "write the file".into())
    .await
    .unwrap_err();
assert_eq!(error, "stalled: identical tool call repeated more than 3 times");
assert!(!actor.latest_assistant_text().contains("上次工具结果"));
```

- [ ] **Step 2: Run the tests and verify failure**

```bash
cargo test -p lato-agent repeated_identical_tool_calls -- --nocapture
cargo test -p lato-agent workspace_change_detection_requires_an_action -- --nocapture
```

Expected: old behavior returns synthetic completion and treats bare `file`/`program` as mutation intent.

- [ ] **Step 3: Remove synthetic completion from the repetition guard**

Keep only `repeated_calls: HashMap<String, usize>`, remove `completed_tool_outputs`, remove `ProcessTool::Complete`, and use:

```rust
let repeats = repeated_calls.entry(fingerprint).or_default();
*repeats += 1;
if *repeats > 3 {
    self.active = false;
    return Err("stalled: identical tool call repeated more than 3 times".into());
}
```

Update both `process_tool_call` call sites to handle only `Executed` and `Cancelled`. Do not change the policy/runtime authorization path.

- [ ] **Step 4: Narrow mutation-intent matching**

```rust
fn task_requires_workspace_change(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "写入", "写个", "写一个", "创建", "新建", "修改", "编辑", "保存到", "生成文件",
        "write ", "create ", "modify ", "edit ", "save ", "add a file", "update ",
    ]
    .iter()
    .any(|term| lower.contains(term))
}
```

- [ ] **Step 5: Run and commit actor hardening**

```bash
cargo test -p lato-agent actor::tests -- --nocapture
git add crates/lato-agent/src/actor.rs
git commit -m "fix: bound actor tool retry termination"
```

Expected: actor tests pass; a mutation prompt still gets one forced-tool retry; repeated calls return the stable error.

### Task 4: Narrow local CLI facts without losing explicit queries

**Files:**
- Modify: `src/cli.rs`
- Modify: `tests/cli_headless.rs`

**Interfaces:**
- Produces: `requested_local_facts(input: &str) -> Vec<LocalFact>` recognizing explicit current facts only.
- Preserves: `local_fact_response(input, cwd, model) -> Option<String>`.
- Preserves: cache writes only after provider discovery returns `Ok`.

- [ ] **Step 1: Add negative and positive matcher tests**

Add to `src/cli.rs` tests:

```rust
#[test]
fn local_fact_matcher_only_intercepts_explicit_current_facts() {
    assert_eq!(requested_local_facts("pwd"), vec![LocalFact::CurrentDirectory]);
    assert_eq!(requested_local_facts("你是什么模型"), vec![LocalFact::Model]);
    assert!(requested_local_facts("which model architecture should I use?").is_empty());
    assert!(requested_local_facts("show me how path handling works").is_empty());
    assert!(requested_local_facts("解释这个 workspace 文件").is_empty());
}
```

Add to `tests/cli_headless.rs`:

```rust
#[test]
fn general_model_question_is_not_stolen_by_local_fact_shortcut() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["-p", "which model architecture should I use?"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}
```

- [ ] **Step 2: Run tests and verify the broad-model false positive**

```bash
cargo test local_fact_matcher_only_intercepts_explicit_current_facts -- --nocapture
cargo test --test cli_headless general_model_question_is_not_stolen_by_local_fact_shortcut -- --nocapture
```

Expected: the old matcher intercepts the general architecture question.

- [ ] **Step 3: Replace broad term matching with explicit patterns**

```rust
fn requested_local_facts(input: &str) -> Vec<LocalFact> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "pwd" {
        return vec![LocalFact::CurrentDirectory];
    }
    let asks_current_model = [
        "当前模型", "现在的模型", "你是什么模型", "你当前是什么模型", "你现在是什么模型",
        "current model", "which model are you", "what model are you",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_current_directory = [
        "当前目录", "当前文件夹", "当前工作目录", "当前工作区路径",
        "current directory", "current folder", "working directory",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_grandparent = [
        "上上层目录", "上两层目录", "祖父目录", "grandparent directory", "two levels up",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let asks_parent = asks_grandparent
        || ["上一层目录", "上一级目录", "父目录", "parent directory", "one level up"]
            .iter()
            .any(|term| lower.contains(term));
    let mut facts = Vec::new();
    if asks_current_model {
        facts.push(LocalFact::Model);
    }
    if asks_current_directory {
        facts.push(LocalFact::CurrentDirectory);
    }
    if asks_parent {
        facts.push(LocalFact::AncestorDirectory(if asks_grandparent { 2 } else { 1 }));
    }
    facts
}
```

- [ ] **Step 4: Verify explicit facts and cache behavior**

```bash
cargo test --test cli_headless headless_answers_ -- --nocapture
cargo test --test cli_headless general_model_question_is_not_stolen_by_local_fact_shortcut -- --nocapture
cargo test --test cli_headless discovered_provider_model_cache_runs_with_persisted_provider_credential -- --nocapture
```

Expected: explicit directory/model tests pass, the general question reaches the fake model, and successful discovery remains cached.

- [ ] **Step 5: Commit CLI shortcut hardening**

```bash
git add src/cli.rs tests/cli_headless.rs
git commit -m "fix: narrow local CLI fact matching"
```

### Task 5: Revalidate aliases, cache, and the offline tool loop

**Files:**
- Verify; modify only on focused failure: `crates/lato-tools/src/dispatch.rs`
- Verify; modify only on focused failure: `crates/lato-tools/src/edit.rs`
- Verify; modify only on focused failure: `crates/lato-tools/src/registry.rs`
- Verify; modify only on focused failure: `crates/lato-ai/src/models_file.rs`
- Verify: `tests/cli_headless.rs`

**Interfaces:**
- Preserves `write`/`write_file`, `content`/`contents`, `old`/`oldText`, and `new`/`newText` compatibility.
- Preserves `FileLocks`, policy preparation, exact approval, and cache persistence.

- [ ] **Step 1: Run focused compatibility tests**

```bash
cargo test -p lato-tools write_file_creates_hello_world_and_denies_env -- --nocapture
cargo test -p lato-tools a3_2_parallel_edit_spellings_serialized -- --nocapture
cargo test -p lato-tools compat_write_file_creates_missing_parent_directories -- --nocapture
cargo test -p lato-tools write_alias_consumes_allow_once_exactly_once -- --nocapture
cargo test --test cli_headless b1_6_headless_http_model_tool_loop_edits_workspace_offline_fixture -- --nocapture
cargo test --test cli_headless discovered_provider_model_cache_runs_with_persisted_provider_credential -- --nocapture
```

Expected: all named tests pass. If one fails, make the smallest correction in the listed compatibility file and rerun the same test before continuing.

- [ ] **Step 2: Confirm diff scope**

```bash
git status --short
git diff --check
git diff --name-only HEAD | sort
```

Expected: `AGENTS.md` remains untracked; `git diff --check` is empty; the feature diff contains only scoped Rust, test, and plan files.

- [ ] **Step 3: Commit a compatibility correction only if Step 1 required one**

```bash
git add crates/lato-ai/src/models_file.rs crates/lato-tools/src/dispatch.rs crates/lato-tools/src/edit.rs crates/lato-tools/src/registry.rs tests/cli_headless.rs
git commit -m "fix: preserve HTTP tool compatibility aliases"
```

Skip this commit if no new correction was required. Existing uncommitted compatibility work is consolidated in Task 6.

### Task 6: Run the release gate, deploy locally, and consolidate the feature

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md`
- Commit: all remaining scoped implementation/test changes.
- Exclude: `AGENTS.md`.

**Interfaces:**
- Produces: installed `lato` binary built from this repository.
- Produces: dated measured acceptance evidence.

- [ ] **Step 1: Format and run complete tests**

```bash
cargo fmt --all -- --check
cargo test --workspace --no-fail-fast
```

Expected: format exits 0 with no output; all workspace and doc tests pass with zero failures.

- [ ] **Step 2: Run Clippy as an error gate**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: exit 0 with no warnings.

- [ ] **Step 3: Install the current repository build**

```bash
cargo install --path .
```

Expected: Cargo replaces or installs the `lato` executable successfully.

- [ ] **Step 4: Run installed-binary offline smokes**

```bash
lato_smoke_home=$(mktemp -d)
LATO_HOME="$lato_smoke_home" lato doctor
LATO_HOME="$lato_smoke_home" lato doctor --json
LATO_HOME="$lato_smoke_home" lato -p "reply with hi only"
LATO_HOME="$lato_smoke_home" lato -p "pwd"
```

Expected: Doctor exits 0 with warning status for an empty home; JSON contains `"schema_version":1`; fake prompt prints `hi`; `pwd` prints the current working directory.

- [ ] **Step 5: Record measured results**

Append a dated subsection to `docs/superpowers/specs/2026-08-31-lato-acceptance-results.md` with the exact test count, Clippy result, install result, and smoke results from Steps 1–4. Do not claim live provider coverage.

- [ ] **Step 6: Commit all remaining scoped feature changes**

```bash
git add crates/lato-agent/src/actor.rs crates/lato-ai/src/api.rs crates/lato-ai/src/models_file.rs crates/lato-ai/src/stream.rs crates/lato-tools/src/dispatch.rs crates/lato-tools/src/edit.rs crates/lato-tools/src/registry.rs src/cli.rs tests/cli_headless.rs docs/superpowers/specs/2026-08-31-lato-acceptance-results.md docs/superpowers/plans/2026-09-02-lato-http-tool-loop-hardening.md
git diff --cached --check
git commit -m "feat: complete HTTP model tool loop"
```

Expected: commit succeeds and `AGENTS.md` is not staged.

- [ ] **Step 7: Verify final repository and binary state**

```bash
git status --short --branch
git log -8 --oneline --decorate
command -v lato
lato --help >/dev/null
```

Expected: only `?? AGENTS.md` remains outside commits; recent history contains design, focused hardening, and final feature commits; installed `lato` resolves and its help exits 0.
