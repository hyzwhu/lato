# Lato Phase 4C2 Grok-Style Automatic Compaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add provider-informed context accounting, 85-percent pre-sampling automatic compaction, and Grok Build-style model-switch compaction while preserving Lato's durable Phase 4C1 replacement protocol.

**Architecture:** `lato-core` owns pure context arithmetic and stable event values. `lato-ai` preserves model metadata and returns per-call usage without changing streamed content. `lato-agent` measures the exact history at each sample boundary and asks `lato-runtime` to run nested maintenance; the runtime remains the only writer of compaction checkpoints. Model endpoints become session-owned, and ACP/TUI switches enter one serialized `RuntimeSession` transaction.

**Tech Stack:** Rust 2024, Tokio, `async-trait`, `futures-util`, serde/serde_json, ACP JSON-RPC, Ratatui, existing Lato journal/projection store.

## Global Constraints

- Follow Grok Build commit `bb7f39d5858cbf5e00de639367f59debbdcb0138` for trigger order, token reseeding, and model-switch behavior.
- Keep the canonical event journal append-only; every history replacement uses the Phase 4C1 checkpoint-first store path.
- Use 85 percent as the default auto-compaction threshold.
- Check the threshold immediately before every model sample, including samples after tool calls in the same turn.
- Provider usage is `input_tokens + output_tokens`; never add cached input or reasoning tokens twice.
- Unknown or zero context windows report an estimate but do not trigger automatic compaction.
- Automatic compaction retains the enclosing `TurnId` and owns a distinct `CompactionId`.
- Cross-family switches compact immediately under the new model when model-authored history exists.
- Same-family switches to a smaller window defer their threshold check to the next pre-sampling boundary.
- Context-overflow retry, preflight recovery, suppression modes, lossy fallback, two-pass compaction, and prefire remain Phase 4C3 work.
- Preserve existing model files, journals, checkpoints, legacy streams, and manual `/compact` behavior.
- Add the pinned Grok Build source paths to `docs/superpowers/reference/lato-upstream-sources.md` for every structurally derived production file.
- Run relevant tests after each task; before delivery run format, workspace check, workspace tests, Clippy with warnings denied, `cargo install --path .`, and an installed-binary smoke test.

---

## File structure

- `crates/lato-core/src/compaction.rs`: pure context-ledger arithmetic, threshold policy, and reseeding.
- `crates/lato-core/src/event.rs`: live context usage event.
- `crates/lato-core/src/state.rs`: nested maintenance state owned by an active turn.
- `crates/lato-core/tests/compaction_contract.rs`: stable arithmetic and serde contracts.
- `crates/lato-ai/src/model_port_adapter.rs`: active model metadata and switch generation.
- `crates/lato-ai/src/stream.rs`: backward-compatible per-call report API and provider usage parsing.
- `crates/lato-ai/src/model_port_adapter/legacy_port.rs`: legacy-to-canonical usage bridge.
- `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`: canonical-to-legacy report bridge.
- `crates/lato-ai/src/catalog.rs`: built-in context-window and family metadata.
- `crates/lato-ai/src/models_file.rs`: optional custom-model metadata.
- `crates/lato-ai/src/codex/events.rs`: Codex Responses usage extraction.
- `crates/lato-agent/src/context_usage.rs`: history estimator and model-switch decision policy.
- `crates/lato-agent/src/actor.rs`: check-before-every-sample and usage confirmation.
- `crates/lato-agent/src/legacy_driver.rs`: driver/runtime automatic-maintenance request and endpoint switching.
- `crates/lato-agent/src/runtime_session.rs`: session-owned endpoint and serialized model-switch transaction.
- `crates/lato-agent/src/host.rs`: ACP endpoint construction and session-targeted switching.
- `crates/lato-runtime/src/driver.rs`: nested automatic-compaction request/reply protocol.
- `crates/lato-runtime/src/session.rs`: durable nested maintenance orchestration.
- `src/client.rs`: context/model-switch client updates and request method.
- `src/tui/backend.rs`: asynchronous model-switch command.
- `src/tui/mod.rs`: remove direct stream mutation.
- `src/tui/state.rs`, `src/tui/render.rs`: context and automatic-compaction presentation.
- `tests/session_compaction_cli.rs`: installed-path automatic compaction and restart coverage.
- `docs/superpowers/reference/lato-upstream-sources.md`: source ledger additions.

### Task 1: Define context accounting and nested-maintenance contracts

**Files:**
- Modify: `crates/lato-core/src/compaction.rs`
- Modify: `crates/lato-core/src/event.rs`
- Modify: `crates/lato-core/src/state.rs`
- Modify: `crates/lato-core/tests/compaction_contract.rs`
- Modify: `crates/lato-core/tests/state_machine.rs`

**Interfaces:**
- Consumes: existing `ModelUsage`, `ContextUsage`, `CompactionPolicy`, `CompactionId`, and `TurnId`.
- Produces: `ContextLedger`, `ContextLedger::observe`, `ContextLedger::measure`, `ContextLedger::reseed`, `ContextUsage::threshold_reached`, `EventPayload::ContextUsageUpdated`, `SessionMachine::request_turn_compaction`, and `SessionMachine::finish_turn_compaction`.

- [ ] **Step 1: Write failing accounting and threshold tests**

Add tests with these exact assertions:

```rust
#[test]
fn grok_default_threshold_is_eighty_five_percent() {
    assert_eq!(CompactionPolicy::default().threshold_percent, 85);
}

#[test]
fn provider_usage_becomes_the_confirmed_baseline_without_double_counting() {
    let mut ledger = ContextLedger::default();
    let usage = ModelUsage {
        input_tokens: Some(800),
        output_tokens: Some(100),
        reasoning_tokens: Some(60),
        cached_input_tokens: Some(400),
    };
    assert!(ledger.observe(&usage, 720));
    assert_eq!(ledger.measure(760, Some(1_000)).estimated_input_tokens, 940);
}

#[test]
fn missing_usage_does_not_erase_a_confirmed_baseline() {
    let mut ledger = ContextLedger::default();
    assert!(ledger.observe(&ModelUsage {
        input_tokens: Some(700), output_tokens: Some(100),
        reasoning_tokens: None, cached_input_tokens: None,
    }, 600));
    assert!(!ledger.observe(&ModelUsage {
        input_tokens: None, output_tokens: None,
        reasoning_tokens: Some(20), cached_input_tokens: Some(50),
    }, 650));
    assert_eq!(ledger.measure(700, None).estimated_input_tokens, 900);
}

#[test]
fn threshold_is_inclusive_and_unknown_windows_do_not_trigger() {
    let at = ContextLedger::default().measure(850, Some(1_000));
    let below = ContextLedger::default().measure(849, Some(1_000));
    let unknown = ContextLedger::default().measure(999_999, None);
    assert!(at.threshold_reached(85));
    assert!(!below.threshold_reached(85));
    assert!(!unknown.threshold_reached(85));
    assert_eq!(unknown.context_window, 0);
}

#[test]
fn replacement_reseed_scales_provider_overhead_and_caps_growth() {
    let mut ledger = ContextLedger::default();
    assert!(ledger.observe(&ModelUsage {
        input_tokens: Some(900), output_tokens: Some(100),
        reasoning_tokens: None, cached_input_tokens: None,
    }, 800));
    ledger.reseed(200);
    assert_eq!(ledger.measure(200, Some(2_000)).estimated_input_tokens, 250);
    ledger.reseed(2_000);
    assert_eq!(ledger.measure(2_000, Some(3_000)).estimated_input_tokens, 250);
}
```

Add state-machine tests proving a running turn can own one compaction, external
starts and manual compaction remain rejected, cancellation marks both scopes,
and finishing maintenance returns to the same running turn.

- [ ] **Step 2: Run the tests and verify failure**

Run:

```bash
cargo test -p lato-core --test compaction_contract --test state_machine
```

Expected: compilation fails because the ledger, context event, and nested
maintenance transitions do not exist and the default is still 80.

- [ ] **Step 3: Implement the pure accounting contract**

Add this state and behavior to `compaction.rs`:

```rust
pub const DEFAULT_COMPACTION_THRESHOLD_PERCENT: u8 = 85;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextLedger {
    confirmed_total_tokens: Option<u64>,
    estimate_at_confirmation: u64,
}

impl ContextLedger {
    pub fn observe(&mut self, usage: &crate::ModelUsage, history_estimate: u64) -> bool {
        let Some(total) = usage
            .input_tokens
            .unwrap_or(0)
            .checked_add(usage.output_tokens.unwrap_or(0))
        else {
            self.confirmed_total_tokens = Some(u64::MAX);
            self.estimate_at_confirmation = history_estimate;
            return true;
        };
        if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
            return false;
        }
        self.confirmed_total_tokens = Some(total);
        self.estimate_at_confirmation = history_estimate;
        true
    }

    pub fn measure(&self, history_estimate: u64, context_window: Option<u64>) -> ContextUsage {
        let estimated_input_tokens = self.confirmed_total_tokens.map_or(history_estimate, |base| {
            base.saturating_add(history_estimate.saturating_sub(self.estimate_at_confirmation))
        });
        let context_window = context_window.unwrap_or(0);
        let utilization_percent = if context_window == 0 {
            0
        } else {
            estimated_input_tokens.saturating_mul(100)
                .checked_div(context_window)
                .unwrap_or(0)
                .min(u64::from(u8::MAX)) as u8
        };
        ContextUsage { estimated_input_tokens, context_window, utilization_percent }
    }

    pub fn reseed(&mut self, replacement_estimate: u64) {
        let reseeded = match (self.confirmed_total_tokens, self.estimate_at_confirmation) {
            (Some(previous), old_estimate) if previous > 0 && old_estimate > 0 => {
                let scaled = (replacement_estimate as u128)
                    .saturating_mul(previous as u128)
                    .saturating_add((old_estimate / 2) as u128)
                    / old_estimate as u128;
                u64::try_from(scaled).unwrap_or(u64::MAX).min(previous)
            }
            _ => replacement_estimate,
        };
        self.confirmed_total_tokens = Some(reseeded);
        self.estimate_at_confirmation = replacement_estimate;
    }
}

impl ContextUsage {
    pub fn threshold_reached(&self, threshold_percent: u8) -> bool {
        self.context_window > 0
            && self.estimated_input_tokens.saturating_mul(100)
                >= self.context_window.saturating_mul(u64::from(threshold_percent))
    }
}
```

Use `DEFAULT_COMPACTION_THRESHOLD_PERCENT` in `CompactionPolicy::default()`.
Add `ContextUsageUpdated { usage: ContextUsage }` to `EventPayload`.

Extend `ActiveTurn` with `active_compaction: Option<ActiveCompaction>`. Implement
`request_turn_compaction(turn_id, compaction_id)`,
`request_turn_compaction_cancel(turn_id, compaction_id)`, and
`finish_turn_compaction(turn_id, compaction_id)` without changing the enclosing
turn ID or phase. Make `request_start`, `request_compaction`, and `finish`
reject or validate nested maintenance consistently.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test -p lato-core --test compaction_contract --test state_machine
```

Expected: all accounting, serialization, and state-machine tests pass.

- [ ] **Step 5: Commit the core contract**

```bash
git add crates/lato-core/src/compaction.rs crates/lato-core/src/event.rs crates/lato-core/src/state.rs crates/lato-core/tests/compaction_contract.rs crates/lato-core/tests/state_machine.rs
git commit -m "feat: define automatic compaction accounting"
```

### Task 2: Preserve model metadata and per-call usage across adapters

**Files:**
- Modify: `crates/lato-ai/src/catalog.rs`
- Modify: `crates/lato-ai/src/api.rs`
- Modify: `crates/lato-ai/src/models_file.rs`
- Modify: `crates/lato-ai/src/provider.rs`
- Modify: `crates/lato-ai/src/model_port_adapter.rs`
- Modify: `crates/lato-ai/src/stream.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/legacy_port.rs`
- Modify: `crates/lato-ai/src/model_port_adapter/stream_adapter.rs`
- Modify: `crates/lato-ai/src/codex/events.rs`
- Modify: `crates/lato-ai/src/codex/mod.rs`
- Modify: `crates/lato-ai/src/codex/models.rs`
- Modify: `crates/lato-ai/src/codex/websocket.rs`
- Modify: `src/cli.rs`
- Test: `crates/lato-ai/src/stream_repair_tests.rs`

**Interfaces:**
- Consumes: `ModelUsage`, `ModelCapabilities`, `ModelSelection`, and existing streamed `StreamPiece` values.
- Produces: `ModelMetadata`, `ModelCallReport`, `ModelStream::stream_with_report`, `ActiveModelPort::metadata`, `ActiveModelPort::generation`, `SwitchableModelStream::set_active`, and usage-aware HTTP/Codex parsers.

- [ ] **Step 1: Write failing metadata and report tests**

Cover these behaviors in the existing module tests:

```rust
#[tokio::test]
async fn canonical_usage_returns_in_the_call_report() {
    let usage = ModelUsage {
        input_tokens: Some(80), output_tokens: Some(20),
        reasoning_tokens: Some(5), cached_input_tokens: Some(40),
    };
    let selection = ModelSelection::new("p", "m1").unwrap();
    let port: Arc<dyn ModelPort> = Arc::new(ScriptedPort {
        events: vec![
            Ok(ModelStreamEvent::Usage(usage.clone())),
            Ok(ModelStreamEvent::Completed { reason: ModelStopReason::Completed }),
        ],
        request: Arc::new(Mutex::new(None)),
    });
    let stream = ModelPortStreamAdapter::new(ActiveModelPort {
        selection,
        metadata: ModelMetadata {
            context_window: Some(2_000),
            model_family: Some("family-a".into()),
        },
        capabilities: port.capabilities(),
        generation: 0,
        port,
    });
    let (tx, mut rx) = mpsc::channel(4);
    let report = stream.stream_with_report(
        0,
        serde_json::json!({"messages":[], "tools":[]}),
        tx,
    ).await.unwrap();
    assert!(rx.recv().await.is_none());
    assert_eq!(report.usage, Some(usage));
}

