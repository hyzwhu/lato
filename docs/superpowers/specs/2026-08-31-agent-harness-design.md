# Lato 设计规格

- 日期：2026-08-31
- 状态：待审阅
- 产品名：`lato`
- 实现仓库：`/Users/huangyongzhao/Documents/work/innovation/lato`（后续代码只写这里；当前已是 `edition = "2024"` 的 bin crate，将扩成 Cargo workspace）
- 验收文档：`docs/superpowers/specs/2026-08-31-lato-acceptance.md`（相对 lato 仓库根）。**测试与阶段完成判定以该文件为准**
- 参照源码（2026-08-31 浅 clone）：
  - Codex：`/tmp/harness-src/codex`（`openai/codex`）
  - Grok Build：`/tmp/harness-src/grok-build`（`xai-org/grok-build`）
  - Pi：`/tmp/harness-src/pi`（`earendil-works/pi`，MIT）

## 1. 目标

建造一套**自主可用、可嵌客户端、可插插件**的编码 Agent harness（产品名 Lato）：模型在策略允许的范围内自己转完 turn，人只在审批点介入。

第一版同时满足：

1. **无头工人**：CLI / CI / 作业系统里把任务跑完。
2. **可嵌入运行时**：TUI、IDE、Web、SDK 共用同一个 agent 脑。
3. **厂商可切换**：API key 一律按「`model.api`（协议）+ URL（host）」接入（一份 `env_api_key`，preset 只是数据）；OAuth 订阅登录 v1 只做 `kimi-coding` 与 `openai-codex`。解析顺序与 Pi 一致。
4. **三平台**：macOS、Linux、**Windows 原生**（不是只支持 WSL）。路径、进程、默认 shell、状态目录、沙箱 profile 在三个 OS 上都是 v1 能力。

成功标准：

- 同一 session 协议可驱动 headless 与交互客户端；客户端不执行工具。
- 换 provider / 换模型不改 turn loop。
- 新工具只注册进目录，不改循环。
- 长任务能 compact、能中断、能在沙箱与审批下失败可恢复。
- `kimi-coding` / `openai-codex` 能走账号订阅登录；其余厂商用 API key。凭证落盘后进程重启仍可用。

## 2. 非目标（v1）

不在第一版实现：

- 完整终端 TUI（ratatui / Ink / Pi TUI）。第一客户端是 stdio ACP + 无头 CLI。
- Grok 的 Rhai workflow、goal orchestrator、Arena 多路竞赛、Grove clone。
- Codex realtime 语音、Guardian 嵌套评审 agent。
- 插件市场 UI、热加载动态库（`.so` / Python entry）。
- 依赖 `@earendil-works/pi-ai` 作为运行时（契约与目录按 Pi **用 Rust 重写**进本仓库，保留 MIT 声明；不把 Pi 当黑盒 SDK）。
- 实现 Gemini CLI / Antigravity（Pi 当前 HEAD 已无这两家）。
- 浏览器内跑 OAuth callback（可后做；v1 以本机 HTTP callback + 粘贴 redirect / device code 为准，callback 跑在 Lato 进程内，不引入 Node）。
- **其余 OAuth**：Anthropic Claude Pro/Max、GitHub Copilot、xAI SuperGrok、OpenRouter PKCE、Radius。这些 id 若出现在 catalog 里，v1 只走 API key（有 key 的）或等后续阶段。
- **专用凭证解析**：Bedrock IAM/IRSA、Vertex ADC、Cloudflare account/gateway id。它们不是「协议 + host + env key」，放到阶段 3。
- 把 WSL 当作「已经支持 Windows」的替代方案（WSL 里跑的是 Linux 构建）。不把 Hyper-V Windows Sandbox 当 v1 默认沙箱。
- ACP `session/load`（v1 只用 `session/resume`）。

## 3. 已确认的设计决策

