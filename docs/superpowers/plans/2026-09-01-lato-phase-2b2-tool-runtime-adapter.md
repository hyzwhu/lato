# Lato Phase 2B-2 Tool Runtime Adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route all nine existing built-in tools through an extensible `ToolRuntime`/`ToolCatalog` membrane while preserving current model schemas, approvals, sandboxing, history, events, and output behavior.

**Architecture:** `SessionActor` receives one immutable `Arc<ToolRuntime>` and uses it for both model-visible definitions and execution. The runtime normalizes legacy wire names, resolves an independently registered `Arc<dyn Tool>`, and invokes it with typed session/turn/call/cancellation context; first-generation built-in adapters delegate to the existing dispatcher.

**Tech Stack:** Rust 2024, Tokio, `tokio-util::sync::CancellationToken`, `async-trait`, Serde JSON, SemVer, existing `lato-core`, `lato-tools`, `lato-agent`, and `lato-runtime` contracts.

## Global Constraints

- Preserve all pre-existing uncommitted user changes; capture a Phase 2B-2 baseline before edits and stage only intentional hunks.
- Continue on the user-approved current `master` branch; do not reset, discard, or overwrite unrelated work.
- `ToolCatalog` must be the production source for both advertised and executable tools.
- Model-facing names, descriptions, JSON schemas, arguments, history, events, CLI, ACP, and textual result formatting must remain compatible.
- Keep `dispatch.rs` and `registry.rs` as compatibility authorities; do not behaviorally rewrite them in this phase.
- New or overriding tools must register without changes to `SessionActor`.
- Internal names are qualified as `builtin:<local-name>`; aliases `Lato:<name>` and `write` are accepted but not advertised.
- Keep `lato-core` independent of provider, CLI, workspace, and tool implementation crates.
- Use stable internal error codes: `tool.not_found`, `tool.invalid_arguments`, `tool.policy_denied`, `tool.cancelled`, `tool.execution_failed`, and `tool.timeout`.
- Do not add a model-backed AgentField graph for deterministic Rust routing.
- Run relevant tests before deployment, then run `cargo install --path .` from the repository root.
- Directly copied or structurally derived Codex/Grok code must retain its Apache-2.0 provenance header and pinned commit.

## File Map

- Create `crates/lato-tools/src/builtin_adapter.rs`: built-in execution environment, descriptor conversion, one `Tool` adapter per built-in registration, and legacy-error classification.
- Create `crates/lato-tools/src/runtime.rs`: wire-name normalization, immutable runtime, model-definition projection, typed invocation, runtime build validation, and builder.
- Modify `crates/lato-tools/src/lib.rs`: export the adapter/runtime modules.
- Create `crates/lato-tools/tests/tool_runtime.rs`: catalog parity, aliases, invocation, cancellation, policy, custom registration, and replacement tests.
- Modify `crates/lato-agent/Cargo.toml`: add the `tokio-util` runtime dependency needed to accept typed cancellation tokens.
- Modify `crates/lato-agent/src/actor.rs`: own/inject the runtime, carry typed context, advertise catalog definitions, and invoke through the runtime.
- Modify `crates/lato-agent/src/legacy_driver.rs`: pass the existing runtime `TurnId` and `CancellationToken` into the actor.
- Modify `crates/lato-agent/tests/legacy_driver.rs`: verify cancellation reaches the tool membrane and normal calls retain turn identity.
- Create `tests/tool_runtime_wiring.rs`: source-level production-wiring guard against direct actor calls to the legacy registry/dispatcher.
- Do not edit protected `crates/lato-tools/src/dispatch.rs` or `crates/lato-tools/src/registry.rs`.

---

### Task 1: Protect the dirty worktree and define executable built-in adapters

**Files:**
- Create: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Test: `crates/lato-tools/tests/tool_runtime.rs`

**Interfaces:**
- Consumes: `lato_core::Tool`, `ToolDescriptor`, `ToolContext`, `ToolOutput`, `ToolError`; `lato_workspace::{FileLocks, SessionTrust}`; existing `dispatch`, `v1_tool_definitions`, and `ToolCall`.
- Produces: `BuiltinToolEnvironment`, `builtin_tools(BuiltinToolEnvironment) -> Result<Vec<Arc<dyn Tool>>, BuiltinAdapterError>`, and nine independently registered adapters named `builtin:<name>`.

- [ ] **Step 1: Capture the protected baseline before any implementation edit**

Run:

