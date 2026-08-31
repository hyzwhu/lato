# Lato 验收文档

- 日期：2026-08-31
- 状态：有效（测试以本文为准）
- 设计规格：`docs/superpowers/specs/2026-08-31-agent-harness-design.md`
- 仓库：`lato` 仓库根（相对路径均相对该根）

后续凡声称某阶段完成、某能力可用、准备合并，**必须按本文对应编号用例验收**。设计规格第 12 节是摘要；与本文冲突时以本文为准，并回改规格。

通过规则：

- 编号用例全部 `PASS` 才算该阶段通过。
- 标 `LIVE` 的需要真实网络与密钥，默认不在 PR CI 跑；本地或 nightly 跑。未跑不得把 LIVE 当过。
- 标 `OS:windows` / `OS:macos` / `OS:linux` 的只在该 OS 上强制。
- 标 `UNIT` 的必须是 `cargo test` 可重复、不打真网。
- 失败要留下命令、退出码、关键日志；禁止用「看起来能跑」代替编号用例。

环境约定：

- 二进制：`lato`（`cargo run -q --` 在未安装时等价）。
- 测试 home：设置 `LATO_HOME` 到临时目录，禁止污染开发者真实 `~/.lato`。
- 无头：`lato -p` 不得等待 TTY。

---

## A. 阶段 0 — 内核

完成定义：ACP host + turn loop + CredentialStore + `ApiKeyPreset`/`env_api_key` + headless CLI。MCP/hooks/skills 空操作。沙箱可 `off`。Windows 路径与默认 shell 正确。不做 compact。

### A0. 无头审批与信任

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A0-1 | UNIT | 无 TTY 调用 `lato -p "ping"`（假模型 fixture，cwd 未在 trust 文件中） | 进程不阻塞；本进程视 cwd 为已信任；`approval_mode=always`；不写全局「永久信任」除非另有 flag |
| A0-2 | UNIT | 交互会话（模拟 TTY）默认开 turn | `approval_mode=ask`；未信任目录要先确认；不自动 always |
| A0-3 | UNIT | `lato -p` 下 mutating 工具命中 deny glob（如 `.env`） | 工具失败回模型或 turn 失败；**不得**执行该写操作 |
| A0-4 | UNIT | `lato -p --ask "ping"`（或规格等价覆盖） | 无 TTY 时不能挂死：要么拒绝启动并提示需要 TTY，要么用 fixture 客户端自动拒绝/允许；不得无限等 stdin |

### A1. ACP 最小集

stdio JSON-RPC。协议版本在 `initialize` 里协商；v1 按下列方法。

必做：`initialize`、`session/new`、`session/prompt`、`session/cancel`、`session/update`（通知）、`session/request_permission`（反向）、`session/list`、`session/resume`、`session/close`、`session/set_model`。

扩展（前缀 `lato/`）：`lato/session/info`、`lato/models/list`、`lato/auth/login`、`lato/auth/logout`、`lato/auth/status`。

不做：`session/load`、Codex `thread/start`。若 `initialize.authMethods` 非空，ACP `authenticate` 必须代理到与 `lato/auth/login` 同一 `CredentialStore`，禁止第二套凭证。

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A1-1 | UNIT | `initialize` | 返回 `protocolVersion` 与 capabilities；列出上表方法；**不**广告 `session/load` |
| A1-2 | UNIT | `session/new` → `session/prompt`（假模型只回文本） | 收到 `session/update` 文本；prompt 完成；exit 成功 |
| A1-3 | UNIT | prompt 中途 `session/cancel` | turn 停；历史若已有 function call 则已 persist；再 prompt 可开新 turn |
| A1-4 | UNIT | 假模型发 mutating tool call | agent 发 `session/request_permission`；客户端回允许一次后才执行 |
| A1-5 | UNIT | `session/list` + `session/close` + `session/resume` 同一 id | resume 不重放全部历史通知；随后 prompt 能续跑 |
| A1-6 | UNIT | `session/set_model` 换到 catalog 内已支持模型 | 后续采样用新模型；换到 `supported: false` 的模型必须错误返回 |
| A1-7 | UNIT | headless CLI `lato -p` | 只通过 in-process ACP，不直连 `run_turn`（可用测试探针或模块边界测试证明） |
| A1-8 | UNIT | 调用 `session/load` | 方法不存在或明确 error（未实现），不得默默当成 resume |

### A2. Loop 与工具（编码最小集）

