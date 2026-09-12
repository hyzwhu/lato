# Lato Phase 7B3 Workflow Rhai Host — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **已实施** |
| 日期 | 2026-09-12 |
| 基线 | 7B2 declarative `prompt`/`steps`/`profile` + `CompletingTaskRunner` CLI |
| 上游 | Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`：`crates/codegen/xai-workflow` + `xai-grok-shell/src/session/workflow/host_service.rs` |
| 后续 | 7B4 TUI `/workflow` 看板与 resume；scratch/templates/git_diff；跨进程 journal resume；7C AgentField |

## 1. 背景

7A 冻结了可信插件 workflow 描述符。7B1/7B2 用 `WorkflowEngine` 按 JSON 步骤 `spawn_and_wait`，CLI 挂 `CompletingTaskRunner`（立刻成功、不进 `RuntimeSession`、不调模型）。那是骨架，不是 Grok Build 的执行模型。

Grok 的工作流是：

```text
Rhai 脚本 (agent / parallel / phase / complete)
        │
        ▼
xai-workflow::run_workflow  +  Journal (seq, req_hash)
        │  WorkflowHostRequest 通道
        ▼
host_service::SpawnAgent → SubagentRequest { owner: workflow }
        │
        ▼
子 agent 跑完 → AgentResult { success, output, ... }
```

7B3 按该分层落地。不再把 JSON 步骤循环当作产品执行面。`prompt`/`steps`/`profile` 编译成顺序 `agent()` 脚本，进入同一引擎。

## 2. 目标

1. 结构移植 `xai-workflow` 的 engine / host / journal / meta / validate 进 `lato-workflow`（Apache-2.0 头 + upstream ledger）。
2. 在 `lato-agent` 实现 `WorkflowHostService`：`SpawnAgent` 走现有 `TaskCoordinator` + `ChildSessionRunner`。
3. `lato workflow run` 执行命名脚本；无 `--model` / `LATO_MODEL` 时与 `-p` 一样用 `default_fake_stream()`。
4. `--validate-only` 用 canned host，不调模型、不 spawn child。
5. 本地 CLI 不依赖 AgentField。不改 TUI。

## 3. 完成定义

1. 脚本方言与 Grok 相同：首句 `let meta = #{...};`，`agent()` / `parallel()` / `phase()` / `complete()` / `budget()`。
2. live `agent()` 以及 `parallel()` 中未 replay 的 item 各计 1 次 `agent_budget`；schema 契约重试不计。超 cap 的 `parallel` 面板零启动。
3. `SpawnAgent` 创建 `TaskOwner::Workflow { run_id, session_id }` 子任务；取消走 `cancel_workflow`，drain 20s。
4. 子 agent `success: false` 是数据；配额/取消/coordinator 关闭/非法 opts 抛错。
5. JSON 描述符编译为顺序 `agent()` 后 `complete(last.output)`；CLI 现有 plugin fixture 仍返回 Completed。
6. `fork_context` 对 CLI / 项目 / 用户 / 插件脚本一律 `HostError::Unsupported`。
7. 门禁：focused tests、Clippy `-D warnings`（`lato-workflow`、`lato-agent`）、`cargo install --path .`、README、ledger。

## 4. 范围与非范围

### 4.1 范围内

| # | 工作 | 摘要 |
| --- | --- | --- |
| 1 | Rhai 引擎 | 移植 `run_workflow`、host 通道、journal record、`extract_meta`、`validate_script` |
| 2 | HostService | `ReserveAgentCalls` / `ReleaseAgentCalls` / `SpawnAgent` / `BudgetQuery` / `Phase` / `Log` |
| 3 | 发现 | `$LATO_HOME/workflows`、`.lato/workflows`、可信插件 `.rhai` 或 JSON 编译脚本 |
| 4 | CLI | `run --model --sandbox --validate-only --agent-budget`；模型语义同 `-p` |
| 5 | JSON 兼容 | 7B2 `steps` → 顺序 `agent()` |
| 6 | 契约重试 | `output_schema` 最多 1 次纠偏；不计第二次预算 |

