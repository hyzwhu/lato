# Lato Phase 7B4 Workflow TUI Runs — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **已实施（2026-09-15，Phase 7B4 gate 通过）** |
| 日期 | 2026-09-13 |
| 基线 | 7B3 Rhai host + `lato workflow run` + TUI `/workflows` 仅列出插件 JSON 描述符 |
| 上游 | Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`：`xai-grok-shell/src/session/workflow/{manager,tracker,notify}.rs` + user-guide `/workflow` |
| 后续 | scratch/templates/git_diff；跨进程 journal resume；模型可见 `workflow` tool；7C AgentField |

## 1. 背景

7B3 把 Grok 的 Rhai 引擎、journal、`SpawnAgent` → `ChildSessionRunner` 接到 CLI。引擎已能 `pause` / `await_user` 并同进程 replay。缺口是产品面：

1. TUI `/workflows` 仍走 7A `materialize_workflows`，看不到 user/project `.rhai`。
2. 没有 `/workflow` 启动、看板、pause/resume/stop。
3. CLI 把 `ScriptOutcome::Paused` 映射成 `workflow.failed`。
4. 没有会话级 run 表与 display name。

Grok 把「定义目录」和「本次会话的 run」分开：`/workflows` 浏览已保存脚本，`/workflow runs` 看 live/retained run，用户只接触 session-unique display name（`review-changes`、`review-changes-2`）。7B4 对齐这一层，不把 Grok pager 整页搬过来。

## 2. 目标

1. 每个 ACP 会话持有一个内存 `WorkflowManager`：最多 4 个 active run；display name 按 `meta.name` 去重编号。
2. 同进程 pause/resume：journal 留在内存；`resume` 继续原脚本、原 args、原 `agent_budget`。
3. TUI：`/workflows` 列定义；`/workflow` 启动/管理；`/workflow runs` 打开看板 overlay。
4. ACP 方法驱动 TUI，不另写旁路。
5. 主会话 turn 不被 workflow 占用；composer 可继续输入。
6. 子 agent 审批走父会话已有 `ToolApproval`。

## 3. 完成定义

1. `lato/session/workflows` 列出与 CLI 相同的 keep-first 目录（user → 可信 project → 可信插件），含 `source`、`compiled`。
2. `/workflow <id> [json-args]` 在当前会话后台启动 run，立刻返回 `{ displayName, runId, status: "active" }`。
3. Display name：首次用 `meta.name`（插件短名），冲突则 `name-2`、`name-3`。内部 `runId`（`wf_<uuid>`）不出现在 slash 参数里。
4. `/workflow pause|resume|stop <displayName>` 与看板快捷键 `p` / `r` / `x` 等价。
5. `await_user` / `pause` → 对应 paused 状态；普通 pause 可用 `resume`；`BudgetLimited` 的 bare resume 拒绝，须带更高 `--agent-budget`（ACP `agentBudget`）。
6. 进程退出或 `session/close`：active run 取消并视为 `interrupted`，本刀不可 resume。
7. CLI `lato workflow run` 遇到 pause 输出 `workflow.paused`（不再伪装 `workflow.failed`）；CLI 不提供 resume（跨进程，7B5）。
8. 门禁：focused tests、clippy `-D warnings`（触及 crate）、`cargo install --path .`、README、ledger。

## 4. 范围与非范围

### 4.1 范围内

| # | 工作 | 摘要 |
| --- | --- | --- |
| 1 | Tracker + Manager | 移植 Grok status/display-name/max-4-active；journal 仅内存 |
| 2 | 目录对齐 | ACP catalog 改用 `lato-agent::workflow::list_workflows` |
| 3 | ACP | `workflow` / `workflow/runs` / `pause` / `resume` / `stop` + `session/update` |
| 4 | TUI | slash、补全、runs overlay、进度通知 |
| 5 | CLI pause 码 | `ScriptOutcome::Paused` → `workflow.paused` |
| 6 | 审批 | 子 `ChildSessionRunner` 继承父 `ToolApproval` |

### 4.2 明确非范围

- `write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since`（仍 `Unsupported`）
- 跨进程 / `session/resume` 后恢复 workflow journal
- 模型可见 `workflow` tool、把 named 脚本注入 skill listing
- AgentField、`fork_context`、embedded builtin
- Grok 式全屏 pager dashboard、`/workflow save`、run 脚本投影落盘
- CLI `lato workflow resume|pause|stop`（无常驻进程）
- 改 ACP 主 turn loop 或让 workflow 占用 `session/prompt`

## 5. 架构

```text
TUI /workflow <name>
        │  lato/session/workflow
        ▼
RuntimeSession.WorkflowManager
  tracker (display names, status, phases, agents_used)
  active: run_id → { cancel, pause_intent, join }
        │
        ▼
run_workflow + Journal::new(None) + WorkflowHostService
        │  ChildSessionRunner (父 ToolApproval + 父 model stream)
        ▼
ScriptOutcome → tracker.status
        │
        ▼