| 决策 | 选择 |
|---|---|
| 产品短名 | `lato`。CLI 二进制、ACP 扩展前缀、状态目录、工具命名空间都用此 id |
| Loop | Codex 式：submission → 一 session 一 active turn → persist-then-execute 工具 → 再采样 |
| 对外协议 | ACP 最小集见第 6 节；内部 queue-pair 不暴露。登录与换模型不得另搞一套凭证语义 |
| 无头 | `lato -p` 隐含 `approval_mode=always`，本进程视 cwd 为已信任，不阻塞 TTY。交互默认仍为 `ask` + 首次目录确认。Deny 在 always 下仍生效 |
| Compact | 阶段 0–1 **不做** compact。超硬上限则 turn 失败，禁止悄悄 truncate。真正 compact 在 loop 稳定之后 |
| 文件锁 | 写路径锁键 = 规范化绝对路径（Windows：去 `\\?\`、统一分隔符、大小写折叠）。与 folder-trust 键同一套规范化 |
| 工具目录 | Grok 式 `ToolKind` + 每步现装；MCP 默认 `search_tool` + `use_tool` |
| Provider | **API key = 数据**：preset `{ id, env, 可选 default_host }` + 模型上的 `api`/`base_url`；唯一 `env_api_key`。**OAuth = 代码**：仅 `kimi-coding`、`openai-codex` |
| 凭证 | 每 provider 一条；`type: api_key \| oauth`；存盘优先于环境变量；禁止刷新失败后静默回落 env |
| 插件 | 目录包（skills / hooks / MCP），信任后才激活可执行部分 |
| 执行与 UI | 工具只在 host 执行；客户端只渲染事件和回答审批 |
| 语言 | **Rust**（Cargo workspace，edition 2024）。v1 不双语言。stream 方言在 `lato-ai` 按协议实现；OAuth 只移植 Kimi Code 与 OpenAI Codex |
| 目标 OS | macOS、Linux、Windows 原生。功能除非标明 OS-specific，否则三端行为一致。CI：Unix 跑全部 UNIT；Windows PR 门禁至少覆盖路径规范化、默认 shell、无头 `-p`，沙箱接线后再加 Restricted Token 用例。细则见验收文档 F 节 |
| 沙箱 | 只罩 `run_terminal_command`。macOS：`sandbox-exec`；Linux：`bwrap`；Windows：Restricted Token + Job Object（对齐 Codex `RestrictedToken`）。不自写 Landlock，不把 Hyper-V Windows Sandbox 当默认依赖。失败则拒绝执行，禁止静默 unsandbox |
| 状态目录 | 用户 home 下的 `.lato/`（Unix `~/.lato/`，Windows `%USERPROFILE%\.lato\`）。`auth.json` Unix 0600 / Windows ACL 仅当前用户；目录 Unix 0700 |

Provider crate 必须保持「id / auth / api / models」边界：`lato-ai` 不得依赖 `lato-agent` / TUI；agent 只通过 `get_auth` 与 `stream` 使用 ai。

禁止为每个 API key 厂商写一套 Provider 实现。新厂商 = 一行 preset（env + 可选 default host + 模型表）；调度键是 **`model.api`（协议）+ URL（host）**，不改 loop、不改 HTTP 客户端（除非要新方言）。

## 4. 总体架构

```text
Client（headless CLI / 未来 TUI / IDE / Web / SDK）
        │  ACP JSON-RPC（stdio 或 socket）
        ▼
Session host
  ├─ SessionActor（一会话一 actor，至多一个 active turn）
  ├─ Models / Provider registry
  │     ├─ ApiKeyPreset 表（id / env / 可选 default host）
  │     ├─ 模型 catalog（每条有 api + 可选 base_url）
  │     └─ OAuth：kimi-coding / openai-codex
  ├─ CredentialStore (auth.json)
  ├─ ToolRegistry（每 step 现装）
  ├─ Plugin snapshot + Hook bus
  ├─ MCP manager（渐进发现）
  └─ WorkspaceOps（真正执行；审批 + 沙箱编排）
```

四条硬缝（loop 不穿过这些缝去认具体厂商或工具名）：

1. `Models.get_auth(model)` → `{ api_key?, headers?, base_url? }`
2. `ToolRegistry.dispatch(call)` → `ToolResult`
3. ACP 事件流 / 反向 `request_permission`
4. `CredentialStore.modify(provider_id, fn)` 唯一写凭证

采样路径：`model.api` 选 stream 实现，URL 取 `model.base_url` 否则 preset default host，再叠 `get_auth` 的 `base_url` 覆盖。loop 不知道这是 Groq 还是 DeepSeek。

## 5. Agent loop

### 5.1 生命周期

```text
Client session/prompt
  → 准入：Start | Steer（挂到当前 turn）| Reject
  → RegularTask
       发 turn/started
       loop（队列里还有 pending 用户输入则再开一轮）:
         run_turn:
           预 compact
           按 mention 预热 MCP
           装配本 step 的 ToolRouter + 取 get_auth()
           记 world-state / AGENTS.md / skills 注入
           UserPromptSubmit hooks
           sampling loop:
             流式模型输出
             若 function call：立刻把 call 写入历史，再并行执行
             drain 完成后把 output 写入历史
             若还需要 follow-up 或有 pending steer → 再采样
             否则 Stop hooks（可续跑）→ 结束
       发 turn/complete
