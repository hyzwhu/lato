# Lato Phase 2A Ports and Tool Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add provider-independent model and tool ports to `lato-core` plus a deterministic layered `ToolCatalog` in `lato-tools`, without changing the current production model/tool loop.

**Architecture:** `lato-core` owns object-safe contracts and serializable types; `lato-tools` owns registration and replacement policy. Existing `ModelStream`, `SessionActor`, centralized registry, and dispatch remain untouched until Phase 2B. The new types are independently contract-tested so current implementations can be adapted incrementally.

**Tech Stack:** Rust 2024, Serde, `async-trait`, `futures-core`, `futures-util`, `semver`, Tokio `CancellationToken`, existing `lato-core` and `lato-tools` crates.

## Global Constraints

- Follow `docs/superpowers/specs/2026-09-01-lato-phase-2a-ports-registry-design.md` exactly.
- Do not modify or stage the pre-existing user-owned dirty files or `AGENTS.md`.
- In particular, do not modify `crates/lato-agent/src/actor.rs`, any current dirty `lato-ai` file, `crates/lato-tools/src/registry.rs`, `crates/lato-tools/src/dispatch.rs`, `crates/lato-tools/src/edit.rs`, `src/cli.rs`, or `tests/cli_headless.rs`.
- `lato-core` must not depend on any Lato crate, HTTP client, provider SDK, CLI, ACP, filesystem tool, or workspace policy implementation.
- `lato-runtime` must remain independent of `lato-agent`, `lato-ai`, `lato-tools`, and `lato-workspace`.
- All traits used across adapter boundaries must be object-safe and usable behind `Arc<dyn Trait>`.
- All persisted/wire enums use explicit `snake_case` serialization.
- Tool replacement is fail-closed and atomic: any rejection leaves the original entry unchanged.
- Existing production behavior and workspace acceptance tests must stay unchanged.
- Run relevant tests before each commit and deploy with `cargo install --path .` after the phase.
- AgentField contract checked for this phase: `2026-03-24-v1`; Phase 2A adds no AgentField dependency or live model execution.

## Starting Worktree Guard

Before Task 1, write hashes outside the repository:

```bash
shasum -a 256 \
  crates/lato-agent/src/actor.rs \
  crates/lato-ai/src/api.rs \
  crates/lato-ai/src/models_file.rs \
  crates/lato-ai/src/stream.rs \
  crates/lato-tools/src/dispatch.rs \
  crates/lato-tools/src/edit.rs \
  crates/lato-tools/src/registry.rs \
  src/cli.rs tests/cli_headless.rs \
  > /tmp/lato-phase2a-user-dirty.sha256
```

Every task ends with:

```bash
shasum -a 256 -c /tmp/lato-phase2a-user-dirty.sha256
```

---

### Task 1: Define the object-safe tool contract

**Files:**
- Modify: `crates/lato-core/Cargo.toml`
- Modify: `crates/lato-core/src/id.rs`
- Create: `crates/lato-core/src/tool.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/tests/tool_contract.rs`

**Interfaces:**
- Consumes: existing `SessionId`, `TurnId`, `AgentError`, `ErrorCategory`, and `Retryability`.
- Produces: `ToolCallId`, `ToolName`, `ToolDescriptor`, `ToolReplacement`, `ToolSource`, `ToolLayer`, `ToolCapability`, `SideEffect`, `ToolConcurrency`, `ToolIdempotency`, `ToolCancellation`, `ToolContext`, `ToolOutput`, `ToolError`, `DescriptorError`, and object-safe `Tool`.

- [ ] **Step 1: Add core dependencies**

Add to `crates/lato-core/Cargo.toml`:

```toml
[dependencies]
async-trait = "0.1"
futures-core = "0.3"
semver = { version = "1", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio-util = { version = "0.7", features = ["rt"] }

[dev-dependencies]
futures-util = "0.3"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

Do not add Tokio runtime itself to normal dependencies.

- [ ] **Step 2: Add the tool-call ID**

Append to `crates/lato-core/src/id.rs`:

```rust
string_id!(ToolCallId);
string_id!(ModelCallId);
```

Export both IDs from `lib.rs`. `ModelCallId` is added now so Task 2 does not reopen the ID macro file.

- [ ] **Step 3: Write failing tool wire and validation tests**

Create `crates/lato-core/tests/tool_contract.rs`:

```rust
use lato_core::{
    DescriptorError, ErrorCategory, Retryability, SideEffect, ToolCancellation, ToolCapability,
    ToolConcurrency, ToolDescriptor, ToolIdempotency, ToolLayer, ToolName, ToolReplacement,
    ToolSource,
};
use semver::Version;

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: ToolName::parse("lato:read_file").unwrap(),
        version: Version::new(1, 2, 0),
        description: "Read a UTF-8 file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        capabilities: vec![ToolCapability::FileRead],
        side_effect: SideEffect::WorkspaceRead,
        concurrency: ToolConcurrency::Parallel,
        idempotency: ToolIdempotency::Idempotent,
        timeout_ms: 30_000,
        max_output_bytes: 64 * 1024,
        cancellation: ToolCancellation::Cooperative,
        source: ToolSource {
            layer: ToolLayer::Builtin,
            id: "lato".into(),
            replacement: None,
        },
    }
}