```bash
git status --short > /tmp/lato-phase2b2-status-before.txt
git diff --binary > /tmp/lato-phase2b2-user-before.patch
cp crates/lato-agent/src/actor.rs /tmp/lato-phase2b2-actor-before.rs
shasum -a 256 \
  crates/lato-agent/src/actor.rs \
  crates/lato-ai/src/api.rs \
  crates/lato-ai/src/models_file.rs \
  crates/lato-ai/src/stream.rs \
  crates/lato-tools/src/dispatch.rs \
  crates/lato-tools/src/edit.rs \
  crates/lato-tools/src/registry.rs \
  src/cli.rs \
  tests/cli_headless.rs \
  > /tmp/lato-phase2b2-protected-before.sha256
git diff -- crates/lato-agent/src/actor.rs > /tmp/lato-phase2b2-actor-before.patch
```

Expected: all commands succeed; `/tmp/lato-phase2b2-*` contains the exact pre-implementation state.

- [ ] **Step 2: Write failing adapter contract tests**

Create `crates/lato-tools/tests/tool_runtime.rs` with these initial tests and shared context:

```rust
use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{BuiltinToolEnvironment, builtin_tools, v1_tool_definitions};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn context(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        cancellation,
    }
}

#[test]
fn builtin_adapters_match_the_v1_model_definition_set() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let mut actual = tools
        .iter()
        .map(|tool| tool.descriptor().name.local_name().to_owned())
        .collect::<Vec<_>>();
    let mut expected = v1_tool_definitions()
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| {
            definition.pointer("/function/name").unwrap().as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 9);
}

#[tokio::test]
async fn cancelled_adapter_does_not_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let read = tools
        .into_iter()
        .find(|tool| tool.descriptor().name.as_str() == "builtin:read_file")
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = read
        .invoke(context(cancellation), serde_json::json!({"path": "missing"}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.cancelled");
}
```

- [ ] **Step 3: Run the focused test and verify the missing API fails**

Run: `cargo test -p lato-tools --test tool_runtime -- --nocapture`

Expected: compilation fails because `BuiltinToolEnvironment` and `builtin_tools` do not exist.

- [ ] **Step 4: Implement the adapter membrane**

Create `builtin_adapter.rs` with this concrete structure:

```rust
use crate::{ToolCall, dispatch, v1_tool_definitions};
use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency,
    ToolContext, ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput,
    ToolSource,
};
use lato_workspace::{FileLocks, SessionTrust};
use semver::Version;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct BuiltinToolEnvironment {
    pub cwd: PathBuf,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
}

#[derive(Debug, thiserror::Error)]
pub enum BuiltinAdapterError {
    #[error("built-in tool definition is malformed: {0}")]
    InvalidDefinition(String),
    #[error("invalid built-in tool name: {0}")]
    InvalidName(#[from] lato_core::ToolNameError),
}

struct LegacyDispatchTool {
    descriptor: ToolDescriptor,
    legacy_name: String,
    environment: BuiltinToolEnvironment,
}

#[async_trait]
impl Tool for LegacyDispatchTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(&self, context: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(tool_error("tool.cancelled", "tool call was cancelled"));
        }
        let content = dispatch(
            &self.environment.locks,
            &self.environment.trust,
            &self.environment.cwd,
            ToolCall { name: self.legacy_name.clone(), arguments },
        )
        .await
        .map_err(classify_legacy_error)?;
        if context.cancellation.is_cancelled() {
            return Err(tool_error("tool.cancelled", "tool call was cancelled"));
        }
        Ok(ToolOutput {
            content,
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

pub fn builtin_tools(
    environment: BuiltinToolEnvironment,
) -> Result<Vec<Arc<dyn Tool>>, BuiltinAdapterError> {
    v1_tool_definitions()
        .as_array()
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("root must be an array".into()))?
        .iter()
        .map(|definition| adapter_from_definition(definition, environment.clone()))
        .collect()
}
```

Implement definition conversion exactly once:

```rust
fn adapter_from_definition(
    definition: &Value,
    environment: BuiltinToolEnvironment,
) -> Result<Arc<dyn Tool>, BuiltinAdapterError> {
    let function = definition
        .get("function")
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("missing function".into()))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("missing function.name".into()))?;
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition(format!("missing description for {name}")))?;
    let input_schema = function
        .get("parameters")
        .cloned()
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition(format!("missing parameters for {name}")))?;
    let (capabilities, side_effect, concurrency, idempotency, cancellation) = metadata(name)?;
    let descriptor = ToolDescriptor {
        name: ToolName::parse(format!("builtin:{name}"))?,
        version: Version::new(1, 0, 0),
        description: description.to_owned(),
        input_schema,
        capabilities,
        side_effect,
        concurrency,
        idempotency,
        timeout_ms: 20_000,
        max_output_bytes: 256 * 1024,
        cancellation,
        source: ToolSource {
            layer: ToolLayer::Builtin,
            id: format!("lato.builtin.{name}"),
            replacement: None,
        },
    };
    Ok(Arc::new(LegacyDispatchTool {
        descriptor,
        legacy_name: name.to_owned(),
        environment,
    }))
}
```

Use an exhaustive `match name` for metadata only:

```rust
fn metadata(name: &str) -> Result<(Vec<ToolCapability>, SideEffect, ToolConcurrency, ToolIdempotency, ToolCancellation), BuiltinAdapterError> {
    let value = match name {
        "read_file" | "list_dir" | "grep" => (
            vec![ToolCapability::FileRead], SideEffect::WorkspaceRead,
            ToolConcurrency::Parallel, ToolIdempotency::Idempotent, ToolCancellation::Cooperative,
        ),
        "write_file" | "search_replace" => (
            vec![ToolCapability::FileWrite], SideEffect::WorkspaceWrite,
            ToolConcurrency::ResourceKeyed, ToolIdempotency::NonIdempotent, ToolCancellation::Cooperative,
        ),
        "run_terminal_command" => (
            vec![ToolCapability::Process], SideEffect::ExternalMutation,
            ToolConcurrency::Serial, ToolIdempotency::NonIdempotent, ToolCancellation::Unsupported,
        ),
        "web_fetch" => (
            vec![ToolCapability::Network], SideEffect::WorkspaceRead,
            ToolConcurrency::Parallel, ToolIdempotency::Idempotent, ToolCancellation::Cooperative,
        ),
        "spawn_subagent" => (
            vec![ToolCapability::Task, ToolCapability::FileWrite], SideEffect::WorkspaceWrite,
            ToolConcurrency::Serial, ToolIdempotency::NonIdempotent, ToolCancellation::Unsupported,
        ),
        "todo_write" => (
            vec![ToolCapability::Task], SideEffect::None,
            ToolConcurrency::Serial, ToolIdempotency::Idempotent, ToolCancellation::Cooperative,
        ),
        other => return Err(BuiltinAdapterError::InvalidDefinition(format!("unknown built-in {other}"))),
    };
    Ok(value)
}
```

Classify only stable, known legacy messages:

```rust
fn classify_legacy_error(message: String) -> ToolError {
    let code = if message.starts_with("missing ") {
        "tool.invalid_arguments"
    } else if message.contains("permission required") || message.contains("denied by") {
        "tool.policy_denied"
    } else {
        "tool.execution_failed"
    };
    tool_error(code, message)
}

fn tool_error(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message, Retryability::Never)
}
```

Export the module from `lib.rs` with `pub mod builtin_adapter;` and `pub use builtin_adapter::*;`.

- [ ] **Step 5: Run focused tests and commit**

Run:

```bash
cargo test -p lato-tools --test tool_runtime -- --nocapture
cargo fmt --check
git diff --check
```

Expected: the two adapter tests pass and all checks exit zero.

Commit only the new adapter, test, and export hunks:

```bash
git add crates/lato-tools/src/builtin_adapter.rs crates/lato-tools/src/lib.rs crates/lato-tools/tests/tool_runtime.rs
git commit -m "feat: adapt built-in tools to tool contract"
```

### Task 2: Build the catalog-authoritative runtime and extension builder

**Files:**
- Create: `crates/lato-tools/src/runtime.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Modify: `crates/lato-tools/tests/tool_runtime.rs`

**Interfaces:**
- Consumes: `builtin_tools`, `BuiltinToolEnvironment`, `ToolCatalog`, `ToolName`, and `Arc<dyn Tool>`.
- Produces: `ToolRuntimeBuilder::{new, register, register_builtin_tools, build}`, `ToolRuntime::{model_definitions, invoke}`, and `RuntimeBuildError`.

- [ ] **Step 1: Add failing runtime, alias, and extension tests**

Append tests that assert:

```rust
#[tokio::test]
async fn runtime_advertises_and_executes_the_same_tools() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello").unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let definitions = runtime.model_definitions();
    assert_eq!(definitions.len(), 9);
    for name in ["read_file", "Lato:read_file", "builtin:read_file"] {
        let output = runtime
            .invoke(context(CancellationToken::new()), name, serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "hello");
    }
}

#[tokio::test]
async fn unknown_wire_name_is_typed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let error = runtime
        .invoke(context(CancellationToken::new()), "missing", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.not_found");
}
```

Add this reusable fake and the two construction tests:

```rust
struct FakeTool {
    descriptor: lato_core::ToolDescriptor,
    output: String,
}