### 4.2 明确非范围

- TUI `/workflows` 看板、`/workflow resume`、display name 管理
- `pause` / `await_user` 的跨进程恢复（引擎可返回 `Paused`；本刀 CLI 把它当失败）
- `write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since`
- 把 workflow 注册成模型可见 tool
- AgentField
- 改 ACP 主循环或把 engine 嵌进 `AcpHost` 会话 turn

## 5. 架构

```text
lato workflow run <id>
        │  resolve name → script + args + agent_budget
        ▼
lato-workflow::run_workflow
  Rhai  +  Journal  +  host_tx
        │
        ▼
lato-agent::WorkflowHostService
  ReserveAgentCalls / SpawnAgent / BudgetQuery
        │
        ▼
ChildSessionRunner  (FakeModelStream | configured_stream)
  RuntimeSession + profile + GitWorkspaceAllocator
        │
        ▼
AgentResult → journal.record → complete(value)
```

`lato-workflow` 禁止依赖 `lato-agent`。`lato-agent` 依赖 `lato-workflow`。7B1 `WorkflowEngine` + `CompletingTaskRunner` 退出 CLI 生产路径；crate 内 coordinator 契约测试可暂时保留 Completing runner，直到 HostService 测试覆盖同等取消/预算断言。

## 6. 脚本与 Host 契约

### 6.1 常量（与 Grok 一致）

| 名 | 值 |
| --- | --- |
| `DEFAULT_AGENT_BUDGET` | 128 |
| `MAX_AGENT_BUDGET` | 1024 |
| `MAX_HOST_CALLS` | 10_000 |
| `MAX_PARALLEL` | 1024 |
| `MAX_WORKFLOW_SOURCE_BYTES` | 1 MiB |
| 并发 slot 默认 | 32，夹 `available_parallelism`（最少 2） |
| schema 重试 | 1 |
| child drain | 20s |
| prompt 上限 | 1 MiB |
| label/phase | 各 256 bytes |

Rhai crate：`rhai = { version = "1.25", features = ["serde"] }`。

### 6.2 `WorkflowHostRequest`

与 Grok `xai-workflow/src/host.rs` 同形：`ReserveAgentCalls`、`ReleaseAgentCalls`、`SpawnAgent`、`Phase`、`Log`、`Telemetry`、`BudgetQuery`、`RenderTemplate`、`WriteScratchFile`、`ReadScratchFile`、`GitDiffSince`。

本刀 HostService **实现**前六项中的预算/spawn/phase/log/budget query。`Telemetry` 可记 tracing 后丢弃。scratch / template / git_diff 回复 `HostError::Unsupported`。

`AgentOpts` / `AgentResult` / `BudgetState` / `HostError` 字段名与 Grok 一致，便于移植引擎。

`budget()` 返回 `{ total, spent, reserved: 0, remaining }`。`reserved` 恒 0。

### 6.3 `SpawnAgent` 映射

1. prompt 非空且 ≤ 1 MiB；否则 `Failed`。
2. `agent_type` → Lato 内置 profile：`explorer` / `reviewer` / `worker`；缺省或 `general-purpose` → `worker`；其它 → `Failed`。
3. `capability_mode` 只能收窄：

   | mode | 效果 |
   | --- | --- |
   | 缺省 | profile 默认 capabilities + workspace |
   | `read-only` | FileRead（保留 profile 已有 NetworkRead），`SharedReadOnly` |
   | `read-write` | 再加 FileWrite；去掉 ProcessSpawn |
   | `execute` / `all` | worker 全量 cap，再与父 ceiling 求交 |

