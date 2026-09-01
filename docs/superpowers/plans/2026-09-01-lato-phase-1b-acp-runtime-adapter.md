# Lato Phase 1B ACP Runtime Adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route the existing ACP, headless, and interactive clients through `lato-runtime` while retaining `SessionActor` as a compatibility turn driver and preserving existing observable behavior.

**Architecture:** Add a `LegacyTurnDriver` adapter around the current `SessionActor`, then add a `RuntimeSession` facade that submits typed commands and translates typed events back to the existing ACP update surface. Replace `AcpHost`'s `HashMap<String, SessionActor>` with runtime-backed sessions without rewriting the current model/tool loop. AgentField remains a future Phase 7 adapter; Phase 1B introduces no AgentField runtime dependency and does not require a provider key.

**Tech Stack:** Rust 2024, Tokio, `async-trait`, `lato-core`, `lato-runtime`, existing ACP JSON-RPC protocol, existing `SessionActor`, existing transcript JSONL store.

## Global Constraints

- Preserve all existing CLI, headless, ACP, model, tool, approval, transcript, and plugin behavior.
- Do not modify the user-owned dirty files present at plan creation, especially `crates/lato-agent/src/actor.rs`, `src/cli.rs`, and `tests/cli_headless.rs`.
- Do not stage or commit `AGENTS.md` or any pre-existing user changes.
- The public client path must be `CLI/headless -> ACP -> RuntimeSession -> lato-runtime -> LegacyTurnDriver -> SessionActor`.
- `lato-core` remains provider-, tool-, client-, and runtime-independent.
- `lato-runtime` must not depend on `lato-agent`, `lato-ai`, `lato-tools`, or `lato-workspace`.
- The compatibility driver may depend on current Lato subsystems because it lives in `lato-agent`.
- Cancellation must travel through `CancellationToken`; steering must be handled inside the compatibility driver without creating a second runtime turn.
- Typed `ModelDelta` events must be translated back to the existing ACP `session/update.params.delta` shape exactly once.
- Existing non-text actor updates such as `session/tool_call` must remain observable through the ACP update sender.
- Run relevant tests before every commit and `cargo install --path .` after the completed feature.
- AgentField live contract checked for this phase: `2026-03-24-v1`; no provider key is configured, so no AgentField model execution belongs in this phase.

## Starting Worktree Guard

Before every task, run:

```bash
git status --short
```

The following pre-existing changes are user-owned and must remain unstaged:

```text
 M crates/lato-agent/src/actor.rs
 M crates/lato-ai/src/api.rs
 M crates/lato-ai/src/models_file.rs
 M crates/lato-ai/src/stream.rs
 M crates/lato-tools/src/dispatch.rs
 M crates/lato-tools/src/edit.rs
 M crates/lato-tools/src/registry.rs
 M src/cli.rs
 M tests/cli_headless.rs
?? AGENTS.md
```

---

### Task 1: Add the `SessionActor` compatibility `TurnDriver`

**Files:**
- Modify: `crates/lato-agent/Cargo.toml`
- Create: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Create: `crates/lato-agent/tests/legacy_driver.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: `SessionActor`, `PromptKind`, `HistoryItem`, `ModelStream`, `FileLocks`, `SessionTrust`, `ToolApproval`, and `lato_runtime::TurnDriver`.
- Produces: `LegacyTurnDriver::new`, `LegacyTurnDriver::history_snapshot`, `LegacyTurnDriver::replace_history`, and a `TurnDriver` implementation.

- [ ] **Step 1: Add dependencies without changing existing dependency direction**

Add these entries to `crates/lato-agent/Cargo.toml`:

```toml
lato-core = { path = "../lato-core" }
lato-runtime = { path = "../lato-runtime" }
```

The dependency direction must remain:

```text
lato-agent -> lato-runtime -> lato-core
lato-agent -> lato-ai/lato-tools/lato-workspace
```

- [ ] **Step 2: Write failing adapter contract tests**

Create `crates/lato-agent/tests/legacy_driver.rs` with these cases:

```rust
use lato_agent::{HistoryItem, LegacyTurnDriver, default_fake_stream};
use lato_core::{Command, EventPayload, SessionId, StartBehavior, StartTurn, UserInput};
use lato_runtime::spawn_session;
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