#[tokio::test]
async fn switch_generation_identifies_the_endpoint_that_completed() {
    let first_endpoint = adapt_model_endpoint(
        "p", "m1",
        ModelMetadata { context_window: Some(4_000), model_family: Some("family-a".into()) },
        Arc::new(FakeModelStream::new(Vec::new())),
    ).unwrap();
    let switchable = SwitchableModelStream::new(first_endpoint);
    let first = switchable.active_model_port().unwrap().generation;
    let second_endpoint = adapt_model_endpoint(
        "p", "m2",
        ModelMetadata { context_window: Some(2_000), model_family: Some("family-b".into()) },
        Arc::new(FakeModelStream::new(Vec::new())),
    ).unwrap();
    switchable.set_active(second_endpoint).await;
    let second = switchable.active_model_port().unwrap().generation;
    assert_eq!(second, first + 1);
}

#[test]
fn old_custom_model_files_keep_optional_metadata_empty() {
    let model: CustomModel = serde_json::from_value(serde_json::json!({
        "provider":"p", "id":"m", "api":"openai-responses",
        "base_url":"https://example.invalid", "env":"KEY"
    })).unwrap();
    assert_eq!(model.context_window, None);
    assert_eq!(model.model_family, None);
}
```

Add parser fixtures for OpenAI Responses `response.usage`, Chat Completions
`usage`, Anthropic `message_start` plus `message_delta`, Google
`usageMetadata`, Bedrock metadata usage, Mistral usage, and Codex Responses
terminal usage. Assert normalized `ModelUsage` values exactly.

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cargo test -p lato-ai model_port_adapter
cargo test -p lato-ai stream_repair
cargo test -p lato-ai codex
```

Expected: compilation fails because report and metadata APIs do not exist.

- [ ] **Step 3: Add backward-compatible stream reporting**

Add the following public values to `stream.rs`:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCallReport {
    pub usage: Option<lato_core::ModelUsage>,
    pub generation: u64,
}

#[async_trait]
pub trait ModelStream: Send + Sync {
    fn active_model_port(&self) -> Option<ActiveModelPort> { None }

    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String>;

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, String> {
        self.stream(prompt_bytes, context, tx).await?;
        Ok(ModelCallReport::default())
    }
}
```

Existing third-party `ModelStream` implementations remain source-compatible
because the new method has a default. Override it in HTTP, custom HTTP, Codex,
`ModelPortStreamAdapter`, and `SwitchableModelStream`.

- [ ] **Step 4: Add model metadata and switch generations**

Define:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelMetadata {
    pub context_window: Option<u64>,
    pub model_family: Option<String>,
}

#[derive(Clone)]
pub struct ActiveModelPort {
    pub selection: ModelSelection,
    pub metadata: ModelMetadata,
    pub capabilities: ModelCapabilities,
    pub generation: u64,
    pub port: Arc<dyn ModelPort>,
}

#[derive(Clone)]
pub struct ActiveModelStream {
    pub stream: Arc<dyn ModelStream>,
    pub port: ActiveModelPort,
}

pub fn adapt_model_endpoint(
    provider: &str,
    model: &str,
    metadata: ModelMetadata,
    legacy: Arc<dyn ModelStream>,
) -> Result<ActiveModelStream, ModelSelectionError> {
    let selection = ModelSelection::new(provider, model)?;
    let port: Arc<dyn ModelPort> = Arc::new(LegacyModelPort::new(
        selection.clone(),
        legacy,
    ));
    let mut capabilities = port.capabilities();
    if metadata.context_window.is_some() {
        capabilities.context_window = metadata.context_window;
    }
    let active = ActiveModelPort {
        selection,
        metadata,
        capabilities,
        generation: 0,
        port,
    };
    let stream: Arc<dyn ModelStream> =
        Arc::new(ModelPortStreamAdapter::new(active.clone()));
    Ok(ActiveModelStream { stream, port: active })
}
```

Keep `adapt_model_stream` as a compatibility wrapper that returns only the
stream field.

Add `#[serde(default)] pub context_window: Option<u64>` and
`#[serde(default)] pub model_family: Option<String>` to `CustomModel`.
Add the same fields to built-in `Model`; encode the following effective catalog
values rather than guessing from model names at runtime:

| Provider/model | Context window | Family |
|---|---:|---|
| `openai/gpt-4.1` | `1_047_576` | `openai` |
| `xai/grok-4` | `256_000` | `grok` |
| Fireworks Llama 3.1 8B | `131_072` | `llama` |
| `kimi-coding/kimi-k2` | `131_072` | `kimi` |
| `openai-codex/codex-mini-latest` | `200_000` | `openai` |
| Google and Vertex Gemini 2.0 Flash | `1_048_576` | `gemini` |
| Azure OpenAI `gpt-4.1` | `1_047_576` | `openai` |
| Bedrock Claude 3.7 Sonnet | `200_000` | `anthropic` |
| MiniMax M2.1 entries | `204_800` | `minimax` |
| GLM 4.5 entries | `131_072` | `glm` |
| Mistral Large | `131_072` | `mistral` |
| SenseNova and deliberately unsupported fixture entries | `None` | provider/API fallback |
| `radius/radius-test` | `10_000` | `fixture-a` |