4. `isolation_worktree: true` → `IsolatedWorktree`，不自动 merge。
5. `fork_context`：非 embedded builtin → `Unsupported`。本刀没有 embedded builtin，故全部拒绝。
6. `output_schema`：校验最终文本；失败则新 child + 纠偏 prompt 重试一次（`ChildSessionRunner` 若无 `resume_from`，不复用会话）。两次都不合 → `AgentResult.success = false`，不抛。重试不计 `agent_budget`。
7. 先拿并发 semaphore，再 `spawn_and_wait`。`await_to_completion` 必须为真；若被 background → `Failed`。
8. `TaskOwner::Workflow { run_id, session_id }`。CLI `session_id` 为 `cli-workflow`。把物化 plugin snapshot 登记进 `SessionPluginSnapshots`，子会话只能收窄。

返回：

```text
AgentResult { agent_id, success, output, cancelled, tokens_used, duration_ms }
```

无 schema 时 `output` 为最终文本 JSON string；有 schema 且通过则为对象。

### 6.4 失败分层

| 类 | 行为 |
| --- | --- |
| 子 agent 失败 / schema 两次失败 | `AgentResult.success = false`；脚本继续；`parallel` 失败槽为 `()` |
| 配额 | `HostError::AgentCallQuotaExceeded` → `WorkflowOutcome::BudgetExceeded` |
| 取消 | `cancel_workflow` + drain → `WorkflowOutcome::Cancelled` |
| coordinator 关闭、非法 opts、`fork_context`、backgrounded | `HostError::Failed` / `Unsupported` → `WorkflowOutcome::Failed` |

CLI 将 `Paused` 映射为失败（本刀无 resume UI），退出码 1，码 `workflow.failed`。

### 6.5 预算与 `BudgetAccount`

logical agent 计数是 Grok 语义，映射到现有 `BudgetAccount.child_tasks`：根任务限额 = `agent_budget`。每次 live spawn 预约 1；成功 settle 1，启动失败 release。schema 重试使用同一已结算/已预约 child，不再加 1。

`ReserveAgentCalls` 在 spawn 之前原子发生；`parallel` 对 live_count 一次 reserve。

## 7. 发现与编译

### 7.1 扫描顺序（keep-first）

1. `$LATO_HOME/workflows/*.rhai`（user）
2. `<git-root 或 cwd>/.lato/workflows/*.rhai`（project；folder 未信任则跳过）
3. 可信且启用插件：`.rhai` 路径或 JSON 描述符编译脚本（plugin；限定名 `plugin/workflow`）

同作用域重名：拒绝解析该短名（`workflow.duplicate_name`）。跨作用域：先到者胜，后到者忽略。

文件：非 symlink 常规文件、≤ 1 MiB、文件名 `<safe-name>.rhai` 且等于 `meta.name`。名字：1–64 位小写字母/数字/单连字符，不以 `-` 开头/结尾，无 `--`。

### 7.2 解析 `lato workflow run <id>`

1. 精确匹配插件限定名 `plugin/workflow`。
2. 否则匹配唯一短名（user/project `meta.name` 或插件 `name`）。
3. 短名冲突 → 错误，要求限定名。
4. 都没有 → `workflow.not_found`，不登记任务、不预约预算。

### 7.3 JSON → 顺序脚本

无 `script` 的 7B2 描述符编译为：

```rhai
let meta = #{
    name: "<name>",
    description: "<description>",
};
let last = ();
// 每步：
last = agent("<prompt>\n\ninput: " + json_encode(args), #{
    agent_type: "<explorer|worker|reviewer>",
    capability_mode: "<read-only|read-write>",
});
complete(last.output);
```

映射：`explorer`/`reviewer` → `read-only`；`worker` → `read-write`。无 `steps` 时合成一步，规则仍是 7B2：`prompt` 否则 `description` 否则 `Run {id}`，默认 worker。

编译脚本没有磁盘文件名约束，也不走「文件名必须等于 meta.name」。`meta.name` 必须满足 Grok 短名规则；描述符名里的 `_` 在编译时改成 `-`，限定名 `plugin/workflow` 不变。引擎移植包含 Grok 的 `json_encode`；生成脚本用 `json_encode(args)` 拼接 input，不手写转义。

## 8. CLI

```text
lato workflow list [--json] [--plugin-dir PATH]
lato workflow run <id>
    [--input JSON]
    [--model provider/model]
    [--sandbox off|workspace|read-only]
    [--validate-only]
    [--agent-budget N]
    [--plugin-dir PATH]
```