fn driver(
) -> (
    Arc<LegacyTurnDriver>,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = LegacyTurnDriver::new(
        "legacy-session".into(),
        default_fake_stream(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates_tx,
        None,
    );
    (Arc::new(driver), updates_rx)
}

#[tokio::test]
async fn legacy_driver_emits_typed_text_and_returns_final_text() {
    let (driver, _updates) = driver();
    let session = spawn_session(SessionId::from("legacy-session"), driver.clone());
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hi"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let mut saw_delta = false;
    let final_text = loop {
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.payload {
            EventPayload::ModelDelta { .. } => saw_delta = true,
            EventPayload::TurnCompleted(output) => break output.final_text,
            EventPayload::TurnFailed { error } => panic!("turn failed: {error}"),
            _ => {}
        }
    };
    assert!(saw_delta);
    assert!(!final_text.is_empty());
}

#[tokio::test]
async fn legacy_driver_history_can_be_hydrated_for_resume() {
    let (driver, _updates) = driver();
    driver
        .replace_history(vec![
            HistoryItem::User("old".into()),
            HistoryItem::AssistantText("answer".into()),
        ])
        .await;
    let history = driver.history_snapshot().await;
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn non_text_actor_updates_are_passed_through() {
    let (_driver, mut updates) = driver();
    assert!(matches!(updates.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
}
```

Do not widen `lato-runtime` internals solely for these tests. Tool-call passthrough is exercised with a scripted stream in Task 4; this task only establishes the constructor and channel ownership contract.

- [ ] **Step 3: Run the adapter test to verify it fails**

Run:

```bash
cargo test -p lato-agent --test legacy_driver
```

Expected: FAIL because `LegacyTurnDriver` does not exist.

- [ ] **Step 4: Implement `LegacyTurnDriver`**

Create `crates/lato-agent/src/legacy_driver.rs`. Its production structure must be:

```rust
use crate::{HistoryItem, PromptKind, SessionActor, ToolApproval};
use async_trait::async_trait;
use lato_ai::ModelStream;
use lato_core::{AgentError, ErrorCategory, Retryability, TurnOutput};
use lato_runtime::{TurnControl, TurnDriver, TurnEventEmitter, TurnRequest};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc};

pub struct LegacyTurnDriver {
    state: Mutex<LegacyState>,
    passthrough: mpsc::UnboundedSender<serde_json::Value>,
}

struct LegacyState {
    actor: SessionActor,
    actor_events: mpsc::UnboundedReceiver<serde_json::Value>,
}

impl LegacyTurnDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        passthrough: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let actor = SessionActor::new(stream, locks, trust, cwd)
            .with_interactive_events(actor_tx, session_id, approval);
        Self {
            state: Mutex::new(LegacyState { actor, actor_events }),
            passthrough,
        }
    }

    pub async fn history_snapshot(&self) -> Vec<HistoryItem> {
        self.state.lock().await.actor.history().to_vec()
    }

    pub async fn replace_history(&self, history: Vec<HistoryItem>) {
        *self.state.lock().await.actor.history_mut() = history;
    }
}
```

Implement `TurnDriver::run` as a bounded outer loop that supports one active actor prompt at a time:

```rust
#[async_trait]
impl TurnDriver for LegacyTurnDriver {
    async fn run(
        &self,
        request: TurnRequest,
        mut control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, AgentError> {
        let mut state = self.state.lock().await;
        let mut input = request.input;
        let mut kind = PromptKind::Start;

        loop {
            enum Next {
                Finished(Result<crate::TurnOutcome, String>),
                Cancelled,
                Steer(lato_core::UserInput),
            }

            let next = {
                let LegacyState { actor, actor_events } = &mut *state;
                let mut prompt = Box::pin(actor.prompt(kind, input.text));
                loop {
                    tokio::select! {
                        biased;
                        _ = control.cancellation.cancelled() => break Next::Cancelled,
                        steer = control.steering.recv() => match steer {
                            Some(steer) => break Next::Steer(steer),
                            None => {}
                        },
                        result = &mut prompt => break Next::Finished(result),
                        actor_event = actor_events.recv() => {
                            if let Some(actor_event) = actor_event {
                                forward_actor_event(&events, &self.passthrough, actor_event)?;
                            }
                        }
                    }
                }
            };

            match next {
                Next::Finished(Ok(_)) => {
                    drain_actor_events(
                        &events,
                        &self.passthrough,
                        &mut state.actor_events,
                    )?;
                    return Ok(TurnOutput {
                        final_text: state.actor.latest_assistant_text(),
                    });
                }
                Next::Finished(Err(message)) => return Err(legacy_error(message)),
                Next::Cancelled => {
                    state.actor.cancel();
                    return Ok(TurnOutput {
                        final_text: String::new(),
                    });
                }
                Next::Steer(steer) => {
                    state.actor.cancel();
                    input = steer;
                    kind = PromptKind::Steer;
                }
            }
        }
    }
}
```

`forward_actor_event` must translate only text deltas into typed events and pass every other JSON update through unchanged:

```rust
fn forward_actor_event(
    events: &TurnEventEmitter,
    passthrough: &mpsc::UnboundedSender<serde_json::Value>,
    actor_event: serde_json::Value,
) -> Result<(), AgentError> {
    if actor_event.get("method").and_then(|value| value.as_str()) == Some("session/update")
        && let Some(delta) = actor_event
            .pointer("/params/delta")
            .and_then(|value| value.as_str())
    {
        return events.model_delta(delta);
    }
    let _ = passthrough.send(actor_event);
    Ok(())
}

fn legacy_error(message: String) -> AgentError {
    AgentError::new(
        "legacy.turn_failed",
        ErrorCategory::Task,
        message,
        Retryability::RequiresDecision,
    )
}
```

Implement `drain_actor_events` using `try_recv` until `Empty`; do not wait after the prompt has completed. It uses the same `forward_actor_event` function.

- [ ] **Step 5: Export the adapter and record its provenance**

Add to `crates/lato-agent/src/lib.rs`:

```rust
pub mod legacy_driver;
pub use legacy_driver::*;
```

Add a row to `docs/superpowers/reference/lato-upstream-sources.md`:

```markdown
| `crates/lato-agent/src/legacy_driver.rs` | Lato `crates/lato-agent/src/actor.rs` plus Codex `codex-rs/core/src/session/handlers.rs` | Compatibility adapter | Delta forwarding, cancellation, steering, history hydration | Runs the existing Lato turn loop behind the typed `TurnDriver` membrane |
```

This file adapts rather than copies upstream code, so do not add a false Apache source header to the Rust file.

- [ ] **Step 6: Run focused and crate tests**

Run:

```bash
cargo test -p lato-agent --test legacy_driver
cargo test -p lato-agent
cargo clippy -p lato-agent --all-targets -- -D warnings
```

Expected: all pass. Verify `crates/lato-agent/src/actor.rs` still has the exact pre-task hash.

- [ ] **Step 7: Commit the compatibility driver**

```bash
git add crates/lato-agent/Cargo.toml Cargo.lock \
  crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/lib.rs \
  crates/lato-agent/tests/legacy_driver.rs \
  docs/superpowers/reference/lato-upstream-sources.md
git commit -m "feat: adapt legacy actor to lato runtime"
```

---

### Task 2: Add a typed `RuntimeSession` facade

**Files:**
- Create: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Create: `crates/lato-agent/tests/runtime_session.rs`

**Interfaces:**
- Consumes: `LegacyTurnDriver`, `lato_core::Command/EventPayload`, and `lato_runtime::SessionHandle`.
- Produces: `RuntimeSession::new`, `prompt`, `cancel`, `shutdown`, `history_snapshot`, `replace_history`, and `RuntimePromptOutcome`.

- [ ] **Step 1: Write failing facade tests**

Create `crates/lato-agent/tests/runtime_session.rs` with tests that use `default_fake_stream()`:

```rust
use lato_agent::{RuntimePromptOutcome, RuntimeSession, default_fake_stream};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

fn session() -> (
    Arc<RuntimeSession>,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = RuntimeSession::new(
        "runtime-session".into(),
        default_fake_stream(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates_tx,
        None,
    );
    (Arc::new(session), updates_rx)
}

#[tokio::test]
async fn prompt_round_trips_through_typed_runtime_events() {
    let (session, mut updates) = session();
    let outcome = session.prompt("hi".into()).await.unwrap();
    assert!(matches!(outcome, RuntimePromptOutcome::Complete { .. }));
    let delta = timeout(Duration::from_secs(1), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delta["method"], "session/update");
    assert!(delta["params"].get("delta").is_some());
}

#[tokio::test]
async fn hydrated_history_is_visible_after_resume() {
    let (session, _) = session();
    session
        .replace_history(vec![lato_agent::HistoryItem::User("restored".into())])
        .await;
    assert_eq!(session.history_snapshot().await.len(), 1);
}

#[tokio::test]
async fn idle_cancel_is_idempotent() {
    let (session, _) = session();
    session.cancel().await.unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p lato-agent --test runtime_session
```

Expected: FAIL because `RuntimeSession` does not exist.

- [ ] **Step 3: Implement the facade**

Create `crates/lato-agent/src/runtime_session.rs` with this public shape:

```rust
use crate::{HistoryItem, LegacyTurnDriver, ToolApproval};
use lato_ai::ModelStream;
use lato_core::{
    AgentError, CancelReason, Command, EventPayload, SessionId, StartBehavior, StartTurn, TurnId,
    UserInput,
};
use lato_runtime::{SessionHandle, spawn_session};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePromptOutcome {
    Complete { text: String },
    Cancelled { reason: CancelReason },
}

pub struct RuntimeSession {
    session_id: SessionId,
    handle: SessionHandle,
    driver: Arc<LegacyTurnDriver>,
    updates: mpsc::UnboundedSender<serde_json::Value>,
    active_turn: Mutex<Option<TurnId>>,
}
```

`RuntimeSession::new` constructs one `LegacyTurnDriver`, coerces a clone to `Arc<dyn TurnDriver>`, and calls `spawn_session` exactly once. `prompt` must:

1. subscribe before submitting;
2. submit `Command::StartTurn` with `StartBehavior::Reject`;
3. consume only events for this `session_id`;
4. record the `TurnId` on `TurnStarted`;
5. translate `ModelDelta` to exactly one ACP delta update;
6. translate `ReasoningDelta` to `session/reasoning` without mixing it into visible answer text;
7. clear the active ID on every terminal event;
8. return `Complete`, `Cancelled`, or the typed `AgentError`.

Use this event translation:

```rust
match event.payload {
    EventPayload::TurnStarted => {
        *self.active_turn.lock().await = event.turn_id;
    }
    EventPayload::ModelDelta { text } => {
        let _ = self.updates.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": self.session_id.as_str(), "delta": text},
        }));
    }
    EventPayload::ReasoningDelta { text } => {
        let _ = self.updates.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/reasoning",
            "params": {"sessionId": self.session_id.as_str(), "delta": text},
        }));
    }
    EventPayload::TurnCompleted(output) => {
        *self.active_turn.lock().await = None;
        return Ok(RuntimePromptOutcome::Complete { text: output.final_text });
    }
    EventPayload::TurnCancelled { reason } => {
        *self.active_turn.lock().await = None;
        return Ok(RuntimePromptOutcome::Cancelled { reason });
    }
    EventPayload::TurnFailed { error } => {
        *self.active_turn.lock().await = None;
        return Err(error);
    }
    EventPayload::SessionStopped => {
        *self.active_turn.lock().await = None;
        return Err(runtime_stopped());
    }
    EventPayload::SessionStarted => {}
}
```

Handle `broadcast::RecvError::Lagged(skipped)` as a structured internal-invariant error containing the skipped count. Handle `Closed` as `runtime.event_bus_closed`.

`cancel` is idempotent when no turn is active:

```rust
pub async fn cancel(&self) -> Result<(), AgentError> {
    let turn_id = self.active_turn.lock().await.clone();
    match turn_id {
        Some(turn_id) => self.handle.submit(Command::CancelTurn { turn_id }).await,
        None => Ok(()),
    }
}
```

`shutdown`, `history_snapshot`, and `replace_history` delegate to the typed handle or compatibility driver. Do not expose the raw `SessionActor`.

- [ ] **Step 4: Export and verify the facade**

Add to `crates/lato-agent/src/lib.rs`:

```rust
pub mod runtime_session;
pub use runtime_session::*;
```

Run:

```bash
cargo test -p lato-agent --test runtime_session
cargo test -p lato-agent --test legacy_driver
cargo test -p lato-core -p lato-runtime -p lato-agent
cargo clippy -p lato-agent --all-targets -- -D warnings
```

Expected: all pass.

- [ ] **Step 5: Commit the facade**

```bash
git add crates/lato-agent/src/runtime_session.rs crates/lato-agent/src/lib.rs \
  crates/lato-agent/tests/runtime_session.rs
