# Lato 可扩展 Agent 架构设计

- 日期：2026-09-01
- 状态：设计已确认，待实施计划
- 产品：`lato`
- 实现语言：Rust 2024 edition
- 仓库：`/Users/huangyongzhao/Documents/work/innovation/lato`
- 参考实现：
  - Codex：`/Users/huangyongzhao/Documents/work/rustproject/codex`
  - Grok Build：`/Users/huangyongzhao/Documents/work/grok-build`
- 前序设计：`docs/superpowers/specs/2026-08-31-agent-harness-design.md`
- 当前验收基线：`docs/superpowers/specs/2026-08-31-lato-acceptance.md`

## 1. 文档定位

本文定义 Lato 从当前可运行 MVP 演进为长期可扩展 Rust coding-agent 内核的目标架构。前序设计和验收文档继续约束迁移期间已经交付的行为；本文负责定义新的模块边界、运行时协议、事件存储、多代理内核及后续扩展方向。实施阶段若需要改变既有验收行为，必须先同步修改验收文档，不能以新架构为由静默破坏现有能力。

已确认的产品选择：

1. 采用“可扩展核心优先”，不追求首版功能数量与 Codex 或 Grok Build 完全相等。
2. 采用“CLI 优先、内核可嵌入”，CLI、headless、ACP 和未来客户端共享同一运行时。
3. 多代理能力“内核原生、功能渐进”：任务树、预算、取消、权限和隔离从一开始进入核心数据模型，首版只开放基础 subagent。
4. 总体方案采用“事件驱动内核 + Ports/Adapters”。
5. AgentField 是未来的进程外适配器，不侵入本地内核，也不是运行 Lato CLI 的前置依赖。

## 2. 代码复用原则

Lato 不为了展示自主实现而重复造轮子。Codex 或 Grok Build 已有成熟、边界合适且许可允许复用的实现时，优先直接复制或做最薄的 Rust 移植。

执行优先级：

1. 已有成熟且许可兼容的实现：直接复制，并保留来源注释。
2. 接口不同但逻辑成熟：复制后包一层 adapter，不重写核心算法。
3. 两边都有实现：选择测试更完整、安全边界更清晰的一份作为主实现。
4. 两边各有优势：保留一个主运行时，只移植另一方缺失的机制，避免拼接两个 loop。
5. 只有当参考实现与 Lato 核心不变量冲突时才自行重写。
6. 复制生产代码时同步复制或改写相关测试。
7. 每次复制记录源仓库、源 commit、原文件、许可、Lato 修改点和后续同步策略。

建议在被复制文件头部使用以下格式：

```rust
// Derived from: <repository>@<commit>:<path>
// License: <license identifier>
// Lato changes: <short description>
```

不得直接复制的内容包括：产品专属账号体系、云端任务、计费、品牌 UI、无关遥测、未确认许可的资源，以及会把外部产品协议类型泄漏进 `lato-core` 的代码。

## 3. 现状与参考实现结论

### 3.1 当前 Lato

当前仓库已经是可运行的 Rust coding-agent MVP，包含：

- `lato-agent`：ACP host、`SessionActor`、基础 turn/tool loop、transcript。
- `lato-ai`：模型目录、凭据、OAuth/API key、多协议请求和流式解析。
- `lato-tools`：内置工具、审批边界、路径锁、shell sandbox、web 安全和初步 worktree。
- `lato-workspace`：workspace trust、审批模式、文件锁和 sandbox profile。
- `lato-mcp`：MCP 进程和渐进发现基础。
- `lato-protocol`：JSON-RPC 与协议类型。
- 根 crate：交互 CLI、headless 和 ACP stdio。

主要结构性问题是 `SessionActor::prompt` 同时承担采样、上下文、流组装、工具循环、停止条件和错误恢复；`lato-tools::dispatch` 依赖集中式 match。继续直接增加功能会提高耦合度，因此必须先抽出协议、状态机和端口。

### 3.2 Codex 中应吸收的设计

经代码图谱和调用链检查，重点参考：