```

约束：

- **一 session 同时一个 active turn。** 新的 Start 可 abort 旧 turn（`TurnAbortReason::Replaced`）。Steer 把输入排进当前 turn，不开第二条模型循环。
- **persist-then-execute。** 取消时历史里必须已有 tool call，不能丢洞。
- **并行执行，串行同文件。** 写路径取 `file_path` / `path` / `target_file`，锁键为规范化绝对路径（见第 3 节）。不同拼写指向同一文件必须同一把锁。
- **停止条件（全部一等公民）：** 模型无 tool call；用户取消；权限拒绝；预算 / max-turns；静止检测（同一工具调用连打超阈值）；Stop hook 未要求续跑；认证耗尽。
- **阶段 0 允许空操作。** `run_turn` 图里的 MCP 预热、skills 注入、全部 hook 点在未实现前必须是 no-op（空 bus、不连 MCP、不注入技能），不得阻塞 turn。阶段 5 再换成真插件/MCP。未注册的 `spawn_subagent` / `web_*` 不进入该 step 的 ToolRouter。
- **阶段 0–1 跳过 compact。** 图中「预 compact」在未实现前是空操作；上下文超过硬上限则 **失败该 turn**（可恢复错误），禁止 truncate 历史、禁止假装已经 compact。

### 5.2 上下文

注入片段必须可哈希、有字节上限，变了才重注入（Codex world-state）：

- 系统 / 开发者指令
- `AGENTS.md` 链（cwd → repo root，含常见兼容名）
- 权限与沙箱说明（只描述 **shell 工具** 的沙箱）
- 技能与插件说明
- 环境（cwd、shell、日期）
- 对话历史（compaction 是唯一允许的历史改写）

Compaction（实现后）：token 过阈值或手动触发；写成显式任务，结果作为 summary + 有限回注，禁止悄悄 truncate。阶段 0–1 未实现：超限即失败。

### 5.3 错误与重试

- 采样瞬态失败：每 step 最多 3 次、每 prompt 最多 10 次、窗口 10 分钟（对齐 Grok）。
- OAuth 401：刷新一次再试；刷新失败 **不** 改用 env key。
- API key 401：失败并提示 `/login <provider>`。
- 工具解析错误：把错误当 tool_result 回模型，不崩 turn。

## 6. 客户端协议（ACP）

v1 实现 Agent Client Protocol 的 Agent 侧。`initialize` 协商 `protocolVersion`（广告 v1 能力，不混用未实现的 v2 专有语义）。

**必做：**

| 方法 | 用途 |
|---|---|
| `initialize` | 版本与 capabilities |
| `session/new` | 新会话 |
| `session/prompt` | 开 turn |
| `session/cancel` | 中断（通知） |
| `session/update` | 文本、思考、tool_call、tool_result（通知） |
| `session/request_permission` | 反向审批 |
| `session/list` | 可恢复会话列表 |
| `session/resume` | 恢复；**不**重放全部历史通知 |
| `session/close` | 关闭 |
| `session/set_model` | 换模型（写入 capabilities；`supported: false` 必须拒绝） |

**扩展（前缀 `lato/`）：** `lato/session/info`、`lato/models/list`、`lato/auth/login`、`lato/auth/logout`、`lato/auth/status`。`plugins/reload` 到阶段 5 再广告。

**v1 不做：** `session/load`（恢复只走 `session/resume`）、Codex `thread/start`、ACP v2 `session/set_config_option`。

若 `initialize` 的 `authMethods` 非空，ACP `authenticate` **必须**代理到与 `lato/auth/login` 同一 `CredentialStore` 与解析顺序。禁止第二套凭证。无 TTY 的登录只走 `lato/auth/login` / CLI。

`lato/auth/login` 方法列表按 provider 裁剪：`openai-codex` 只有 oauth；普通 API key preset 只有 api_key；`kimi-coding` 两者都有。

headless CLI **必须**走同一 ACP（可 in-process channel），禁止再写一套直接调 `run_turn` 的旁路。

权限提示的决策：允许一次 / 本 session 允许 / 拒绝 / 取消。客户端只回决策。`lato -p` 不发阻塞式 `request_permission`（always）；deny 仍短路执行。

## 7. 工具系统

### 7.1 注册表

- 工具 id：`Namespace:name`（如 `Lato:read_file`）。
- `ToolKind`：Read、Edit、Execute、Search、ListDir、Plan、WebSearch、WebFetch、Task、Skill、SearchTool、UseTool、AskUser、Other。
- 每 step `build_tool_router`：按 session 能力、plan mode、子代理类型裁剪。
- 进程级 `register_tool_pack`：核心 crate 外的工具在第一次 builder 之前注册。
- Capability 收紧时：`kind: None` 的 MCP 工具默认丢弃（与 Grok 一致，避免未知工具漏过）。

### 7.2 v1 内建工具

最小可编码集合：

| 工具 | 作用 |
|---|---|
| `run_terminal_command` | shell（可后台）；OS 沙箱只罩它 |
| `read_file` / `list_dir` / `grep` | 读与搜 |
| `search_replace` | 默认编辑 |
| `apply_patch` | Codex 方言 preset，非默认 |
| `todo_write` | 任务列表 |
| `spawn_subagent` | 子会话（v1 可串行；worktree 隔离为 v1.1） |
| `search_tool` / `use_tool` | MCP 渐进发现 |
| `web_search` / `web_fetch` | 检索（SSRF 限制） |

Plan mode、scheduler、workflow、图像/视频生成：**v1 不做**。

### 7.3 执行管线

每个 mutating 调用：

```text
PreToolUse hook（deny / rewrite / ask）
  → 审批（策略：ask / auto / always）
  → 选沙箱
  → 执行
  → 失败可升级隔离且不再问人（Codex orchestrator）
  → PostToolUse
