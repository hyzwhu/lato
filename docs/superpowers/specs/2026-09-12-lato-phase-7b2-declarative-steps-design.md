# Lato Phase 7B2 Declarative Workflow Steps — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **实施中** |
| 日期 | 2026-09-12 |
| 基线 | 7B1 `WorkflowEngine` |
| 后续 | 脚本引擎 / 会话 slash；不在本刀 |

## 目标

描述符可声明 `prompt`、`profile`（`explorer` / `worker` / `reviewer`）和可选 `steps[]`。引擎按顺序 spawn 每一步，目标字符串带上 prompt 与 input。`child_tasks` 预约等于步数。仍不解释 Rhai、不在本 crate 调模型。

## DoD

1. 无 `steps` 时合成一步：`prompt` 否则 `description` 否则 `Run {id}`，profile 默认 worker。
2. 非法 profile 丢弃该 workflow 并诊断 `workflow.invalid_profile`。
3. 步数 > 16 截断并诊断；步数计入预算预约。
4. `wait` 逐步 spawn；取消仍释放全部预约。
5. 不修改 TUI / 脏的 agent 会话文件。