session/update { sessionUpdate: "lato/workflow", run: {...} }
```

`lato-workflow` 仍不依赖 `lato-agent`。Manager 住在 `lato-agent`。

常量与 Grok 对齐：

| 名 | 值 |
| --- | --- |
| `WORKFLOW_MAX_ACTIVE_RUNS_PER_SESSION` | 4 |
| display name | `meta.name`，冲突 `name-2`… |
| run id | `wf_` + uuid 简写，仅内部 |
| journal | 内存；不写 `session_dir/workflows/` |
| 同进程 resumable | paused 族、`failed`、`cancelled` |
| 不可 resume | `complete`、`interrupted`、`active` |
| BudgetLimited resume | 仅当新 `agent_budget` > `agents_used` |

## 6. Run 状态

与 Grok `WorkflowRunStatus` 同形（snake_case JSON）：

`active` · `user_paused` · `back_off_paused` · `no_progress_paused` · `infra_paused` · `blocked`（verification pause）· `budget_limited` · `interrupted` · `complete` · `failed` · `cancelled`

映射：

| 引擎 | 状态 |
| --- | --- |
| 正在跑 | `active` |
| `Paused { User }` | `user_paused` |
| `Paused { BackOff }` | `back_off_paused` |
| `Paused { NoProgress }` | `no_progress_paused` |
| `Paused { Infra }` | `infra_paused` |
| `Paused { Verification }` | `blocked` |
| `BudgetExceeded` | `budget_limited` |
| `Cancelled` | `cancelled` |
| `Failed` | `failed` |
| `Completed` | `complete` |
| 进程/会话结束时仍 active | `interrupted` |

`pause` 命令：对 `active` 置 `pause_intent`；脚本下一次 host 边界看到取消/pause 后进入 paused 或 cancelled。对已 paused 的 run，`pause` 是 no-op 成功。

`stop`：`cancel_workflow(run_id)` + drain（7B3 已有 20s）。

`resume`：拒绝 `budget_limited` 除非请求带更大 `agentBudget`；拒绝 `interrupted` / `complete` / `active`。Resume 必须复用原 script 与原 args。

## 7. ACP

在 `METHODS_IMPLEMENTED` 增加且只增加这些：

| 方法 | 作用 |
| --- | --- |
| `lato/session/workflows` | 已有；改为 7B3 registry |
| `lato/session/workflow` | 启动。params: `sessionId`, `name`, `args?`, `agentBudget?` |
| `lato/session/workflow/runs` | 列出本会话 run |
| `lato/session/workflow/pause` | `sessionId`, `name`（display name） |
| `lato/session/workflow/resume` | `sessionId`, `name`, `agentBudget?` |
| `lato/session/workflow/stop` | `sessionId`, `name` |

启动成功：

```json
{ "runId": "wf_…", "displayName": "review-changes-2", "status": "active" }
```

run 列表项：

```json
{
  "runId": "wf_…",
  "displayName": "review-changes-2",
  "status": "active",
  "phase": "Review",
  "agentsUsed": 3,
  "agentBudget": 128,
  "pauseMessage": null,
  "elapsedMs": 1200
}
```

进度通知（已有 `session/update` 通道，不新开 socket）：

```json
{
  "sessionUpdate": "lato/workflow",
  "sessionId": "s…",
  "run": {
    "displayName": "…",
    "status": "active",
    "phase": "Review",
    "agentsUsed": 1,
    "agentBudget": 128,
    "elapsedMs": 800
  }
}
```

## 8. TUI 设计

- `/workflows`: 浏览所有已保存工作流定义（keep-first 目录：user -> project -> plugin），展示 id、name、description、steps、agentBudget。
- `/workflow <id> [json-args]`: 在当前会话后台启动 run，即时返回 `{ displayName, runId, status: "active" }`。
- `/workflow runs`: 打开 runs 看板（overlay dialog）。
  - 显示 run 列表：displayName, status, phase, agentsUsed/agentBudget, elapsedMs, pauseMessage。
  - 快捷键：
    - `p`: 暂停高亮的 run (`/workflow pause <displayName>`)
    - `r`: 恢复高亮的 run (`/workflow resume <displayName>`)
    - `x`: 停止高亮的 run (`/workflow stop <displayName>`)
    - `Esc` / `q`: 关闭 overlay
- `/workflow pause <displayName>`: 暂停指定 display name 的 run。
- `/workflow resume <displayName> [new-budget]`: 恢复指定 display name 的 run。
- `/workflow stop <displayName>`: 停止指定 display name 的 run。
- 进度通知：TUI 监听 `session/update` 中 `sessionUpdate == "lato/workflow"`，更新后台状态或在 status line / 界面提示。

## 9. CLI 与 错误处理

- CLI `lato workflow run` 遇到 `ScriptOutcome::Paused` 输出 `workflow.paused`（退出码与错误码对齐，不再输出 `workflow.failed`）。
- 错误码新增：`workflow.paused`。
- 子 agent 执行继承父会话的 `ToolApproval`。

## 10. 验证与门禁

- 单元测试与集成测试覆盖：
  - Tracker: display name 分配去重编号、状态流转、active 计数。
  - WorkflowManager: launch, pause, resume, stop, max 4 active runs 限制, budget check。
  - ACP methods: `lato/session/workflow`, `workflow/runs`, `pause`, `resume`, `stop`, `workflows` catalog 对齐。
  - CLI: `workflow.paused` 退出码与 JSON 输出。
  - TUI: `/workflow` 命令与 completion, runs overlay dialog。
- Gate:
  - `cargo test` 触及 crates。
  - `cargo clippy -D warnings` 触及 crates。
  - `cargo install --path .` 安装测试通过。
  - 更新 README.md 和 docs/superpowers/reference/lato-upstream-sources.md。