| 标志 | 语义 |
| --- | --- |
| `--input` | 脚本 `args`；缺省 `{}`；非法 JSON 退出 2 |
| `--model` / `LATO_MODEL` | 与 `-p` 相同；都无则 `default_fake_stream()` |
| `--sandbox` | 默认 `SessionTrust::for_headless_prompt` 的 sandbox |
| `--validate-only` | canned host：`success: true` + 小对象；仍执行配额算术 |
| `--agent-budget` | 1..=1024；缺省描述符/`meta` 或 128 |

成功 stdout：

```json
{ "runId": "wf-1", "status": "completed", "output": <complete value> }
```

`--validate-only` 的 `output` 来自 canned 路径上的 `complete`。失败：stderr + 非 0，稳定 `workflow.*` 码。

`list` 增加 `source`（`user` / `project` / `plugin`）以及是否为编译脚本。

## 9. 验收矩阵

| ID | 验收 |
| --- | --- |
| R-1 | `extract_meta` 要求首句 `let meta`；非法名拒绝 |
| R-2 | canned `validate_only` 不调用 `ChildSessionRunner`、不碰网络 |
| R-3 | live `agent()` 计 1；schema 重试不计 |
| R-4 | `parallel` 超 cap → 零 child、`budget_exceeded` |
| R-5 | `read-only` child 不能获得 FileWrite |
| R-6 | `fork_context: true` → `Unsupported` |
| R-7 | 取消 run → `cancel_workflow` + `workflow.cancelled` |
| R-8 | JSON plugin CLI fixture 仍 Completed（fake stream） |
| R-9 | 未知 id → `workflow.not_found`，无任务、无预约 |
| R-10 | 未信任项目 `.lato/workflows` 不出现在 list/run |
| R-11 | focused tests + clippy + `cargo install --path .` + README + ledger |

## 10. 文件结构（实施时）

**`lato-workflow`（移植 + 编译器）**

- `src/host.rs` `journal.rs` `meta.rs` `engine.rs` `validate.rs` `run.rs` — 源自 `xai-workflow`
- `src/compile.rs` — JSON steps → Rhai
- 保留 `config.rs` / `types.rs` / `error.rs`；`CompletingTaskRunner` 不再被 CLI 使用

**`lato-agent`**

- `src/workflow/host_service.rs` — SpawnAgent
- `src/workflow/registry.rs` — 发现
- `src/workflow/mod.rs` — 装配 `run_named`

**根 crate**

- `src/workflow.rs` — CLI 走 registry + host + `configured_stream`
- `src/args.rs` — 新 flags
- `tests/workflow_cli.rs` — 保持并扩展 validate-only
- README Plugin Workflows；`docs/superpowers/reference/lato-upstream-sources.md`

## 11. 开放问题（已拍板）

1. 7B3 是否上完整 Rhai，而不是只换 `ChildSessionRunner` → **是**，按 Grok 分层。
2. JSON steps 是否删除 → **否**，编译进同一引擎。
3. CLI 无模型时是否失败 → **否**，与 `-p` 一样 fake stream。
4. `Paused` 本刀如何处理 → CLI 当失败；引擎类型保留给 7B4。

## 12. 参考

- Grok Build `crates/codegen/xai-workflow/{lib,engine,host,journal,meta,validate,run}.rs`
- Grok Build `crates/codegen/xai-grok-shell/src/session/workflow/{host_service,registry,schema_contract}.rs`
- `docs/superpowers/specs/2026-09-01-lato-extensible-agent-design.md` §12.5、§17 Phase 7
- `docs/superpowers/specs/2026-09-12-lato-phase-7a-workflow-interface-design.md`
- `docs/superpowers/specs/2026-09-12-lato-phase-7b2-declarative-steps-design.md`
- `crates/lato-agent/src/subagent/runner.rs` `ChildSessionRunner`
- `crates/lato-core/src/task.rs` `TaskOwner::Workflow`
