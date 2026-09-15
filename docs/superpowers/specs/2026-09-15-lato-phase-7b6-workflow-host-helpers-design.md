# Lato Phase 7B6 Workflow Host Helpers — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **已实施** |
| 日期 | 2026-09-15 |
| 基线 | 7B5：ACP 会话 journal 落盘 + 同进程/跨进程 resume；HostService 对 scratch/template/git_diff 仍 `Unsupported` |
| 上游 | Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138` host helpers + create-workflow skill：`write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since` |
| 后续 | 模型可见 `workflow` tool；7C AgentField |

## 1. 背景

Rhai 引擎（7B3）已经把这四个 host 调用编进 `WorkflowHostRequest`；canned `--validate-only` 返回桩数据。Live `WorkflowHostService` 一律 `HostError::Unsupported`，所以真实脚本一写报告或拉 diff 就失败。7B5 把 run 目录落到 `$LATO_HOME/sessions/<sid>/workflows/<runId>/`，scratch 文件可以放在同一棵树里，resume 时未 journal 的后续 `read_scratch_file` 才找得到盘上的内容。

Grok 语义（create-workflow Host API）：

- `write_scratch_file(name, content)` → 稳定 run-relative id，形如 `scratch/report.md`；`name` 是单路径分量。
- `read_scratch_file(name)` 按同一个 `name` 读回。
- `render_template(name, map)` 是 **内置** 模板，纯字符串替换；不可信字段要先 `json_encode`。
- `git_diff_since(commit)` 返回 diff 文本。
- `fingerprint` / `json_encode` 已在引擎里，本刀不改。

## 2. 目标

1. Live HostService 实现这四项；不再返回 `Unsupported`（`fork_context` 仍 `Unsupported`）。
2. ACP 会话：scratch 落在该 run 目录的 `scratch/` 下，跨 `session/resume` 仍在。
3. CLI `lato workflow run`：无会话目录时用 host 生命周期内的临时目录；进程退出即丢，不假装 CLI 可 resume。
4. 失败闭：坏名字、超配额、缺文件、未知模板、非 git 仓库 → `HostError::Failed`（可 catch，journal sentinel），不是静默空串。

## 3. 完成定义

1. `write_scratch_file("report.md", body)` 成功返回 `"scratch/report.md"`；随后 `read_scratch_file("report.md")` 得到同一 `body`。
2. `name` 必须是单分量：`^[A-Za-z0-9._-]{1,128}$`，拒绝 `..`、`/`、空串。违规 → `Failed("invalid scratch name")`。
3. 单文件 ≤ 1 MiB，每 run 合计 ≤ 8 MiB、最多 64 个文件。超限 → `Failed("scratch byte quota exceeded")` 或 `Failed("scratch file quota exceeded")`。
4. 缺文件 `read_scratch_file` → `Failed("scratch file not found: {name}")`。
5. ACP：文件写在 `$LATO_HOME/sessions/<sid>/workflows/<runId>/scratch/<name>`。`session/resume` 后续跑时，未 journal 的 `read_scratch_file` 仍能读到 pause 前写过的文件。
6. `render_template(name, vars)`：只解析 **内置闭集**（见 §6）；`{ident}` 用 `vars.ident` 的字符串值替换（缺键 → 空串；非 string 用 JSON 文本）。未知 `name` → `Failed("unknown template: {name}")`。不读用户/项目文件。
7. `git_diff_since(commit)` 在 host `cwd` 执行 `git diff -- <commit>`（`commit` 作单一参数，不得以 `-` 开头、不得含空白/NUL）。非仓库、git 失败、输出 > 1 MiB → `Failed`。成功返回 diff 文本（可为空）。
8. `--validate-only` 仍走 canned 桩，不碰磁盘、不跑 git。
9. 门禁：focused tests、clippy `-D warnings`（触及 crate）、`cargo install --path .`、README、ledger。规格状态改为已实施。

## 4. 范围与非范围

### 4.1 范围内

| # | 工作 | 摘要 |
| --- | --- | --- |
| 1 | scratch | 校验名字、配额、读写；ACP 落盘，CLI 用临时目录 |
| 2 | templates | 内置闭集 + `{ident}` 替换 |
| 3 | git_diff | `cwd` 上安全的 `git diff -- <commit>`，输出封顶 |
| 4 | 接线 | `WorkflowHostParams.scratch_dir`；Manager 传入 run 的 `scratch/`；CLI 传入 temp |
| 5 | 测试/文档 | `workflow_host` + 一条 persist/resume 读回；README；ledger |

### 4.2 明确非范围

- 模型可见 `workflow` tool、AgentField、`fork_context`、`/workflow save`
- CLI `workflow resume|pause|stop`
- Grok 全屏 pager 渲染 scratch 报告（返回 path 即可；TUI 本刀不打开文件）
- 用户可编辑的模板目录、从磁盘加载 `.rhai` 模板
- 改 journal 格式或 7B5 restore 规则

## 5. 架构

```text
script: write_scratch_file / read_scratch_file / render_template / git_diff_since
        │  已有引擎 host_call（journal record/replay）
        ▼
