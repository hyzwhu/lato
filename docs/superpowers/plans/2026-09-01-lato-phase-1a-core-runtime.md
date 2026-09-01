# Lato Phase 1A Core Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the typed Command/Event contract, deterministic session state machine, and cancellable Tokio session runtime that later plans will connect to the existing ACP host and `SessionActor`.

**Architecture:** Introduce a dependency-light `lato-core` crate for domain types and invariants, plus a `lato-runtime` crate for actor scheduling, cancellation, steering, replacement, and ordered event envelopes. This Phase 1A deliberately does not replace the existing ACP path yet; it produces independently tested foundations so Phase 1B can adapt `AcpHost` without designing core semantics inside transport code.

**Tech Stack:** Rust 2024, Cargo workspace, Tokio channels/tasks, `tokio-util::sync::CancellationToken`, Serde, `thiserror`, `async-trait`.

## Global Constraints

- Preserve all existing CLI, headless, ACP, model, tool, approval, and transcript behavior.
- Do not modify the existing dirty files in `crates/lato-agent`, `crates/lato-ai`, `crates/lato-tools`, `src/cli.rs`, or `tests/cli_headless.rs` during Phase 1A.
- Rust 2024 edition only; no second implementation language.
- `lato-core` must not depend on Tokio, reqwest, CLI code, model providers, tools, or workspace implementations.
- `lato-runtime` may depend on `lato-core`, Tokio, `tokio-util`, and `async-trait`; it must not depend on `lato-agent`, `lato-ai`, `lato-tools`, `lato-workspace`, or any client crate.
- One session has at most one active foreground turn.
- Every loop, queue, and dynamic branch has a fixed capacity or integer limit.
- Child plans must use the types and names defined here rather than create competing Command/Event types.
- Copy mature Codex/Grok Build code when its boundary fits. Preserve an exact source commit/path/license record and carry over relevant tests.
- Codex source baseline: `633ab199cfd724aa78013c006b27a2b3d049fc3b`, Apache-2.0.
- Grok Build source baseline: `bb7f39d5858cbf5e00de639367f59debbdcb0138`, Apache-2.0.
- Use TDD: write each failing test, observe the expected failure, implement the minimum behavior, rerun the focused test, then rerun the crate suite.
- After the phase passes, run the workspace tests and deploy locally with `cargo install --path .`.
- Commit only files owned by the current task. Never stage unrelated user changes.

## Scope Boundary

This plan implements Phase 1A only. The following independently reviewable plans come afterward:

1. Phase 1B: adapt `SessionActor` and `AcpHost` to the runtime Command/Event API.
2. Phase 2: `ModelPort`, `Tool`, registry, and tool execution pipeline.
3. Phase 3: `PolicyEngine`, approvals, capabilities, and sandbox contracts.
4. Phase 4: event journal, snapshots, replay, and compaction.
5. Phase 5: task tree, budgets, profiles, and worktree-isolated subagents.
6. Phase 6: hooks, skills, MCP registry, and plugin manifests.
7. Phase 7: workflows and AgentField/daemon adapters.

## File Structure

Files created or modified by this plan:

```text
Cargo.toml                                      workspace membership only
crates/lato-core/Cargo.toml                     dependency-light domain crate
crates/lato-core/src/lib.rs                     public exports
crates/lato-core/src/id.rs                      typed session/turn/event IDs
crates/lato-core/src/error.rs                   structured runtime error
crates/lato-core/src/command.rs                 Command and request payloads
crates/lato-core/src/event.rs                   Event payloads and envelopes
crates/lato-core/src/state.rs                   deterministic session state machine
crates/lato-core/tests/serde_contract.rs         wire round-trip tests
crates/lato-core/tests/state_machine.rs          transition and invariant tests
crates/lato-runtime/Cargo.toml                  Tokio runtime crate
crates/lato-runtime/src/lib.rs                  public exports
crates/lato-runtime/src/driver.rs               TurnDriver contract and emitter
crates/lato-runtime/src/session.rs              session actor loop and handle
crates/lato-runtime/tests/session_runtime.rs     async runtime contract tests
docs/superpowers/reference/lato-upstream-sources.md
                                                source/license/provenance ledger
README.md                                       concise architecture status note
```

---

### Task 1: Record the upstream source and reuse ledger

**Files:**
- Create: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: the two pinned source repositories and commits in Global Constraints.
- Produces: the mandatory provenance record every later copy task updates.

- [ ] **Step 1: Create the reference directory and ledger with exact pinned sources**

Use `apply_patch` to create this exact document:

```markdown
# Lato Upstream Source Ledger

This file records code copied or structurally derived from upstream agent implementations.
Every copied production file and its tests must add a row before merge.

## Pinned baselines

| Project | Repository path | Commit | License |
|---|---|---|---|
| Codex | `/Users/huangyongzhao/Documents/work/rustproject/codex` | `633ab199cfd724aa78013c006b27a2b3d049fc3b` | Apache-2.0 |
| Grok Build | `/Users/huangyongzhao/Documents/work/grok-build` | `bb7f39d5858cbf5e00de639367f59debbdcb0138` | Apache-2.0 |

## Reuse records

| Lato target | Upstream source | Reuse mode | Tests carried over | Lato changes |
|---|---|---|---|---|
| `crates/lato-core/src/state.rs` | Codex `codex-rs/core/src/session/session.rs` and `codex-rs/core/src/session/handlers.rs` | Structural derivation | Single-active-turn, replace, cancel, shutdown state tests | Reduced to a provider- and transport-independent state machine |
| `crates/lato-runtime/src/session.rs` | Codex `codex-rs/core/src/session/handlers.rs::submission_loop` | Structural derivation | Submission ordering, cancellation, replacement, shutdown tests | Tokio channels expose typed Lato Command/Event values |
| `crates/lato-runtime/src/driver.rs` | Grok Build `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs` | Structural derivation | Child/event channel completion and cancellation tests | Generalized from child tasks to a foreground turn driver |

## Required source header

Copied or substantially derived Rust files begin with an exact project, commit, and path. For example:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values
```