- `codex-rs/core/src/session/handlers.rs::submission_loop`：命令入口、单活动 turn、中断和完整 shutdown 生命周期。
- `codex-rs/core/src/session/turn.rs::run_turn`：turn 生命周期及采样协调，但不复制其已膨胀的单函数形态。
- `codex-rs/core/src/session/turn_context.rs::TurnContext`：每 turn 不可变配置快照。
- `codex-rs/core/src/session/session.rs::Session`：一会话至多一个活动任务。
- `codex-rs/rollout/src/recorder.rs::RolloutRecorder`：单写者、flush、恢复和持久化。
- Codex tool handlers、approval policy、command safety、sandbox backend、MCP 结果清洗、输出截断、skills 和 collaboration 生命周期。

Codex 的优势是协议边界、安全模型、恢复语义和多客户端可用性。Lato 吸收这些不变量，不复制其全部 crate 数量和产品外围能力。

### 3.3 Grok Build 中应吸收的设计

重点参考：

- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/`：模型与工具运行时的产品级组合方式，但不复制大型 agent 对象。
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/`：有界 ingress、spawn queue、child reporter、deadline 和后台任务语义。
- `crates/codegen/xai-grok-hooks/src/`：hook 发现、matcher、顺序 gate、非阻塞 hook、HTTP hook 和环境变量展开。
- `crates/codegen/xai-workflow/src/`：host-call limit、agent budget、request hash、journal record/replay。
- Grok Build 的渐进式工具发现、session compaction 防护和 worktree/subagent 组织方式。

Grok Build 的优势是扩展机制、工作流、任务协调、渐进式发现和丰富的故障处理。Lato 只吸收可复用的运行时机制。

## 4. 总体架构

```text
CLI / Headless / ACP / future TUI, HTTP, AgentField
                         │
                 Command / Event protocol
                         │
                Agent API / SessionActor
                         │
                     TurnEngine
              ┌──────────┼──────────┐
           ModelPort   ToolRuntime   TaskRuntime
              │            │            │
          Providers   Policy/Sandbox  Subagents
              │            │            │
              └────── EventStore / Journal ──────┘
                              │
                    Hooks / Skills / MCP / Plugins
```

核心规则：

- 外部入口只提交 `Command`，只消费 `Event`，不直接修改 session 内部状态。
- 一个 session 同时最多有一个前台 turn。
- `TurnEngine` 负责采样、工具循环、上下文控制、停止条件和重试协调。
- 工具调用必须经过统一 policy、审批和 sandbox 管线。
- 重要状态变化先写 journal，再通知客户端。
- subagent 继承父任务预算、取消令牌、权限上限和关联 ID。
- 扩展优先采用 Rust trait 和进程外协议；首版不承诺 Rust 动态链接 ABI。

## 5. 目标 crate 边界

| Crate | 职责 | 禁止依赖或承担的职责 |
|---|---|---|
| `lato-protocol` | 版本化 Command/Event、ID、错误 wire types、序列化 | 文件系统、网络、模型实现 |
| `lato-core` | 领域模型、ports、状态机、不变量 | Tokio actor、CLI、reqwest、具体 provider |
| `lato-runtime` | Tokio actor、turn 调度、任务树、取消、预算 | UI 呈现、具体协议解析 |
| `lato-ai` | 模型目录、鉴权、请求、流解析、provider adapters | session 状态、工具执行 |
| `lato-tools` | Tool trait、descriptor、registry、执行管线、输出管理 | CLI 审批 UI、模型循环 |
| `lato-workspace` | 路径边界、审批策略、sandbox、worktree、资源锁 | 模型协议 |
| `lato-store` | journal、snapshot、replay、compaction、索引 | turn 决策和 UI |
| `lato-extensions` | hooks、skills、MCP、plugin manifest、profiles | 绕过 ToolRuntime 或 PolicyEngine |
| `lato-agent` | 组装核心组件，提供可嵌入 `Agent` API | 复制各子系统实现 |
| 根 `lato` | CLI、交互呈现、ACP stdio 入口 | 直接执行工具或调用 provider |

迁移不采用一次性重写。先从当前 `SessionActor::prompt` 抽出 turn 状态机和 ports，再迁移存储、工具策略和 subagent。

