# Lato Phase 7B7：模型可见 Workflow Tool 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **已冻结，待实现** |
| 规格版本 | v1.0 |
| 日期 | 2026-09-16 |
| 适用版本 | `origin/master` ≥ `86cd365`（包含 7B5/7B6） |
| 目标版本 | Phase 7B7；具体发行版本待确认 |
| Grok Build 参考点 | named workflow 的模型可发现、启动和状态查询；只借鉴流程与完成度，不复制品牌文案、代码或资产 |
| 上游依赖 | 7B4 会话级 `WorkflowManager`；7B5 journal/跨进程恢复；7B6 Host helpers；现有 `ToolRuntime`/policy/approval membrane |
| 后续 | 7C AgentField，必须另立 design spec，不得并入本刀 |

## 1. 决策摘要

Phase 7B7 向主会话模型注册一个内建工具 `workflow`。模型可用它：

1. `list`：发现当前会话可见的 named workflow；
2. `start`：按名字启动一个后台 run；
3. `status`：查询一个 run 或最近 run 列表。

工具只是一层薄适配器：它必须调用当前 `RuntimeSession` 已挂载的同一个
`WorkflowManager`，复用 7B4–7B6 的 registry、并发上限、journal、host service、
更新广播和恢复规则。工具调用在拿到启动快照后立即返回；workflow 自己的模型调用
和 host loop 继续在 Manager 管理的后台任务中运行。禁止创建第二个主会话 turn loop、
第二个 Manager，或经 ACP JSON-RPC 自我回调。

v1 不让模型 `pause`、`resume`、`stop`。这些改变既有 run 的控制动作仍由用户通过
TUI/ACP 发起，避免模型自行恢复已被用户暂停的工作或停止用户启动的 run。

## 2. 用户价值与用户故事

**用户价值：** 用户描述目标后，主模型能主动发现并调用已经审核的编排脚本，不必让
用户记住 `/workflow` 命令；同时所有启动仍经过与普通工具相同的权限和审计膜。

- 作为交互式用户，我希望模型在适合时列出/启动 named workflow，并立即继续主会话。
- 作为安全敏感用户，我希望项目和插件 workflow 仍受 trust、policy 与审批约束。
- 作为恢复会话的用户，我希望模型查询到恢复后的真实 run，而不是另一份内存副本。
- 作为验收人员，我希望每次工具调用都有稳定、结构化、可截断的结果和错误码。

## 3. 范围、非目标与兼容性

### 3.1 范围内

| 编号 | 功能 | 目标 | 优先级 | 主要风险 |
| --- | --- | --- | --- | --- |
| 7B7-1 | 模型工具注册 | 主会话模型看见唯一 `workflow` 工具 | P0 | wire name 冲突或未绑定会话 |
| 7B7-2 | `list` | 返回当前 trust/plugin snapshot 下的 named scripts | P0 | 泄露不可信项目定义 |
| 7B7-3 | `start` | 经 policy/approval 后调用同一 Manager 启动后台 run | P0 | 绕过审批、重复启动、另开 loop |
| 7B7-4 | `status` | 按 display name/run id 查询真实快照或有界列表 | P0 | 状态映射失真、输出无界 |
| 7B7-5 | 测试与文档 | 覆盖权限、状态、并发、resume、入口回归 | P0 | 仅单测适配器而漏掉端到端接线 |

### 3.2 明确非目标

- 7C AgentField、AgentField UI/调度/协议；
- 模型发起 `pause`、`resume`、`stop`，或 CLI 增加这些子命令；
- 创建/编辑/保存 workflow，生成 `.rhai`，或 `/workflow save`；
- 等待 run 完成的阻塞式工具调用、把 run 输出注入当前工具结果；
- 新的 turn loop、Manager、journal 格式或 workflow 引擎；
- Grok 全屏 dashboard、语音、Grove、插件市场。

### 3.3 兼容范围与迁移

- 工具为新增能力；现有 `/workflow ...`、ACP 方法和 `lato workflow list|run` 行为不变。
- 7B5 `run.json` / `script.rhai` / `journal.jsonl` 无 schema migration。
- 旧会话能在 `session/resume` 挂载 Manager 后使用工具；无法挂载时 fail closed。
- OpenAI/Anthropic/Codex 等模型适配器继续消费统一 `ToolDescriptor`，不得写方言专用分支。

## 4. 模型可见协议

### 4.1 描述符

建议 canonical name 为 `builtin:workflow`，模型 wire name 为 `workflow`，版本 `1.0.0`。