#[test]
fn qualified_names_and_descriptors_have_stable_json() {
    let descriptor = descriptor();
    descriptor.validate().unwrap();
    let value = serde_json::to_value(descriptor).unwrap();
    assert_eq!(value["name"], "lato:read_file");
    assert_eq!(value["version"], "1.2.0");
    assert_eq!(value["side_effect"], "workspace_read");
    assert_eq!(value["source"]["layer"], "builtin");
}

#[test]
fn tool_names_are_strictly_qualified() {
    assert!(ToolName::parse("read_file").is_err());
    assert!(ToolName::parse(":read_file").is_err());
    assert!(ToolName::parse("lato:read file").is_err());
    assert_eq!(ToolName::parse("lato:read_file").unwrap().namespace(), "lato");
}

#[test]
fn descriptor_validation_rejects_unsafe_empty_limits() {
    let mut value = descriptor();
    value.timeout_ms = 0;
    assert_eq!(value.validate(), Err(DescriptorError::ZeroTimeout));
    value.timeout_ms = 1;
    value.max_output_bytes = 0;
    assert_eq!(value.validate(), Err(DescriptorError::ZeroOutputLimit));
}

#[test]
fn descriptor_rejects_duplicate_capabilities() {
    let mut value = descriptor();
    value.capabilities.push(ToolCapability::FileRead);
    assert_eq!(value.validate(), Err(DescriptorError::DuplicateCapability));
}

#[test]
fn tool_errors_convert_without_string_classification() {
    let error = lato_core::ToolError::new(
        "tool.invalid_arguments",
        "path is required",
        Retryability::Never,
    );
    let agent: lato_core::AgentError = error.into();
    assert_eq!(agent.category, ErrorCategory::Tool);
    assert_eq!(agent.code, "tool.invalid_arguments");
}

