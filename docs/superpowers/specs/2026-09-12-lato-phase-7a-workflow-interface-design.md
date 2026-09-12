# Lato Phase 7A Workflow Interface & Manifest — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **A-level gate passed**（2026-09-12；见 `docs/testing/reports/phase-7a-workflow-interface-release-gate-2026-09-12.md`） |
| 日期 | 2026-09-12 |
| 基线 commit | `master @ be7c450`（Streamable HTTP 客户端已合入工作区） |
| 上游边界 | 可扩展 Agent 设计 §12.5、§17 Phase 7；Phase 6A 快照契约；`TaskOwner::Workflow` / `cancel_workflow` |
| 后续 | 7B 脚本引擎与可恢复运行；7C AgentField / daemon 适配 |

---

## 1. 背景

Phase 6 已把插件快照、Skills、Hooks、MCP 接到同一信任/启用/generation 边界。`lato-core` 已有 `TaskOwner::Workflow { run_id, session_id }`，`TaskCoordinator::cancel_workflow` 已能按 `run_id` 取消工作流子树。可扩展 Agent 设计 §12.5 规定：**首版只定义接口和 manifest**，不复制 Grok Build 的脚本 sandbox、有界并行、journal replay。

当前仓库没有 workflow 描述符物化，也没有 `Workflow` trait 实现面。若直接上 Rhai 引擎或 AgentField，会在契约未冻结时引入第二条执行通道。7A 先冻结「谁可以声明 workflow、如何命名、如何收窄、如何取消」，运行时保持 **inert**：可以列出与校验，不得编排 agent。

## 2. 目标

交付 Phase 7A，使可信且启用的插件 workflow 成为 **冻结快照上的只读描述符 + 稳定 trait**，而不是可执行编排器。

1. 仅从 turn 冻结的 `PluginSnapshot` 消费 trusted ∧ enabled 的 workflow 描述符。
2. 定义 `Workflow` trait、上下文、结果、稳定错误码。
3. 声明的 `agent_budget` 只记录上限，不消耗 `BudgetLedger`。
4. 未来运行必须能挂到已有 `TaskOwner::Workflow` 与 `cancel_workflow`；本阶段不启动 run。
5. 本地 CLI **不**依赖 AgentField。

## 3. 完成定义（Definition of Done）

1. 未信任/未启用插件的 workflow 描述符集为空；坏 JSON / 路径逃逸被隔离并诊断。
2. `materialize_workflows(snapshot)` 产出 generation 绑定的不可变 `WorkflowDescriptorSet`。
3. 限定名稳定（建议 `plugin/workflow` 或 `plugin__workflow`，全文一种，禁止静默覆盖）。
4. `Workflow::run` 在 7A 对所有已物化描述符返回 `workflow.not_implemented`（或等价 inert 结果）；测试证明没有子 agent、没有工具调用、没有预算消耗。
5. 描述符可声明 `agent_budget`（闭区间 1..=1024，默认 128）；非法值被诊断并丢弃该条目。
6. 子会话只能收窄可见 workflow 集合，不能恢复父已移除项。
7. 门禁：`cargo fmt --check`、focused tests、`cargo test --workspace`、Clippy `-D warnings`、`cargo install --path .`、README 一小节、本设计状态改为 A-level 门禁通过。

## 4. 范围与非范围

### 4.1 范围内（Phase 7A）

| # | 工作流 | 摘要 |
| --- | --- | --- |
| 1 | Manifest 契约 | `plugin.json` 增加 `workflows`：路径或 inline；文件须落在插件根内 |
| 2 | 描述符物化 | `lato-extensions` 从 `PluginSnapshot::active_plugins()` 物化；`PluginComponentKind::Workflows` |
| 3 | Trait 与错误 | `lato-workflow`（新 crate）持有 `Workflow`、`WorkflowContext`、`WorkflowOutcome`、`WorkflowError` |
| 4 | 预算声明 | 描述符记录 cap；不调用 `BudgetLedger::reserve` |
| 5 | 取消挂钩文档 + 契约测试 | 证明 `TaskOwner::Workflow` + `cancel_workflow` 仍是唯一取消面；7A 不新开取消通道 |
| 6 | 收窄 | `derive_child` 后可见 workflow 只能减少 |
| 7 | 门禁 | tests、clippy、install、README |

### 4.2 明确非范围

- Rhai / 任何脚本解释器、`agent()` / `parallel()` / `phase()` host API。
- 可恢复 journal replay、pause/resume、跨进程 run。
- AgentField adapter、daemon 客户端协议。
- TUI `/workflows` 面板、slash 启动。
- MCP Resources / Prompts。
- 把 workflow 注册成模型可见 tool（7B 再决定是否经 `ToolRuntime`）。

## 5. 架构边界

```text
PluginSnapshot (gen N, trusted+enabled only)
        │
        ▼
   materialize_workflows          ← lato-extensions
   - parse plugin.json workflows / file / inline
   - path confinement, name normalize, collisions
        │
        ▼
   WorkflowDescriptorSet          ← lato-workflow
   - generation, descriptors, diagnostics
   - inert Workflow impl: run → not_implemented
        │
        ▼
   (7B) WorkflowEngine            ← 仍走 TaskCoordinator
   - TaskOwner::Workflow { run_id, session_id }
   - cancel_workflow(run_id)
   - BudgetLedger 在真正 spawn 时扣减
```

**安全不变量：** 7A 不得调用模型、工具、shell 或 MCP。描述符物化失败不得拖垮同代 Skills/Hooks/MCP。

## 6. 配置契约

### 6.1 来源

唯一输入：当前 turn 冻结 `PluginSnapshot` 中 **active** 插件的 `workflows` 字段（路径或 inline）。项目插件在 folder 未信任时零描述符。