#[async_trait::async_trait]
impl lato_core::Tool for FakeTool {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, lato_core::ToolError> {
        Ok(lato_core::ToolOutput {
            content: self.output.clone(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn fake_descriptor(
    name: &str,
    layer: lato_core::ToolLayer,
    replacement: Option<lato_core::ToolReplacement>,
) -> lato_core::ToolDescriptor {
    lato_core::ToolDescriptor {
        name: lato_core::ToolName::parse(name).unwrap(),
        version: semver::Version::new(1, 0, 0),
        description: format!("fake {name}"),
        input_schema: serde_json::json!({"type": "object"}),
        capabilities: vec![lato_core::ToolCapability::Other("test".into())],
        side_effect: lato_core::SideEffect::None,
        concurrency: lato_core::ToolConcurrency::Parallel,
        idempotency: lato_core::ToolIdempotency::Idempotent,
        timeout_ms: 1_000,
        max_output_bytes: 1_024,
        cancellation: lato_core::ToolCancellation::Cooperative,
        source: lato_core::ToolSource { layer, id: format!("test.{name}"), replacement },
    }
}

#[tokio::test]
async fn higher_layer_replaces_a_builtin_without_duplicate_advertisement() {
    let root = tempfile::tempdir().unwrap();
    let mut builder = lato_tools::ToolRuntimeBuilder::new();
    builder.register_builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    }).unwrap();
    let target = lato_core::ToolName::parse("builtin:read_file").unwrap();
    builder.register(Arc::new(FakeTool {
        descriptor: fake_descriptor(
            target.as_str(),
            lato_core::ToolLayer::SessionOverride,
            Some(lato_core::ToolReplacement { target, compatible_major: 1 }),
        ),
        output: "replacement".into(),
    })).unwrap();
    let runtime = builder.build().unwrap();
    assert_eq!(
        runtime.model_definitions().iter().filter(|value| {
            value.pointer("/function/name").and_then(|name| name.as_str()) == Some("read_file")
        }).count(),
        1
    );
    let output = runtime.invoke(
        context(CancellationToken::new()), "read_file", serde_json::json!({}),
    ).await.unwrap();
    assert_eq!(output.content, "replacement");
}

#[test]
fn different_namespaces_cannot_advertise_the_same_local_name() {
    let mut builder = lato_tools::ToolRuntimeBuilder::new();
    for name in ["project:read", "session:read"] {
        builder.register(Arc::new(FakeTool {
            descriptor: fake_descriptor(name, lato_core::ToolLayer::Builtin, None),
            output: name.into(),
        })).unwrap();
    }
    let error = match builder.build() {
        Ok(_) => panic!("ambiguous wire names must fail the build"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        lato_tools::RuntimeBuildError::AmbiguousWireName { wire_name, .. }
            if wire_name == "read"
    ));
}
```

- [ ] **Step 2: Verify the runtime API is absent**

Run: `cargo test -p lato-tools --test tool_runtime -- --nocapture`

Expected: compilation fails for missing `ToolRuntimeBuilder`, `builtin_tool_runtime`, or `RuntimeBuildError`.

- [ ] **Step 3: Implement runtime construction and validation**

Create `runtime.rs` around these exact public signatures:

```rust
pub struct ToolRuntime {
    catalog: ToolCatalog,
    wire_names: std::collections::BTreeMap<String, ToolName>,
}

#[derive(Default)]
pub struct ToolRuntimeBuilder {
    catalog: ToolCatalog,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Builtin(#[from] BuiltinAdapterError),
    #[error("model-facing tool name {wire_name} is ambiguous between {first} and {second}")]
    AmbiguousWireName { wire_name: String, first: ToolName, second: ToolName },
}

impl ToolRuntimeBuilder {
    pub fn new() -> Self;
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError>;
    pub fn register_builtin_tools(&mut self, environment: BuiltinToolEnvironment) -> Result<(), RuntimeBuildError>;
    pub fn build(self) -> Result<ToolRuntime, RuntimeBuildError>;
}

impl ToolRuntime {
    pub fn model_definitions(&self) -> Vec<serde_json::Value>;
    pub async fn invoke(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;
}

pub fn builtin_tool_runtime(
    environment: BuiltinToolEnvironment,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError>;
```

During `build`, iterate `catalog.descriptors()`, index each `descriptor.name.local_name()`, and reject two different canonical names that project to the same wire name. Replacements of the same canonical name are already resolved by `ToolCatalog` and do not conflict.

Project each descriptor into the existing OpenAI-compatible shape:

```rust
serde_json::json!({
    "type": "function",
    "function": {
        "name": descriptor.name.local_name(),
        "description": descriptor.description,
        "parameters": descriptor.input_schema,
    }
})
```

Invocation rules are exact:

```rust
if context.cancellation.is_cancelled() {
    return Err(ToolError::new(
        "tool.cancelled",
        "tool call was cancelled",
        Retryability::Never,
    ));
}
let normalized = wire_name.strip_prefix("Lato:").unwrap_or(wire_name);
let normalized = if normalized == "write" { "write_file" } else { normalized };
let canonical = if normalized.contains(':') {
    ToolName::parse(normalized).ok()
} else {
    self.wire_names.get(normalized).cloned()
};
let Some(canonical) = canonical else {
    return Err(not_found(wire_name));
};
let Some(tool) = self.catalog.resolve(&canonical) else {
    return Err(not_found(wire_name));
};
tool.invoke(context, arguments).await
```

Export the new module from `lib.rs`.

- [ ] **Step 4: Prove catalog parity and replacement behavior**

Run:

```bash
cargo test -p lato-tools --test tool_runtime -- --nocapture
cargo test -p lato-tools --test tool_catalog -- --nocapture
cargo fmt --check
git diff --check
```

Expected: all runtime and existing catalog tests pass; nine model definitions are generated from resolved descriptors; replacement and ambiguity tests pass.

- [ ] **Step 5: Commit the runtime**

```bash
git add crates/lato-tools/src/runtime.rs crates/lato-tools/src/lib.rs crates/lato-tools/tests/tool_runtime.rs
git commit -m "feat: add catalog-driven tool runtime"
```

### Task 3: Route SessionActor definitions and execution through ToolRuntime

**Files:**
- Modify: `crates/lato-agent/Cargo.toml`
- Modify: `crates/lato-agent/src/actor.rs`
- Create: `tests/tool_runtime_wiring.rs`

**Interfaces:**
- Consumes: `builtin_tool_runtime`, `BuiltinToolEnvironment`, `ToolRuntime`, `ToolContext`, `SessionId`, `TurnId`, `ToolCallId`, and `CancellationToken`.
- Produces: `SessionActor::new_with_tool_runtime(...)`, `SessionActor::prompt_with_context(...)`, and an actor path with no direct `dispatch()` or `v1_tool_definitions()` call.

- [ ] **Step 1: Write the production-wiring guard before touching the dirty actor**

Create `tests/tool_runtime_wiring.rs`:

```rust
#[test]
fn actor_uses_the_tool_runtime_for_definitions_and_execution() {
    let actor = include_str!("../crates/lato-agent/src/actor.rs");
    assert!(!actor.contains("dispatch("), "actor must not dispatch tools directly");
    assert!(!actor.contains("v1_tool_definitions("), "actor must not own a second tool list");
    assert!(actor.contains("tool_runtime.model_definitions()"));
    assert!(actor.contains("tool_runtime.invoke("));
}
```

- [ ] **Step 2: Run the guard and verify the old wiring fails**

Run: `cargo test --test tool_runtime_wiring -- --nocapture`

Expected: FAIL because production actor still contains direct legacy calls.

- [ ] **Step 3: Add typed actor context and runtime injection**

Add `tokio-util = { version = "0.7", features = ["rt"] }` to `lato-agent` dependencies.

Replace direct tool imports with:

```rust
use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{
    BuiltinToolEnvironment, ToolRuntime, bound_tool_output, builtin_tool_runtime,
};
use tokio_util::sync::CancellationToken;
```

Add actor fields:

```rust
tool_runtime: Arc<ToolRuntime>,
session_id: SessionId,
turn_id: TurnId,
turn_cancellation: CancellationToken,
next_local_turn: u64,
next_local_call: u64,
```

Keep the existing constructor signature. It builds the default runtime from clones of `cwd`, `locks`, and `trust`, with a precise static-invariant message:

```rust
let tool_runtime = builtin_tool_runtime(BuiltinToolEnvironment {
    cwd: cwd.clone(),
    locks: locks.clone(),
    trust: trust.clone(),
})
.expect("static built-in tool descriptors must form a valid runtime");
Self::new_with_tool_runtime(stream, locks, trust, cwd, tool_runtime)
```

Add the injection constructor:

```rust
pub fn new_with_tool_runtime(
    stream: Arc<dyn ModelStream>,
    locks: Arc<FileLocks>,
    trust: SessionTrust,
    cwd: PathBuf,
    tool_runtime: Arc<ToolRuntime>,
) -> Self
```

Initialize local IDs as non-empty compatibility identities: `SessionId::from("local-session")`, `TurnId::from("local-turn-0")`, and a fresh `CancellationToken`. In `with_interactive_events`, parse the supplied session string before storing it and assign it to `self.session_id`.

- [ ] **Step 4: Split prompt entry from typed prompt execution**

Keep `prompt` for direct tests and old callers:

```rust
pub async fn prompt(&mut self, kind: PromptKind, text: String) -> Result<TurnOutcome, String> {
    self.next_local_turn += 1;
    let turn_id = TurnId::from(format!("local-turn-{}", self.next_local_turn));
    self.prompt_with_context(kind, text, turn_id, CancellationToken::new()).await
}
```

Move the existing prompt loop, unchanged except for tool runtime usage, into:

```rust
pub async fn prompt_with_context(
    &mut self,
    _kind: PromptKind,
    text: String,
    turn_id: TurnId,
    cancellation: CancellationToken,
) -> Result<TurnOutcome, String>
```

At entry assign `self.turn_id = turn_id` and `self.turn_cancellation = cancellation`. Treat either `self.cancelled` or `self.turn_cancellation.is_cancelled()` as cancellation at every existing cancellation check.

Build the model context with:

```rust
let tool_runtime = self.tool_runtime.clone();
let mut context = serde_json::json!({
    "messages": history_to_messages(&self.history),
    "tools": tool_runtime.model_definitions(),
});
```

- [ ] **Step 5: Replace direct dispatch with typed runtime invocation**

In `process_tool_call`, preserve persistence, approval, and event ordering. Immediately before execution, construct:

```rust
let call_id = ToolCallId::parse(id.clone()).unwrap_or_else(|_| {
    self.next_local_call += 1;
    ToolCallId::from(format!("local-tool-call-{}", self.next_local_call))
});
let context = ToolContext {
    session_id: self.session_id.clone(),
    turn_id: self.turn_id.clone(),
    call_id,
    cancellation: self.turn_cancellation.clone(),
};
let out = self
    .tool_runtime
    .invoke(context, &name, arguments)
    .await
    .map(|output| output.content)
    .unwrap_or_else(|error| format!("ERROR: {}", error.message));
```

Keep `bound_tool_output`, completed-output caching, and `HistoryItem::ToolResult` exactly after this block. Keep actor-side `requires_approval(&name)` unchanged because it owns interactive approval.

- [ ] **Step 6: Run focused actor and wiring tests**

Run:

```bash
cargo test -p lato-agent actor -- --nocapture
cargo test --test tool_runtime_wiring -- --nocapture
cargo test -p lato-tools --test tool_runtime -- --nocapture
cargo fmt --check
git diff --check
```

Expected: actor tests pass, source guard passes, and runtime tests remain green.

- [ ] **Step 7: Prove protected edits are isolated and commit**

Run:

```bash
git diff -- crates/lato-tools/src/dispatch.rs crates/lato-tools/src/registry.rs
git diff -- crates/lato-agent/src/actor.rs > /tmp/lato-phase2b2-actor-after.patch
git diff --word-diff=porcelain -- crates/lato-agent/src/actor.rs
```

Expected: dispatcher/registry diffs equal their pre-existing baseline and contain no Phase 2B-2 edits; actor diff contains the old user hunks plus only runtime/context integration.

Stage the actor with patch mode. Answer `y` only for hunks that add `ToolRuntime`, typed IDs/cancellation, `new_with_tool_runtime`, `prompt_with_context`, catalog definitions, or runtime invocation; answer `n` for every hunk already present in `/tmp/lato-phase2b2-actor-before.patch`. Use `s` to split a mixed hunk and `e` only when splitting cannot isolate it:

```bash
git add -p crates/lato-agent/src/actor.rs
git add crates/lato-agent/Cargo.toml tests/tool_runtime_wiring.rs
git diff --cached --check
git diff --cached -- crates/lato-agent/src/actor.rs
git commit -m "feat: route actor tools through tool runtime"
```

Before committing, compare the cached actor diff with `/tmp/lato-phase2b2-actor-before.patch`. The commit must include only intentional Phase 2B-2 actor hunks, `Cargo.toml`, and the wiring test.

### Task 4: Propagate runtime turn identity and cancellation through LegacyTurnDriver

**Files:**
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/tests/legacy_driver.rs`

**Interfaces:**
- Consumes: `SessionActor::prompt_with_context(PromptKind, String, TurnId, CancellationToken)`, `SessionActor::new_with_tool_runtime`, and existing `TurnRequest`/`TurnControl`.
- Produces: `LegacyTurnDriver::new_with_tool_runtime(...)` plus runtime-owned turn IDs and cancellation tokens in every production `ToolContext`.

- [ ] **Step 1: Add an instrumented custom tool test**

In `tests/legacy_driver.rs`, define the recording tool completely:

```rust
struct RecordingTool {
    tx: mpsc::UnboundedSender<(SessionId, lato_core::TurnId, lato_core::ToolCallId, bool)>,
}

#[async_trait]
impl lato_core::Tool for RecordingTool {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        lato_core::ToolDescriptor {
            name: lato_core::ToolName::parse("session:record").unwrap(),
            version: semver::Version::new(1, 0, 0),
            description: "record typed tool context".into(),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![lato_core::ToolCapability::Other("test".into())],
            side_effect: lato_core::SideEffect::None,
            concurrency: lato_core::ToolConcurrency::Serial,
            idempotency: lato_core::ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: lato_core::ToolCancellation::Cooperative,
            source: lato_core::ToolSource {
                layer: lato_core::ToolLayer::SessionOverride,
                id: "test.recording".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: lato_core::ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, lato_core::ToolError> {
        self.tx
            .send((
                context.session_id,
                context.turn_id,
                context.call_id,
                context.cancellation.is_cancelled(),
            ))
            .unwrap();
        Ok(lato_core::ToolOutput {
            content: "recorded".into(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}
```

Add `semver = "1"` to `crates/lato-agent` dev-dependencies for this descriptor. Build a `ToolRuntimeBuilder`, register `RecordingTool`, build the runtime, and construct the driver with the new injection constructor. Script `FakeModelStream` to emit call ID `record-call-1`, tool name `record`, and `{}` arguments, followed by a text completion. Compare the recorded `TurnId` to the observed `TurnStarted` event ID, compare `ToolCallId` to `record-call-1`, compare `SessionId` to `legacy-session`, and assert the cancellation flag is false.

Add a second scripted recording call, submit `CancelTurn` after `TurnStarted`, and assert either no recording arrives within 100 ms or its cancellation flag is true; the terminal runtime event must be `TurnCancelled`, never `TurnCompleted`.

- [ ] **Step 2: Run the identity test and verify it fails**

Run: `cargo test -p lato-agent --test legacy_driver -- --nocapture`

Expected: the new identity assertion fails because `LegacyTurnDriver` still calls the untyped `prompt` entry.

- [ ] **Step 3: Add the injectable driver constructor**

Keep `LegacyTurnDriver::new` source-compatible and delegate both constructors to a private initializer:

```rust
#[allow(clippy::too_many_arguments)]
pub fn new_with_tool_runtime(
    session_id: String,
    stream: Arc<dyn ModelStream>,
    locks: Arc<FileLocks>,
    trust: SessionTrust,
    cwd: PathBuf,
    passthrough: mpsc::UnboundedSender<serde_json::Value>,
    approval: Option<Arc<dyn ToolApproval>>,
    tool_runtime: Arc<lato_tools::ToolRuntime>,
) -> Self {
    let (actor_tx, actor_events) = mpsc::unbounded_channel();
    let actor = SessionActor::new_with_tool_runtime(stream, locks, trust, cwd, tool_runtime)
        .with_interactive_events(actor_tx, session_id, approval);
    Self {
        state: Mutex::new(LegacyState { actor, actor_events }),
        passthrough,
    }
}
```

The existing `new` continues to construct `SessionActor::new(...)`; do not duplicate the `LegacyState` assembly beyond these two short constructors.

- [ ] **Step 4: Pass the existing runtime context into the actor**

At the start of `TurnDriver::run`, retain:

```rust
let turn_id = request.turn_id.clone();
let cancellation = control.cancellation.clone();
```

Replace the prompt construction with:

```rust
let mut prompt = Box::pin(actor.prompt_with_context(
    kind,
    input.text,
    turn_id.clone(),
    cancellation.clone(),
));
```

Do not call `cancellation.cancel()` for steering. The runtime token belongs to the enclosing runtime turn; dropping the current prompt future and calling the existing actor-local `cancel()` is sufficient before starting the steered input with the same runtime identity.

- [ ] **Step 5: Run driver, runtime, and actor tests**

Run:

```bash
cargo test -p lato-agent --test legacy_driver -- --nocapture
cargo test -p lato-agent actor -- --nocapture
cargo test -p lato-runtime -- --nocapture
cargo fmt --check
git diff --check
```

Expected: identity and cancellation tests pass; existing steering and cancellation tests remain green.

- [ ] **Step 6: Commit context propagation**

```bash
git add crates/lato-agent/Cargo.toml crates/lato-agent/src/legacy_driver.rs crates/lato-agent/tests/legacy_driver.rs
git commit -m "feat: propagate turn context to tool calls"
```

### Task 5: Harden compatibility, policy, and extension behavior

**Files:**
- Modify: `crates/lato-tools/tests/tool_runtime.rs`
- Modify: `crates/lato-tools/src/runtime.rs`
- Modify: `crates/lato-tools/src/builtin_adapter.rs`
- Modify: `tests/tool_runtime_wiring.rs`

**Interfaces:**
- Consumes: completed `ToolRuntime`, built-in adapter, and actor integration.
- Produces: regression coverage proving the migration does not weaken approval or diverge advertised/executable tools.

- [ ] **Step 1: Add the complete compatibility matrix**

Add tests with concrete assertions for:

```rust
#[tokio::test]
async fn write_alias_consumes_allow_once_exactly_once() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), true);
    trust.allow_once();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: trust.clone(),
    })
    .unwrap();
    runtime
        .invoke(
            context(CancellationToken::new()),
            "write",
            serde_json::json!({"path": "a.txt", "contents": "one"}),
        )
        .await
        .unwrap();
    assert!(!trust.has_allow_once());
    let error = runtime
        .invoke(
            context(CancellationToken::new()),
            "write_file",
            serde_json::json!({"path": "b.txt", "contents": "two"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.policy_denied");
}

#[tokio::test]
async fn malformed_and_denied_calls_are_classified() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "SECRET=1").unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();

    let malformed = runtime
        .invoke(
            context(CancellationToken::new()),
            "read_file",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(malformed.code, "tool.invalid_arguments");

    let denied = runtime
        .invoke(
            context(CancellationToken::new()),
            "write_file",
            serde_json::json!({"path": ".env", "contents": "SECRET=2"}),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "tool.policy_denied");
    assert_eq!(std::fs::read_to_string(root.path().join(".env")).unwrap(), "SECRET=1");
}

#[test]
fn every_advertised_tool_has_one_executable_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    for definition in runtime.model_definitions() {
        let name = definition.pointer("/function/name").unwrap().as_str().unwrap();
        assert_eq!(
            runtime.descriptor_for_wire_name(name).unwrap().name.local_name(),
            name
        );
    }
}
```

Add the read-only `ToolRuntime::descriptor_for_wire_name(&str) -> Option<ToolDescriptor>` helper in Step 3 so the parity test never reaches into the catalog directly.

Extend `tool_runtime_wiring.rs` to assert that `actor.rs` contains `new_with_tool_runtime` and that `dispatch.rs` remains referenced only from `builtin_adapter.rs` in production source.

- [ ] **Step 2: Run the tests and observe any classification/parity failures**

Run:

```bash
cargo test -p lato-tools --test tool_runtime -- --nocapture
cargo test --test tool_runtime_wiring -- --nocapture
```

Expected: new tests either pass immediately or fail only at the precise missing helper/classification assertion.

- [ ] **Step 3: Apply the minimum regression fix**

If parity needs a public lookup, add exactly:

```rust
pub fn descriptor_for_wire_name(&self, wire_name: &str) -> Option<ToolDescriptor> {
    let canonical = self.resolve_wire_name(wire_name)?;
    self.catalog.descriptor(&canonical).cloned()
}
```

Factor the normalization already used by `invoke` into private `resolve_wire_name`; do not duplicate alias logic. The two expected classification branches are the existing `missing path` prefix and `denied by write policy` substring already defined in Task 1.

- [ ] **Step 4: Run all changed-crate tests and commit**

Run:

```bash
cargo test -p lato-core -- --nocapture
cargo test -p lato-tools -- --nocapture
cargo test -p lato-agent -- --nocapture
cargo test --test tool_runtime_wiring -- --nocapture
cargo fmt --check
git diff --check
```

Expected: every command exits zero.

Commit only the regression tests and any minimal fix they required:

```bash
git add crates/lato-tools/src/runtime.rs crates/lato-tools/src/builtin_adapter.rs crates/lato-tools/tests/tool_runtime.rs tests/tool_runtime_wiring.rs
git commit -m "test: harden tool runtime migration"
```

### Task 6: Run release gates, verify user changes, install, and smoke-test Lato

**Files:**
- Verify only; no planned source modification.

**Interfaces:**
- Consumes: the complete Phase 2B-2 implementation.
- Produces: release evidence and an installed `lato` binary.

- [ ] **Step 1: Run formatting, strict lint, and the full workspace suite**

Run:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --nocapture
git diff --check
cargo tree -p lato-core
```

Expected: all checks pass; `lato-core` has no dependency on other Lato crates, HTTP clients, or CLI crates.

- [ ] **Step 2: Verify the production wiring and protected worktree**

Run:

```bash
cargo test --test tool_runtime_wiring -- --nocapture
git diff -- crates/lato-tools/src/dispatch.rs > /tmp/lato-phase2b2-dispatch-after.patch
git diff -- crates/lato-tools/src/registry.rs > /tmp/lato-phase2b2-registry-after.patch
git diff -- crates/lato-agent/src/actor.rs > /tmp/lato-phase2b2-actor-final.patch
git status --short
```

Expected: wiring guard passes; dispatcher and registry still contain only the user's baseline changes; actor retains every baseline hunk plus the committed runtime integration; unrelated dirty-file hashes match `/tmp/lato-phase2b2-protected-before.sha256`.

- [ ] **Step 3: Install the repository binary**

Run: `cargo install --path .`

Expected: installation succeeds and reports the `lato` executable installed under Cargo's bin directory.

- [ ] **Step 4: Smoke-test the installed command and a real tool call**

Run:

```bash
lato --help
LATO_HOME="$(mktemp -d)" lato -p "Use read_file to read Cargo.toml, then reply with only the package name."
```

Expected: help exits zero; the prompt traverses the installed model/tool loop, executes `read_file` through `ToolRuntime`, and returns `lato` or the actual root package name. If the configured provider is temporarily unavailable, retain the complete error as evidence and run the repository's deterministic headless tool fixture; do not weaken the live-smoke criterion silently.

- [ ] **Step 5: Record the final commit and handoff evidence**

Run:

```bash
git log --oneline --decorate -8
git status --short
```

Expected: Phase 2B-2 has focused commits for adapters, runtime, actor wiring, turn propagation, and regression hardening; only pre-existing user-owned modifications remain unstaged. No additional commit is required for a verification-only task.
