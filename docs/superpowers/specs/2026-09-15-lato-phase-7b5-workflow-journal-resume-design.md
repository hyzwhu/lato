# Lato Phase 7B5 Cross-Process Workflow Resume — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **已实施** |
| 日期 | 2026-09-15 |
| 基线 | 7B4 in-memory `WorkflowManager` + ACP `/workflow` 看板 + `Journal::new(None)` |
| 上游 | Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`：session `workflows/` 落盘、paused journal replay、active-at-exit → `Interrupted` |
| 后续 | scratch/templates/git_diff；模型可见 `workflow` tool；7C AgentField |

## 1. 背景

7B4 把会话级 run 表、display name、同进程 pause/resume/stop、TUI 看板接到 ACP。journal 只活在进程里：`session/resume` 之后看板是空的，paused run 无法续跑。`lato-workflow::Journal` 本身已经支持 `path: Some` 的 JSONL 落盘与 `load` 回放（7B3 引擎测试覆盖过）；缺的是 Manager 把它接到会话目录，并在新进程里按规则恢复。

Grok 的跨进程语义（create-workflow skill / journal resume）：

- 已提交的 host 调用结果落盘；resume 复用原脚本、原 args、原 `agent_budget`。
- **paused 族**在进程退出后可恢复、可 resume。
- **当时仍 active** 的 run 恢复成终态 `interrupted`，不可 resume（外部副作用没有跨进程调用身份）。
- 用户只接触 display name；内部 `runId` 不进 slash 参数。

## 2. 目标

1. ACP 会话的 workflow run 把 journal + 不可变脚本 + args + tracker 快照写到该会话目录。
2. `session/resume` 后 `WorkflowManager` 从磁盘重建；paused / blocked / failed / cancelled / budget_limited 可按 7B4 规则 `/workflow resume`。
3. 进程崩溃时仍标为 `active` 的 run → `interrupted`，不可 resume。
4. `session/close` 仍取消当时 active 的 run 并标 `interrupted`（与 7B4 §3.6 相同），但 **paused 落盘保留**。
5. CLI `lato workflow resume|pause|stop` **仍不做**：`workflow run` 是一次性进程，没有常驻 Manager；跨进程续跑只走 ACP `session/resume` + 会话内 `/workflow resume`。

## 3. 完成定义

1. ACP 会话每次 launch 在 `$LATO_HOME/sessions/<sessionId>/workflows/<runId>/` 写下 `run.json`、`script.rhai`、`journal.jsonl`（`Journal::new(Some(journal.jsonl))`）。`workflows_dir = None` 的 Manager（单测、CLI `lato workflow run`）保持 7B4 纯内存，不落盘。
2. `session/resume` 后 `lato/session/workflow/runs` 能列出上次会话留下的 run；display name 与 `runId` 保持原值，不重新分配。
3. 磁盘上 `status == "active"` 的 run 恢复时改写为 `interrupted` 并落盘，resume 拒绝（`NotResumable`）。
4. paused 族、`blocked`、`failed`、`cancelled` 恢复后可 `resume`；`budget_limited` 仍须更高 `agentBudget`；`complete` / `interrupted` 只作历史，不可 resume。Resume 必须用落盘的原脚本与原 args，不得重新 `resolve_workflow`。
5. 单个 run 的 journal/`run.json` 损坏：跳过该 run，**不得**让整个 `session/resume` 失败。
6. `session/close`：active → cancel + `interrupted` 并落盘；paused 保持 paused。之后再 `session/resume` 仍能看到这些 paused run。
7. CLI 不新增 `workflow resume|pause|stop` 子命令；`lato workflow run` 遇 pause 仍输出 `workflow.paused` 后退出。
8. 门禁：focused tests、clippy `-D warnings`（触及 crate）、`cargo install --path .`、README、upstream ledger。7B4 同进程 resume 回归必须绿。

## 4. 范围与非范围

### 4.1 范围内

| # | 工作 | 摘要 |
| --- | --- | --- |
| 1 | 落盘 | 每 run 一个目录：`run.json` + `script.rhai` + `journal.jsonl` |
| 2 | Restore | `WorkflowManager::new(..., Some(dir))` 扫描并重建 tracker / journals / workflows / args |
| 3 | Active-at-exit | 磁盘 `active` → `interrupted` |
| 4 | ACP | `attach_workflow_manager` 传入会话 `workflows/`；resume 后 `workflow/runs` 可见 |
| 5 | CLI 评估 | 明确保持「无常驻进程则不做」 |

本刀纳入的「脚本投影」**仅** run 目录里的 `script.rhai`（resume 需要的不可变快照），不是用户命令 `/workflow save`，也不写回 `$LATO_HOME/workflows` 或 `.lato/workflows`。

### 4.2 明确非范围

- `write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since`（仍 `Unsupported`；WIN-24 下一刀）
- 模型可见 `workflow` tool、把 named 脚本注入 skill listing
- AgentField、`fork_context`、embedded builtin
- `/workflow save`、把 run 脚本存成可发现的 named workflow
- CLI `lato workflow resume|pause|stop`
- 改 ACP 主 turn loop；不把 CLI `workflow run` 接到会话 Manager
- Grok 式全屏 pager dashboard

## 5. 架构

```text
ACP session/new | session/resume
        │  attach_workflow_manager
        ▼
WorkflowManager { workflows_dir: Some($LATO_HOME/sessions/<sid>/workflows) }
        │  restore: 扫描 run 目录
        ▼
tracker + journals + captured script/args
        │
        ▼  launch / resume