Use `provider/api` as the family fallback only in the endpoint-construction
helper. Add catalog tests that assert every supported built-in has a non-empty
family and every entry except the explicitly unknown SenseNova entry has a
non-zero window. If an authoritative provider contract contradicts a number
during implementation, update this table and the design spec in the same
commit before changing code; never silently substitute a guessed value.

`SwitchableModelStream::set_active` accepts an `ActiveModelStream` containing
both the stream and active port, increments an `AtomicU64`, updates both
surfaces under one write lock, and stamps the same generation on reports and
snapshots. Remove the old stream-only `set` method after all in-repo callers
migrate in Task 6.

- [ ] **Step 5: Normalize provider usage**

Refactor the parser result to retain terminal usage:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedModelOutput {
    pub pieces: Vec<StreamPiece>,
    pub usage: Option<ModelUsage>,
}
```

`ModelEventParser::accept` updates its latest normalized usage whenever a
provider event supplies it. `stream_http_request_with_report` returns the final
usage after all pieces are sent. Keep `stream_http_request` and
`parse_stream_body` as compatibility wrappers that discard only the report,
not streamed content.

In `LegacyModelPort`, call `legacy.stream_with_report`, emit
`ModelStreamEvent::Usage` before `Completed`, and never emit usage after a
failed/cancelled call. In `ModelPortStreamAdapter`, retain the last usage event
and return it in `ModelCallReport` after `Completed`.

- [ ] **Step 6: Run model-layer tests**

Run:

```bash
cargo test -p lato-ai
cargo test -p lato-core --test model_port
```

Expected: all model, parser, cancellation, and adapter tests pass.

- [ ] **Step 7: Commit the model boundary**

```bash
git add crates/lato-ai src/cli.rs crates/lato-core/tests/model_port.rs
git commit -m "feat: preserve model usage and context metadata"
```

### Task 3: Add agent-side history estimation and switch policy

**Files:**
- Create: `crates/lato-agent/src/context_usage.rs`
- Modify: `crates/lato-agent/src/lib.rs`
- Test: `crates/lato-agent/tests/context_usage.rs`

**Interfaces:**
- Consumes: `HistoryItem`, `ContextLedger`, `ContextUsage`, `ActiveModelPort`, and `CompactionPolicy`.
- Produces: `estimate_history_tokens`, `ContextTracker`, `SwitchCompaction`, and `decide_switch_compaction`.

- [ ] **Step 1: Write failing pure-policy tests**

Create `crates/lato-agent/tests/context_usage.rs` with fixtures proving:

```rust
fn model(family: &str, context_window: u64) -> ModelMetadata {
    ModelMetadata {
        context_window: Some(context_window),
        model_family: Some(family.to_owned()),
    }
}

#[test]
fn history_estimate_counts_text_and_serialized_tool_payloads() {
    let history = vec![
        HistoryItem::User("12345678".into()),
        HistoryItem::ToolCall {
            id: "c1".into(), name: "read_file".into(),
            arguments: serde_json::json!({"path":"abcdef"}),
        },
        HistoryItem::ToolResult { id: "c1".into(), output: "12345678".into() },
    ];
    assert_eq!(estimate_history_tokens(&history), 11);
}

#[test]
fn same_family_shrink_defers_only_when_new_threshold_is_reached() {
    assert_eq!(decide_switch_compaction(&model("a", 2_000), &model("a", 1_000), 850, true, 85), SwitchCompaction::BeforeNextSample);
    assert_eq!(decide_switch_compaction(&model("a", 2_000), &model("a", 1_000), 849, true, 85), SwitchCompaction::None);
}

#[test]
fn cross_family_with_assistant_history_compacts_immediately() {
    assert_eq!(decide_switch_compaction(&model("a", 2_000), &model("b", 3_000), 10, true, 85), SwitchCompaction::Immediate);
    assert_eq!(decide_switch_compaction(&model("a", 2_000), &model("b", 3_000), 10, false, 85), SwitchCompaction::None);
}
```

- [ ] **Step 2: Run the test and verify failure**

```bash
cargo test -p lato-agent --test context_usage
```

Expected: compilation fails because the context usage module is absent.

- [ ] **Step 3: Implement the focused estimator and tracker**

Define the public policy surface:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchCompaction {
    None,
    Immediate,
    BeforeNextSample,
}

pub struct ContextTracker {
    ledger: ContextLedger,
    pending_model_switch: bool,
}

impl ContextTracker {
    pub fn measure(&self, history: &[HistoryItem], active: &ActiveModelPort) -> ContextUsage {
        self.ledger.measure(
            estimate_history_tokens(history),
            active.metadata.context_window.or(active.capabilities.context_window),
        )
    }

    pub fn observe(
        &mut self,
        history: &[HistoryItem],
        report: &ModelCallReport,
        active_generation: u64,
    ) -> bool {
        if report.generation != active_generation {
            return false;
        }
        report.usage.as_ref().is_some_and(|usage| {
            self.ledger.observe(usage, estimate_history_tokens(history))
        })
    }

    pub fn reseed(&mut self, history: &[HistoryItem]) {
        self.ledger.reseed(estimate_history_tokens(history));
    }

    pub fn mark_model_switch_check(&mut self) {
        self.pending_model_switch = true;
    }

    pub fn take_model_switch_check(&mut self) -> bool {
        std::mem::take(&mut self.pending_model_switch)
    }
}
```

Implement bytes/4 counting without allocating strings for text variants:

```rust
pub fn estimate_history_tokens(history: &[HistoryItem]) -> u64 {
    let bytes = history.iter().fold(0_u64, |total, item| {
        let item_bytes = match item {
            HistoryItem::System(text)
            | HistoryItem::User(text)
            | HistoryItem::AssistantText(text)
            | HistoryItem::CompactionSummary(text) => text.len() as u64,
            HistoryItem::ToolCall { id, name, arguments } => {
                (id.len() as u64)
                    .saturating_add(name.len() as u64)
                    .saturating_add(serde_json::to_vec(arguments)
                        .map(|bytes| bytes.len() as u64)
                        .unwrap_or(u64::MAX))
            }
            HistoryItem::ToolResult { id, output } => (id.len() + output.len()) as u64,
        };
        total.saturating_add(item_bytes)
    });
    bytes / 4
}

pub fn has_model_authored_history(history: &[HistoryItem]) -> bool {
    history.iter().any(|item| matches!(
        item,
        HistoryItem::AssistantText(_)
            | HistoryItem::ToolCall { .. }
            | HistoryItem::CompactionSummary(_)
    ))
}
```

