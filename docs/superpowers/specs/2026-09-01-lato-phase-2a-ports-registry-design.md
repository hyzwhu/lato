# Lato Phase 2A：模型端口与工具注册表设计

## 1. 范围与目标

Phase 2A 建立模型和工具的稳定扩展边界，不迁移当前 provider、工具实现或 `SessionActor`。本阶段必须在不修改用户现有脏文件的前提下完成，并为 Phase 2B 的兼容适配器提供可直接实现的接口。

完成后：

- `lato-core` 拥有 provider 无关的模型请求、统一流事件、能力描述和 `ModelPort`。
- `lato-core` 拥有实现无关的工具 descriptor、调用上下文、输出、错误和对象安全 `Tool` trait。
- `lato-tools` 拥有确定性的分层 `ToolCatalog`，但旧集中式 registry/dispatch 继续作为生产路径。
- 新 provider 不需要修改 session 协议；新工具可注册而无需修改新的 catalog。
- Phase 2B 可以把当前 `ModelStream` 和内置工具逐个包成 adapter。

明确不包含：

- 修改 `crates/lato-agent/src/actor.rs`。
- 修改当前 `lato-ai` provider、SSE parser 或 `ModelStream`。
- 修改当前 `lato-tools/src/registry.rs`、`dispatch.rs` 或内置工具。
- 在 turn loop 中启用新端口。
- Policy、审批、sandbox、journal、MCP 或 AgentField adapter。

AgentField 仍是 Phase 7 的远程运行适配器。本阶段不引入 AgentField SDK、控制面或模型依赖。设计时核对的实时 AgentField contract 为 `2026-03-24-v1`；本机没有 provider key，因此没有 AgentField 模型 smoke。

## 2. 方案选择

采用方案 A：核心契约进入 `lato-core`，工具注册实现进入 `lato-tools`。

未采用的方案：

- 直接迁移当前实现：会与用户正在进行的 `lato-ai`、`lato-tools`、`SessionActor` 修改冲突，也会把接口设计和兼容修复混在同一提交中。
- 新建 `lato-ports` crate：隔离更强，但偏离已批准的 crate 边界，并增加跨 crate 类型转换。

Phase 2 拆为两个可独立验收的子阶段：

- Phase 2A：类型、ports、catalog、契约测试。
- Phase 2B：当前 model/tool 实现的 adapters 和 turn-loop 迁移。

## 3. 依赖边界

`lato-core` 新增依赖：

- `async-trait`：对象安全异步 port。
- `futures-core`：provider 无关的流接口。
- `serde_json`：模型内容、tool schema 和参数的规范 JSON 边界。
- `semver`（Serde feature）：工具版本和 replacement 兼容检查。
- `tokio-util` 的 `CancellationToken`：与现有 runtime 一致的分层取消原语。

`lato-core` 仍禁止依赖：

- `lato-runtime`、`lato-agent`、`lato-ai`、`lato-tools`、`lato-workspace`。
- HTTP client、provider SDK、文件工具、CLI 或 ACP。

`lato-tools` 新增对 `lato-core` 的单向依赖。`lato-runtime` 继续只依赖 `lato-core`。

```text
lato-agent ────────────────┐
                           v
lato-ai adapters ─────> lato-core <──── lato-tools catalog/adapters
                           ^
                           │
                      lato-runtime
```

## 4. 模型端口

### 4.1 身份与选择

新增类型化 `ModelCallId`。模型选择使用：

```rust
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
}
```

两部分都使用 checked constructor 拒绝空值。每个 turn 在开始时持有一份 selection snapshot；后续全局模型切换不改变进行中的请求。

### 4.2 规范消息

```rust
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ModelContent>,
}

pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

pub enum ModelContent {
    Text { text: String },
    Image { media_type: String, data: String },
    ToolCall { call_id: ToolCallId, name: ToolName, arguments: Value },
    ToolResult { call_id: ToolCallId, output: String },
}
```

Phase 2A 只定义规范形状，不负责把当前 `HistoryItem` 投影成该结构。Image 的 `data` 允许 data URL、provider 可接受的 URL 或 adapter 生成的引用；具体安全和大小限制在 Phase 2B/3 实现。

### 4.3 请求与参数

```rust
pub struct ModelRequest {
    pub call_id: ModelCallId,
    pub selection: ModelSelection,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDescriptor>,
    pub parameters: SamplingParameters,
}
```

`SamplingParameters` 的首版字段为 `temperature`、`max_output_tokens`、`tool_choice` 和 `response_schema`，全部可选。核心不添加 provider-specific 参数逃生口；兼容性需求必须先归一化或在具体 adapter 配置中处理，不能进入 journal/wire contract。