git commit -m "feat: add typed runtime session facade"
```

---

### Task 3: Move `AcpHost` sessions onto `RuntimeSession`

**Files:**
- Modify: `crates/lato-agent/src/host.rs`
- Create: `crates/lato-agent/tests/acp_runtime.rs`

**Interfaces:**
- Consumes: `RuntimeSession` and `RuntimePromptOutcome` from Task 2.
- Produces: runtime-backed implementations of ACP `session/new`, `session/prompt`, `session/cancel`, `session/close`, and `session/resume`.

- [ ] **Step 1: Write end-to-end ACP equivalence tests**

Create `crates/lato-agent/tests/acp_runtime.rs`:

```rust
use lato_agent::{AcpHost, default_fake_stream};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn host() -> (
    AcpHost,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        AcpHost::new(
            cwd.clone(),
            SessionTrust::for_headless_prompt(&cwd),
            updates_tx,
            default_fake_stream(),
        ),
        updates_rx,
    )
}

async fn new_session(host: &mut AcpHost) -> String {
    host.handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn acp_host_source_routes_sessions_through_runtime_facade() {
    let source = include_str!("../src/host.rs");
    assert!(source.contains("HashMap<String, Arc<RuntimeSession>>"));
    assert!(!source.contains(".prompt(PromptKind::Start"));
}

#[tokio::test]
async fn acp_prompt_emits_delta_and_returns_the_same_final_text() {
    let (mut host, mut updates) = host();
    let sid = new_session(&mut host).await;
    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    let mut deltas = String::new();
    while let Ok(update) = updates.try_recv() {
        if update["method"] == "session/update"
            && let Some(delta) = update["params"]["delta"].as_str()
        {
            deltas.push_str(delta);
        }
    }
    assert!(!deltas.is_empty());
    assert_eq!(response["result"]["text"], deltas);
}

#[tokio::test]
async fn acp_close_stops_and_removes_the_runtime_session() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    host.handle(req(
        2,
        "session/close",
        serde_json::json!({"sessionId": sid}),
    ))
    .await
    .unwrap();
    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "after close"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["error"]["message"], "unknown session");
}
```

Append this exact private-state test to the existing `#[cfg(test)]` module in `host.rs`; it avoids process-global environment mutation:

```rust
#[tokio::test]
async fn runtime_resume_hydrates_transcript_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = TranscriptStore::open(directory.path()).unwrap();
    store
        .append(
            "resume-1",
            &[
                crate::HistoryItem::User("old".into()),
                crate::HistoryItem::AssistantText("answer".into()),
            ],
        )
        .unwrap();
    let mut host = host();
    host.transcripts = Some(store);
    host.handle(req(
        1,
        "session/resume",
        serde_json::json!({"sessionId": "resume-1"}),
    ))
    .await
    .unwrap();
    let history = host.sessions["resume-1"].history_snapshot().await;
    assert_eq!(history.len(), 2);
}
```

- [ ] **Step 2: Run the test to verify the runtime assertion fails**

```bash
cargo test -p lato-agent --test acp_runtime
```

Expected: FAIL at `acp_host_source_routes_sessions_through_runtime_facade` because `AcpHost` still owns `SessionActor` directly.

- [ ] **Step 3: Replace the session table and centralize construction**

In `crates/lato-agent/src/host.rs`, replace:

```rust
sessions: HashMap<String, SessionActor>,
```

with:

```rust
sessions: HashMap<String, Arc<RuntimeSession>>,
```

Update imports to use `RuntimePromptOutcome` and `RuntimeSession`, and remove the direct `PromptKind`/`SessionActor` imports.