#[test]
fn replacement_shape_is_explicit() {
    let replacement = ToolReplacement {
        target: ToolName::parse("lato:read_file").unwrap(),
        compatible_major: 1,
    };
    assert_eq!(serde_json::to_value(replacement).unwrap()["compatible_major"], 1);
}
```

- [ ] **Step 4: Run the test to verify it fails**

```bash
cargo test -p lato-core --test tool_contract
```

Expected: FAIL with unresolved tool-contract imports.

- [ ] **Step 5: Implement the tool types and trait**

Create `crates/lato-core/src/tool.rs` with these exact public shapes:

```rust
use crate::{AgentError, ErrorCategory, Retryability, SessionId, ToolCallId, TurnId};
use async_trait::async_trait;
use semver::Version;
use serde_json::Value;
use std::{collections::HashSet, fmt, str::FromStr};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ToolNameError> {
        let value = value.into();
        let Some((namespace, name)) = value.split_once(':') else {
            return Err(ToolNameError::MissingNamespace);
        };
        if namespace.is_empty() || name.is_empty() {
            return Err(ToolNameError::EmptyPart);
        }
        if value.matches(':').count() != 1
            || !namespace.chars().all(valid_name_char)
            || !name.chars().all(valid_name_char)
        {
            return Err(ToolNameError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }
    pub fn namespace(&self) -> &str { self.0.split_once(':').unwrap().0 }
    pub fn local_name(&self) -> &str { self.0.split_once(':').unwrap().1 }
}

fn valid_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ToolName {
    type Err = ToolNameError;
    fn from_str(value: &str) -> Result<Self, Self::Err> { Self::parse(value) }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolNameError {
    #[error("tool name must include a namespace")]
    MissingNamespace,
    #[error("tool namespace and name must not be empty")]
    EmptyPart,
    #[error("tool name contains an invalid character")]
    InvalidCharacter,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability { FileRead, FileWrite, Process, Network, Memory, Task, Other(String) }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect { None, WorkspaceRead, WorkspaceWrite, ExternalMutation }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency { Parallel, Serial, ResourceKeyed }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdempotency { Idempotent, WithKey, NonIdempotent }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCancellation { Cooperative, KillProcess, Unsupported }

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLayer { Builtin, User, TrustedProject, SessionOverride }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolReplacement { pub target: ToolName, pub compatible_major: u64 }

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolSource {
    pub layer: ToolLayer,
    pub id: String,
    pub replacement: Option<ToolReplacement>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolDescriptor {
    pub name: ToolName,
    pub version: Version,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: Vec<ToolCapability>,
    pub side_effect: SideEffect,
    pub concurrency: ToolConcurrency,
    pub idempotency: ToolIdempotency,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub cancellation: ToolCancellation,
    pub source: ToolSource,
}

impl ToolDescriptor {
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if self.description.trim().is_empty() { return Err(DescriptorError::EmptyDescription); }
        if !self.input_schema.is_object() { return Err(DescriptorError::SchemaNotObject); }
        if self.timeout_ms == 0 { return Err(DescriptorError::ZeroTimeout); }
        if self.max_output_bytes == 0 { return Err(DescriptorError::ZeroOutputLimit); }
        let mut capabilities = HashSet::new();
        if self.capabilities.iter().any(|value| !capabilities.insert(value)) {
            return Err(DescriptorError::DuplicateCapability);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DescriptorError {
    #[error("tool description must not be empty")] EmptyDescription,
    #[error("tool input schema must be a JSON object")] SchemaNotObject,
    #[error("tool timeout must be greater than zero")] ZeroTimeout,
    #[error("tool output limit must be greater than zero")] ZeroOutputLimit,
    #[error("tool capabilities must not contain duplicates")] DuplicateCapability,
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolOutput {
    pub content: String,
    pub metadata: Value,
    pub truncated: bool,
    pub artifact_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryability: Retryability,
}

impl ToolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryability: Retryability) -> Self {
        Self { code: code.into(), message: message.into(), retryability }
    }
}

impl From<ToolError> for AgentError {
    fn from(error: ToolError) -> Self {
        AgentError::new(error.code, ErrorCategory::Tool, error.message, error.retryability)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    async fn invoke(&self, context: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError>;
}
```

- [ ] **Step 6: Export and verify**

Add `mod tool;` and export every public tool type from `lib.rs`. Add `ModelCallId` and `ToolCallId` to ID exports.

Run:

```bash
cargo test -p lato-core --test tool_contract
cargo test -p lato-core
cargo clippy -p lato-core --all-targets --no-deps -- -D warnings
```

Expected: all pass and an `Arc<dyn Tool>` compile assertion is included in the test.

- [ ] **Step 7: Commit**

```bash
git add Cargo.lock crates/lato-core
git commit -m "feat: define lato tool contracts"
```

---

### Task 2: Define the streaming model port

**Files:**
- Create: `crates/lato-core/src/model.rs`
- Modify: `crates/lato-core/src/lib.rs`
- Create: `crates/lato-core/tests/model_port.rs`

**Interfaces:**
- Consumes: `ModelCallId`, `ToolCallId`, `ToolName`, `ToolDescriptor`, `AgentError`, and `Retryability`.
- Produces: `ModelSelection`, `ModelMessage`, `ModelRole`, `ModelContent`, `SamplingParameters`, `ToolChoice`, `ModelRequest`, `ModelCapabilities`, `ToolCallDelta`, `ModelUsage`, `ModelStopReason`, `ModelStreamEvent`, `ModelEventStream`, `ModelError`, and object-safe `ModelPort`.

- [ ] **Step 1: Write failing serialization and object-safety tests**

Create `crates/lato-core/tests/model_port.rs`:

```rust
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use lato_core::{
    AgentError, ErrorCategory, ModelCapabilities, ModelContent, ModelError, ModelEventStream,
    ModelMessage, ModelPort, ModelRequest, ModelRole, ModelSelection, ModelStopReason,
    ModelStreamEvent, ModelUsage, Retryability, SamplingParameters, ToolChoice,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct ScriptedPort;

#[async_trait]
impl ModelPort for ScriptedPort {
    async fn stream(
        &self,
        _request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(ModelStreamEvent::TextDelta { text: "hi".into() }),
            Ok(ModelStreamEvent::Usage(ModelUsage {
                input_tokens: Some(3), output_tokens: Some(1),
                reasoning_tokens: None, cached_input_tokens: None,
            })),
            Ok(ModelStreamEvent::Completed { reason: ModelStopReason::Completed }),
        ])))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        call_id: "model-call-1".into(),
        selection: ModelSelection::new("openai", "gpt-test").unwrap(),
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text { text: "hello".into() }],
        }],
        tools: Vec::new(),
        parameters: SamplingParameters {
            temperature: None,
            max_output_tokens: Some(32),
            tool_choice: Some(ToolChoice::Auto),
            response_schema: None,
        },
    }
}

#[tokio::test]
async fn model_port_is_object_safe_and_streams_canonical_events() {
    let port: Arc<dyn ModelPort> = Arc::new(ScriptedPort);
    let events = port.stream(request(), CancellationToken::new()).await.unwrap();
    let events: Vec<_> = events.collect().await;
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], Ok(ModelStreamEvent::TextDelta { .. })));
    assert!(matches!(events[2], Ok(ModelStreamEvent::Completed { .. })));
}