```

沙箱不罩 MCP 与插件进程。MCP 安全靠信任 + hook + 审批。

### 7.4 插件 / skills / hooks / MCP

插件 = 目录，清单 `plugin.json`（兼容无清单的 `skills/` `hooks/` `.mcp.json` 约定）：

- 发现顺序：CLI `--plugin-dir` → 项目 `.lato/plugins` → 用户 `$LATO_HOME/plugins`
- 项目插件默认不信任；用户/CLI 自动信任
- 未信任则不启动该插件的 hooks / MCP

Skills：`SKILL.md` 目录；显式 `/skill` 或模型按描述选用。

Hooks 事件（v1）：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Stop`、`PreCompact`、`PostCompact`。PreToolUse 可改参数但不可改工具名。

MCP：stdio + streamable HTTP；工具名 `server__tool`。默认不把全部 MCP 工具展开进模型；`search_tool` 发现，`use_tool` 调用。配置了少量 server 时允许直接展开（session 选项）。

## 8. Provider 层

### 8.1 合同

语义对齐 Pi `packages/ai`，用 Rust 表达。API key 厂商不是 40 个类型，是一份 preset 数据 + 按 `api` 分发的 stream 实现。

```rust
/// 一条 API key 厂商。实现上就是表里的一行。协议不写在 preset 上。
pub struct ApiKeyPreset {
    pub id: String,
    pub name: String,
    pub default_base_url: Option<String>, // 可空：host 在模型上（Azure / OpenCode）
    pub env: &'static [&'static str],
    // models: 静态 catalog；每条 Model 带 api + 可选 base_url
}

pub enum ModelApi {
    OpenaiCompletions,
    OpenaiResponses,
    OpenaiCodexResponses,
    AzureOpenaiResponses,
    AnthropicMessages,
    GoogleGenerativeAi,
    GoogleVertex,
    BedrockConverseStream,
    MistralConversations,
    PiMessages,
}

/// OAuth 只为订阅登录存在。v1 仅 kimi-coding 与 openai-codex 实现此 trait。
pub struct OAuthAuth { /* login / refresh / to_auth；UI 走 AuthInteraction */ }
```

`AuthInteraction`：`prompt(secret|text|select|manual_code)` + `notify(auth_url|device_code|info|progress)`。UI 不进 provider。

`Models::login(provider_id, LoginMethod::Oauth | LoginMethod::ApiKey, interaction)` 是唯一登录入口。

- `openai-codex`：OAuth-only，禁止伪造 API key login。
- 普通 preset：只有 `api_key`。
- `kimi-coding`：双通道（OAuth 订阅 + `KIMI_API_KEY`）。

### 8.2 解析顺序（必须与 Pi 一致）