Use saturating totals and round each full-history byte total down once, matching
Grok's aggregate estimate.

Give `decide_switch_compaction` this exact signature so pure tests do not need
live provider objects:

```rust
pub fn decide_switch_compaction(
    previous: &ModelMetadata,
    candidate: &ModelMetadata,
    estimated_tokens: u64,
    has_model_authored_history: bool,
    threshold_percent: u8,
) -> SwitchCompaction {
    if has_model_authored_history
        && matches!(
            (&previous.model_family, &candidate.model_family),
            (Some(old), Some(new)) if old != new
        )
    {
        return SwitchCompaction::Immediate;
    }
    let (Some(old_window), Some(new_window)) =
        (previous.context_window, candidate.context_window)
    else {
        return SwitchCompaction::None;
    };
    if old_window <= new_window {
        return SwitchCompaction::None;
    }
    let usage = ContextUsage {
        estimated_input_tokens: estimated_tokens,
        context_window: new_window,
        utilization_percent: estimated_tokens.saturating_mul(100)
            .checked_div(new_window)
            .unwrap_or(0)
            .min(u64::from(u8::MAX)) as u8,
    };
    if usage.threshold_reached(threshold_percent) {
        SwitchCompaction::BeforeNextSample
    } else {
        SwitchCompaction::None
    }
}
```

- [ ] **Step 4: Run the pure agent tests**

```bash
cargo test -p lato-agent --test context_usage
```

Expected: all estimator, threshold, generation, and switch-decision tests pass.

- [ ] **Step 5: Commit the agent policy**

```bash
git add crates/lato-agent/src/context_usage.rs crates/lato-agent/src/lib.rs crates/lato-agent/tests/context_usage.rs
git commit -m "feat: add grok-style context tracking policy"
```

### Task 4: Add nested automatic compaction to the runtime

**Files:**
- Modify: `crates/lato-runtime/src/driver.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-runtime/src/lib.rs`
- Modify: `crates/lato-runtime/tests/session_runtime.rs`

**Interfaces:**
- Consumes: Task 1 nested state transitions and the existing Phase 4C1 `CompactionRequest`, `CompactionCandidate`, and `SessionStore`.
- Produces: `TurnEventEmitter::compact`, `AutomaticCompactionRequest`, `AutomaticCompactionOutcome`, and runtime handling that replies with installed messages only after durable replacement.

- [ ] **Step 1: Write failing nested-maintenance tests**

Add a `ThresholdDriver` fixture whose `run` sends one automatic request, waits
for the reply, records returned history, and then completes. Assert:

```rust
assert_eq!(observed_turn_ids, vec![TurnId::from("turn-1")]);
assert_eq!(compaction_triggers, vec![CompactionTrigger::Threshold]);
assert_eq!(store.replay(&sid).await.unwrap().projection.messages, compacted);
assert_eq!(driver.installed_inside_turn(), compacted);
```

Also test model-switch trigger identity, automatic cancellation through
`CancelTurn`, manual compaction rejection while maintenance runs, checkpoint
failure retaining old history, and post-marker reconciliation returning the
committed replacement.

- [ ] **Step 2: Run focused runtime tests and verify failure**

```bash
cargo test -p lato-runtime --test session_runtime automatic_compaction -- --nocapture
```

Expected: compilation fails because a running driver cannot request durable
maintenance.

- [ ] **Step 3: Add the driver request/reply boundary**

Add these values to `driver.rs`:

```rust
#[derive(Clone, Debug)]
pub struct AutomaticCompactionRequest {
    pub trigger: CompactionTrigger,
    pub usage: ContextUsage,
    pub messages: Vec<ModelMessage>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutomaticCompactionOutcome {
    Compacted(Vec<ModelMessage>),
    ContinueUnchanged,
}

impl TurnEventEmitter {
    pub async fn compact(
        &self,
        request: AutomaticCompactionRequest,
    ) -> Result<AutomaticCompactionOutcome, AgentError> {
        let (reply, result) = oneshot::channel();
        self.tx.send(DriverMessage::AutomaticCompactionRequested {
            turn_id: self.turn_id.clone(), request, reply,
        }).map_err(|_| event_bus_closed())?;
        result.await.map_err(|_| event_bus_closed())?
    }
}
```

The request includes the immutable history snapshot because the driver's
actor-state mutex is held by the enclosing turn. The runtime must never call
`install_history` for nested maintenance; it returns the durably committed
messages so the waiting actor installs them itself.

- [ ] **Step 4: Orchestrate nested maintenance through the existing store**

In `SessionLoop`, handle `AutomaticCompactionRequested` only when `turn_id`
matches the active turn and no compaction exists. Use
`request_turn_compaction`, write `CompactionRequested`, run
`driver.compact`, call the same `replace_history` and reconciliation helpers as
manual compaction, emit normal lifecycle events, then reply:

- `Compacted(messages)` after a confirmed marker and projection;
- `ContinueUnchanged` for `compaction.nothing_to_compact` and ordinary
  pre-marker model/validation failures;
- `Err` for cancellation, auth-class failures, and storage/reconciliation
  failures.

Store an `owner_turn: Option<TurnId>` and reply sender on
`ActiveRuntimeCompaction`. `CancelTurn` cancels both tokens. Do not finish the
enclosing state-machine turn when maintenance completes.

- [ ] **Step 5: Run runtime tests**

```bash
cargo test -p lato-runtime --test session_runtime
```

Expected: all existing manual compaction tests and new nested-maintenance tests
pass.

- [ ] **Step 6: Commit runtime orchestration**

```bash
git add crates/lato-runtime crates/lato-core/src/state.rs crates/lato-core/tests/state_machine.rs
git commit -m "feat: orchestrate automatic compaction within turns"
```

### Task 5: Check and compact before every agent sample

**Files:**
- Modify: `crates/lato-agent/src/actor.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/tests/legacy_driver.rs`
- Modify: `crates/lato-agent/tests/compaction_runtime.rs`