| 元数据 | 值 |
| --- | --- |
| capability | `task_control`（现有 `ToolCapability::TaskControl`） |
| side effect | `external_mutation`；因为 `start` 可派生模型调用、文件/进程/网络副作用 |
| concurrency | `serial`（对单会话 Manager 的启动/查询顺序稳定） |
| idempotency | `non_idempotent`（`start` 每次均创建新 run） |
| cancellation | `cooperative`；取消工具调用不等于停止已成功启动的 run |
| timeout | 20 秒；只覆盖解析、审批与 launch 返回，不等待 run |
| max output | 64 KiB；列表超过上限按 §7 截断 |

统一输入 schema：

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "action": { "type": "string", "enum": ["list", "start", "status"] },
    "name": { "type": "string", "minLength": 1, "maxLength": 256 },
    "run": { "type": "string", "minLength": 1, "maxLength": 256 },
    "args": { "type": "object" },
    "agentBudget": { "type": "integer", "minimum": 1 }
  },
  "required": ["action"]
}
```

条件约束由 invoke 层二次校验：`start` 必须有 `name`，可带 `args` 和
`agentBudget`；`status` 可带 `run`；`list` 不接受其余字段。未知字段或错误组合返回
`workflow.invalid_arguments`，绝不猜测或忽略。

### 4.2 `list`

调用既有 `list_workflows(cwd, lato_home, plugin_snapshot, cwd_trusted)`，保持 keep-first：
用户 → 受信项目 → 受信且启用的插件。结果最多 64 条，稳定顺序与现有 registry 一致：

```json
{
  "action": "list",
  "workflows": [{
    "id": "plugin/review-changes",
    "name": "review-changes",
    "description": "Review a diff",
    "source": "plugin",
    "agentBudget": 32
  }],
  "truncated": false
}
```

不返回 script 正文、磁盘绝对路径、插件秘密或未通过 trust 筛选的项目定义。

### 4.3 `start`

1. 在当前 turn 固定的 plugin snapshot 上调用现有 `resolve_workflow`；名称解析、重复短名、
   项目信任规则完全复用 registry。
2. `args` 缺省 `{}`；必须是 JSON object，序列化后最大 64 KiB。
3. `agentBudget` 缺省使用 workflow 声明值；显式值复用 `clamp_agent_budget`，不得超出引擎上限。
4. policy 对 descriptor + 精确参数做决策。受现有 `ToolDescriptor` 静态 side-effect 元数据约束，
   v1 的三个 action 都走普通 human approval 路径；`start` 的审批摘要至少
   显示 resolved workflow id、来源、预算和 args 摘要。审批只授权这一次、这一组规范化参数，
   不扩大 sandbox/trust。
5. grant 消耗后，调用当前 `RuntimeSession::workflow_launch`；由它取得当前 snapshot 并调用同一
   Manager。成功后立即返回初始 run 快照，不等待 phase 或完成。

```json
{
  "action": "start",
  "run": {
    "runId": "wf_...",
    "displayName": "review-changes-2",
    "status": "active",
    "detailStatus": "active",
    "phase": null,
    "agentBudget": 32,
    "agentsUsed": 0,
    "pauseMessage": null,
    "elapsedMsFloor": 0
  }
}
```

审批被拒时不得 resolve 后偷偷启动；工具调用取消若发生在 launch 前则不启动，若 launch 已成功
则返回/记录 run，不把调用取消解释为 stop。

### 4.4 `status`

- `run` 可匹配精确 `runId` 或 `displayName`；同时匹配不同对象时以 `runId` 精确匹配优先。
- 缺 `run` 时返回 Manager 最近的至多 64 条 run；顺序与 `WorkflowTracker::list` 一致。
- 查询本身只读，但 v1 单工具共用 `external_mutation` 描述符，因此仍要求一次性 human approval；
  后续若要免批查询，必须先另立规格支持 action-level policy metadata，不能在工具内绕过 policy。
- 未命中返回 `workflow.run_not_found`，不返回空成功。

## 5. 状态合同

对模型暴露稳定的四类 `status`，并保留无损 `detailStatus`：

| Manager `detailStatus` | 模型 `status` | 说明 |
| --- | --- | --- |
| `active` | `active` | 后台执行中 |
| `user_paused`, `back_off_paused`, `no_progress_paused`, `infra_paused`, `blocked`, `budget_limited` | `paused` | 未终结；是否可 resume 仍由既有 Manager 规则决定 |
| `complete` | `completed` | 成功完成 |
| `interrupted`, `failed`, `cancelled` | `interrupted` | 未成功终结；用 `detailStatus` 区分原因 |

这是展示归一化，不改 `WorkflowRunStatus`、journal 或恢复语义。特别是磁盘上的 `active` 在
跨进程恢复后仍由 7B5 改写为 `interrupted`；工具不得把它报告成 active。所有返回还包含
`phase`、预算、已用 agent 数、pause message 和 elapsed floor；不得返回 journal 内容。

## 6. 架构与接线

```text
main-session model
      │ tool call: workflow
      ▼