### 4.4 能力

```rust
pub struct ModelCapabilities {
    pub tool_use: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: bool,
    pub vision: bool,
    pub structured_output: bool,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}
```

能力是事实描述，不是策略许可。即使 provider 支持 tool use，当前 turn 仍只能获得 policy 允许的 descriptor。

### 4.5 流事件

```rust
pub enum ModelStreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallDelta(ToolCallDelta),
    Usage(ModelUsage),
    Completed { reason: ModelStopReason },
}
```

`ToolCallDelta` 包含稳定 `index`，以及可分片到达的 `call_id`、`name` 和 `arguments_delta`。Phase 2B 的 assembler 必须完整组装并解析 JSON 后，才能提交给 Tool runtime。

`ModelUsage` 使用可选 input/output/reasoning/cache token 数，保留 provider 未返回某字段的事实，禁止用零伪装未知。

`ModelStopReason` 首版包括 Completed、ToolCalls、Length、ContentFilter、Cancelled 和 Other(String)。

### 4.6 Port

```rust
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

约束：

- 返回 `Ok(stream)` 只表示请求已建立，不表示 turn 成功。
- 建连错误由外层 `Result` 返回；流中断由 stream item 的 `Err(ModelError)` 返回。
- 正常流必须以恰好一个 Completed 结束，Completed 后不得产生事件。
- cancellation 必须最终终止流；provider adapter 负责关闭网络/body/task。
- 重试不在 port trait 默认实现中，Phase 2B 根据 `Retryability` 和副作用阶段决定。

## 5. 工具契约

### 5.1 身份

新增 `ToolCallId` 和 `ToolName`。

`ToolName` wire 格式为 `<namespace>:<name>`。namespace 和 name 必须非空，只允许 ASCII 字母、数字、`_`、`-` 和 `.`；未限定名称不进入新 catalog。当前 `Lato:read_file` 可在 adapter 中规范化为 `lato:read_file`，但 Phase 2A 不改变旧 wire 名称。

### 5.2 Descriptor

```rust
pub struct ToolDescriptor {
    pub name: ToolName,
    pub version: semver::Version,
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
```

枚举：

- `ToolCapability`：FileRead、FileWrite、Process、Network、Memory、Task、Other(String)。
- `SideEffect`：None、WorkspaceRead、WorkspaceWrite、ExternalMutation。
- `ToolConcurrency`：Parallel、Serial、ResourceKeyed。
- `ToolIdempotency`：Idempotent、WithKey、NonIdempotent。
- `ToolCancellation`：Cooperative、KillProcess、Unsupported。
- `ToolLayer`：Builtin、User、TrustedProject、SessionOverride。
- `ToolSource`：layer、source ID、可选 replacement 声明。

Descriptor validation 拒绝：

- description 为空。
- input schema 不是 JSON object。
- timeout 或输出上限为零。
- 重复 capability。

replacement target 与 descriptor name 的一致性需要读取现有注册项，因此由 Catalog 注册验证处理，而不是 descriptor 的独立 shape validation。

### 5.3 Replacement

```rust
pub struct ToolReplacement {
    pub target: ToolName,
    pub compatible_major: u64,
}
```

覆盖已有工具必须同时满足：

1. 新工具 layer 严格高于旧工具。
2. 新 descriptor 显式声明 replacement。
3. replacement target 等于被覆盖工具 name。
4. `compatible_major` 等于旧版本 major。
5. 新工具自身版本 major 等于 `compatible_major`。

任一条件不满足即拒绝，registry 保持原值不变。

### 5.4 调用

```rust
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError>;
}
```

首版 `ToolOutput` 包含 `content`、`metadata`、`truncated` 和可选 `artifact_path`。`ToolError` 包含稳定 code、message、retryability；输出给用户前仍需由上层脱敏。

Phase 2A 不实现 schema validator 和固定执行管线。Catalog 只做注册与解析；Phase 2B/3 将 Validate、Policy、Approval、Hooks、Locks、Sandbox、Execute、Output store 分开实现。

## 6. ToolCatalog

`lato-tools/src/catalog.rs` 新增：

```rust
pub struct ToolCatalog {
    tools: BTreeMap<ToolName, RegisteredTool>,
}