**Interfaces:**
- Consumes: `ContextTracker`, `ModelStream::stream_with_report`, and `TurnEventEmitter::compact`.
- Produces: a pre-sampling maintenance hook used at every loop iteration, context status emissions, and same-turn request rebuild after successful compaction.

- [ ] **Step 1: Write failing sample-boundary tests**

Use scripted model ports with 1,000-token windows. Add tests that prove:

- 849 estimated tokens sample directly;
- 850 estimated tokens invoke the compactor before the provider;
- a first provider call below threshold can emit a large tool result and cause
  compaction before the second provider call;
- successful maintenance preserves one `TurnId` and the second request sees a
  `CompactionSummary`;
- an ordinary summary failure emits failure and performs exactly one provider
  call with old history;
- cancellation during compaction performs no provider call;
- a report whose generation no longer matches does not confirm usage.

Record ordering as:

```rust
assert_eq!(calls, vec!["compact", "sample"]);
assert_eq!(turn_ids.iter().collect::<BTreeSet<_>>().len(), 1);
```

- [ ] **Step 2: Run agent tests and verify failure**

```bash
cargo test -p lato-agent --test legacy_driver --test compaction_runtime automatic -- --nocapture
```

Expected: tests fail because the actor samples without measuring or requesting
maintenance.

- [ ] **Step 3: Add the pre-sampling hook**

Add `ContextTracker` to `SessionActor`. Immediately before constructing each
model request:

```rust
let active = self.stream.active_model_port();
if let Some(active) = active {
    let usage = self.context_tracker.measure(&self.history, &active);
    self.emit_context_usage(usage.clone());
    let trigger = if self.context_tracker.take_model_switch_check() {
        usage.threshold_reached(CompactionPolicy::default().threshold_percent)
            .then_some(CompactionTrigger::ModelSwitch)
    } else {
        usage.threshold_reached(CompactionPolicy::default().threshold_percent)
            .then_some(CompactionTrigger::Threshold)
    };
    if let Some(trigger) = trigger {
        self.run_automatic_compaction(trigger, usage).await?;
    }
}
```

`run_automatic_compaction` converts history to `ModelMessage`, calls the
runtime emitter, installs returned messages through
`model_messages_to_history`, reseeds the tracker, and then continues the loop
to rebuild the provider request. It never streams compaction summary deltas.

- [ ] **Step 4: Confirm usage only after successful model completion**

Replace the spawned `stream.stream` call with `stream.stream_with_report`.
After the output channel closes, await the task and pass its report to
`ContextTracker::observe`. Emit `EventPayload::ContextUsageUpdated` through a
new `TurnEventEmitter::context_usage` helper. Failed and cancelled calls skip
confirmation.

Classify authentication failures at the boundary with one shared helper:

```rust
fn is_auth_failure(error: &AgentError) -> bool {
    let message = error.message.to_ascii_lowercase();
    error.code == "model.auth"
        || message.contains("model.auth")
        || message.contains("http 401")
        || message.contains("http 403")
        || message.contains("oauth refresh failed")
}
```

Ordinary automatic compaction errors map to `ContinueUnchanged`, while this
auth class and all storage/reconciliation errors abort the turn.

- [ ] **Step 5: Run agent tests**

```bash
cargo test -p lato-agent --test legacy_driver --test compaction_runtime
```

Expected: all manual and automatic compaction tests pass, including the
tool-loop boundary.

- [ ] **Step 6: Commit sample-boundary integration**

```bash
git add crates/lato-agent/src/actor.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/tests/legacy_driver.rs crates/lato-agent/tests/compaction_runtime.rs
git commit -m "feat: compact automatically before model sampling"
```

### Task 6: Make model switching session-scoped and transactional

**Files:**
- Modify: `crates/lato-agent/src/context_usage.rs`
- Modify: `crates/lato-agent/src/legacy_driver.rs`
- Modify: `crates/lato-agent/src/runtime_session.rs`
- Modify: `crates/lato-agent/src/host.rs`
- Modify: `crates/lato-core/src/command.rs`
- Modify: `crates/lato-core/src/journal.rs`
- Modify: `crates/lato-core/tests/journal_contract.rs`
- Modify: `crates/lato-runtime/src/session.rs`
- Modify: `crates/lato-agent/tests/runtime_session.rs`
- Modify: `crates/lato-agent/tests/acp_runtime.rs`

**Interfaces:**
- Consumes: `ActiveModelStream`, `SwitchCompaction`, the Phase 4C1 compact operation, and the `RuntimeSession` submission gate.
- Produces: `PreparedModelSwitch`, `RuntimeSession::switch_model`, `Command::SelectModel`, `JournalRecord::ModelSelected`, `SessionProjection::model_selection`, per-session stream/port ownership, and ACP `session/set_model` transaction results.

- [ ] **Step 1: Write failing session-isolation and switch-policy tests**

Add tests for two sessions created from the same host:

```rust
let first_session = new_session(&mut host).await;
let second_session = new_session(&mut host).await;
set_model(&mut host, &first_session, "fixture", "small-b").await.unwrap();
assert_eq!(session_model(&host, &first_session).await, "fixture/small-b");
assert_eq!(session_model(&host, &second_session).await, "fixture/large-a");
```

Add deterministic endpoint fixtures proving:

- cross-family assistant history compacts immediately with the new port;
- system/user-only history does not compact;
- same-family larger/equal windows do not mark maintenance;
- same-family smaller window at 85 percent marks `ModelSwitch` for the next
  sample;
- candidate construction failure preserves the old endpoint;
- non-auth immediate compaction failure keeps the new endpoint and returns a
  warning;
- concurrent turn or compaction returns `session_busy`.

- [ ] **Step 2: Run switch tests and verify failure**

```bash
cargo test -p lato-agent --test runtime_session model_switch -- --nocapture
cargo test -p lato-agent --test acp_runtime model_switch -- --nocapture
```

Expected: tests fail because the host-level switchable mutates every session
and bypasses compaction.

- [ ] **Step 3: Move endpoint ownership into each runtime session**

Introduce:

```rust
pub struct PreparedModelSwitch {
    pub active: lato_ai::ActiveModelStream,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchOutcome {
    pub provider: String,
    pub model: String,
    pub compaction_warning: Option<AgentError>,
}
```