阶段 0 ToolRouter 仅：`read_file`、`list_dir`、`grep`、`search_replace`、`run_terminal_command`、`todo_write`。不出现 `spawn_subagent` / `web_*` / `search_tool`。

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A2-1 | UNIT | fixture：模型先 `read_file` 再文本 | persist-then-execute：历史先有 call 再有 result；最终有 assistant 文本 |
| A2-2 | UNIT | 取消发生在 tool 执行前 | 历史已有 tool call，无「空洞」 |
| A2-3 | UNIT | 并行两个 `search_replace` 指向同一文件的不同路径拼写 | 串行化；最终文件内容确定、无交错损坏 |
| A2-4 | UNIT | 无 MCP、无 hook、无 skill 目录 | turn 成功；不是报错退出 |
| A2-5 | UNIT | 装配后的工具列表 | 不含 `spawn_subagent`、`web_search`、`web_fetch`、`search_tool`、`use_tool` |
| A2-6 | UNIT | 上下文超过阶段 0–1 硬上限（fixture 撑大） | turn 失败，错误表明未 compact / 需新 session；**不得**悄悄 truncate 历史 |
| A2-7 | UNIT | 同一 session 第二个 `session/prompt` 在已有 active turn 时 | 按 Start 则 abort 旧 turn（`Replaced`）或按 Steer 挂到当前 turn；不得两条采样循环 |

### A3. 路径规范化与文件锁