ToolRuntime（schema → policy → approval/grant → audit）
      │ session-bound WorkflowTool
      ▼
RuntimeSession
      ├─ list/resolve：现有 registry + turn plugin snapshot
      └─ start/status：已挂载的 Arc<WorkflowManager>
                          │
                          └─ 既有 background run / journal / host service / session updates
```

实现边界：

- `lato-tools` 保持通用，不依赖 `lato-agent`。`WorkflowTool` 应放在 `lato-agent`，实现
  `lato_core::Tool`，通过 session-owned handle/weak binding 访问 `RuntimeSession`。
- 构建主会话 `ToolRuntime` 时注册该工具；subagent 的通用 builtin runtime 默认不注册，防止
  子代理递归启动 workflow。本刀只保证主会话模型可见。
- 为打破 RuntimeSession 与 ToolRuntime 初始化先后，可用一次性 session handle/`Weak` 接线；
  不得用全局 map、静态当前 session 或 ACP 回环。
- `session/new` 和 `session/resume` 都必须在首个模型 turn 前完成 Manager 挂载与工具绑定。
- workflow 的 `session/update` 仍由现有 Manager subscriber 发出；工具不得重复广播。

## 7. 权限、安全、隐私与失败合同

### 7.1 权限拦截

1. 单一 `workflow` descriptor 静态标记为外部副作用，因此 `list/status/start` 均经 ToolRuntime
   显式审批。实现不得把 descriptor 标成只读后在 `start` 内产生未授权副作用。
2. 项目 workflow 仅在 `cwd_trusted()` 时可发现/解析；插件必须受信且启用。用户 workflow 也不
   因来源可信而跳过 `start` 审批。
3. workflow 内部每个 host/tool 动作继续走既有 sandbox、trust、approval 与预算；外层批准不
   是内部动作的万能许可。
4. 子会话不得通过继承一个可用的 `WorkflowTool` 扩权；主会话专属注册需有回归测试。
5. args、description、pause message 视为不可信文本；输出 JSON 编码，不拼进 shell/路径。

### 7.2 稳定错误码

| code | 条件 | 可重试 |
| --- | --- | --- |
| `workflow.invalid_arguments` | schema 条件、args 大小/类型、预算不合法 | 修参后可重试 |
| `workflow.not_found` | named script 不存在或对当前 trust 不可见 | 否 |
| `workflow.duplicate_name` | 短名歧义 | 用 qualified id 可重试 |
| `workflow.permission_denied` | policy/用户审批拒绝 | 由用户决定 |
| `workflow.unavailable` | session 未绑定 Manager/正在 teardown | 否 |
| `workflow.too_many_active_runs` | 已有 4 个 active run | 状态变化后可重试 |
| `workflow.persistence_failed` | 7B5 launch 落盘失败并回滚 | 修复环境后可重试 |
| `workflow.run_not_found` | status 未命中 | 否 |
| `workflow.output_too_large` | 无法在 64 KiB 内安全返回 | 缩小查询后可重试 |

错误不得泄露绝对路径、脚本内容、approval token 或模型密钥。日志可记录 session id、run id、
action、错误码和耗时，不记录完整 args。

### 7.3 边界与异常流程

- 同名 workflow：短名失败并提示 qualified id，绝不任选一个。
- 并发启动：Manager 原子执行 4-active 上限；两个 tool calls 不可绕过。
- 审批期间 snapshot 改变：approval fingerprint 绑定 resolved id/source/args/budget；执行前若 generation
  或解析结果变化，作废批准并返回 `workflow.catalog_changed`，不启动旧/新任一版本。
- persistence 失败：沿用 7B5 全回滚，工具返回失败且 `status` 不得出现幽灵 run。
- session close：沿用 shutdown → interrupted；晚到工具调用返回 unavailable。
- 输出过长：列表优先按条目边界截断并置 `truncated=true`；单个字段仍超限则返回稳定错误。

## 8. 主流程

1. 模型调用 `workflow {"action":"list"}`，只看到当前可信 registry。
2. 模型选择 id，调用 `start`；UI 展示精确审批摘要。
3. 用户批准；ToolRuntime 发放并消费一次性 grant；session 调用同一 Manager。
4. 工具立即返回 `active` 快照，主 turn 可继续；后台 updates 正常到达 TUI/ACP。
5. 后续 turn 调用 `status`，看到 `paused` / `completed` / `interrupted` 及 detail。

异常流程：拒批、找不到/歧义、达到并发上限、journal 写盘失败均返回结构化错误；不得降级为
内存-only、不得另启 Manager、不得用文本伪造成功。

## 9. 必要测试与严格验收门槛

### 9.1 Focused tests（全部必过）

1. descriptor/schema：wire name 唯一；三 action 与条件参数；未知字段、非 object args、>64 KiB 拒绝。
2. list trust matrix：user 可见；untrusted project/plugin 不可见；trusted+enabled 可见；重复短名行为一致。
3. start happy path：一次审批只启动一次，返回 active，主 turn 不等待 workflow 完成。
4. 同一 Manager：TUI/ACP 启动的 run 可被 tool `status` 看见，tool 启动的 run 出现在既有 board/update；
   断言没有第二个 Manager/turn loop。
5. permission matrix：拒批零副作用；旧/篡改 grant 失败；sandbox/trust 不扩大；内部 host 工具仍独立过膜。
6. lifecycle matrix：所有 11 个 detail status 准确归一到 active/paused/completed/interrupted；字段无损。
7. concurrency：4 active 时第五次稳定失败；并发 start 不越界；查询不死锁。
8. persistence：launch 落盘失败全回滚；resume 后 paused/interrupted run 可由同一 tool 查询。
9. cancellation：launch 前取消不启动；launch 后取消不 stop；session close 后 unavailable。
10. output/privacy：64 条上限、64 KiB 上限、无绝对路径/script/journal/secret；恶意字符串 JSON 安全。
11. registration：主会话模型可见；subagent runtime 不可见；session/new 和 resume 首 turn 均可用。
12. 回归：现有 `/workflow`、ACP workflow methods、CLI list/run、7B5/7B6 focused tests 全绿。

### 9.2 发布门禁

- `cargo fmt --all -- --check`；
- 触及 crate 的 focused tests，以及 workspace 相关集成测试；
- `cargo clippy --workspace --all-targets -- -D warnings`；
- `cargo install --path .`，安装后二进制能完成 README 的真实交互冒烟；
- README 增加模型工具协议、审批、安全边界、状态含义和“不支持模型 pause/resume/stop”；
- `docs/superpowers/ledger.md` 更新新增模块/接线；本规格状态改为“已实施”；
- Linux/macOS/Windows CI 全绿；P0/P1 为零，P2/P3 有明确处置结论。

### 9.3 可判定验收标准

| ID | 操作 | 预期；否则 FAIL |
| --- | --- | --- |
| AC-01 | 主会话请求 tool catalog | 恰有一个 wire name `workflow`，schema 与 §4 一致 |
| AC-02 | 未信任项目执行 list/start | 项目脚本不出现且不能按猜测名称启动 |
| AC-03 | start 后立即观察主 turn | 20 秒内返回初始快照，主 turn 未被 workflow 占用 |
| AC-04 | 拒绝审批后查询 runs/磁盘 | 无新增 run、目录或 journal |
| AC-05 | 同一 run 在 tool、TUI、ACP 查询 | runId/displayName/status/预算字段一致 |
| AC-06 | 构造全部 detail status | 四类 status 映射逐项符合 §5 |
| AC-07 | 4 active 后并发启动两次 | 均不造成 active_count > 4；失败码稳定 |
| AC-08 | 杀进程后 resume session 并 status | 原 active 报 interrupted；paused 保持 paused detail |
| AC-09 | 模型尝试 pause/resume/stop 或子代理调用 | schema/目录中不可达，不发生状态变化 |
| AC-10 | 执行完整门禁与 README 冒烟 | 命令全绿、文档与实际一致、可复现证据齐全 |

## 10. 实施门槛、依赖、降级与回滚

**实施门槛：** 本规格评审通过；7B5/7B6 保持在目标基线；开发者确认 ToolRuntime session binding
方案不会引入引用环；测试能注入 fake approval 和 deterministic Manager。

**阻塞项：** 若现有 ToolRuntime 无法安全绑定 RuntimeSession，应先增加窄的 session handle 接口；不得
用全局状态或 ACP self-call 赶工。目标发行版本目前待确认，不阻塞代码设计。

**降级方案：** 若发布前模型工具存在 P0/P1，可不注册 `WorkflowTool`，保留既有 TUI/ACP/CLI
workflow 能力；不得保留部分 action 或绕过审批的隐藏开关。

**回滚方案：** 回退工具注册和适配器提交即可；不删除 run journal、不迁移 schema、不影响已有
`/workflow` 和 ACP 接口。若已由工具启动 run，回滚前按既有 session shutdown 将 active 置
interrupted，保留审计证据。

## 11. 完成定义（DoD）

规格范围全部实现；AC-01～AC-10 与 §9 测试有可复现证据；README/ledger/规格状态同步；严格验收官
逐项给出 PASS，P0/P1 为零，P2/P3 有处置；回滚经过验证。开发完成、PR 绿或作者自测均不等于验收
或发布。7C AgentField 仍保持独立，不能以“顺手实现”计入本刀。