Each `RuntimeSession` creates its own `SwitchableModelStream` and
`SwitchableModelPort` from the host's initial endpoint. Remove shared mutable
endpoint fields from `AcpHost`; retain only an immutable default endpoint for
new sessions and a default model selection.

Add `LegacyTurnDriver::active_model`, `LegacyTurnDriver::activate_model`,
`LegacyTurnDriver::has_model_authored_history`, and
`LegacyTurnDriver::mark_model_switch_check`. Activating updates stream and port
atomically through the `ActiveModelStream` value.

Add this durable core command/record pair:

```rust
Command::SelectModel {
    selection: ModelSelection,
    model_family: Option<String>,
    context_window: Option<u64>,
}

JournalRecord::ModelSelected {
    selection: ModelSelection,
    model_family: Option<String>,
    context_window: Option<u64>,
}
```

Project the most recent record into these backward-compatible fields on
`SessionProjection`:

```rust
pub model_selection: Option<ModelSelection>,
pub model_family: Option<String>,
pub model_context_window: Option<u64>,
```

`SessionLoop` accepts `SelectModel` only while idle and commits the journal
record with `SyncData`. It does not emit a client model-changed notification;
the outer transaction publishes that only after any immediate compaction.

- [ ] **Step 4: Implement the serialized switch transaction**

`RuntimeSession::switch_model` acquires `submission_gate`, rejects a non-empty
`active_operation`, snapshots old metadata and usage, installs the candidate,
submits `Command::SelectModel`, and calls `decide_switch_compaction`. If the
journal commit fails, reactivate the old endpoint before returning the error.

For `Immediate`, invoke the existing compact wait loop with
`CompactionTrigger::ModelSwitch` while retaining the gate. Record ordinary
failure as `compaction_warning` and keep the new endpoint. Return auth errors
after keeping the new selection, matching Grok Build. For
`BeforeNextSample`, mark the driver's context tracker and return successfully.
For `None`, return immediately.

Split `compact` into a public gate-taking method and a private
`compact_with_gate_held(trigger, user_context)` helper so immediate switching
does not recursively lock the gate.

- [ ] **Step 5: Route ACP switching to the target session**

Require `sessionId`, `provider`, and `model` in `session/set_model`. Construct
and validate the candidate completely before calling `switch_model`. Do not
mutate any host-global stream. Return:

```json
{
  "supported": true,
  "provider": "fixture",
  "model": "small-b",
  "compactionWarning": null
}
```

Emit `lato/session/model_changed` only after the transaction completes. Include
the selected provider/model and optional warning. Keep unsupported dialect,
unknown model, credential, and OAuth refresh errors unchanged.

For compatibility, also accept standard ACP `modelId`; when `provider` is
absent, resolve `modelId` uniquely in the built-in/custom catalog and reject an
ambiguous ID. On resume, resolve `replay.projection.model_selection` before
constructing `RuntimeSession`; fall back to the host default only when no
`ModelSelected` record exists. A persisted selection that can no longer be
resolved returns an explicit `model.unavailable_on_resume` error instead of
silently changing models.

- [ ] **Step 6: Run agent and ACP tests**

```bash
cargo test -p lato-agent --test runtime_session --test acp_runtime
```

Expected: session isolation, immediate family compaction, deferred shrink
compaction, failure semantics, and existing ACP tests pass.

- [ ] **Step 7: Commit session-scoped switching**

```bash
git add crates/lato-agent/src/context_usage.rs crates/lato-agent/src/legacy_driver.rs crates/lato-agent/src/runtime_session.rs crates/lato-agent/src/host.rs crates/lato-core/src/command.rs crates/lato-core/src/journal.rs crates/lato-core/tests/journal_contract.rs crates/lato-runtime/src/session.rs crates/lato-agent/tests/runtime_session.rs crates/lato-agent/tests/acp_runtime.rs
git commit -m "feat: make model switches session scoped"
```

### Task 7: Expose context and transactional switching in the client and TUI

**Files:**
- Modify: `src/client.rs`
- Modify: `src/tui/backend.rs`
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/state.rs`
- Modify: `src/tui/render.rs`
- Modify: `src/tui/i18n.rs`
- Modify: `tests/tui_cli.rs`

**Interfaces:**
- Consumes: `lato/session/context`, `lato/session/model_changed`, `session/set_model`, and existing compaction notifications.
- Produces: `ClientUpdate::ContextUsage`, `ClientUpdate::ModelChanged`, `InteractiveAcpClient::set_model`, `BackendCommand::SwitchModel`, and context status rendering.

- [ ] **Step 1: Write failing client and reducer tests**

Add parser tests for:

```rust
let raw = serde_json::json!({
    "method":"lato/session/context",
    "params":{"sessionId":"s1","estimatedInputTokens":850,"contextWindow":1000,"utilizationPercent":85}
});
assert_eq!(ClientUpdate::from_json(&raw), ClientUpdate::ContextUsage {
    estimated_input_tokens: 850,
    context_window: Some(1000),
    utilization_percent: Some(85),
});
```

Add TUI reducer/render tests proving known windows render
`context: 850 / 1000 (85%)`, unknown windows render `context: 850`, automatic
compaction displays the trigger, and `ModelChanged` updates the model only after
the backend result.

- [ ] **Step 2: Run UI tests and verify failure**

```bash
cargo test --bin lato client
cargo test --bin lato tui
cargo test --test tui_cli
```

Expected: compilation fails because the new client updates and backend command
do not exist.

- [ ] **Step 3: Add client updates and model switch request**

Add:

```rust
ContextUsage {
    estimated_input_tokens: u64,
    context_window: Option<u64>,
    utilization_percent: Option<u8>,
},
ModelChanged {
    provider: String,
    model: String,
    warning: Option<lato_core::AgentError>,
},
```

to `ClientUpdate`. Map a wire `contextWindow` of zero to `None` and omit the
percentage. Implement:

```rust
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchResponse {
    pub supported: bool,
    pub provider: String,
    pub model: String,
    pub compaction_warning: Option<lato_core::AgentError>,
}