## 6. 命令、事件与会话

### 6.1 命令模型

```rust
enum Command {
    StartTurn(StartTurn),
    SteerTurn(UserInput),
    CancelTurn { turn_id: TurnId },
    Approve(ApprovalDecision),
    SetModel(ModelSelection),
    Compact(CompactionRequest),
    SpawnTask(SpawnTaskRequest),
    CancelTask { task_id: TaskId },
    Shutdown,
}
```

`StartTurn` 在 session idle 时创建 turn。session 已有 active turn 时，调用方必须明确选择 steer、replace 或 reject；运行时不得隐式启动第二条采样循环。

### 6.2 事件模型

```rust
enum Event {
    SessionStarted(SessionSnapshot),
    TurnStarted(TurnStarted),
    ModelDelta(ModelDelta),
    ReasoningDelta(ReasoningDelta),
    ToolCallProposed(ToolCall),
    ApprovalRequested(ApprovalRequest),
    ToolCallStarted(ToolCall),
    ToolCallCompleted(ToolOutcome),
    TaskSpawned(TaskSnapshot),
    TaskProgress(TaskProgress),
    TaskCompleted(TaskOutcome),
    UsageUpdated(Usage),
    ContextCompacted(CompactionRecord),
    TurnCompleted(TurnOutcome),
    TurnFailed(AgentError),
    TurnCancelled(CancelReason),
    SessionStopped,
}
```

所有事件放入统一 envelope：

```rust
struct EventEnvelope {
    schema_version: u16,
    event_id: EventId,
    session_id: SessionId,
    turn_id: Option<TurnId>,
    task_id: Option<TaskId>,
    parent_event_id: Option<EventId>,
    sequence: u64,
    timestamp: SystemTime,
    payload: Event,
}
```

`sequence` 在单 session 内严格单调递增。事件写入成功后才能被客户端观察。EventStore 只接受规范事件，UI delta 或第三方协议对象必须先归一化。

### 6.3 Turn 状态机

```text
Idle
  → Preparing
  → Sampling
  → AwaitingApproval
  → ExecutingTools
  → RecordingResults
  → Sampling ...
  → Completed | Failed | Cancelled
```

不变量：

- 单 session 至多一个前台 turn。
- steer 进入当前 turn 的 pending input，不破坏已记录历史。
- 取消令牌按 session → turn → task → tool process 分层传播。
- tool call 在执行前持久化，取消也不能抹掉模型曾请求的调用。
- turn 完成前刷新 journal；客户端断开不等于 turn 取消。
- hook、MCP 或 UI observer 失败不能破坏核心状态机。
- 采样轮数、工具调用、subagent 数、任务深度、token、费用和时间都有硬上限。

## 7. Journal、快照与恢复

首版事件存储使用 append-only JSONL，优先可调试性；后续可增加 CBOR 或压缩文件，但规范事件语义不变。

设计：

- 每个 session 独立 journal，单写者 actor 保证顺序。
- 写入缓冲后 flush；涉及外部副作用的关键边界允许 `sync_data`。
- 定期生成 `SessionSnapshot`，恢复时加载最近快照并重放后续事件。
- 原始事件不可原地修改。
- compaction 产生新的 `CompactionRecord`，不伪装成历史从未发生。
- session 列表使用轻量索引；索引可重建，不是事实源。
- journal 保存规范化工具结果，恢复不依赖原插件仍然存在。
- 每个可能产生副作用的工具记录幂等性和恢复决策，避免崩溃后盲目重放。

恢复规则：

| 崩溃点 | 恢复行为 |
|---|---|
| tool call 尚未持久化 | 该调用不存在，不执行 |
| tool call 已记录但未开始 | 标记 interrupted，由策略决定重试或回模型 |
| tool started 但无 outcome | 非幂等工具标记 uncertain，要求用户或 verifier 处理 |
| outcome 已记录 | 不重复执行，继续投影上下文 |
| snapshot 写入中 | 丢弃不完整 snapshot，从前一快照重放 |
| compaction 中 | 保留原上下文投影，记录 compaction failure |

## 8. 模型层