对齐 Pi `packages/ai/src/auth/resolve.ts`：

1. 调用方 runtime override（不落盘，给 CI）
2. store 已有 `oauth` → 距过期 < 5 分钟则在 `modify` 锁内 refresh → `to_auth`
3. store 已有 `api_key` → `resolve(credential)`
4. store 空 → `api_key.resolve(None)` 扫环境变量
5. 都没有 → 未配置

禁止：OAuth 刷新失败后改用 env key；store 类型与 handler 不匹配时改走另一通道。

每 provider **一条**凭证。Login 覆盖旧条目。Logout 删除后 env 才重新生效。

`$LATO_HOME/auth.json` 形状兼容 Pi，便于迁移：

```json
{
  "openai-codex": { "type": "oauth", "access": "...", "refresh": "...", "expires": 0 },
  "kimi-coding": { "type": "oauth", "access": "...", "refresh": "...", "expires": 0 },
  "openai": { "type": "api_key", "key": "sk-..." },
  "xai": { "type": "api_key", "key": "$XAI_API_KEY" }
}
```

`key` 支持字面量、`$ENV`、`!command`（进程内缓存 stdout），与 Pi 相同。文件 0600，`modify` 带文件锁。

### 8.3 Stream 方言（真正要写的 HTTP）

实现按 **协议** 分，不按厂商分。host 来自 preset 或 `get_auth` 的 `base_url`。

| `model.api` | 用途 |
|---|---|
| `openai-completions` | 大多数 API-key 厂商、OpenRouter、Groq、DeepSeek、自定义 OpenAI 兼容端 |
| `openai-responses` | `openai`、`xai` |
| `openai-codex-responses` | 仅 `openai-codex`（ChatGPT 订阅） |
| `azure-openai-responses` | Azure |
| `anthropic-messages` | Anthropic、Kimi、MiniMax、部分 Copilot |
| `google-generative-ai` | Gemini API |
| `google-vertex` | Vertex |
| `bedrock-converse-stream` | Bedrock |
| `mistral-conversations` | Mistral |
| `pi-messages` | Radius |

调度键是 **每条模型的 `model.api`**，不是 preset 上的单一 `api`。同一 preset 可以挂多种方言（Fireworks、OpenCode、Copilot）：阶段 1 只跑得通已实现的那几种，其余模型标不可用。

URL：`model.base_url` → 否则 `preset.default_base_url` → 否则 `get_auth` 覆盖。缺 URL 则该模型不可用。

用户自定义 endpoint（`models.json`）仍是：选一种 `api` + 一个 `base_url` + 一个 env 名 + 模型列表。

### 8.4 API key preset（env + 模型上的协议与 host）

v1 **用数据注册** Pi 里所有走 `envApiKeyAuth` 的 id。代码路径只有一条：读 key → 按 **`model.api`** 选 stream → 请求打到该模型解析出的 URL。头由方言解释（例如 `anthropic-messages`：`apiKey` 走 `x-api-key`，已有 `headers` 则原样用，以覆盖 `ANTHROPIC_AUTH_TOKEN` Bearer）。

ENV 名对齐 Pi `env-api-keys.ts`。共享 ENV 的对（`opencode`/`opencode-go`、`moonshotai`/`moonshotai-cn`、`qwen-token-plan`/`qwen-token-plan-individual`）必须分 id、分 catalog、分 host，但 **共用同一个 api key 实现**。

preset 全集（实现时从 Pi HEAD 对表，日期写入 catalog 注释）：

`openai`、`azure-openai-responses`、`google`、`deepseek`、`nvidia`、`groq`、`cerebras`、`mistral`、`huggingface`、`fireworks`、`together`、`baseten`、`vercel-ai-gateway`、`zai`、`zai-coding-cn`、`opencode`、`opencode-go`、`ant-ling`、`minimax`、`minimax-cn`、`moonshotai`、`moonshotai-cn`、`qwen-token-plan`、`qwen-token-plan-individual`、`qwen-token-plan-cn`、`xiaomi`、`xiaomi-token-plan-cn`、`xiaomi-token-plan-ams`、`xiaomi-token-plan-sgp`、`xai`、`openrouter`、`kimi-coding`、`github-copilot`（仅 `COPILOT_GITHUB_TOKEN` 这条 key 路径）、`anthropic`（`ANTHROPIC_API_KEY`；`ANTHROPIC_AUTH_TOKEN` 按 Pi 改为 Bearer）。