pub async fn set_model(&mut self, selection: &str) -> Result<ModelSwitchResponse, String> {
    let (provider, model) = selection.split_once('/')
        .ok_or("model must be provider/model")?;
    let id = self.take_id();
    let session_id = self.session_id.clone();
    let response = self.host.handle(req(
        id,
        "session/set_model",
        serde_json::json!({
            "sessionId": session_id,
            "provider": provider,
            "model": model,
        }),
    )).await.ok_or("no response")?;
    serde_json::from_value(response_result(&response)?.clone())
        .map_err(|error| error.to_string())
}
```

- [ ] **Step 4: Route the TUI through the backend**

Replace direct `configured_stream` plus `switchable.set` behavior with
`BackendCommand::SwitchModel(selection)`. The backend owns the mutable client,
awaits `client.set_model`, and emits `BackendEvent::ModelSwitched` only on
success. Persist the selected model after success. Keep the old model visible
and show the error on failure.

Remove the `switchable` argument from `execute_effects` and any now-unused TUI
bootstrap plumbing. Busy-state handling rejects switches while a turn or
compaction task is active.

- [ ] **Step 5: Render context and automatic compaction status**

Store the most recent context update in `AppState`. Render it in the status
line using saturating integer values. Extend compaction updates to retain the
wire trigger and show localized `automatic`, `model switch`, or `manual`
labels. Do not append compaction model deltas to `messages`.

- [ ] **Step 6: Run client and TUI tests**

```bash
cargo test --bin lato
cargo test --test tui_cli
```

Expected: all client, backend, reducer, render, and CLI tests pass.

- [ ] **Step 7: Commit the UI integration**

```bash
git add src/client.rs src/tui tests/tui_cli.rs
git commit -m "feat: surface automatic context management in tui"
```

### Task 8: Add end-to-end recovery coverage and provenance

**Files:**
- Modify: `tests/session_compaction_cli.rs`
- Modify: `docs/superpowers/reference/lato-upstream-sources.md`
- Modify: `docs/testing/lato-agent-test-cases.md` only if the user's existing edits can be preserved without overlap; otherwise leave it untouched and report the overlap.

**Interfaces:**
- Consumes: the complete 4C2 behavior from Tasks 1-7.
- Produces: black-box threshold/model-switch/restart coverage and complete upstream attribution.

- [ ] **Step 1: Write failing end-to-end tests**

Add deterministic fake providers with explicit windows and usage. Cover:

1. a session reaches exactly 85 percent, emits `Threshold`, checkpoints a
   replacement, finishes the original prompt, restarts, and continues from the
   compacted history;
2. a same-family smaller-window switch returns first, then emits `ModelSwitch`
   before the next sample;
3. a cross-family switch compacts before `session/set_model` returns;
4. two sessions on one host keep independent selected models;
5. an unknown context window never auto-compacts but still emits an estimated
   token count.

Assert journal order explicitly:

```rust
assert!(requested_sequence < checkpoint_marker_sequence);
assert!(checkpoint_marker_sequence < resumed_turn_completion_sequence);
assert_eq!(replay.projection.active_checkpoint_id, Some(checkpoint_id));
```

- [ ] **Step 2: Run end-to-end tests and verify failure**

```bash
cargo test --test session_compaction_cli automatic -- --nocapture
cargo test --test session_compaction_cli model_switch -- --nocapture
```

Expected: new tests fail until all 4C2 wiring is present.

- [ ] **Step 3: Add source-ledger records and file headers**

Add ledger rows for:

- `crates/lato-core/src/compaction.rs` from Grok
  `xai-chat-state/src/actor/state.rs`, `mutations.rs`, and `queries.rs`;
- `crates/lato-agent/src/context_usage.rs` and `actor.rs` from the same chat-state
  files and `xai-grok-shell/src/session/compaction.rs`;
- `crates/lato-runtime/src/session.rs` from Grok
  `xai-grok-shell/src/session/compaction.rs`;
- `crates/lato-agent/src/runtime_session.rs` and `host.rs` from Grok
  `xai-grok-shell/src/agent/handlers/model_switch.rs` and
  `session/acp_session_impl/model_switch.rs`.

Use the required exact header format and describe Lato's provider-neutral,
checkpoint-first changes. Do not attribute generic parser code to Grok Build.

- [ ] **Step 4: Run all focused Phase 4C2 tests**

```bash
cargo test -p lato-core --test compaction_contract --test state_machine
cargo test -p lato-ai
cargo test -p lato-runtime --test session_runtime
cargo test -p lato-agent --test context_usage --test legacy_driver --test compaction_runtime --test runtime_session --test acp_runtime
cargo test --bin lato
cargo test --test session_compaction_cli --test tui_cli
```

Expected: every command exits zero.

- [ ] **Step 5: Commit end-to-end coverage and provenance**

```bash
git add tests/session_compaction_cli.rs docs/superpowers/reference/lato-upstream-sources.md
git commit -m "test: cover automatic compaction recovery"
```

### Task 9: Run release gates and deploy locally

**Files:**
- Modify only files required by formatter or genuine test fixes; do not touch unrelated user files.

**Interfaces:**
- Consumes: all Phase 4C2 commits.
- Produces: verified workspace and an installed `lato` binary.

- [ ] **Step 1: Format and verify formatting**

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: both commands exit zero and formatting changes are limited to Phase
4C2 files.

- [ ] **Step 2: Run workspace compile and tests**

```bash
cargo check --workspace
cargo test --workspace --no-fail-fast
```

Expected: all crates compile and every test passes.

- [ ] **Step 3: Run Clippy as an error gate**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: zero warnings and zero errors.

- [ ] **Step 4: Inspect the final diff and dirty tree**

```bash
git diff --check
git status --short
git log --oneline -12
```

Expected: Phase 4C2 changes are committed; only the user's pre-existing files
remain dirty or untracked.

- [ ] **Step 5: Install from the repository root**

```bash
cargo install --path .
```

Expected: Cargo reports that `lato` was installed or replaced successfully.

- [ ] **Step 6: Smoke-test the installed binary**

Use a temporary `LATO_HOME` and deterministic fake-provider fixture exposed by
the integration test harness:

```bash
lato --version
cargo test --test session_compaction_cli installed_binary_automatic_compaction_smoke -- --nocapture
```

Expected: the version command succeeds and the smoke test proves the installed
binary completes a prompt after automatic compaction and leaves a recoverable
checkpoint.

- [ ] **Step 7: Commit any gate-only fixes**

If formatting or a genuine gate fix changed tracked Phase 4C2 files:

```bash
git add crates src tests docs/superpowers/reference/lato-upstream-sources.md
git commit -m "chore: finalize automatic compaction"
```

Expected: no empty commit is created when no gate-only changes were needed.