Add a private constructor helper on `AcpHost`:

```rust
fn make_runtime_session(&self, sid: &str) -> Arc<RuntimeSession> {
    Arc::new(RuntimeSession::new(
        sid.to_string(),
        self.stream.clone(),
        self.locks.clone(),
        self.trust.clone(),
        self.cwd.clone(),
        self.updates.clone(),
        self.tool_approval.clone(),
    ))
}
```

- [ ] **Step 4: Migrate ACP session methods**

Use `make_runtime_session` in `session/new`.

For `session/prompt`, clone the `Arc<RuntimeSession>` before awaiting. Preserve the permission request heuristic. Map the result as follows:

```rust
match session.prompt(text).await {
    Ok(RuntimePromptOutcome::Complete { text }) => {
        let history = session.history_snapshot().await;
        if let Some(store) = &self.transcripts {
            let start = *self.persisted.get(sid).unwrap_or(&0);
            if let Err(error) = store.append(sid, &history[start..]) {
                return Some(err(id, -32000, format!("persist transcript: {error}")));
            }
            self.persisted.insert(sid.to_string(), history.len());
        }
        let _ = self.updates.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": sid, "text": text},
        }));
        Some(ok(id, serde_json::json!({"status": "complete", "text": text})))
    }
    Ok(RuntimePromptOutcome::Cancelled { .. }) => {
        Some(ok(id, serde_json::json!({"status": "cancelled", "text": ""})))
    }
    Err(error) => Some(err(id, -32000, error.to_string())),
}
```