示例（形状，host / 每模型 `api` 以 Pi 源码为准）：

| id | 典型 model.api | default host（示意） | env |
|---|---|---|---|
| `openai` | `openai-responses` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `xai` | `openai-responses` | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `groq` | `openai-completions` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `anthropic` | `anthropic-messages` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `kimi-coding` | `anthropic-messages` | `https://api.kimi.com/coding` | `KIMI_API_KEY` |
| `openrouter` | `openai-completions` | OpenRouter OpenAI 兼容根 | `OPENROUTER_API_KEY` |
| `fireworks` | 按模型：`openai-completions` 或 `anthropic-messages` | `https://api.fireworks.ai/inference` | `FIREWORKS_API_KEY` |
| `opencode` | 按模型：最多四种方言 | 无 preset host（在模型上） | `OPENCODE_API_KEY` |
| `azure-openai-responses` | `azure-openai-responses` | 无 preset host（部署 URL 在模型上） | `AZURE_OPENAI_API_KEY` |

`github-copilot` 可留在表里走 `COPILOT_GITHUB_TOKEN`；v1 **不**做 Copilot OAuth，不要在 UI 上写成「支持 Copilot 订阅」。

`models.json` / CLI 覆盖：同一结构，用户自己填 `api` + `base_url` + env。`llama.cpp`（`LLAMA_BASE_URL` + `openai-completions`）也是这一行，只是模型列表要 `refresh_models`（阶段 4）。

**不能塞进这张表的**（阶段 3）：`amazon-bedrock`、`google-vertex`（ADC）、`cloudflare-workers-ai`、`cloudflare-ai-gateway`、`radius` 动态目录。它们不出现在可登录列表，除非该阶段已接线。

**方言未落地的模型：** 仍可出现在 `models/list`，但必须带 `supported: false` 与 `reason: dialect_unimplemented`（或等价字段）。`session/set_model` 与采样拒绝，错误信息指向所缺方言。允许 `lato login <id> --api-key` 先存 key。专用 resolve 未接线的 id 则连 login 都不提供。

图像 provider **v1 不做**。

模型表：v1 嵌入移植自 Pi 的静态 catalog（注明来源与日期）。不在 v1 接 models.dev 自动爬取。

### 8.5 OAuth（v1 仅两家）

| id | 方言 | host | 说明 |
|---|---|---|---|
| `kimi-coding` | `anthropic-messages` | `https://api.kimi.com/coding` | Kimi Code 订阅；可另走 `KIMI_API_KEY` |
| `openai-codex` | `openai-codex-responses` | `https://chatgpt.com/backend-api` | ChatGPT Plus/Pro。与平台 `openai` **分 id**，禁止混 resolve |

OAuth 实现按 Pi 行为用 Rust 重写，NOTICE 列出 MIT 来源：`kimi-coding`、`openai-codex`，外加它们用到的 `pkce` / callback / device-code 公共件。

不移植：`anthropic`、`github-copilot`、`xai`、`openrouter`、`radius` 的 OAuth 文件。

### 8.6 与 loop 的接法

采样前：若 `model.api` 未实现则拒绝（与 `models/list.supported` 一致）。否则 `auth = models.get_auth(model).await`，把 `api_key/headers/base_url` 交给该方言的 `stream`。工具层看不到 oauth vs key，也看不到厂商名。

`/login`：

- 无 TUI：`lato login <id> --api-key` 或 `lato login <id> --oauth`
- `--oauth` 仅 `kimi-coding` 与 `openai-codex` 合法
- 交互式：先选 provider；若两家双通道再选方法；单一方法跳过第二屏

## 9. 权限、沙箱、信任

审批模式：`ask`（交互默认）、`auto`（策略允许的自动过）、`always`（`lato -p` / CI 隐含）。Deny 规则与 hooks 在 always 下仍生效。`always` 不得作为交互安装默认。

`lato -p`：不提示 folder trust、不阻塞 TTY；cwd 仅本进程视为已信任（默认不把「永久信任」写入磁盘）。交互会话：未信任目录必须确认后才加载项目插件 / 项目 hooks。

沙箱 profile：`off` | `workspace`（可写 cwd + tmp）| `read-only`。三平台都要实现这三档：

| OS | 包装 |
|---|---|
| macOS | `sandbox-exec`（seatbelt） |
| Linux | `bwrap` |
| Windows | Restricted Token + Job Object |

