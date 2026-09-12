# Lato Phase 7B1 Workflow Engine (single-step) — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **实施中** |
| 日期 | 2026-09-12 |
| 基线 | Phase 7A inert descriptors |
| 后续 | 7B2 脚本/多步；7C AgentField |

## 目标

在 7A 描述符之上增加 **可启动、可取消** 的 run：`WorkflowEngine` 用现有 `TaskCoordinator` 登记 `TaskOwner::Workflow` 根，spawn **恰好一个** worker 子任务，成功则结算 `BudgetAccount.child_tasks`，失败/取消则释放预约。不解释 Rhai，不调模型。

## DoD

1. 未知 id → `workflow.not_found`，不登记任务、不预约预算。
2. 已知 id → 预约 `child_tasks=1`，`register_root` + `spawn_and_wait` 一步。
3. 成功：`WorkflowStatus::Completed`，ledger spent.child_tasks == 1，reserved == 0。
4. `cancel(run_id)` 调用 `cancel_workflow`；挂起的 run 得到 `workflow.cancelled` 并释放预约。
5. 第二次并发 run 在 `child_tasks` 限额 1 时 `workflow.budget_exceeded`。
6. 不依赖 AgentField；不改 TUI。

## 非范围

Rhai、`parallel()`、pause/resume、journal replay、模型调用、slash `/workflow`。