Do not copy product-specific account, cloud-task, billing, UI, branding, or unrelated telemetry code.
```

- [ ] **Step 2: Verify both pinned commits and license files**

Run:

```bash
test "$(git -C /Users/huangyongzhao/Documents/work/rustproject/codex rev-parse HEAD)" = "633ab199cfd724aa78013c006b27a2b3d049fc3b"
test "$(git -C /Users/huangyongzhao/Documents/work/grok-build rev-parse HEAD)" = "bb7f39d5858cbf5e00de639367f59debbdcb0138"
rg -n "Apache License" /Users/huangyongzhao/Documents/work/rustproject/codex/LICENSE /Users/huangyongzhao/Documents/work/grok-build/LICENSE
```

Expected: both `test` commands exit 0; `rg` prints an Apache License match for both files.

- [ ] **Step 3: Verify the ledger contains no unpinned reuse row**

Run:

```bash
rg -n "633ab199cfd724aa78013c006b27a2b3d049fc3b|bb7f39d5858cbf5e00de639367f59debbdcb0138|Apache-2.0" docs/superpowers/reference/lato-upstream-sources.md
```

Expected: both commits and the license identifier appear.

- [ ] **Step 4: Commit the ledger**

```bash
git add docs/superpowers/reference/lato-upstream-sources.md
git commit -m "docs: record lato upstream source baselines"
```

---

### Task 2: Create `lato-core` with typed IDs and structured errors

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/lato-core/Cargo.toml`
- Create: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/src/id.rs`
- Create: `crates/lato-core/src/error.rs`
- Create: `crates/lato-core/tests/serde_contract.rs`

**Interfaces:**
- Consumes: Serde and `thiserror` only.
- Produces: `SessionId`, `TurnId`, `EventId`, `AgentError`, `ErrorCategory`, and `Retryability`.

- [ ] **Step 1: Add the crate to the workspace and declare its dependencies**

Add `"crates/lato-core"` to the root workspace members. Create:

```toml
[package]
name = "lato-core"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
thiserror = "2"

[dev-dependencies]
serde_json = "1"
```

- [ ] **Step 2: Write the failing ID and error serialization test**

Create `crates/lato-core/tests/serde_contract.rs`:

```rust
use lato_core::{AgentError, ErrorCategory, Retryability, SessionId, TurnId};

#[test]
fn ids_and_errors_have_stable_json_shapes() {
    let session_id = SessionId::from("session-7");
    let turn_id = TurnId::from("turn-9");
    assert_eq!(serde_json::to_string(&session_id).unwrap(), "\"session-7\"");
    assert_eq!(serde_json::to_string(&turn_id).unwrap(), "\"turn-9\"");

    let error = AgentError::new(
        "runtime.bus_closed",
        ErrorCategory::InternalInvariant,
        "runtime command bus closed",
        Retryability::Never,
    );
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["code"], "runtime.bus_closed");
    assert_eq!(value["category"], "internal_invariant");
    assert_eq!(value["retryability"], "never");
}

#[test]
fn empty_ids_are_rejected_by_checked_constructor() {
    assert!(SessionId::parse(" ").is_err());
    assert!(TurnId::parse("").is_err());
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run:

```bash
cargo test -p lato-core --test serde_contract
```

Expected: FAIL because the crate or exported types do not exist.

- [ ] **Step 4: Implement typed IDs**

Create `crates/lato-core/src/id.rs`:

```rust
use std::{fmt, str::FromStr};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(IdError::Empty(stringify!($name)));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::parse(value).expect("static IDs must not be empty")
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::parse(value).expect("runtime IDs must not be empty")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
    };
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IdError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
}

string_id!(SessionId);
string_id!(TurnId);
string_id!(EventId);
```

- [ ] **Step 5: Implement structured errors**

Create `crates/lato-core/src/error.rs`:

```rust
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    InvalidInput,
    Configuration,
    Model,
    Tool,
    Policy,
    Sandbox,
    Storage,
    Extension,
    Task,
    InternalInvariant,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    Never,
    Safe,
    AfterBackoff,
    RequiresDecision,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AgentError {
    pub code: String,
    pub category: ErrorCategory,
    pub message: String,
    pub retryability: Retryability,
}

impl AgentError {
    pub fn new(
        code: impl Into<String>,
        category: ErrorCategory,
        message: impl Into<String>,
        retryability: Retryability,
    ) -> Self {
        Self {
            code: code.into(),
            category,
            message: message.into(),
            retryability,
        }
    }
}
```

- [ ] **Step 6: Export the modules**

Create `crates/lato-core/src/lib.rs`:

```rust
mod error;
mod id;

pub use error::{AgentError, ErrorCategory, Retryability};
pub use id::{EventId, IdError, SessionId, TurnId};
```

- [ ] **Step 7: Run focused and crate tests**

```bash
cargo test -p lato-core --test serde_contract
cargo test -p lato-core
```

Expected: both commands PASS.

- [ ] **Step 8: Commit the core foundation**

```bash
git add Cargo.toml Cargo.lock crates/lato-core
git commit -m "feat: add lato core identifiers and errors"
```

---

### Task 3: Define the Command/Event contract

**Files:**
- Create: `crates/lato-core/src/command.rs`
- Create: `crates/lato-core/src/event.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Modify: `crates/lato-core/tests/serde_contract.rs`

**Interfaces:**
- Consumes: `SessionId`, `TurnId`, `EventId`, and `AgentError` from Task 2.
- Produces: `Command`, `StartTurn`, `StartBehavior`, `UserInput`, `EventPayload`, `EventEnvelope`, `CancelReason`, and `TurnOutput`.

- [ ] **Step 1: Add failing wire-shape tests**

Replace the existing `use lato_core` declaration with this combined import:

```rust
use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventEnvelope, EventId, EventPayload,
    Retryability, SessionId, StartBehavior, StartTurn, TurnId, TurnOutput, UserInput,
};
```

Then append these tests to `crates/lato-core/tests/serde_contract.rs`:

```rust