```rust
#[async_trait]
trait ModelPort: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError>;

    fn capabilities(&self) -> ModelCapabilities;
}
```

统一流事件：

```rust
enum ModelStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta(ToolCallDelta),
    Usage(Usage),
    Completed(ModelStopReason),
}
```

要求：

- 供应商 SSE/JSON 在 `lato-ai` 内归一化。
- tool-call 分片完整组装并校验后才能进入 ToolRuntime。
- 模型目录、鉴权、请求构造和响应解析分离。
- provider/model 由配置选择，核心不硬编码模型。
- 每个 turn 固化 `ModelSelectionSnapshot`；切换只影响下一 turn。
- capability 显式描述 tool use、reasoning、vision、context、parallel calls 和 structured output。
- 只重试可恢复的网络、限流和明确无副作用阶段。

当前 `lato-ai` 的多供应商目录、鉴权、发现和流解析保留，并逐步适配 `ModelPort`。参考实现中更完整的协议 parser 可以直接复制。

## 9. 工具运行时

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError>;
}
```

`ToolDescriptor` 至少包含 qualified name、版本、输入 schema、capability、副作用等级、并行性、幂等性、超时、输出上限、取消行为和来源。

固定执行管线：

```text
Resolve
→ Validate schema
→ Evaluate policy
→ Request approval when needed
→ Run pre-tool hooks
→ Acquire resource locks
→ Enter sandbox
→ Execute
→ Store/truncate large output
→ Run post-tool hooks
→ Emit canonical outcome
```

工具注册表按以下顺序分层：

```text
builtin < user extension < trusted project extension < session override
```

同名默认拒绝覆盖。只有显式声明替换目标和兼容版本时才能替换。模型只看到当前 turn 获准使用的工具。工具数量较多时，先暴露分类和搜索工具，再按需加载完整 schema。

## 10. Policy、审批与 Sandbox

```rust
enum PolicyDecision {
    Allow,
    Deny { reason: String },
    Ask(ApprovalRequest),
    AllowInSandbox(SandboxProfile),
}
```

策略输入包括工具身份、来源、规范化参数、workspace 信任、当前权限上限、文件/网络/进程影响范围、父任务 capability 和审批凭据。

安全不变量：

- 子任务只能收窄父任务权限。
- 审批发生在真实执行边界。
- `allow once` 绑定规范化调用指纹。
- deny 优先级最高，workspace trust 不能覆盖 deny。
- 项目 hooks、MCP 和 plugins 在未信任时禁用。
- 所有审批决定写 journal。
- shell、文件和网络分别判定，不能只使用一个 `trusted` 布尔值。
- sandbox 后端缺失或启动失败时 fail closed。
- 文件路径经过绝对化、规范化、软链接边界检查和平台大小写处理。
- `web_fetch` 拒绝 loopback、私网、link-local、非 HTTP(S)、危险重定向和 DNS 重绑定。
- 输出、日志、事件、环境变量和 hook 错误统一脱敏。

首版保留 `Off`、`WorkspaceWrite`、`ReadOnly` 三种 profile。`Off` 仅能由明确高信任配置启用，不能作为 sandbox 失败后的降级路径。

## 11. 多代理任务树

### 11.1 任务模型

```rust
struct TaskNode {
    id: TaskId,
    parent_id: Option<TaskId>,
    owner: AgentId,
    kind: TaskKind,
    status: TaskStatus,
    scope: TaskScope,
    budget: Budget,
    permissions: CapabilitySet,
    workspace: WorkspaceLease,
    result_contract: ResultContract,
}
```

状态：

```text
Queued
→ Preparing
→ Running
→ WaitingForChildren | WaitingForApproval
→ Verifying
→ Completed | Failed | Cancelled | TimedOut
```

所有动态委派受最大深度、每父任务最大子任务数、全局并发、token、费用、工具调用、时间、重试和 worktree 数量限制。

### 11.2 Agent Profile

```rust
struct AgentProfile {
    name: String,
    instructions: String,
    model_policy: ModelPolicy,
    tool_filter: ToolFilter,
    workspace_mode: WorkspaceMode,
    verification: VerificationPolicy,
}
```

首版内置：

- `explorer`：只读搜索与证据收集。
- `worker`：在有界范围和独立 worktree 中修改代码。
- `reviewer`：只读检查结果，不修改 worker 输出。

profile 是数据，不是调度器中的硬编码角色类。后续可由 skills、plugins 或用户配置扩展。

### 11.3 Spawn 流程

```text
Parent decides delegation
→ Validate result contract
→ Reserve budget
→ Narrow permissions
→ Allocate workspace/worktree
→ Persist TaskSpawnRequested
→ Spawn child runtime
→ Stream child events
→ Verify result contract
→ Return structured result
```

子任务必须提供单一目标、上下文范围、工具范围、预算、截止时间、输出结构和完成/失败条件。父任务不复制完整对话，只传必要 context package、artifact 或事件引用。

首版支持条件委派、运行时 fan-out、有界并行和 reviewer 发现明确问题后的二次修复任务；不支持无限递归、自行扩大权限、自动合并、自动发布和跨运行自我修改配置。

### 11.4 验证阶梯

- 搜索/总结：schema 和引用文件存在性。
- 代码修改：编译、测试、lint 和其他程序化不变量。
- 风险修改：独立 reviewer。
- 合并、部署、外部写操作：用户审批。

代码任务默认形态：

```text
Explore
→ Plan
→ Worker(s) in isolated worktrees
→ Programmatic verification
→ Reviewer
→ Parent decides integration
```

实际 fan-out 由任务可分解性决定，并受整数预算限制。

### 11.5 Workspace 隔离

- 只读任务可以共享主 workspace。
- 并行写任务默认独立 Git worktree 和分支。
- 非 Git 目录不允许多 agent 并行写，降级为串行或临时复制。
- 子任务完成不自动合并，只返回 patch、commit 或 artifact。
- `WorkspaceManager` 统一管理 worktree 创建、租约、保留和清理。

## 12. 扩展系统

### 12.1 Hooks

标准 hook 点包括 SessionStart/Stop、TurnStart/End、Pre/PostModel、Pre/PostTool、TaskSpawn/Complete 和 Before/AfterCompact。

- `Observer` 异步旁路运行，失败不阻塞主流程。
- `Gate` 顺序运行，可允许、拒绝或补充上下文，必须有超时和明确 fail-open/fail-closed 策略。
- command hook 和 HTTP hook 使用不同执行器。
- HTTP hook 复用 SSRF 防护。
- hook 不能扩大权限或修改不可变事件。

优先复制 Grok Build hooks 的 discovery、matcher、environment expansion、sequential gate、non-blocking dispatch 和 HTTP 安全实现。

### 12.2 Skills

```text
skill-name/
├── SKILL.md
├── references/
├── scripts/
├── assets/
└── skill.toml
```

skill 是指令和资源包，不直接获得执行权限。加载采用渐进披露：先摘要，命中后读完整指令，仅在引用时加载附加资源。skill script 仍通过普通 ToolRuntime、PolicyEngine 和 sandbox。

### 12.3 MCP

`lato-mcp` 负责 transport、server 生命周期、qualified names、schema 缓存、渐进发现、超时、取消和结果清洗。MCP 工具进入统一 ToolRegistry，不能绕过 policy。项目 MCP 在 workspace 未信任时禁用。

### 12.4 Plugins

首版 plugin 是清单和资源的分发单元：

```text
my-plugin/
├── lato-plugin.toml
├── skills/
├── hooks/
├── mcp/
├── profiles/
└── workflows/
```

清单声明 plugin ID、语义版本、最低 Lato 版本、capabilities、配置 schema、权限请求、依赖、冲突和内容哈希。外部原生能力通过 MCP/子进程或稳定协议接入。Rust `dylib` ABI 不在首版范围；核心接口稳定后再评估 WASM Component Model。

### 12.5 Workflows

首版只定义接口和 manifest：

```rust
trait Workflow {
    async fn run(
        &self,
        context: WorkflowContext,
        input: Value,
    ) -> Result<WorkflowOutcome, WorkflowError>;
}
```

后续可复制 Grok Build workflow 的脚本 sandbox、agent budget、host-call limit、request hash、journal record/replay 和有界并行。工作流调用 agent 或工具时仍通过核心运行时。

## 13. 配置与兼容性

配置合并顺序：

```text
built-in defaults
< user config
< project config
< profile config
< session config
< CLI arguments
< one-command override
```

安全字段只能收窄，不能由低信任层扩大。配置分为：

- `StaticConfig`：数据目录、协议主版本等运行时不可变字段。
- `SessionConfig`：创建 session 时固化。
- `TurnConfig`：每 turn 生成不可变快照。
- `ReloadableConfig`：模型目录、展示偏好和可安全刷新的扩展配置。

合并后生成带字段来源的 `ResolvedConfig`。secret 使用引用，不进入普通配置、事件和日志。

显式版本包括 `protocol_version`、`manifest_version`、`tool_schema_version`、`event_schema_version` 和 `min_lato_version`。新增可选字段可以向后兼容；删除字段、改变含义或改变默认安全行为必须提升主版本。未知权限、hook 类型或 manifest 主版本默认拒绝加载。

## 14. 错误模型

```rust
struct AgentError {
    code: ErrorCode,
    category: ErrorCategory,
    message: String,
    retryability: Retryability,
    severity: Severity,
    source: ErrorSource,
    context: ErrorContext,
}
```

类别包括 InvalidInput、Configuration、Model、Tool、Policy、Sandbox、Storage、Extension、Task 和 InternalInvariant。

规则：

- 面向用户的信息简洁可操作，详细 cause chain 进入脱敏诊断日志。
- retryability 由类型决定，不依赖字符串匹配。
- 工具默认不自动重试，除非显式声明幂等。
- observer hook 失败为 warning；gate hook 按策略处理。
- journal 写入失败为致命错误，停止继续产生不可恢复状态。
- 子任务错误局部化，由父任务决定降级、重试或终止。

## 15. 可观测性

统一关联链：

```text
session_id → turn_id → task_id → model_call_id/tool_call_id
```

内核发布 tracing spans、metrics、usage/cost events 和 journal events，并允许可选 OpenTelemetry exporter。默认本地运行不上传遥测。

核心指标包括 turn 成功率与时延、首 token 时延、模型重试、工具成功率、审批等待、sandbox 启动、context 大小、compaction、subagent fan-out/深度/排队、预算消耗、扩展失败率和 journal flush/snapshot/recovery 时延。

## 16. 测试策略

### 16.1 单元与状态机测试

覆盖状态迁移、配置合并、权限收窄、预算、事件升级、路径规范化、schema 和输出截断。

### 16.2 端口契约测试

每个 `ModelPort`、`Tool`、`EventStore`、`SandboxBackend` 和 `ExtensionHost` 实现必须通过共享 contract suite。

### 16.3 确定性运行时测试

使用 scripted model 覆盖文本、单/并行 tool calls、call 分片、steer/cancel、context overflow、限流、流中断、重复调用和拒绝使用必要工具。

### 16.4 安全测试

覆盖路径穿越、软链接逃逸、大小写绕过、shell 混淆、环境变量泄漏、SSRF/重定向/DNS 重绑定、审批重放、不可信扩展、子任务权限提升、预算逃逸和 sandbox fail-closed。

### 16.5 故障注入

在 tool call 记录前后、执行中、journal flush、snapshot、compaction、worktree 创建和父任务取消等边界模拟崩溃。恢复后状态必须确定，且不能重复不可安全重放的副作用。

### 16.6 集成与跨平台

- CLI 和 ACP 驱动同一内核并观察等价事件。
- MCP、hooks 和 worktree 使用本地 fixture。
- LIVE provider tests 不进入默认 PR CI。
- macOS、Linux、Windows 执行编译和平台行为门禁。
- 常规门禁：fmt、clippy `-D warnings`、workspace tests。

## 17. 分阶段迁移路线

### Phase 0：行为基线

- 冻结现有 acceptance matrix。
- 补齐模型流、审批、取消、ACP、工具和恢复 fixtures。
- 建立参考代码来源清单。
- 验收：workspace tests、clippy、CLI/headless/ACP smoke、`cargo install --path .`。

### Phase 1：协议与核心状态机

- 新增 `lato-core`、`lato-runtime`。
- 定义 Command/Event、ID、错误和状态机。
- CLI/ACP 改为命令与事件；旧 `prompt` 暂作兼容适配器。
- 引入分层取消和终止原因。
- 验收：CLI/ACP 事件等价，核心不依赖具体 provider 或客户端。

### Phase 2：模型与工具端口

- 当前模型流接入 `ModelPort`。
- 建立 Tool trait、descriptor、registry 和执行管线。
- 从集中式 `dispatch` 逐个迁出内置工具。
- 接入 schema、输出存储、截断、并行性和幂等性。
- 验收：新增工具不修改 turn loop；新增 provider 不修改 core。

### Phase 3：Policy、审批与 Sandbox

- 建立统一 PolicyEngine。
- 将 workspace trust 拆成文件、进程、网络和扩展 capabilities。
- 审批绑定调用指纹。
- 平台 sandbox 统一 contract。
- 验收：无旁路、权限不可扩大、后端失败不裸执行。

### Phase 4：Journal、快照与压缩

- 新增 `lato-store`。
- transcript 迁移为事件 journal。
- 实现 snapshot/replay/list/resume/index/compaction。
- 加入故障注入。
- 验收：关键边界崩溃可恢复，非幂等副作用不重复执行。

### Phase 5：基础多代理

- 实现 TaskTree、BudgetLedger、AgentProfile、WorkspaceLease。
- 提供 explorer、worker、reviewer。
- 支持 spawn/send/wait/cancel 和事件聚合。
- 写任务默认独立 worktree。
- 验收：取消向下传播、权限预算不可逃逸、并行写隔离。

### Phase 6：扩展系统

- 新增 `lato-extensions`。
- 依次实现 hooks、skills、MCP registry 和 plugin manifest。
- 验收：不可信项目不执行扩展，observer/gate 行为确定，历史不依赖插件仍安装。

### Phase 7：工作流与远程适配

- 引入有预算、可恢复 workflow。
- 增加 AgentField adapter 和未来 daemon 客户端协议。
- 验收：本地 CLI 不依赖 AgentField，远程失败不污染本地 session，远程 execution ID 可关联本地事件。

每阶段完成 feature 或 bug fix 后，按仓库规则运行相关测试，并执行 `cargo install --path .` 部署本地 `lato` 命令。

## 18. 首个稳定版本完成定义

- CLI、headless、ACP 共用同一内核。
- 至少支持 OpenAI-compatible 与 Anthropic-compatible 协议。
- 模型、工具、存储和 sandbox 均可替换。
- session 可恢复，事件可重放。
- 安全策略集中并 fail closed。
- MCP、skills 和基础 hooks 可扩展。
- subagent 具有任务树、预算、取消和 worktree 隔离。
- 所有 loop 和动态 fan-out 都有硬上限。
- 安装后使用 `lato` 即可运行。

## 19. 明确非目标

- 首版完整 TUI/pager 和语音。
- 云端账号、计费、团队权限和云任务。
- 自动 commit、merge、push、deploy 或 publish。
- 无限递归 agent。
- agent 自行修改全局配置或自动上线新版本。
- Rust `dylib` 插件 ABI。
- 为追求形式而复制 Codex/Grok Build 的全部 crate。
- 与核心 coding-agent 无关的产品专属外围能力。

## 20. 架构验收清单

实施评审必须回答：

- 客户端是否只使用 Command/Event？
- 新 provider 是否无需修改 turn loop？
- 新工具是否无需修改集中式 dispatch？
- 外部工具是否全部经过 policy、审批和 sandbox 边界？
- journal 是否先于外部可见事件写入？
- 崩溃恢复是否避免重复不可重放副作用？
- 子任务权限、预算和取消是否由父级约束？
- 并行写是否隔离到独立 worktree？
- 每个动态循环是否有明确整数上限？
- 扩展卸载后历史 session 是否仍可读取？
- CLI 是否仍可通过 `cargo install --path .` 本地部署并运行？

任一答案为否，都不能声称对应阶段完成。