`plugin.json` 示例：

```json
{
  "name": "demo-plugin",
  "workflows": {
    "review-changes": {
      "description": "Review a diff",
      "agentBudget": 32,
      "whenToUse": "After a local patch is ready"
    }
  }
}
```

或 `"workflows": "workflows.json"` / `"workflows": ["workflows/"]`，解析后的文件必须仍在插件 canonical root 内。

### 6.2 描述符字段

| 字段 | 约束 |
| --- | --- |
| `id` | `{plugin}/{workflow}`，workflow 名 `[a-z0-9][a-z0-9_-]{0,63}` |
| `plugin_name` | 来自快照 |
| `name` | 规范化后的 workflow 名 |
| `description` | 可选；截断到现有诊断/描述上限同类（建议 4 KiB） |
| `when_to_use` | 可选短句 |
| `agent_budget` | `u32`，默认 128，硬顶 1024，最小 1 |
| `source_dir` | 插件根 |
| `generation` | 快照 generation |

7A **不**解析脚本正文。若文件存在但无法识别为描述符对象，隔离该条目。

### 6.3 碰撞

跨插件相同限定名：保留先到者，诊断 `workflow.collision`。禁止静默覆盖。

## 7. Trait

```rust
pub struct WorkflowContext {
    pub run_id: String,
    pub session_id: SessionId,
    pub generation: u64,
    pub cancel: CancellationToken,
    pub agent_budget: u32,
}

pub struct WorkflowOutcome {
    pub run_id: String,
    pub status: WorkflowStatus, // NotImplemented | (7B: Completed/Failed/Cancelled)
    pub output: Value,
}

#[async_trait]
pub trait Workflow: Send + Sync {
    fn descriptor(&self) -> &WorkflowDescriptor;
    async fn run(
        &self,
        context: WorkflowContext,
        input: Value,
    ) -> Result<WorkflowOutcome, WorkflowError>;
}
```

7A 提供 `InertWorkflow`：`run` 立即返回 `Err(WorkflowError::NotImplemented)` 或 `status: NotImplemented`（择一，全文一致）。推荐 **错误码** `workflow.not_implemented`，避免调用方误认为成功。

`run_id` 由调用方传入；7A 测试用固定 fixture id。7B 再规定生成规则（session 内唯一、用户可见 display name 另计）。

## 8. 预算与取消

- 描述符上的 `agent_budget` 是 **声明的硬顶**，写入 `WorkflowContext.agent_budget`。
- 7A 禁止 `BudgetLedger` 记账。测试断言 ledger 在 `run` 前后不变。
- 取消：不新增 API。契约测试复用现有 `cancel_workflow(run_id)`（已在 `lato-runtime` 覆盖）。7A 只文档化：未来 engine 创建的任务必须使用 `TaskOwner::Workflow`，丢弃 spawn future 必须走现有 workflow drop 收割。

## 9. Reload 与子会话

- N / N+1：与 MCP 相同，描述符集绑定 generation；7A 无运行中的 engine，故无「进行中 turn 继续用旧 run」。
- `derive_child`：增加 workflow 名 allowlist 收窄（可与 MCP ceiling 并列 `WorkflowCapabilityCeiling`）。默认继承父全集；子只能 remove。测试：父有 A、B，子 allowlist A，物化结果不含 B；子不能把父未启用插件的 workflow 加回来。

## 10. 验收矩阵

| ID | 验收 |
| --- | --- |
| W-1 | 仅 trusted+enabled 物化；未信任项目 workflow 文件零描述符 |
| W-2 | 路径逃逸拒绝；坏 JSON 隔离 |
| W-3 | 限定名稳定；碰撞诊断且保留先到 |
| W-4 | `agent_budget` 缺省 128；0 或 1025 丢弃并诊断 |
| W-5 | `InertWorkflow::run` 不 spawn、不调 ToolRuntime、不改 BudgetLedger |
| W-6 | 错误码 `workflow.not_implemented` 稳定 |
| W-7 | 子会话只能收窄 |
| W-8 | `PluginComponentKind::Workflows` 在有描述符时出现 |
| W-9 | workspace tests、Clippy、`cargo install --path .`、README |

## 11. 文件结构（实施时）

- 新建 `crates/lato-workflow/`：`lib.rs`、`error.rs`、`types.rs`、`inert.rs`
- `crates/lato-extensions/src/workflows/mod.rs`：物化
- 修改 `manifest.rs`、`registry.rs`（LoadedPlugin 字段 + component kind）
- 测试：`crates/lato-extensions/tests/workflow_config.rs`、`crates/lato-workflow` 单元测试
- README：Plugin Workflows 一小节，标明 **inert，执行属 7B**

## 12. 开放问题（本草案已拍板，评审可改）

1. 限定名用 `plugin/workflow`（路径风格）还是 `plugin__workflow`（与 MCP `server__tool` 一致）→ **推荐 `plugin/workflow`**，因为 workflow 不是 tool wire name。
2. `run` 用 `Result<Outcome, Error>` 还是成功 Outcome 带 `NotImplemented` → **推荐 Err**，调用方不能忽略。
3. 7A 是否在 `lato doctor` 列出 workflow 名 → **否**，避免用户以为可运行；README 写明 inert。

## 13. 参考

- `docs/superpowers/specs/2026-09-01-lato-extensible-agent-design.md` §12.5、§17 Phase 7
- `docs/superpowers/specs/2026-09-07-lato-phase-6a-plugin-runtime-foundation-design.md` §16
- `crates/lato-core/src/task.rs` `TaskOwner::Workflow`
- `crates/lato-runtime/src/task/protocol.rs` `cancel_workflow`
- Phase 6C MCP 物化模式：`materialize_mcp` / `McpDescriptorSet`