#[test]
fn command_uses_tagged_snake_case_wire_shape() {
    let command = Command::StartTurn(StartTurn {
        input: UserInput::text("inspect the repository"),
        behavior: StartBehavior::Replace,
    });
    let value = serde_json::to_value(command).unwrap();
    assert_eq!(value["type"], "start_turn");
    assert_eq!(value["input"]["text"], "inspect the repository");
    assert_eq!(value["behavior"], "replace");
}

#[test]
fn event_envelope_round_trips_without_losing_identity() {
    let envelope = EventEnvelope {
        schema_version: 1,
        event_id: EventId::from("event-1"),
        session_id: SessionId::from("session-1"),
        turn_id: Some(TurnId::from("turn-1")),
        parent_event_id: None,
        sequence: 3,
        timestamp_ms: 1_700_000_000_000,
        payload: EventPayload::TurnCompleted(TurnOutput {
            final_text: "done".into(),
        }),
    };
    let encoded = serde_json::to_string(&envelope).unwrap();
    let decoded: EventEnvelope = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, envelope);
}

#[test]
fn cancellation_reason_is_explicit() {
    let payload = EventPayload::TurnCancelled {
        reason: CancelReason::Replaced,
    };
    assert_eq!(serde_json::to_value(payload).unwrap()["reason"], "replaced");
}
```

- [ ] **Step 2: Run the contract test to verify it fails**

```bash
cargo test -p lato-core --test serde_contract
```

Expected: FAIL with unresolved imports for the Command/Event types.

- [ ] **Step 3: Implement commands**

Create `crates/lato-core/src/command.rs`:

```rust
use crate::TurnId;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    StartTurn(StartTurn),
    SteerTurn(UserInput),
    CancelTurn { turn_id: TurnId },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StartTurn {
    pub input: UserInput,
    pub behavior: StartBehavior,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartBehavior {
    Reject,
    Replace,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct UserInput {
    pub text: String,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}
```

- [ ] **Step 4: Implement events and envelopes**

Create `crates/lato-core/src/event.rs`:

```rust
use crate::{AgentError, EventId, SessionId, TurnId};

pub const EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EventEnvelope {
    pub schema_version: u16,
    pub event_id: EventId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub parent_event_id: Option<EventId>,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub payload: EventPayload,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    SessionStarted,
    TurnStarted,
    ModelDelta { text: String },
    ReasoningDelta { text: String },
    TurnCompleted(TurnOutput),
    TurnFailed { error: AgentError },
    TurnCancelled { reason: CancelReason },
    SessionStopped,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TurnOutput {
    pub final_text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    User,
    Replaced,
    Shutdown,
}
```

- [ ] **Step 5: Export the contract**

Replace `crates/lato-core/src/lib.rs` with:

```rust
mod command;
mod error;
mod event;
mod id;

pub use command::{Command, StartBehavior, StartTurn, UserInput};
pub use error::{AgentError, ErrorCategory, Retryability};
pub use event::{
    CancelReason, EVENT_SCHEMA_VERSION, EventEnvelope, EventPayload, TurnOutput,
};
pub use id::{EventId, IdError, SessionId, TurnId};
```

- [ ] **Step 6: Run the focused contract test**

```bash
cargo test -p lato-core --test serde_contract
```

Expected: PASS.

- [ ] **Step 7: Commit the contract**

```bash
git add crates/lato-core/src crates/lato-core/tests/serde_contract.rs
git commit -m "feat: define lato command and event contract"
```

---

### Task 4: Implement the deterministic session state machine

**Files:**
- Create: `crates/lato-core/src/state.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/tests/state_machine.rs`

**Interfaces:**
- Consumes: `TurnId` and `StartBehavior`.
- Produces: `SessionMachine`, `SessionPhase`, `ActiveTurn`, `StartDecision`, and `TransitionError`.

- [ ] **Step 1: Write failing transition tests**

Create `crates/lato-core/tests/state_machine.rs`:

```rust
use lato_core::{
    SessionMachine, SessionPhase, StartBehavior, StartDecision, TransitionError, TurnId,
};

#[test]
fn starts_only_one_foreground_turn() {
    let mut machine = SessionMachine::new();
    assert_eq!(
        machine
            .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
            .unwrap(),
        StartDecision::StartNow,
    );
    assert_eq!(
        machine.request_start(TurnId::from("turn-2"), StartBehavior::Reject),
        Err(TransitionError::TurnAlreadyActive),
    );
}

#[test]
fn replacement_cancels_old_turn_before_new_turn_starts() {
    let mut machine = SessionMachine::new();
    machine
        .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
        .unwrap();
    assert_eq!(
        machine
            .request_start(TurnId::from("turn-2"), StartBehavior::Replace)
            .unwrap(),
        StartDecision::CancelThenStart {
            active: TurnId::from("turn-1"),
            pending: TurnId::from("turn-2"),
        },
    );
    assert!(machine.active_turn().unwrap().cancel_requested);
}

#[test]
fn only_the_active_turn_can_finish() {
    let mut machine = SessionMachine::new();
    machine
        .request_start(TurnId::from("turn-1"), StartBehavior::Reject)
        .unwrap();
    assert_eq!(
        machine.finish(&TurnId::from("turn-2")),
        Err(TransitionError::NotActiveTurn),
    );
    machine.finish(&TurnId::from("turn-1")).unwrap();
    assert_eq!(machine.phase(), &SessionPhase::Idle);
}

#[test]
fn stopped_sessions_reject_new_turns() {
    let mut machine = SessionMachine::new();
    machine.stop();
    assert_eq!(
        machine.request_start(TurnId::from("turn-1"), StartBehavior::Reject),
        Err(TransitionError::SessionStopped),
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p lato-core --test state_machine
```

Expected: FAIL with unresolved state-machine imports.

- [ ] **Step 3: Implement the state machine**

Create `crates/lato-core/src/state.rs` with the required provenance header:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/session.rs and codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: reduced the single-active-turn lifecycle to a transport-independent state machine

use crate::{StartBehavior, TurnId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Idle,
    Running(ActiveTurn),
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTurn {
    pub id: TurnId,
    pub cancel_requested: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartDecision {
    StartNow,
    CancelThenStart { active: TurnId, pending: TurnId },
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TransitionError {
    #[error("a foreground turn is already active")]
    TurnAlreadyActive,
    #[error("the requested turn is not the active turn")]
    NotActiveTurn,
    #[error("the session is stopped")]
    SessionStopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMachine {
    phase: SessionPhase,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self {
            phase: SessionPhase::Idle,
        }
    }

    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    pub fn active_turn(&self) -> Option<&ActiveTurn> {
        match &self.phase {
            SessionPhase::Running(active) => Some(active),
            SessionPhase::Idle | SessionPhase::Stopped => None,
        }
    }

    pub fn request_start(
        &mut self,
        turn_id: TurnId,
        behavior: StartBehavior,
    ) -> Result<StartDecision, TransitionError> {
        match &mut self.phase {
            SessionPhase::Idle => {
                self.phase = SessionPhase::Running(ActiveTurn {
                    id: turn_id,
                    cancel_requested: false,
                });
                Ok(StartDecision::StartNow)
            }
            SessionPhase::Running(_) if behavior == StartBehavior::Reject => {
                Err(TransitionError::TurnAlreadyActive)
            }
            SessionPhase::Running(active) => {
                active.cancel_requested = true;
                Ok(StartDecision::CancelThenStart {
                    active: active.id.clone(),
                    pending: turn_id,
                })
            }
            SessionPhase::Stopped => Err(TransitionError::SessionStopped),
        }
    }

    pub fn request_cancel(&mut self, turn_id: &TurnId) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn_mut(turn_id) else {
            return Err(TransitionError::NotActiveTurn);
        };
        active.cancel_requested = true;
        Ok(())
    }

    pub fn finish(&mut self, turn_id: &TurnId) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn() else {
            return Err(TransitionError::NotActiveTurn);
        };
        if &active.id != turn_id {
            return Err(TransitionError::NotActiveTurn);
        }
        self.phase = SessionPhase::Idle;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.phase = SessionPhase::Stopped;
    }

    fn active_turn_mut(&mut self, turn_id: &TurnId) -> Option<&mut ActiveTurn> {
        match &mut self.phase {
            SessionPhase::Running(active) if &active.id == turn_id => Some(active),
            SessionPhase::Idle | SessionPhase::Running(_) | SessionPhase::Stopped => None,
        }
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Export state types**

Add to `crates/lato-core/src/lib.rs`:

```rust
mod state;

pub use state::{
    ActiveTurn, SessionMachine, SessionPhase, StartDecision, TransitionError,
};
```

- [ ] **Step 5: Run state and crate tests**

```bash
cargo test -p lato-core --test state_machine
cargo test -p lato-core
```

Expected: both commands PASS.

- [ ] **Step 6: Commit the state machine**

```bash
git add crates/lato-core/src crates/lato-core/tests/state_machine.rs
git commit -m "feat: add deterministic session state machine"
```

---

### Task 5: Create the cancellable Tokio session runtime

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/lato-runtime/Cargo.toml`
- Create: `crates/lato-runtime/src/lib.rs`
- Create: `crates/lato-runtime/src/driver.rs`
- Create: `crates/lato-runtime/src/session.rs`
- Create: `crates/lato-runtime/tests/session_runtime.rs`

**Interfaces:**
- Consumes: all public Task 3/4 types.
- Produces: `TurnDriver`, `TurnRequest`, `TurnControl`, `TurnEventEmitter`, `SessionHandle`, and `spawn_session`.

- [ ] **Step 1: Add the runtime crate and dependencies**

Add `"crates/lato-runtime"` to workspace members. Create:

```toml
[package]
name = "lato-runtime"
version = "0.1.0"
edition = "2024"

[dependencies]
async-trait = "0.1"
lato-core = { path = "../lato-core" }
tokio = { version = "1", features = ["macros", "rt", "sync", "time"] }
tokio-util = { version = "0.7", features = ["rt"] }

[dev-dependencies]
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }
```

- [ ] **Step 2: Write failing async contract tests**

Create `crates/lato-runtime/tests/session_runtime.rs`:

```rust
use async_trait::async_trait;
use lato_core::{
    CancelReason, Command, EventPayload, StartBehavior, StartTurn, TurnOutput, UserInput,
};
use lato_runtime::{
    TurnControl, TurnDriver, TurnEventEmitter, TurnRequest, spawn_session,
};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

struct EchoDriver;

#[async_trait]
impl TurnDriver for EchoDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        events.model_delta(request.input.text.clone())?;
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }
}

struct BlockingDriver;

#[async_trait]
impl TurnDriver for BlockingDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        mut control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(TurnOutput { final_text: String::new() }),
            steer = control.steering.recv() => Ok(TurnOutput {
                final_text: format!("steered:{}", steer.unwrap().text),
            }),
        }
    }
}

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
) -> lato_core::EventEnvelope {
    timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

#[tokio::test]
async fn emits_ordered_start_delta_and_completion() {
    let session = spawn_session("session-1".into(), Arc::new(EchoDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let delta = next_event(&mut events).await;
    let completed = next_event(&mut events).await;

    assert!(matches!(started.payload, EventPayload::SessionStarted));
    assert!(matches!(turn_started.payload, EventPayload::TurnStarted));
    assert_eq!(delta.payload, EventPayload::ModelDelta { text: "hello".into() });
    assert_eq!(
        completed.payload,
        EventPayload::TurnCompleted(TurnOutput { final_text: "hello".into() }),
    );
    assert_eq!(
        [started.sequence, turn_started.sequence, delta.sequence, completed.sequence],
        [1, 2, 3, 4],
    );
}

#[tokio::test]
async fn steer_is_delivered_to_the_active_turn() {
    let session = spawn_session("session-2".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("start"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::SteerTurn(UserInput::text("more")))
        .await
        .unwrap();

    let mut final_text = None;
    for _ in 0..4 {
        if let EventPayload::TurnCompleted(output) = next_event(&mut events).await.payload {
            final_text = Some(output.final_text);
            break;
        }
    }
    assert_eq!(final_text.as_deref(), Some("steered:more"));
}

#[tokio::test]
async fn replace_cancels_before_starting_the_pending_turn() {
    let session = spawn_session("session-3".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Replace,
        }))
        .await
        .unwrap();

    let mut saw_cancel = false;
    let mut saw_second_start_after_cancel = false;
    for _ in 0..6 {
        match next_event(&mut events).await.payload {
            EventPayload::TurnCancelled { reason: CancelReason::Replaced } => saw_cancel = true,
            EventPayload::TurnStarted if saw_cancel => {
                saw_second_start_after_cancel = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_cancel);
    assert!(saw_second_start_after_cancel);
}
```

- [ ] **Step 3: Run the runtime test to verify it fails**

```bash
cargo test -p lato-runtime --test session_runtime
```

Expected: FAIL because `lato-runtime` and its public interfaces do not exist.

- [ ] **Step 4: Implement the driver contract**

Create `crates/lato-runtime/src/driver.rs`:

```rust
// Derived from: Grok-Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: generalized the child reporter channel into a foreground turn event emitter

use async_trait::async_trait;
use lato_core::{AgentError, ErrorCategory, Retryability, TurnId, TurnOutput, UserInput};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct TurnRequest {
    pub turn_id: TurnId,
    pub input: UserInput,
}

pub struct TurnControl {
    pub cancellation: CancellationToken,
    pub steering: mpsc::UnboundedReceiver<UserInput>,
}

#[derive(Clone)]
pub struct TurnEventEmitter {
    pub(crate) turn_id: TurnId,
    pub(crate) tx: mpsc::UnboundedSender<DriverMessage>,
}

impl TurnEventEmitter {
    pub fn model_delta(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.send(DriverEvent::ModelDelta(text.into()))
    }

    pub fn reasoning_delta(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.send(DriverEvent::ReasoningDelta(text.into()))
    }

    fn send(&self, event: DriverEvent) -> Result<(), AgentError> {
        self.tx
            .send(DriverMessage::Event {
                turn_id: self.turn_id.clone(),
                event,
            })
            .map_err(|_| {
                AgentError::new(
                    "runtime.event_bus_closed",
                    ErrorCategory::InternalInvariant,
                    "runtime event bus closed",
                    Retryability::Never,
                )
            })
    }
}

#[async_trait]
pub trait TurnDriver: Send + Sync + 'static {
    async fn run(
        &self,
        request: TurnRequest,
        control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, AgentError>;
}

#[derive(Debug)]
pub(crate) enum DriverEvent {
    ModelDelta(String),
    ReasoningDelta(String),
}

#[derive(Debug)]
pub(crate) enum DriverMessage {
    Event { turn_id: TurnId, event: DriverEvent },
    Finished {
        turn_id: TurnId,
        result: Result<TurnOutput, AgentError>,
    },
}
```

- [ ] **Step 5: Implement the session actor loop**

Create `crates/lato-runtime/src/session.rs`:

```rust
// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: replaced Codex protocol operations with typed Lato Command/Event values

use crate::driver::{
    DriverEvent, DriverMessage, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
};
use lato_core::{
    AgentError, CancelReason, Command, ErrorCategory, EventEnvelope, EventId, EventPayload,
    Retryability, SessionId, SessionMachine, StartDecision, StartTurn, TransitionError, TurnId,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct SessionHandle {
    command_tx: mpsc::Sender<SubmittedCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
}

impl SessionHandle {
    pub async fn submit(&self, command: Command) -> Result<(), AgentError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(SubmittedCommand { command, reply_tx })
            .await
            .map_err(|_| bus_closed("runtime.command_bus_closed", "runtime command bus closed"))?;
        reply_rx
            .await
            .map_err(|_| bus_closed("runtime.reply_bus_closed", "runtime reply bus closed"))?
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_tx.subscribe()
    }
}

pub fn spawn_session(session_id: SessionId, driver: Arc<dyn TurnDriver>) -> SessionHandle {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
    let session = SessionLoop::new(session_id, driver, command_rx, event_tx.clone());
    tokio::spawn(session.run());
    SessionHandle { command_tx, event_tx }
}

struct SubmittedCommand {
    command: Command,
    reply_tx: oneshot::Sender<Result<(), AgentError>>,
}

struct ActiveRuntimeTurn {
    id: TurnId,
    cancellation: CancellationToken,
    steering: mpsc::UnboundedSender<lato_core::UserInput>,
    cancel_reason: Option<CancelReason>,
}

struct SessionLoop {
    session_id: SessionId,
    driver: Arc<dyn TurnDriver>,
    machine: SessionMachine,
    command_rx: mpsc::Receiver<SubmittedCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
    driver_tx: mpsc::UnboundedSender<DriverMessage>,
    driver_rx: mpsc::UnboundedReceiver<DriverMessage>,
    active: Option<ActiveRuntimeTurn>,
    pending_start: Option<StartTurn>,
    sequence: u64,
    started_emitted: bool,
}

impl SessionLoop {
    fn new(
        session_id: SessionId,
        driver: Arc<dyn TurnDriver>,
        command_rx: mpsc::Receiver<SubmittedCommand>,
        event_tx: broadcast::Sender<EventEnvelope>,
    ) -> Self {
        let (driver_tx, driver_rx) = mpsc::unbounded_channel();
        Self {
            session_id,
            driver,
            machine: SessionMachine::new(),
            command_rx,
            event_tx,
            driver_tx,
            driver_rx,
            active: None,
            pending_start: None,
            sequence: 0,
            started_emitted: false,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        self.shutdown();
                        break;
                    };
                    self.emit_session_started_once();
                    let result = self.handle_command(command.command);
                    let _ = command.reply_tx.send(result);
                    if matches!(self.machine.phase(), lato_core::SessionPhase::Stopped) {
                        break;
                    }
                }
                message = self.driver_rx.recv() => {
                    let Some(message) = message else {
                        self.shutdown();
                        break;
                    };
                    self.handle_driver_message(message);
                }
            }
        }
    }

    fn handle_command(&mut self, command: Command) -> Result<(), AgentError> {
        match command {
            Command::StartTurn(start) => self.request_start(start),
            Command::SteerTurn(input) => {
                let active = self.active.as_ref().ok_or_else(|| {
                    invalid_state("runtime.no_active_turn", "cannot steer an idle session")
                })?;
                active.steering.send(input).map_err(|_| {
                    bus_closed("runtime.steer_bus_closed", "turn steering bus closed")
                })
            }
            Command::CancelTurn { turn_id } => {
                self.machine
                    .request_cancel(&turn_id)
                    .map_err(transition_error)?;
                let active = self.active.as_mut().ok_or_else(|| {
                    invalid_state("runtime.no_active_turn", "cannot cancel an idle session")
                })?;
                active.cancel_reason = Some(CancelReason::User);
                active.cancellation.cancel();
                Ok(())
            }
            Command::Shutdown => {
                self.shutdown();
                Ok(())
            }
        }
    }

    fn request_start(&mut self, start: StartTurn) -> Result<(), AgentError> {
        let turn_id = next_turn_id();
        match self
            .machine
            .request_start(turn_id.clone(), start.behavior)
            .map_err(transition_error)?
        {
            StartDecision::StartNow => self.launch(turn_id, start),
            StartDecision::CancelThenStart { active, .. } => {
                let current = self.active.as_mut().ok_or_else(|| {
                    invalid_state("runtime.active_turn_missing", "state machine has no runtime turn")
                })?;
                if current.id != active {
                    return Err(invalid_state(
                        "runtime.turn_identity_mismatch",
                        "state machine and runtime turn IDs differ",
                    ));
                }
                current.cancel_reason = Some(CancelReason::Replaced);
                current.cancellation.cancel();
                self.pending_start = Some(start);
                Ok(())
            }
        }
    }

    fn launch(&mut self, turn_id: TurnId, start: StartTurn) -> Result<(), AgentError> {
        if self.active.is_some() {
            return Err(invalid_state(
                "runtime.active_turn_exists",
                "cannot launch a second foreground turn",
            ));
        }
        let cancellation = CancellationToken::new();
        let (steering, steering_rx) = mpsc::unbounded_channel();
        let driver = self.driver.clone();
        let driver_tx = self.driver_tx.clone();
        let emitter = TurnEventEmitter {
            turn_id: turn_id.clone(),
            tx: driver_tx.clone(),
        };
        let request = TurnRequest {
            turn_id: turn_id.clone(),
            input: start.input,
        };
        let control = TurnControl {
            cancellation: cancellation.clone(),
            steering: steering_rx,
        };
        let finished_turn_id = turn_id.clone();
        tokio::spawn(async move {
            let result = driver.run(request, control, emitter).await;
            let _ = driver_tx.send(DriverMessage::Finished {
                turn_id: finished_turn_id,
                result,
            });
        });
        self.active = Some(ActiveRuntimeTurn {
            id: turn_id.clone(),
            cancellation,
            steering,
            cancel_reason: None,
        });
        self.emit(Some(turn_id), EventPayload::TurnStarted);
        Ok(())
    }

    fn handle_driver_message(&mut self, message: DriverMessage) {
        match message {
            DriverMessage::Event { turn_id, event } => {
                if self.active.as_ref().map(|active| &active.id) != Some(&turn_id) {
                    return;
                }
                let payload = match event {
                    DriverEvent::ModelDelta(text) => EventPayload::ModelDelta { text },
                    DriverEvent::ReasoningDelta(text) => EventPayload::ReasoningDelta { text },
                };
                self.emit(Some(turn_id), payload);
            }
            DriverMessage::Finished { turn_id, result } => {
                let Some(active) = self.active.take() else {
                    return;
                };
                if active.id != turn_id {
                    self.active = Some(active);
                    return;
                }
                let _ = self.machine.finish(&turn_id);
                if let Some(reason) = active.cancel_reason {
                    self.emit(Some(turn_id), EventPayload::TurnCancelled { reason });
                } else {
                    match result {
                        Ok(output) => self.emit(Some(turn_id), EventPayload::TurnCompleted(output)),
                        Err(error) => {
                            self.emit(Some(turn_id), EventPayload::TurnFailed { error })
                        }
                    }
                }
                if let Some(start) = self.pending_start.take() {
                    let pending_id = next_turn_id();
                    if self
                        .machine
                        .request_start(pending_id.clone(), start.behavior)
                        .is_ok()
                    {
                        let _ = self.launch(pending_id, start);
                    }
                }
            }
        }
    }

    fn emit_session_started_once(&mut self) {
        if !self.started_emitted {
            self.started_emitted = true;
            self.emit(None, EventPayload::SessionStarted);
        }
    }

    fn shutdown(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            self.emit(
                Some(active.id),
                EventPayload::TurnCancelled {
                    reason: CancelReason::Shutdown,
                },
            );
        }
        self.machine.stop();
        self.emit(None, EventPayload::SessionStopped);
    }

    fn emit(&mut self, turn_id: Option<TurnId>, payload: EventPayload) {
        self.sequence += 1;
        let sequence = self.sequence;
        let envelope = EventEnvelope {
            schema_version: lato_core::EVENT_SCHEMA_VERSION,
            event_id: EventId::from(format!("{}-event-{sequence}", self.session_id)),
            session_id: self.session_id.clone(),
            turn_id,
            parent_event_id: None,
            sequence,
            timestamp_ms: now_ms(),
            payload,
        };
        let _ = self.event_tx.send(envelope);
    }
}

fn next_turn_id() -> TurnId {
    TurnId::from(format!(
        "turn-{}",
        NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn transition_error(error: TransitionError) -> AgentError {
    invalid_state("runtime.invalid_transition", error.to_string())
}

fn invalid_state(code: &str, message: impl Into<String>) -> AgentError {
    AgentError::new(
        code,
        ErrorCategory::InvalidInput,
        message,
        Retryability::RequiresDecision,
    )
}

fn bus_closed(code: &str, message: &str) -> AgentError {
    AgentError::new(
        code,
        ErrorCategory::InternalInvariant,
        message,
        Retryability::Never,
    )
}
```

- [ ] **Step 6: Export runtime interfaces**

Create `crates/lato-runtime/src/lib.rs`:

```rust
mod driver;
mod session;

pub use driver::{TurnControl, TurnDriver, TurnEventEmitter, TurnRequest};
pub use session::{SessionHandle, spawn_session};
```

- [ ] **Step 7: Run the runtime tests**

```bash
cargo test -p lato-runtime --test session_runtime -- --nocapture
cargo test -p lato-runtime
```

Expected: all tests PASS. If the replacement test times out, inspect event ordering; do not increase its one-second timeout to hide a deadlock.

- [ ] **Step 8: Run dependency-boundary checks**

```bash
cargo tree -p lato-core
cargo tree -p lato-runtime
```

Expected: `lato-core` contains only Serde/thiserror dependencies; `lato-runtime` does not list `lato-agent`, `lato-ai`, `lato-tools`, or `lato-workspace`.

- [ ] **Step 9: Commit the runtime**

```bash
git add Cargo.toml Cargo.lock crates/lato-runtime
git commit -m "feat: add cancellable lato session runtime"
```

---

### Task 6: Complete runtime edge-case contracts

**Files:**
- Modify: `crates/lato-runtime/tests/session_runtime.rs`
- Modify: `crates/lato-runtime/src/session.rs` only if a new test exposes a defect.

**Interfaces:**
- Consumes: `SessionHandle::submit`, `SessionHandle::subscribe`, and the runtime semantics from Task 5.
- Produces: regression coverage for rejection, explicit cancellation, shutdown, stale driver events, and bounded lag.

- [ ] **Step 1: Add a test that rejects a second start in Reject mode**

Append:

```rust
#[tokio::test]
async fn reject_mode_does_not_start_a_second_turn() {
    let session = spawn_session("session-reject".into(), Arc::new(BlockingDriver));
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.invalid_transition");
}
```

- [ ] **Step 2: Add explicit cancellation and shutdown tests**

Append:

```rust
#[tokio::test]
async fn cancel_targets_the_active_turn_and_emits_user_reason() {
    let session = spawn_session("session-cancel".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let turn_id = turn_started.turn_id.unwrap();
    assert!(matches!(started.payload, EventPayload::SessionStarted));
    session
        .submit(Command::CancelTurn { turn_id })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::User,
        },
    );
}

#[tokio::test]
async fn shutdown_stops_the_session_and_rejects_future_commands() {
    let session = spawn_session("session-stop".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    assert!(matches!(next_event(&mut events).await.payload, EventPayload::SessionStarted));
    assert!(matches!(next_event(&mut events).await.payload, EventPayload::TurnStarted));
    session.submit(Command::Shutdown).await.unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::Shutdown,
        },
    );
    assert!(matches!(next_event(&mut events).await.payload, EventPayload::SessionStopped));
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("after stop"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.command_bus_closed");
}
```

- [ ] **Step 3: Run the new focused tests**

```bash
cargo test -p lato-runtime reject_mode_does_not_start_a_second_turn -- --exact --nocapture
cargo test -p lato-runtime cancel_targets_the_active_turn_and_emits_user_reason -- --exact --nocapture
cargo test -p lato-runtime shutdown_stops_the_session_and_rejects_future_commands -- --exact --nocapture
```

Expected: all three PASS. If any test fails, make the minimum change in `session.rs` while preserving earlier event ordering tests.

- [ ] **Step 4: Run the complete core/runtime suite repeatedly**

```bash
for run in 1 2 3 4 5; do
  cargo test -p lato-core -p lato-runtime || exit 1
done
```

Expected: five consecutive passes with no timeout or ordering flake.

- [ ] **Step 5: Commit edge-case coverage**

```bash
git add crates/lato-runtime/src/session.rs crates/lato-runtime/tests/session_runtime.rs
git commit -m "test: harden lato runtime lifecycle contracts"
```

---

### Task 7: Document the phase boundary and run repository gates

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: the completed `lato-core` and `lato-runtime` crates.
- Produces: an accurate repository status statement and a verified locally installed binary.

- [ ] **Step 1: Add a concise architecture status section to README**

Append before the existing Security section:

```markdown
## Runtime architecture

Lato is migrating to a typed Command/Event core without replacing the working CLI in one step.
`lato-core` owns provider-independent IDs, errors, commands, events, and session invariants.
`lato-runtime` owns the cancellable Tokio session loop and the `TurnDriver` boundary.
The current ACP host remains the production path until the Phase 1B adapter is complete; this keeps
headless, interactive, provider, tool, approval, and transcript behavior stable during migration.

Copied or structurally derived upstream code is pinned in
`docs/superpowers/reference/lato-upstream-sources.md`.
```

- [ ] **Step 2: Verify formatting and static analysis**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -p lato-core -p lato-runtime --all-targets -- -D warnings
```

Expected: both commands exit 0 with no warnings.

- [ ] **Step 3: Run focused and workspace tests**

```bash
cargo test -p lato-core -p lato-runtime
cargo test --workspace
```

Expected: all tests PASS. Existing acceptance tests must remain unchanged and green.

- [ ] **Step 4: Check for accidental changes to user-owned dirty files**

Run:

```bash
git status --short
git diff --name-only
```

Expected: Phase 1A changes are limited to the files listed in this plan. Pre-existing user changes may still appear, but their diffs must not have grown because of this phase.

- [ ] **Step 5: Install the local command**

```bash
cargo install --path .
lato -p "reply with hi only"
```

Expected: installation succeeds and the smoke command prints a response containing `hi` with exit code 0.

- [ ] **Step 6: Commit the README update**

```bash
git add README.md
git commit -m "docs: describe lato core runtime migration"
```

- [ ] **Step 7: Record the final phase verification without modifying acceptance semantics**

Run:

```bash
git log --oneline --max-count=7
git status --short
```

Expected: the Task 1-7 commits are visible; only unrelated pre-existing user changes remain uncommitted.

## Phase 1A Completion Gate

Phase 1A is complete only when all statements are true:

- `lato-core` has no runtime/provider/tool/client dependency.
- `lato-runtime` has no dependency on the existing agent implementation.
- Command/Event JSON shapes are tested and versioned.
- The state machine rejects a second foreground turn unless replacement is explicit.
- Replacement emits cancellation before the pending turn starts.
- Steering reaches the active driver through a dedicated channel.
- Cancellation uses a hierarchical-capable `CancellationToken`.
- Runtime event sequences are strictly increasing within a session.
- Runtime tests pass five consecutive times without ordering flakes.
- Existing workspace tests pass without changing acceptance expectations.
- `cargo install --path .` succeeds and the installed `lato` smoke test works.
- The source ledger pins every structurally derived implementation to an exact upstream commit.

## Self-Review Mapping

| Approved design requirement | Implemented by |
|---|---|
| Typed Command/Event boundary | Tasks 2-3 |
| Structured error categories and retryability | Task 2 |
| One active foreground turn | Task 4 |
| Explicit reject/replace behavior | Tasks 4-6 |
| Cancellation and steering primitives | Task 5 |
| Ordered session event envelopes | Tasks 3 and 5 |
| Fixed channel capacities | Task 5 |
| Codex/Grok Build reuse provenance | Tasks 1, 4, and 5 |
| Provider/client independence | Tasks 2, 5, and 7 |
| Preserve current runnable Lato | Task 7 |

Phase 1B must not begin until this completion gate passes.