锁键与信任键使用同一套规范化：绝对路径；Windows 去掉 `\\?\`、统一分隔符、按大小写折叠。

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A3-1 | UNIT / OS:windows | 对 `C:\Work\App\src\Lib.rs`、`c:/work/app/src/lib.rs`、`\\?\C:\Work\App\src\Lib.rs` 取锁键 | 三者相等 |
| A3-2 | UNIT / OS:windows | 两并行 edit 分别用上面不同拼写 | 仍串行，文件不损坏 |
| A3-3 | UNIT / OS:macos,linux | `./a.txt` 与绝对路径指向同一 inode | 锁键相等 |
| A3-4 | UNIT / OS:windows | folder trust 键对上述拼写 | 同一工作区只问一次（交互）或 `-p` 下视为同一 cwd |
| A3-5 | UNIT / OS:windows | `run_terminal_command` 默认 shell | `pwsh` 若存在否则 `powershell.exe`；POSIX `bash -lc` 不是默认 |

### A4. 凭证（尚无 HTTP 方言也可）

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A4-1 | UNIT | 空 store + env `OPENAI_API_KEY` | `get_auth(openai)` 成功，source 为 env |
| A4-2 | UNIT | store 已有 api_key，同时有 env | 用 store，不用 env |
| A4-3 | UNIT | store oauth 刷新失败 | **不**回落 env key |
| A4-4 | UNIT | `lato login xai --oauth` | 非零退出；不写 oauth 条目 |
| A4-5 | UNIT | `lato login openai --api-key`（stdin 或 flag 提供 key） | `auth.json` 出现 `"openai": { "type": "api_key", ... }`；Unix 0600 / Windows 仅当前用户 |
| A4-6 | UNIT | `lato/auth/status` | 不泄露完整密钥 |

### A5. 阶段 0 CLI 冒烟

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| A5-1 | UNIT | `lato -p "reply with hi only"` + 假模型 | 退出码 0；stdout/ACP 含文本；不交互 |
| A5-2 | UNIT | `LATO_HOME` 隔离 | 不创建真实用户 home 下的 `.lato`（除非测试故意不设） |

阶段 0 不要求真实改仓库测。`lato -p "fix the tests"` 的 LIVE 放阶段 1。

---

## B. 阶段 1 — 三种 stream 方言

完成定义：`openai-completions`、`openai-responses`、`anthropic-messages` 能对 `supported: true` 的模型发真实协议请求（CI 用 VCR）。

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| B1-1 | UNIT | 同一 `env_api_key`，Groq completions vs xAI responses 两条 VCR | 打到各自 host；无按厂商分叉的客户端代码路径（共享 stream 实现） |
| B1-2 | UNIT | Fireworks（或 fixture 多 api preset）：一种 api 已实现、一种未实现 | `lato/models/list` 上分别 `supported: true/false`；对 false `set_model` 失败 |
| B1-3 | UNIT | 每种已实现方言至少一条录制 fixture | 请求形状符合该协议；CI 断网仍过 |
| B1-4 | UNIT | `google`（`google-generative-ai`）采样 | 拒绝，`reason: dialect_unimplemented` |
| B1-5 | UNIT | Bedrock / Vertex / Cloudflare `lato login --api-key` | 不可登录（不在可登录列表） |
| B1-6 | LIVE | `OPENAI_API_KEY` 或 `XAI_API_KEY` 或 `KIMI_API_KEY`：`lato login <id> --api-key` 后 `lato -p "fix the tests"` 在带失败测试的临时仓库 | 改测试或实现并使测试通过；无 TTY；不提示 folder trust |
| B1-7 | UNIT | Anthropic：credential 为 raw api key | 请求带 `x-api-key`；`ANTHROPIC_AUTH_TOKEN` 路径带 `Authorization: Bearer` |

---

## C. 阶段 2 — 两家 OAuth

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| C1-1 | UNIT | mock token：`lato login kimi-coding --oauth` | store `type=oauth`；重启进程 `get_auth` 仍可用 |
| C1-2 | UNIT | mock：`lato login openai-codex --oauth` | 同上；id 不是 `openai` |
| C1-3 | UNIT | `lato login openai-codex --api-key` | 拒绝（OAuth-only） |
| C1-4 | UNIT | 两家之外 `--oauth` | 拒绝 |
| C1-5 | UNIT | oauth 401 → 刷新成功再试 | 一次 refresh；失败则不改用 env |
| C1-6 | LIVE | 真实 Kimi Code 与 ChatGPT 订阅各登录一次 | 能完成一轮带工具的 `-p` 任务 |

---

## D. 沙箱（v1 能力；阶段 0 可 `off`，本栏在沙箱接线后强制）

| ID | 类型 | 步骤 | 期望 |
|---|---|---|---|
| D1-1 | UNIT / OS:macos | profile=`workspace`，shell 写 cwd 文件 | 成功 |
| D1-2 | UNIT / OS:macos | 同 profile，写 cwd 外（如 `/tmp/lato-sandbox-deny` 或 `$HOME/outside`） | 失败；进程未被 unsandbox 后成功 |
| D1-3 | UNIT / OS:linux | 同 D1-1/D1-2 用 `bwrap` | 同期望 |
| D1-4 | UNIT / OS:windows | Restricted Token：写 cwd 成功，写 cwd 外失败 | 同期望 |
| D1-5 | UNIT | 包装二进制缺失或启动失败 | 该次 `run_terminal_command` 失败；exit 非 0 或 tool_result 为错误；**不得**在 `off` 下重跑成功 |
| D1-6 | UNIT | `read_file` / `search_replace` 在沙箱 `workspace` 时 | 仍由 host 进程执行（不进 OS 沙箱）；审批/deny glob 仍生效 |

---

## E. 阶段 3–5（摘要门禁）

阶段未开始则整节 N/A，不算失败。

| ID | 阶段 | 期望 |
|---|---|---|
| E3-1 | 3 | Azure/Gemini/Mistral/Bedrock/Vertex/Cloudflare 按规格接线后各有 VCR；未接线的仍不可登录或 `supported: false` |
| E4-1 | 4 | `models.json` 自定义 `api+base_url+env` 能出现在 list 且可采样（方言已实现时）；llama.cpp `refresh_models` |
| E5-1 | 5 | 插件目录信任模型；`spawn_subagent` 若开放并行则 worktree 隔离；三平台沙箱策略加严有对应用例 |

---

## F. CI 矩阵

| ID | 门禁 | 内容 |
|---|---|---|
| F1 | 每个 PR（Linux 和/或 macOS） | 全部 `UNIT` 且未标单一 OS 的用例；VCR；ACP stdio |
| F2 | 每个 PR `OS:windows`（`windows-latest` 或等价） | **至少** A3-1、A3-2、A3-5、A0-1；沙箱接线后加 D1-4、D1-5。不是阶段 0 就把全部用例在 Windows 全量复制一份 |
| F3 | PR 禁止 | 打真实厂商 HTTP（LIVE） |
| F4 | 声称阶段 N 完成 | 本文该阶段全部非 LIVE 编号为 PASS，LIVE 在发布说明里单独列出已跑/未跑 |

---

## G. 反例（任何阶段都有效）

| ID | 步骤 | 期望 |
|---|---|---|
| G1 | `lato login anthropic --oauth` / `xai --oauth` / `openrouter --oauth` | 失败 |
| G2 | CLI 旁路直接 `run_turn` 完成一次用户任务 | 不允许（架构测试） |
| G3 | 交互默认配置为 `always` | 不允许 |
| G4 | 静默 truncate 对话历史 | 不允许 |

---

## 记录模板

```text
phase: 0
case: A0-1
result: PASS | FAIL | N/A | LIVE-SKIPPED
os: macos | linux | windows
command: ...
exit_code: ...
notes: ...
```

阶段完成清单写入实现计划或 PR 正文，并链接到本文件的章节（A/B/C/D）。