#[tokio::test]
async fn pre_cancelled_requests_fail_with_typed_model_error() {
    let token = CancellationToken::new();
    token.cancel();
    let error = match ScriptedPort.stream(request(), token).await {
        Ok(_) => panic!("pre-cancelled request unexpectedly produced a stream"),
        Err(error) => error,
    };
    assert_eq!(error.code, "model.cancelled");
    let agent: AgentError = error.into();
    assert_eq!(agent.category, ErrorCategory::Model);
}

#[test]
fn model_events_have_stable_tagged_json() {
    let event = ModelStreamEvent::Completed { reason: ModelStopReason::ToolCalls };
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["type"], "completed");
    assert_eq!(value["reason"], "tool_calls");
}

#[test]
fn selection_rejects_empty_parts() {
    assert!(ModelSelection::new("", "model").is_err());
    assert!(ModelSelection::new("provider", " ").is_err());
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p lato-core --test model_port
```

Expected: FAIL with unresolved model-port imports.

- [ ] **Step 3: Implement model types and port**

Create `crates/lato-core/src/model.rs`:

```rust
use crate::{
    AgentError, ErrorCategory, ModelCallId, Retryability, ToolCallId, ToolDescriptor, ToolName,
};
use async_trait::async_trait;
use futures_core::Stream;
use serde_json::Value;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelSelection { pub provider: String, pub model: String }

impl ModelSelection {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Result<Self, ModelSelectionError> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() { return Err(ModelSelectionError::EmptyProvider); }
        if model.trim().is_empty() { return Err(ModelSelectionError::EmptyModel); }
        Ok(Self { provider, model })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelSelectionError {
    #[error("model provider must not be empty")] EmptyProvider,
    #[error("model ID must not be empty")] EmptyModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole { System, User, Assistant, Tool }

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContent {
    Text { text: String },
    Image { media_type: String, data: String },
    ToolCall { call_id: ToolCallId, name: ToolName, arguments: Value },
    ToolResult { call_id: ToolCallId, output: String },
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelMessage { pub role: ModelRole, pub content: Vec<ModelContent> }

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SamplingParameters {
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub response_schema: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice { Auto, None, Required, Specific(ToolName) }

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelRequest {
    pub call_id: ModelCallId,
    pub selection: ModelSelection,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDescriptor>,
    pub parameters: SamplingParameters,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelCapabilities {
    pub tool_use: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: bool,
    pub vision: bool,
    pub structured_output: bool,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    pub call_id: Option<ToolCallId>,
    pub name: Option<ToolName>,
    pub arguments_delta: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason { Completed, ToolCalls, Length, ContentFilter, Cancelled, Other(String) }

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallDelta(ToolCallDelta),
    Usage(ModelUsage),
    Completed { reason: ModelStopReason },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ModelError {
    pub code: String,
    pub message: String,
    pub retryability: Retryability,
}

impl ModelError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryability: Retryability) -> Self {
        Self { code: code.into(), message: message.into(), retryability }
    }
    pub fn cancelled() -> Self {
        Self::new("model.cancelled", "model request cancelled", Retryability::Never)
    }
}

impl From<ModelError> for AgentError {
    fn from(error: ModelError) -> Self {
        AgentError::new(error.code, ErrorCategory::Model, error.message, error.retryability)
    }
}

pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ModelError>> + Send + 'static>>;

#[async_trait]
pub trait ModelPort: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;
    fn capabilities(&self) -> ModelCapabilities;
}
```

- [ ] **Step 4: Export and verify**

Add `mod model;` and export all public model types from `lib.rs`.

Run:

```bash
cargo test -p lato-core --test model_port
cargo test -p lato-core
cargo clippy -p lato-core --all-targets --no-deps -- -D warnings
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-core/src/model.rs crates/lato-core/src/lib.rs crates/lato-core/tests/model_port.rs
git commit -m "feat: define lato streaming model port"
```

---

### Task 3: Implement the layered `ToolCatalog`

**Files:**
- Modify: `crates/lato-tools/Cargo.toml`
- Create: `crates/lato-tools/src/catalog.rs`
- Modify: `crates/lato-tools/src/lib.rs`
- Create: `crates/lato-tools/tests/tool_catalog.rs`

**Interfaces:**
- Consumes: `Tool`, `ToolDescriptor`, `ToolLayer`, `ToolName`, and `DescriptorError`.
- Produces: `ToolCatalog`, `RegistrationOutcome`, and `CatalogError`.

- [ ] **Step 1: Add the one-way core dependency**

Add to `crates/lato-tools/Cargo.toml`:

```toml
lato-core = { path = "../lato-core" }
async-trait = "0.1"
semver = { version = "1", features = ["serde"] }
```

`async-trait` and `semver` are used by catalog integration tests and future adapters; do not add a dependency on `lato-runtime` or `lato-agent`.

- [ ] **Step 2: Write failing catalog tests**

Create `crates/lato-tools/tests/tool_catalog.rs` with a `FakeTool` implementing `Tool`. Its descriptor constructor accepts name, version, layer, and optional replacement. Add these exact tests:

```rust
use async_trait::async_trait;
use lato_core::{
    SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput,
    ToolReplacement, ToolSource,
};
use lato_tools::{CatalogError, RegistrationOutcome, ToolCatalog};
use semver::Version;
use std::sync::Arc;

struct FakeTool { descriptor: ToolDescriptor }

#[async_trait]
impl Tool for FakeTool {
    fn descriptor(&self) -> ToolDescriptor { self.descriptor.clone() }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: self.descriptor.name.to_string(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn tool(
    name: &str,
    major: u64,
    layer: ToolLayer,
    replacement: Option<ToolReplacement>,
) -> Arc<dyn Tool> {
    Arc::new(FakeTool {
        descriptor: ToolDescriptor {
            name: ToolName::parse(name).unwrap(),
            version: Version::new(major, 0, 0),
            description: format!("test tool {name}"),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![ToolCapability::Other("test".into())],
            side_effect: SideEffect::None,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource { layer, id: format!("source-{layer:?}"), replacement },
        },
    })
}

#[test]
fn catalog_lists_descriptors_in_qualified_name_order() {
    let mut catalog = ToolCatalog::new();
    catalog.register(tool("zeta:run", 1, ToolLayer::Builtin, None)).unwrap();
    catalog.register(tool("alpha:read", 1, ToolLayer::Builtin, None)).unwrap();
    let names: Vec<_> = catalog.descriptors().into_iter().map(|value| value.name.to_string()).collect();
    assert_eq!(names, ["alpha:read", "zeta:run"]);
}

#[test]
fn duplicate_without_replacement_is_rejected_atomically() {
    let mut catalog = ToolCatalog::new();
    catalog.register(tool("lato:read", 1, ToolLayer::Builtin, None)).unwrap();
    let error = catalog.register(tool("lato:read", 1, ToolLayer::User, None)).unwrap_err();
    assert!(matches!(error, CatalogError::ReplacementRequired { .. }));
    assert_eq!(catalog.descriptor(&"lato:read".parse().unwrap()).unwrap().source.layer, ToolLayer::Builtin);
}

#[test]
fn lower_or_equal_layer_cannot_replace() {
    let mut catalog = ToolCatalog::new();
    let target = ToolName::parse("lato:read").unwrap();
    catalog.register(tool(target.as_str(), 1, ToolLayer::User, None)).unwrap();
    let replacement = Some(ToolReplacement { target, compatible_major: 1 });
    let error = catalog.register(tool("lato:read", 1, ToolLayer::User, replacement)).unwrap_err();
    assert!(matches!(error, CatalogError::LowerOrEqualLayer { .. }));
}

#[test]
fn replacement_target_and_major_must_match() {
    let mut catalog = ToolCatalog::new();
    catalog.register(tool("lato:read", 1, ToolLayer::Builtin, None)).unwrap();
    let wrong_target = Some(ToolReplacement {
        target: ToolName::parse("lato:write").unwrap(), compatible_major: 1,
    });
    assert!(matches!(
        catalog.register(tool("lato:read", 1, ToolLayer::User, wrong_target)).unwrap_err(),
        CatalogError::ReplacementTargetMismatch { .. }
    ));
    let wrong_major = Some(ToolReplacement {
        target: ToolName::parse("lato:read").unwrap(), compatible_major: 2,
    });
    assert!(matches!(
        catalog.register(tool("lato:read", 2, ToolLayer::User, wrong_major)).unwrap_err(),
        CatalogError::IncompatibleMajorVersion { .. }
    ));
}

#[test]
fn explicit_compatible_higher_layer_replacement_succeeds() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog.register(tool(name.as_str(), 1, ToolLayer::Builtin, None)).unwrap();
    let replacement = Some(ToolReplacement { target: name.clone(), compatible_major: 1 });
    assert_eq!(
        catalog.register(tool(name.as_str(), 1, ToolLayer::User, replacement)).unwrap(),
        RegistrationOutcome::Replaced,
    );
    assert_eq!(catalog.descriptor(&name).unwrap().source.layer, ToolLayer::User);
}
```

- [ ] **Step 3: Run the test to verify it fails**

```bash
cargo test -p lato-tools --test tool_catalog
```

Expected: FAIL because `ToolCatalog` does not exist.

- [ ] **Step 4: Implement the catalog**

Create `crates/lato-tools/src/catalog.rs`:

```rust
use lato_core::{DescriptorError, Tool, ToolDescriptor, ToolLayer, ToolName};
use std::{collections::BTreeMap, sync::Arc};

struct RegisteredTool {
    descriptor: ToolDescriptor,
    tool: Arc<dyn Tool>,
}

#[derive(Default)]
pub struct ToolCatalog {
    tools: BTreeMap<ToolName, RegisteredTool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationOutcome { Inserted, Replaced }

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid descriptor for {name}: {source}")]
    InvalidDescriptor { name: ToolName, source: DescriptorError },
    #[error("tool {name} already exists and replacement was not declared")]
    ReplacementRequired { name: ToolName },
    #[error("tool {name} cannot be replaced from an equal or lower layer")]
    LowerOrEqualLayer { name: ToolName, existing: ToolLayer, incoming: ToolLayer },
    #[error("replacement target {target} does not match tool {name}")]
    ReplacementTargetMismatch { name: ToolName, target: ToolName },
    #[error("replacement for {name} is not compatible with major version {existing_major}")]
    IncompatibleMajorVersion { name: ToolName, existing_major: u64, incoming_major: u64, declared_major: u64 },
}

impl ToolCatalog {
    pub fn new() -> Self { Self::default() }
    pub fn len(&self) -> usize { self.tools.len() }
    pub fn is_empty(&self) -> bool { self.tools.is_empty() }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError> {
        let descriptor = tool.descriptor();
        descriptor.validate().map_err(|source| CatalogError::InvalidDescriptor {
            name: descriptor.name.clone(), source,
        })?;
        let name = descriptor.name.clone();
        let Some(existing) = self.tools.get(&name) else {
            self.tools.insert(name, RegisteredTool { descriptor, tool });
            return Ok(RegistrationOutcome::Inserted);
        };
        if descriptor.source.layer <= existing.descriptor.source.layer {
            return Err(CatalogError::LowerOrEqualLayer {
                name, existing: existing.descriptor.source.layer, incoming: descriptor.source.layer,
            });
        }
        let Some(replacement) = &descriptor.source.replacement else {
            return Err(CatalogError::ReplacementRequired { name });
        };
        if replacement.target != name {
            return Err(CatalogError::ReplacementTargetMismatch {
                name, target: replacement.target.clone(),
            });
        }
        let existing_major = existing.descriptor.version.major;
        let incoming_major = descriptor.version.major;
        if replacement.compatible_major != existing_major || incoming_major != existing_major {
            return Err(CatalogError::IncompatibleMajorVersion {
                name, existing_major, incoming_major, declared_major: replacement.compatible_major,
            });
        }
        self.tools.insert(name, RegisteredTool { descriptor, tool });
        Ok(RegistrationOutcome::Replaced)
    }

    pub fn resolve(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|entry| entry.tool.clone())
    }

    pub fn descriptor(&self, name: &ToolName) -> Option<&ToolDescriptor> {
        self.tools.get(name).map(|entry| &entry.descriptor)
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|entry| entry.descriptor.clone()).collect()
    }
}
```

If borrow checking requires extracting existing metadata before insertion, copy only layer and major into local variables; do not clone or remove the existing implementation before every validation succeeds.

- [ ] **Step 5: Export and verify**

Add to `crates/lato-tools/src/lib.rs`:

```rust
pub mod catalog;
pub use catalog::*;
```

Run:

```bash
cargo test -p lato-tools --test tool_catalog
cargo test -p lato-core -p lato-tools
cargo clippy -p lato-core -p lato-tools --all-targets --no-deps -- -D warnings
```

Expected: all pass without modifying legacy `registry.rs` or `dispatch.rs`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.lock crates/lato-tools/Cargo.toml crates/lato-tools/src/catalog.rs \
  crates/lato-tools/src/lib.rs crates/lato-tools/tests/tool_catalog.rs
git commit -m "feat: add layered lato tool catalog"
```

---

### Task 4: Harden contracts and record upstream provenance

**Files:**
- Modify: `crates/lato-core/tests/model_port.rs`
- Modify: `crates/lato-core/tests/tool_contract.rs`
- Modify: `crates/lato-tools/tests/tool_catalog.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`

**Interfaces:**
- Consumes: all Phase 2A public types.
- Produces: regression coverage for stream failures, unknown usage, custom enum arms, catalog atomicity, resolved invocation, and source ledger records.

- [ ] **Step 1: Add model edge-case tests**

Add exact assertions:

```rust
#[test]
fn unknown_usage_is_not_serialized_as_zero() {
    let usage = ModelUsage {
        input_tokens: None, output_tokens: None,
        reasoning_tokens: None, cached_input_tokens: None,
    };
    let value = serde_json::to_value(usage).unwrap();
    assert!(value["input_tokens"].is_null());
    assert!(value["output_tokens"].is_null());
}

#[test]
fn provider_specific_stop_reasons_remain_explicit() {
    let reason = ModelStopReason::Other("safety_review".into());
    let encoded = serde_json::to_string(&reason).unwrap();
    let decoded: ModelStopReason = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, reason);
}
```

Add this scripted stream-failure fixture and test:

```rust
struct InterruptedPort;

#[async_trait]
impl ModelPort for InterruptedPort {
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        Ok(Box::pin(stream::iter(vec![Err(ModelError::new(
            "model.stream_interrupted",
            "connection closed",
            Retryability::Safe,
        ))])))
    }

    fn capabilities(&self) -> ModelCapabilities { ModelCapabilities::default() }
}

#[tokio::test]
async fn established_stream_reports_interruptions_as_items() {
    let mut events = InterruptedPort
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let error = events.next().await.unwrap().unwrap_err();
    assert_eq!(error.code, "model.stream_interrupted");
    assert_eq!(error.retryability, Retryability::Safe);
    assert!(events.next().await.is_none());
}
```

- [ ] **Step 2: Add tool and catalog atomicity tests**

Add these tests and helper. Extend imports with `SessionId`, `ToolCallId`, `TurnId`, and `CancellationToken`:

```rust
fn context() -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("tool-call-1"),
        cancellation: CancellationToken::new(),
    }
}

#[test]
fn rejected_replacement_does_not_mutate_the_existing_descriptor() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog.register(tool(name.as_str(), 1, ToolLayer::Builtin, None)).unwrap();
    let before = catalog.descriptor(&name).cloned().unwrap();
    let incompatible = Some(ToolReplacement { target: name.clone(), compatible_major: 2 });
    assert!(catalog
        .register(tool(name.as_str(), 2, ToolLayer::User, incompatible))
        .is_err());
    assert_eq!(catalog.descriptor(&name), Some(&before));
}

#[tokio::test]
async fn resolved_tool_invokes_through_the_object_safe_membrane() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog.register(tool(name.as_str(), 1, ToolLayer::Builtin, None)).unwrap();
    let resolved = catalog.resolve(&name).unwrap();
    let output = resolved.invoke(context(), serde_json::json!({})).await.unwrap();
    assert_eq!(output.content, name.as_str());
}
```

- [ ] **Step 3: Record source reuse**

Append ledger rows:

```markdown
| `crates/lato-core/src/tool.rs` | Grok Build `crates/common/xai-tool-runtime/src/tool.rs` and `dispatch.rs` | Structural derivation | Object safety, JSON membrane, typed errors | Reduced to Lato core descriptor/invoke contracts; streaming tool output deferred |
| `crates/lato-tools/src/catalog.rs` | Codex `codex-rs/core/src/tools/registry.rs` | Structural derivation | Duplicate rejection, deterministic registration, runtime/spec separation | Added explicit layer and semver replacement rules |
| `crates/lato-core/src/model.rs` | Codex `codex-rs/model-provider/src/provider.rs` | Structural derivation | Object-safe provider boundary and capabilities | Added normalized Lato request and stream event types |
```

Add exact provenance headers to those three Rust files with pinned commit, Apache-2.0, and Lato changes. Do not claim copied code where only API ideas were used; use `Derived from`.

- [ ] **Step 4: Run repeated suites and dependency checks**

```bash
for run in 1 2 3 4 5; do
  cargo test -p lato-core -p lato-tools || exit 1
done
cargo tree -p lato-core
cargo tree -p lato-tools
cargo clippy -p lato-core -p lato-tools --all-targets --no-deps -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: five passes; `lato-core` has no Lato/provider/client/workspace dependency; current dirty files are unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/lato-core/tests/model_port.rs crates/lato-core/tests/tool_contract.rs \
  crates/lato-tools/tests/tool_catalog.rs crates/lato-core/src/model.rs \
  crates/lato-core/src/tool.rs crates/lato-tools/src/catalog.rs \
  docs/superpowers/reference/lato-upstream-sources.md
git commit -m "test: harden lato port and catalog contracts"
```

---

### Task 5: Document Phase 2A and run repository gates

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: completed model/tool contracts and catalog.
- Produces: accurate migration status and installed local command.

- [ ] **Step 1: Update the runtime architecture section**

Append after the `LegacyTurnDriver` paragraph:

```markdown
Phase 2A adds provider-independent `ModelPort` and `Tool` contracts in `lato-core`, plus a layered,
fail-closed `ToolCatalog` in `lato-tools`. The current provider and built-in tool implementations still
run through compatibility code; Phase 2B will adapt them one at a time without changing these contracts.
```

- [ ] **Step 2: Run focused and workspace gates**

```bash
cargo fmt --all -- --check
cargo clippy -p lato-core -p lato-tools --all-targets --no-deps -- -D warnings
cargo test -p lato-core -p lato-tools
cargo test --workspace
```

If the existing local SSE fixture reports transient `WouldBlock (os 35)`, rerun that exact test and the full workspace suite. Do not change its timeout or acceptance behavior.

- [ ] **Step 3: Verify boundaries**

```bash
cargo tree -p lato-core | rg 'lato-(runtime|agent|ai|tools|workspace)' && exit 1 || true
cargo tree -p lato-runtime | rg 'lato-(agent|ai|tools|workspace)' && exit 1 || true
rg -n 'pub trait (ModelPort|Tool)|pub struct ToolCatalog' \
  crates/lato-core/src crates/lato-tools/src
```

Expected: dependency checks produce no forbidden match; all three interfaces are found.

- [ ] **Step 4: Deploy and smoke test**

```bash
cargo install --path .
lato -p "reply with hi only"
```

Expected: install succeeds and output contains `hi`.

- [ ] **Step 5: Verify dirty-file preservation**

```bash
shasum -a 256 -c /tmp/lato-phase2a-user-dirty.sha256
git status --short
git diff --check
```

Expected: every hash is `OK`; only the original user changes and `AGENTS.md` remain outside committed Phase 2A work.

- [ ] **Step 6: Commit documentation**

```bash
git add README.md
git commit -m "docs: describe lato phase 2a extension ports"
```

## Phase 2A Completion Gate

- `Arc<dyn ModelPort>` and `Arc<dyn Tool>` compile and are exercised.
- Model events express text, reasoning, tool-call deltas, usage, completion, and stream errors.
- Tool descriptor carries every field required by the approved design.
- ToolCatalog registration and listing are deterministic.
- Same-name replacement requires a strictly higher layer and compatible explicit declaration.
- Every rejected replacement leaves the existing registration unchanged.
- `lato-core` has no dependency on another Lato crate or concrete provider/tool implementation.
- Legacy provider/tool/actor production files remain unchanged.
- Workspace tests and installed command smoke pass.
- Source ledger pins every structurally derived Phase 2A implementation.

Phase 2B must not begin until this gate passes.