某 profile 在当前 OS 起不来 → **拒绝该次 shell**，不得改成 `off` 继续跑。Windows 不是「先 unsandbox 凑合用」。接线后的验收见验收文档 D 节。

默认 shell：Unix `bash` 或用户 `$SHELL`；Windows 为 PowerShell（`pwsh` 优先，否则 `powershell.exe`）。路径、引号、环境块按 OS 处理，禁止把 POSIX 路径假设写进工具层。

工作区信任键与文件锁键同一套规范化（第 3 节）。Windows 上 `C:\Work\A`、`c:/work/a`、`\\?\C:\Work\A` 视为同一目录。

## 10. 持久化

| 数据 | 位置 |
|---|---|
| 凭证 | `$LATO_HOME/auth.json`（默认 Unix `~/.lato/`，Windows `%USERPROFILE%\.lato\`） |
| 用户配置 | `$LATO_HOME/config.toml` |
| 会话 | `$LATO_HOME/sessions/<id>.jsonl`（可追加的 transcript） |
| 项目覆盖 | `<repo>/.lato/config.toml`、`AGENTS.md` |

会话可 resume。v1 不做跨会话 memory / dream。

## 11. 分阶段交付

阶段只表示**实现深度**。API key preset 可以早注册；OAuth 方法只在两家上广告；未实现方言的模型必须 `supported: false`。

| 阶段 | 交付 |
|---|---|
| 0 内核 | ACP 第 6 节最小集、SessionActor、turn loop、CredentialStore、`ApiKeyPreset` + `env_api_key`、解析顺序、headless CLI（走 ACP）。路径规范化与默认 shell 含 Windows。MCP / hooks / skills / compact 为空操作。ToolRouter 仅编码最小集。沙箱可 `off`。`-p` = always + 本进程信任 cwd。验收：验收文档 A 节 |
| 1 标准 key | 三种方言 HTTP + VCR。验收文档 B 节 |
| 2 订阅登录 | 两家 OAuth + `openai-codex-responses`。验收文档 C 节 |
| 3 云特例 | 其余方言与专用 resolve。验收文档 E3 |
| 4 动态与本地 | llama.cpp / `models.json`。验收文档 E4 |
| 5 产品面 | 插件、worktree、沙箱收紧。验收文档 E5；沙箱接线后的 OS 用例见 D 节 |

阶段 0–1 结束后应能：`lato login openai --api-key`（或 env `OPENAI_API_KEY` / `XAI_API_KEY` / `KIMI_API_KEY`）+ `lato -p "fix the tests"` 在仓库里改代码并跑测试。对应验收文档 B1-6（LIVE）与 B 节 UNIT。

阶段 2 结束后应能：`lato login openai-codex --oauth` 与 `lato login kimi-coding --oauth`，订阅凭证落盘后同样跑任务。对应验收文档 C 节。

**阶段完成 = 验收文档对应章节全部非 LIVE 编号 PASS。** 不以设计规格第 11 节叙述代替测试。

## 12. 测试

测试用例、CI 矩阵、记录模板见：

**`docs/superpowers/specs/2026-08-31-lato-acceptance.md`**

本节省略重复。新增能力先加验收编号再写代码。与本文冲突时改规格并同步验收文档。

## 13. 建议补充（对话里没拍板，建议写入范围或明确推迟）

下面这些不影响「像 Codex 一样转、像 Pi 一样解析 key」，但自主跑和可维护性差很多。建议 **采纳带 \*** 的项进 v1，其余标明推迟。

1. **\* Replay / eval 夹具。** 把 turn 输入、工具结果、最终 diff 存成 case，防止换模型或改 prompt 时回归。
2. **\* 输出截断策略。** 工具结果进模型有字节上限（Grok 20k 字符 / MCP 20k 字节量级），溢出落盘路径，模型只看到摘要。否则长日志会撑死 context。
3. **\* 静止检测。** 同一工具同一参数连打 N 次则 EndTurn，避免 hook/goal 空转。
4. **模型目录新鲜度。** v1 嵌入静态表即可；建议预留 `models-store.json` 缓存形状，阶段 4 再联网刷新。
5. **Pi `auth.json` 只读导入。** 启动时若本仓库还没有凭证、但存在 `~/.pi/agent/auth.json`，可提示导入。方便迁移，不做默认同步双写。只导入 Lato 认识的通道（API key 各 id、以及两家 OAuth）。
6. **代理与企业网。** 认 `HTTP_PROXY` / `HTTPS_PROXY`。国内厂商（ZAI CN、Moonshot CN、Qwen CN、Xiaomi CN）几乎必用。建议 v1 就读这些变量。
7. **请求级缓存头。** Anthropic / OpenAI prompt cache 能明显降成本。可在对应 stream 方言里跟 Pi 一样做，不单开项目。
8. **子代理隔离。** v1 允许同仓串行子代理；**worktree 隔离建议作为 v1.1 硬需求**，否则并行改代码会互相覆盖。
9. **可观测性。** 至少：session id、provider、模型、input/output tokens、工具名、耗时，打到 stderr JSON 或本地日志。不上完整 OTEL 也可以。
10. **安全默认。** `always` 审批不得作为交互默认；CI 显式开。`.env`、私钥 glob 在 workspace 沙箱 deny。
11. **许可证流程。** 按 Pi 契约重写的 OAuth/API 代码在 `NOTICE` 列出 MIT 来源与日期。Codex `apply_patch` 若移植需保留 Apache 声明（Grok 已这样标注）。
12. **产品名与协议前缀。** 已定为 `lato` / `lato/` / `~/.lato/`，客户端与 crate 从一开始就用此 id，禁止再写 `harness` 占位。

明确推迟：跨会话 memory、语音、图像生成、插件市场、多用户服务端、把 Codex 当 MCP server 再包一层、其余五家 OAuth。

## 14. 仓库布局

代码只写在 `lato` 仓库。现有根 `Cargo.toml`（`[package] name = "lato"`）保留为 **CLI 二进制**；在同仓库加 workspace members：

```text
lato/                          # 本 git 仓库根；CLI package name = lato
  Cargo.toml                   # workspace + lato bin
  docs/superpowers/specs/      # 设计规格 + 验收文档
  crates/
    lato-protocol/             # ACP 类型与 JSON-RPC
    lato-ai/                   # preset 表、env_api_key、stream 方言、两家 OAuth、catalog
    lato-agent/                # SessionActor、turn loop、compaction
    lato-tools/                # 内建工具、ToolRegistry
    lato-workspace/            # 文件系统、沙箱包装、审批
    lato-mcp/                  # MCP 客户端