pub struct RegisteredTool {
    descriptor: ToolDescriptor,
    tool: Arc<dyn Tool>,
}
```

公开方法：

- `new()` / `default()`。
- `register(Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError>`。
- `resolve(&ToolName) -> Option<Arc<dyn Tool>>`。
- `descriptor(&ToolName) -> Option<&ToolDescriptor>`。
- `descriptors() -> Vec<ToolDescriptor>`，按 qualified name 确定性排序。
- `len()` / `is_empty()`。

`register` 在读取 descriptor 后先执行 validation，再处理冲突。成功返回 Inserted 或 Replaced；重复注册相同 Arc 仍按 collision 处理，避免静默掩盖装配错误。

错误包括 InvalidDescriptor、Duplicate、LowerOrEqualLayer、ReplacementRequired、ReplacementTargetMismatch 和 IncompatibleMajorVersion。错误携带工具 name，但不持有 tool implementation，保证失败后 registry 没有半写入状态。

Catalog 不处理 policy、turn 过滤或动态 tool search。后续调用方可以从确定性的 descriptor 列表生成获准子集。

## 7. 错误与 wire 稳定性

模型和工具错误都保持结构化 code 与 `Retryability`。错误类型可转换为 `AgentError`，category 分别固定为 Model 和 Tool；转换不得通过 message 字符串猜测类别。

所有需要持久化或跨 adapter 的类型使用 `snake_case` enum 和显式 tagged enum。`CancellationToken`、trait object 和 registry handle 不序列化。

Phase 2A 的 JSON 契约测试固定：

- model stream event tag。
- tool descriptor enum 值。
- qualified tool name。
- semver 字符串。
- errors 的 code/retryability。

新增可选字段可以保持 schema version；改变字段含义、tag 或安全默认值必须进入后续版本迁移。

## 8. 测试策略

### 8.1 Core 类型测试

- checked model/tool identity。
- model message/request/event JSON round trip。
- capabilities 与 usage unknown 字段。
- descriptor validation。
- ModelError/ToolError 到 AgentError 的精确 category。

### 8.2 Port contract fixture

用 scripted `ModelPort` 验证：

- 事件顺序为 delta → usage → completed。
- 建连错误与流中错误分离。
- cancellation 能结束阻塞流。
- completed 后没有事件。

Phase 2A 提供可复用的测试 helper 或公开 contract assertion 函数时，不得把 Tokio task、HTTP 或 provider 类型泄漏进 core API。

### 8.3 Catalog 测试

- 插入和确定性排序。
- 同 layer 重名拒绝。
- 低 layer 覆盖高 layer 拒绝。
- 无 replacement 声明拒绝。
- target 不同拒绝。
- major 不兼容拒绝。
- 合法 higher-layer replacement 成功。
- 所有失败都保持原注册项。

### 8.4 Repository gates

- `cargo fmt --all -- --check`。
- Phase 2A crates 的 Clippy `--no-deps -D warnings`。
- `cargo test -p lato-core -p lato-tools`。
- `cargo test --workspace`。
- `cargo tree -p lato-core` 和 `cargo tree -p lato-tools` 边界检查。
- `cargo install --path .` 与已安装 `lato` headless smoke。

Workspace Clippy 目前可能被用户脏文件中的既有 warning 阻挡；不得为通过门禁而修改这些文件，必须同时提供 Phase 2A crate 自身的 clean Clippy 证据。

## 9. 来源与复用

优先参考并允许直接复制：

- Codex `codex-rs/core/src/tools/registry.rs`：确定性注册、重复名称拒绝、runtime 与 descriptor 分离。
- Grok Build `crates/common/xai-tool-runtime/src/tool.rs`：typed tool 与 object-safe JSON membrane、stream 终止不变量。
- Grok Build `crates/common/xai-tool-runtime/src/dispatch.rs`：object-safe dispatch 和 terminal 结果约束。
- Codex `codex-rs/model-provider/src/provider.rs`：object-safe provider future 和 provider capability 分离。

直接或结构派生的实现必须在 source ledger 记录上游固定 commit、路径、许可证和 Lato 修改。Phase 2A 不复制与 Lato 目标无关的产品 UI、遥测、账号、云端或插件市场代码。

## 10. 完成定义

Phase 2A 完成必须同时满足：

- `ModelPort` 可被 `Arc<dyn ModelPort>` 使用。
- Model event stream 能表达文本、reasoning、tool-call 分片、usage、stop 和错误。
- Tool trait 可被 `Arc<dyn Tool>` 使用。
- Descriptor 包含已批准设计要求的版本、schema、能力、副作用、并行性、幂等性、超时、输出上限、取消和来源。
- ToolCatalog 的每一种覆盖失败都不会改变原注册项。
- Catalog 列表排序确定。
- 当前旧 model/tool/actor 文件未被修改或暂存。
- Phase 1A/1B 和 workspace 测试继续通过。
- 本地安装后的 `lato` 命令继续可运行。

Phase 2B 才开始适配当前 `ModelStream`、生成新的 descriptor，并把内置工具逐个从集中式 dispatch 迁出。