For `session/cancel`, call `session.cancel().await` when present but preserve the existing idempotent `{"status":"cancelled"}` response for unknown or idle sessions.

For `session/close`, remove the session, then call `shutdown().await`; return the existing `{"closed":true}` shape.

For `session/resume`, create the runtime session, load transcript history, call `replace_history(history).await`, set the persisted length, then insert. Preserve `replayed:false` and existing missing-file behavior.

- [ ] **Step 5: Run all ACP and client tests**

```bash
cargo test -p lato-agent --test acp_runtime
cargo test -p lato-agent host::tests -- --nocapture
cargo test --test cli_headless a1_1_stdio_acp_cli_initializes_and_rejects_session_load -- --exact
cargo test --test cli_headless a1_7_cli_uses_acp_not_actor_prompt_symbol -- --exact
cargo test --test cli_headless a5_1_headless_prompt_fake_model -- --exact
```

Expected: all pass without modifying `tests/cli_headless.rs`.

- [ ] **Step 6: Commit the ACP migration**

```bash
git add crates/lato-agent/src/host.rs crates/lato-agent/tests/acp_runtime.rs
git commit -m "feat: route acp sessions through lato runtime"
```

---

### Task 4: Harden runtime-backed ACP lifecycle and transcript behavior

**Files:**
- Modify: `crates/lato-agent/tests/acp_runtime.rs`
- Modify: `crates/lato-agent/src/host.rs` for private transcript regression tests
- Modify: `crates/lato-agent/src/runtime_session.rs` only if tests expose a defect