```

`lato-ai` 不得依赖 `lato-agent`。`lato-agent` 只通过 `get_auth` 与 `stream` 使用 `lato-ai`。CLI 通过 in-process ACP channel 调 host，不直连 `run_turn`。

`lato-ai` 内部建议按协议拆模块（`api/openai_completions`、`api/anthropic_messages`、…），OAuth 只出现在 `auth/oauth/kimi_coding` 与 `auth/oauth/openai_codex`。preset 表是数据，不是每个厂商一个 `.rs`。

## 15. 开放问题

已关闭：

1. **产品短名**：`lato`（状态目录、ACP 扩展前缀、CLI 二进制、工具命名空间）。
2. **语言**：Rust。代码目录 `innovation/lato`。
3. **OAuth 范围**：v1 仅 `kimi-coding` 与 `openai-codex`。API key 按 `model.api` + URL 支持 catalog 内全部 env-key 厂商。
4. **调度键**：协议在模型上（`model.api`），host 在模型或 preset 上；preset 不写死单一 `api`。
5. **未落地方言**：`models/list` 带 `supported: false` / `reason: dialect_unimplemented`；采样与 `set_model` 拒绝；允许先存 API key。
6. **阶段 0 空操作**：MCP 预热、skills 注入、hook bus、compact 未实现前必须 no-op；超上下文上限则失败，不 truncate。
7. **Windows**：v1 愿景含 Windows 原生（路径、shell、状态目录、沙箱 Restricted Token）。不是 WSL 替代。
8. **无头**：`lato -p` = always + 本进程信任 cwd；交互默认 ask。
9. **ACP 最小集**：第 6 节；不做 `session/load`；`authenticate` 若存在则代理到 `lato/auth/login`。
10. **文件锁 / 信任键**：规范化绝对路径，Windows 大小写折叠。
11. **验收**：`docs/superpowers/specs/2026-08-31-lato-acceptance.md`。

仍待确认（未拍板则用默认）：

12. **第 13 节带 \*** 的三项（replay、截断、静止检测）是否进 v1。默认按「进 v1」写。
13. **是否只读导入 `~/.pi/agent/auth.json`。** 默认：提示导入，不自动；且只导入 Lato 已支持的通道。

---

阶段是否完成看验收文档，不看本节叙述。通过后下一份文档才是分阶段实现计划。