run_workflow_recovering(Journal::new(Some(journal.jsonl)))
        │
        ▼
settle → 更新 run.json 状态；journal 已按行 append
```

`lato-workflow` 仍不依赖 `lato-agent`。落盘格式属于 `lato-agent` Manager。

| 名 | 值 |
| --- | --- |
| 会话 run 根 | `$LATO_HOME/sessions/<sessionId>/workflows/` |
| 每 run 目录 | `<runId>/`（`runId` 已是 `wf_<ts>-<seq>`，可作目录名） |
| `run.json` | version=1 元数据 + args（camelCase，与 ACP 快照对齐） |
| `script.rhai` | 启动时捕获的不可变脚本 |
| `journal.jsonl` | 现有 `Journal` JSONL |
| 历史上限 | 仍 `WORKFLOW_HISTORY_MAX = 64`；超出的旧目录 restore 时跳过（不删） |
| `workflows_dir = None` | 7B4 行为：纯内存 |

## 6. 落盘格式

`run.json`：

```json
{
  "version": 1,
  "runId": "wf_…",
  "displayName": "review-changes-2",
  "status": "user_paused",
  "phase": "Review",
  "agentBudget": 128,
  "agentsUsed": 3,
  "pauseMessage": "need human",
  "elapsedMsFloor": 1200,
  "workflowId": "review-changes",
  "source": "user",
  "compiled": false,
  "description": "…",
  "args": {}
}
```

写入时机：launch 成功后立刻；每次 tracker 状态变化（pause/resume/settle/interrupt/stop）后立刻。失败闭：launch 若写盘失败则不启动 run，返回错误，不退回成内存-only。

`script.rhai` 只在 launch 写一次，resume 不得改。

`journal.jsonl` 由 `Journal::record` 自己 append；Manager 在 spawn 时把内存里的 journal（带 path）交给 `run_workflow_recovering`，settle 后再存回 `inner.journals`。

## 7. Restore 规则

`WorkflowManager::new` 在 `workflows_dir` 为 `Some` 时扫描一层子目录。每个目录：

1. 读 `run.json`。缺文件、`version != 1`、JSON 损坏 → 跳过该 run。
2. 读 `script.rhai`。缺失或超过 `MAX_WORKFLOW_SOURCE_BYTES`（1 MiB）→ 跳过。
3. `Journal::load(journal.jsonl)`。`UnsafeRestore` / parse / sequence 错误 → 跳过（不让会话 resume 失败）。缺文件当作空 journal。
4. 若 `status == "active"`：改成 `interrupted`（pause_message 默认 `"process exited while active"`），立刻写回 `run.json`。不进入 resumable 集。
5. 其余状态原样进入 tracker；`inner.workflows` / `args` / `journals` 按 `runId` 填好。
6. display name **用落盘值**，不走 `allocate_display_name`。随后新 launch 的去重必须看见这些名字。
7. 扫描顺序：按 `run.json` 的 `elapsedMsFloor` 或目录 mtime 稳定排序；只装入最近 64 条。`active` 计数只含恢复后仍为 `Active` 的（正常应为 0）。

同进程 resume（7B4）不变：仍用 `inner.journals` 里那份带 path 的 `Journal`，不必先 drop Manager。

跨进程 resume：新 Manager restore 后再调用现有 `resume(display_name, agent_budget)`。

## 8. ACP 与会话生命周期

`AcpHost::attach_workflow_manager` 传入 `effective_lato_home()/sessions/<sid>/workflows`。`session/new` 与 `session/resume` 走同一条 attach；new 时目录为空。

Restore 完成后，对每个恢复的 run **发一条** `session/update`（`sessionUpdate: "lato/workflow"`），让 TUI 看板有当前快照。不重放该 run 历史上每一次进度通知。

`session/close` / shutdown：与 7B4 一样取消 active → `interrupted`，并且把最终 `run.json` 写盘。paused 不改状态。

## 9. CLI 评估（本刀结论）

保持「无常驻进程则不做」：

- `lato workflow run` 退出后没有 Manager 可寻址；pause 已经用 `workflow.paused` 表达。
- 跨进程续跑绑定的是 ACP 会话身份，不是 CLI 调用身份。
- 若以后做 CLI resume，需要显式 `session-id`、重建 host/stream/approval，那是单独产品刀，不混进 7B5。

README 7B4 节把「本刀不可 resume / CLI 无 resume」改成 7B5 语义：会话 resume 之后看板可续跑 paused run；CLI 子命令仍然没有。

## 10. 验证与门禁

至少覆盖：

- 两个 Manager 实例、同一 `workflows_dir`：`await_user` pause → drop → restore → resume → `complete`。
- 磁盘 `active` → restore 后 `interrupted`，`resume` 为 `NotResumable`。
- `budget_limited` restore 后 bare resume 仍 `BudgetNotRaised`。
- 坏 `journal.jsonl` / 坏 `run.json` 不阻止其它 run restore，也不让 `session/resume` 失败。
- display name 跨 restore 保持，新 launch 不会抢已占用名。
- 7B4 回归：`workflows_dir = None` 时同进程 `await_user_then_resume_completes` 等仍绿。
- CLI：`lato workflow --help` 无 resume/pause/stop；`workflow run` pause 仍 `workflow.paused`。

Gate：focused tests、clippy `-D warnings`（lato-workflow / lato-agent / lato-protocol / bin lato `--all-targets`）、`cargo install --path .`、README、ledger。规格状态改为已实施。