**Interfaces:**
- Consumes: the migrated ACP host and runtime facade.
- Produces: regression coverage for event identity, no duplicate deltas, cancellation, close, resume, and transcript append boundaries.

- [ ] **Step 1: Add exact ACP lifecycle regression tests**

Append to `crates/lato-agent/tests/acp_runtime.rs`:

```rust
#[tokio::test]
async fn idle_cancel_is_idempotent_and_session_remains_usable() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    let cancelled = host
        .handle(req(
            2,
            "session/cancel",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert_eq!(cancelled["result"]["status"], "cancelled");
    let prompt = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "still alive"}),
        ))
        .await
        .unwrap();
    assert_eq!(prompt["result"]["status"], "complete");
}

#[tokio::test]
async fn sequential_prompts_reuse_the_runtime_session_and_retain_history() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    for (id, text) in [(2, "first"), (3, "second")] {
        let response = host
            .handle(req(
                id,
                "session/prompt",
                serde_json::json!({"sessionId": sid, "text": text}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["status"], "complete");
    }
    let listed = host
        .handle(req(4, "session/list", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| value.as_str() == Some(sid.as_str()))
            .count(),
        1,
    );
}
```

The existing `acp_prompt_emits_delta_and_returns_the_same_final_text` test is the no-duplicate-delta contract: concatenated deltas must equal the final text exactly.

Append this transcript boundary test to the private `host.rs` test module:

```rust
#[tokio::test]
async fn resumed_transcript_appends_without_duplicating_loaded_rows() {
    let directory = tempfile::tempdir().unwrap();
    let store = TranscriptStore::open(directory.path()).unwrap();
    store
        .append(
            "resume-append",
            &[
                crate::HistoryItem::User("old".into()),
                crate::HistoryItem::AssistantText("answer".into()),
            ],
        )
        .unwrap();
    let mut host = host();
    host.transcripts = Some(store.clone());
    host.handle(req(
        1,
        "session/resume",
        serde_json::json!({"sessionId": "resume-append"}),
    ))
    .await
    .unwrap();
    host.handle(req(
        2,
        "session/prompt",
        serde_json::json!({"sessionId": "resume-append", "text": "new"}),
    ))
    .await
    .unwrap();
    let loaded = store.load("resume-append").unwrap();
    let old_user_rows = loaded
        .iter()
        .filter(|item| matches!(item, crate::HistoryItem::User(text) if text == "old"))
        .count();
    assert_eq!(old_user_rows, 1);
    assert!(loaded.len() > 2);
}
```

- [ ] **Step 2: Run each new test individually**

```bash
cargo test -p lato-agent --test acp_runtime -- --nocapture
```

Expected: all pass. If a defect is exposed, apply the smallest fix in `runtime_session.rs` or `host.rs`; do not change `SessionActor` in Phase 1B and do not call live providers.

- [ ] **Step 3: Run the core/runtime/agent suite five times**

```bash
for run in 1 2 3 4 5; do
  cargo test -p lato-core -p lato-runtime -p lato-agent || exit 1
done
```

Expected: five consecutive passes without event-ordering flakes or one-second timeout failures.

- [ ] **Step 4: Run static checks**

```bash
cargo fmt --all -- --check
cargo clippy -p lato-core -p lato-runtime -p lato-agent --all-targets -- -D warnings
cargo tree -p lato-runtime | rg 'lato-(agent|ai|tools|workspace)' && exit 1 || true
git diff --check
```

Expected: all gates pass and `lato-runtime` remains independent.

- [ ] **Step 5: Commit lifecycle coverage**

If tests required no production fix:

```bash
git add crates/lato-agent/tests/acp_runtime.rs
git commit -m "test: harden runtime backed acp lifecycle"
```

If a production fix was necessary, include only the exact clean Phase 1B file together with its regression test.

---