WorkflowHostService.handle_request
        │
        ├─ scratch_dir/<name>     （ACP: runDir/scratch；CLI: tempdir）
        ├─ builtin templates      （代码内闭集）
        └─ git -C cwd diff -- <commit>
```

`lato-workflow` 仍不实现这些 IO；只传 `WorkflowHostRequest`。实现留在 `lato-agent` HostService。

| 名 | 值 |
| --- | --- |
| scratch 文件名 | `^[A-Za-z0-9._-]{1,128}$` |
| 单文件 | 1 MiB |
| 每 run 合计 | 8 MiB |
| 每 run 文件数 | 64 |
| 返回 id | `scratch/{name}`（正斜杠，POSIX） |
| git diff 输出 | 1 MiB |
| 模板名 | kebab-case，与 scratch 名规则相同 |

## 6. 内置模板

本刀只内置最少可用的闭集，避免编造一份没有上游对照的 Grok 模板表：

| name | 正文 |
| --- | --- |
| `identity` | `{text}` |

替换规则：非重叠扫描 `{` + ident + `}`，ident 为 `[A-Za-z_][A-Za-z0-9_]*`。`vars` 里同名键：string 原样替换；其它 JSON 用 compact JSON 文本；缺键 → 空串。不递归。多余键忽略。

以后要加 Grok 同名模板，另开刀扩表，不在本刀猜。

## 7. 接线

`WorkflowHostParams` 增加：

```rust
pub scratch_dir: Option<PathBuf>,
```

- `Some(path)`：Host 创建该目录并在其中读写。Manager 在 spawn 时传 `run_dir.join("scratch")`（7B5 已有 run 目录时）。
- `None`：Host 自己建一个 `tempfile::TempDir`，随 HostService task 结束 drop。CLI `lato workflow run` 走这条。

`start_host` 测试夹具：默认 `None`（临时目录）即可覆盖读写；另测 `Some(persist_scratch)` 证明落盘。

错误字符串要稳定，便于 journal prune 测试对照：`scratch byte quota exceeded`、`scratch file not found: {name}`。

## 8. 验证与门禁

至少覆盖：

- write → 返回 `scratch/report.md` → read 回原文。
- `../x`、`a/b`、空名、超长名 → `Failed`。
- 超 1 MiB 单文件、超 8 MiB 合计 → `Failed("scratch byte quota exceeded")`。
- 缺文件 read → `Failed("scratch file not found: …")`。
- ACP：pause 前 write；新 Manager restore 后 live `read_scratch_file`（脚本在 await_user 之后 read）得到原文。
- `render_template("identity", #{ text: "hi" })` → `"hi"`；未知名 → `Failed`。
- git 仓库：改文件后 `git_diff_since(HEAD)` 含该改动；`cwd` 非仓库 → `Failed`；`commit` 以 `-` 开头 → `Failed`。
- `--validate-only` 仍不写盘。
- `fork_context` 仍 `Unsupported`。

Gate：`cargo test -p lato-agent --test workflow_host` 以及一条 manager/host resume+scratch 测试；clippy `-D warnings`；`cargo install --path .`；README 7B5 节补一句 host helpers；ledger 更新 HostService 行。