### Task 5: Complete Phase 1B documentation, workspace verification, and deployment

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: completed runtime-backed ACP path.
- Produces: accurate architecture status and locally installed `lato` binary.

- [ ] **Step 1: Update the runtime architecture status**

Replace the Phase 1A migration paragraph in `README.md` with:

```markdown
## Runtime architecture

Lato's headless, interactive, and stdio clients share the ACP host. ACP sessions submit typed
commands to `lato-runtime` and consume versioned events from it. `lato-core` owns provider-independent
IDs, errors, commands, events, and session invariants; `lato-runtime` owns cancellation, steering,
replacement, event ordering, and the `TurnDriver` boundary.

The existing model/tool loop currently runs behind `LegacyTurnDriver`, a compatibility adapter around
`SessionActor`. This keeps provider, tool, approval, and transcript behavior stable while Phase 2
moves those capabilities behind dedicated ports.

Copied or structurally derived upstream code is pinned in
[`docs/superpowers/reference/lato-upstream-sources.md`](docs/superpowers/reference/lato-upstream-sources.md).
```

- [ ] **Step 2: Run focused and workspace gates**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p lato-core -p lato-runtime -p lato-agent
cargo test --workspace
```

Expected: all pass. If an existing SSE fixture fails once with `WouldBlock (os 35)`, rerun that exact test and then the full workspace suite; do not weaken the fixture or timeout.

- [ ] **Step 3: Verify the actual client path**

Run:

```bash
rg -n 'RuntimeSession|spawn_session|Command::StartTurn' crates/lato-agent/src
rg -n 'SessionActor' src/client.rs src/cli.rs src/stdio.rs
```

Expected: `RuntimeSession` and typed commands appear under `lato-agent`; root clients contain no direct `SessionActor` usage.

- [ ] **Step 4: Deploy and smoke test**

```bash
cargo install --path .
lato -p "reply with hi only"
```

Expected: install exits 0 and smoke output contains `hi`.

- [ ] **Step 5: Verify user-owned dirty files are unchanged**

Compare hashes captured before Task 1 for all pre-existing dirty files. Run:

```bash
git status --short
git diff --name-only
```

Expected: the original dirty files and `AGENTS.md` remain unstaged; Phase 1B introduced no diff in `actor.rs`, `src/cli.rs`, or `tests/cli_headless.rs`.

- [ ] **Step 6: Commit documentation**

```bash
git add README.md
git commit -m "docs: describe runtime backed acp sessions"
```

## Phase 1B Completion Gate

Phase 1B is complete only when all statements are true:

- ACP `session/new` creates one `RuntimeSession` and one Tokio runtime loop.
- ACP `session/prompt` submits `Command::StartTurn` rather than calling `SessionActor::prompt` directly.
- ACP model deltas cross `EventPayload::ModelDelta` and preserve the existing JSON update shape.
- Non-text actor events remain visible through ACP.
- ACP close and cancel map to typed runtime commands.
- Resume hydrates legacy history before the next runtime turn.
- Transcript append boundaries do not duplicate loaded history.
- Headless, interactive, and stdio clients still share the ACP host.
- `lato-runtime` remains independent of the existing agent/provider/tool implementation.
- Existing workspace tests pass without changing acceptance semantics.
- The installed `lato` command passes the headless smoke test.
- All pre-existing user modifications remain unstaged and unchanged.

## Self-Review Mapping

| Approved design requirement | Implemented by |
|---|---|
| Old `prompt` remains a compatibility adapter | Task 1 |
| CLI/ACP use typed Command/Event | Tasks 2-3 |
| Cancellation and steering cross the runtime boundary | Tasks 1-2 |
| Existing ACP event shapes remain compatible | Tasks 2-4 |
| Transcript list/resume/append remains stable | Tasks 3-4 |
| Core independence is preserved | Tasks 1, 4, and 5 |
| Existing user work is preserved | Every task |
| Local `lato` remains runnable | Task 5 |

Phase 2 must not begin until this completion gate passes.
