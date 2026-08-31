# Lato 设计规格

- 日期：2026-08-31
- 状态：待审阅
- 产品名：`lato`
- 实现仓库：`/Users/huangyongzhao/Documents/work/innovation/lato`（后续代码只写这里；当前已是 `edition = "2024"` 的 bin crate，将扩成 Cargo workspace）
- 参照源码（2026-08-31 浅 clone）：
  - Codex：`/tmp/harness-src/codex`（`openai/codex`）
  - Grok Build：`/tmp/harness-src/grok-build`（`xai-org/grok-build`）
  - Pi：`/tmp/harness-src/pi`（`earendil-works/pi`，MIT）

## 1. 目标

建造一套**自主可用、可嵌客户端、可插插件**的编码 Agent harness（产品名 Lato）：模型在策略允许的范围内自己转完 turn，人只在审批点介入。

第一版同时满足：

1. **无头工人**：CLI / CI / 作业系统里把任务跑完。
2. **可嵌入运行时**：TUI、IDE、Web、SDK 共用同一个 agent 脑。
3. **厂商可切换**：Pi 当前内建的全部聊天 provider 都能登录（OAuth）或用 API key 跑；解析顺序与 Pi 一致。

成功标准：

- 同一 session 协议可驱动 headless 与交互客户端；客户端不执行工具。
- 换 provider / 换模型不改 turn loop。
- 新工具只注册进目录，不改循环。
- 长任务能 compact、能中断、能在沙箱与审批下失败可恢复。
- `/login` 对双通道厂商能选「账号订阅」或「API key」；凭证落盘后进程重启仍可用。

## 2. 非目标（v1）

不在第一版实现：

- 完整终端 TUI（ratatui / Ink / Pi TUI）。第一客户端是 stdio ACP + 无头 CLI。
- Grok 的 Rhai workflow、goal orchestrator、Arena 多路竞赛、Grove clone。
- Codex realtime 语音、Guardian 嵌套评审 agent。
- 插件市场 UI、热加载动态库（`.so` / Python entry）。
- 依赖 `@earendil-works/pi-ai` 作为运行时（契约与目录按 Pi **用 Rust 重写**进本仓库，保留 MIT 声明；不把 Pi 当黑盒 SDK）。
- 实现 Gemini CLI / Antigravity（Pi 当前 HEAD 已无这两家）。
- 浏览器内跑 OAuth callback（可后做；v1 以本机 HTTP callback + 粘贴 redirect / device code 为准，callback 跑在 Lato 进程内，不引入 Node）。

## 3. 已确认的设计决策

| 决策 | 选择 |
|---|---|
| 产品短名 | `lato`。CLI 二进制、ACP 扩展前缀、状态目录、工具命名空间都用此 id |
| Loop | Codex 式：submission → 一 session 一 active turn → persist-then-execute 工具 → 再采样 |
| 对外协议 | ACP（`session/new\|prompt\|cancel` + `session/request_permission`）；内部 queue-pair 不暴露 |
| 工具目录 | Grok 式 `ToolKind` + 每步现装；MCP 默认 `search_tool` + `use_tool` |
| Provider | 全抄 Pi `builtinProviders()`（40 家聊天）+ `llama.cpp` 扩展；login 与 API key 双通道 |
| 凭证 | 每 provider 一条；`type: api_key \| oauth`；存盘优先于环境变量；禁止刷新失败后静默回落 env |
| 插件 | 目录包（skills / hooks / MCP），信任后才激活可执行部分 |
| 执行与 UI | 工具只在 host 执行；客户端只渲染事件和回答审批 |
| 语言 | **Rust**（Cargo workspace，edition 2024）。v1 不双语言。Pi 的 7 个 OAuth + 10 种 stream API 按同一契约在 `lato-ai` 重写；OS 沙箱 v1 用进程包装（macOS `sandbox-exec` / Linux `bwrap`），不自写 Landlock |
| 状态目录 | `~/.lato/`（`auth.json` 0600，目录 0700） |

Provider crate 必须保持「id / auth / api / models」边界：`lato-ai` 不得依赖 `lato-agent` / TUI；agent 只通过 `get_auth` 与 `stream` 使用 ai。

Rust 代价（已接受）：阶段 2 的 OAuth 与多方言移植比 TypeScript 慢，**不因此缩小阶段 0–1 的 40 家注册表或三种 stream 方言**。

## 4. 总体架构

```text
Client（headless CLI / 未来 TUI / IDE / Web / SDK）
        │  ACP JSON-RPC（stdio 或 socket）
        ▼
Session host
  ├─ SessionActor（一会话一 actor，至多一个 active turn）
  ├─ Models / Provider registry     ← Pi 契约，Rust 实现
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
- **并行执行，串行同文件。** 写路径按 `file_path` / `path` / `target_file` 加锁。
- **停止条件（全部一等公民）：** 模型无 tool call；用户取消；权限拒绝；预算 / max-turns；静止检测（同一工具调用连打超阈值）；Stop hook 未要求续跑；认证耗尽。

### 5.2 上下文

注入片段必须可哈希、有字节上限，变了才重注入（Codex world-state）：

- 系统 / 开发者指令
- `AGENTS.md` 链（cwd → repo root，含常见兼容名）
- 权限与沙箱说明（只描述 **shell 工具** 的沙箱）
- 技能与插件说明
- 环境（cwd、shell、日期）
- 对话历史（compaction 是唯一允许的历史改写）

Compaction：token 过阈值或手动触发；写成显式任务，结果作为 summary + 有限回注，禁止悄悄 truncate。

### 5.3 错误与重试

- 采样瞬态失败：每 step 最多 3 次、每 prompt 最多 10 次、窗口 10 分钟（对齐 Grok）。
- OAuth 401：刷新一次再试；刷新失败 **不** 改用 env key。
- API key 401：失败并提示 `/login <provider>`。
- 工具解析错误：把错误当 tool_result 回模型，不崩 turn。

## 6. 客户端协议（ACP）

v1 实现 `agent-client-protocol` 的 Agent 侧：

| 方法 | 用途 |
|---|---|
| `initialize` / `authenticate` | 能力广告、鉴权方法列表 |
| `session/new` `load` `list` `resume` `close` | 会话 |
| `session/prompt` | 开 turn |
| `session/cancel` | 中断 |
| `session/set_model` | 换模型 |
| 反向 `session/request_permission` | 审批 |
| 通知 `session/update` | 文本、思考、tool_call、tool_result |

扩展方法（v1 最小集，前缀 `lato/`）：

- `session/info`、`models/list`
- `auth/login`、`auth/logout`、`auth/status`（供无头与未来 TUI）
- `plugins/reload`

不在 v1 做 Codex `thread/start` JSON-RPC。headless CLI **必须**走同一 ACP（可 in-process channel），禁止再写一套直接调 `run_turn` 的旁路。

权限提示的决策：允许一次 / 本 session 允许 / 拒绝 / 取消。客户端只回决策。

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

- 发现顺序：CLI `--plugin-dir` → 项目 `.lato/plugins` → 用户 `~/.lato/plugins`
- 项目插件默认不信任；用户/CLI 自动信任
- 未信任则不启动该插件的 hooks / MCP

Skills：`SKILL.md` 目录；显式 `/skill` 或模型按描述选用。

Hooks 事件（v1）：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Stop`、`PreCompact`、`PostCompact`。PreToolUse 可改参数但不可改工具名。

MCP：stdio + streamable HTTP；工具名 `server__tool`。默认不把全部 MCP 工具展开进模型；`search_tool` 发现，`use_tool` 调用。配置了少量 server 时允许直接展开（session 选项）。

## 8. Provider 层（契约全抄 Pi，Rust 实现）

### 8.1 合同

语义与 Pi `packages/ai` 一致；类型用 Rust 表达，不引入 TypeScript 运行时。

```rust
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub auth: ProviderAuth, // api_key 与 oauth 至少一项
    // models() / refresh_models() / api: StreamImpl 或按 ModelApi 映射
}

pub struct ApiKeyAuth {
    pub name: String,
    // login(interaction) -> ApiKeyCredential（可选）
    // resolve(input) -> Option<AuthResult>
}

pub struct OAuthAuth {
    pub name: String,
    pub is_subscription: bool,
    pub login_label: Option<String>,
    // login(interaction) -> OAuthCredential
    // refresh(credential, signal) -> OAuthCredential
    // to_auth(credential) -> ModelAuth
}
```

`AuthInteraction`：`prompt(secret|text|select|manual_code)` + `notify(auth_url|device_code|info|progress)`。UI 不进 provider。

`Models::login(provider_id, LoginMethod::Oauth | LoginMethod::ApiKey, interaction)` 是唯一登录入口。OAuth-only 厂商禁止伪造 API key login。

### 8.2 解析顺序（必须与 Pi 一致）

对齐 Pi `packages/ai/src/auth/resolve.ts`：

1. 调用方 runtime override（不落盘，给 CI）
2. store 已有 `oauth` → 距过期 < 5 分钟则在 `modify` 锁内 refresh → `to_auth`
3. store 已有 `api_key` → `resolve(credential)`
4. store 空 → `api_key.resolve(None)` 扫环境 / ADC / IAM
5. 都没有 → 未配置

禁止：OAuth 刷新失败后改用 env key；store 类型与 handler 不匹配时改走另一通道。

每 provider **一条**凭证。Login 覆盖旧条目。Logout 删除后 env 才重新生效。

`~/.lato/auth.json` 形状兼容 Pi，便于迁移：

```json
{
  "xai": { "type": "oauth", "access": "...", "refresh": "...", "expires": 0 },
  "anthropic": { "type": "api_key", "key": "sk-ant-..." }
}
```

`key` 支持字面量、`$ENV`、`!command`（进程内缓存 stdout），与 Pi 相同。文件 0600，`modify` 带文件锁。

### 8.3 Stream 方言（真正要写的 HTTP）

| `model.api` | 用途 |
|---|---|
| `openai-completions` | 大多数 API-key 厂商、OpenRouter |
| `openai-responses` | `openai`、`xai` |
| `openai-codex-responses` | ChatGPT 订阅 Codex |
| `azure-openai-responses` | Azure |
| `anthropic-messages` | Anthropic、Kimi、MiniMax、部分 Copilot |
| `google-generative-ai` | Gemini API |
| `google-vertex` | Vertex |
| `bedrock-converse-stream` | Bedrock |
| `mistral-conversations` | Mistral |
| `pi-messages` | Radius |

一个 provider 可对多种 `model.api` 做 map（Copilot、OpenCode、Fireworks）。

### 8.4 内建 provider 全集

来源：Pi `builtinProviders()`。v1 **全部注册**；实现按第 11 节分层，但 `/login` 与 `models/list` 从层 1 起就露出全部 id。

**双通道（OAuth + API key）**

| id | ENV | OAuth |
|---|---|---|
| `anthropic` | `ANTHROPIC_API_KEY`（另 `ANTHROPIC_AUTH_TOKEN`→Bearer） | Claude Pro/Max |
| `github-copilot` | `COPILOT_GITHUB_TOKEN` | Copilot；可按 `availableModelIds` 过滤 |
| `xai` | `XAI_API_KEY` | SuperGrok / X Premium，device code |
| `kimi-coding` | `KIMI_API_KEY` | Kimi Code |
| `openrouter` | `OPENROUTER_API_KEY` | PKCE，mint 用户 key |
| `radius` | `RADIUS_API_KEY` | 动态目录 |

**仅 OAuth**

| id | 说明 |
|---|---|
| `openai-codex` | ChatGPT Plus/Pro。与平台 `openai` 分 id，禁止混 resolve |

**标准 API key（`envApiKeyAuth`）**

`openai`、`azure-openai-responses`、`google`、`deepseek`、`nvidia`、`groq`、`cerebras`、`mistral`、`huggingface`、`fireworks`、`together`、`baseten`、`vercel-ai-gateway`、`zai`、`zai-coding-cn`、`opencode`、`opencode-go`、`ant-ling`、`minimax`、`minimax-cn`、`moonshotai`、`moonshotai-cn`、`qwen-token-plan`、`qwen-token-plan-individual`、`qwen-token-plan-cn`、`xiaomi`、`xiaomi-token-plan-cn`、`xiaomi-token-plan-ams`、`xiaomi-token-plan-sgp`。

ENV 名对齐 Pi `env-api-keys.ts`。共享 ENV 的对（`opencode`/`opencode-go`、`moonshotai`/`moonshotai-cn`、`qwen-token-plan`/`qwen-token-plan-individual`）必须分 provider id、分 catalog。

**专用 ApiKeyAuth**

| id | resolve |
|---|---|
| `amazon-bedrock` | bearer / profile / IAM / ECS / IRSA |
| `google-vertex` | API key 或 ADC（需 project + location） |
| `cloudflare-workers-ai` | key + account id |
| `cloudflare-ai-gateway` | 再加 gateway id |

**扩展（同一注册表）**

| id | 说明 |
|---|---|
| `llama.cpp` | `LLAMA_BASE_URL`，动态本地模型，`openai-completions` |
| `models.json` 自定义 | 用户声明 api + baseUrl + env key 名 |

图像：`openrouter` 图像 provider **v1 不做**（见非目标）；接口预留 `ImagesModels` 以免以后拆 auth。

模型表：v1 嵌入移植自 Pi 的静态 catalog（注明来源与日期）。Radius / llama.cpp 除外，它们必须 `refresh_models`。不在 v1 接 models.dev 自动爬取。

OAuth 实现（按 Pi 行为用 Rust 重写，NOTICE 列出 MIT 来源）：`anthropic`、`openai-codex`、`github-copilot`、`xai`、`kimi-coding`、`openrouter`、`radius`，外加 `pkce`、`device-code`。

### 8.5 与 loop 的接法

采样前：`auth = models.get_auth(model).await`，把 `api_key/headers/base_url` 交给 `provider.api.stream`。工具层看不到 oauth vs key。

`/login`：先选方法（账号 vs API key），再选 provider。单一方法的厂商跳过第一屏。无 TUI 时：`lato login <provider> --api-key` / `--oauth`。

## 9. 权限、沙箱、信任

审批模式：`ask`（默认）、`auto`（策略允许的自动过）、`always`（无头 / CI）。Deny 规则与 hooks 在 always 下仍生效。

沙箱 profile：`off` | `workspace`（可写 cwd + tmp）| `read-only`。v1 用 OS 包装实现，失败则拒绝执行而非静默 unsandbox。

工作区信任：首次在某目录跑要确认（对齐 Grok folder trust）。未信任则项目插件与项目 hooks 不加载。

## 10. 持久化

| 数据 | 位置 |
|---|---|
| 凭证 | `~/.lato/auth.json` |
| 用户配置 | `~/.lato/config.toml` |
| 会话 | `~/.lato/sessions/<id>.jsonl`（可追加的 transcript） |
| 项目覆盖 | `<repo>/.lato/config.toml`、`AGENTS.md` |

会话可 resume。v1 不做跨会话 memory / dream。

## 11. 分阶段交付

阶段只表示**实现深度**，不表示从注册表里删厂商。

| 阶段 | 交付 |
|---|---|
| 0 内核 | ACP host、SessionActor、turn loop、CredentialStore、`create_provider`、解析顺序、headless 客户端 |
| 1 标准 key | 先实现 `openai-completions`、`openai-responses`、`anthropic-messages`。注册表列入全部 40 个 id；**能真正发请求的是这三种方言上的厂商**（含 openai、xai 的 key 路径、anthropic 的 key 路径、OpenRouter、ZAI、Qwen、Moonshot、MiniMax、Groq 等）。`google` / `mistral` / `azure-openai-responses` 等要等本表方言落地后才可跑 |
| 2 双通道 login | A+B 组七个 OAuth（stream 已在阶段 1 的沿用现成方言；`openai-codex-responses` 在本阶段补） |
| 3 云特例 | 其余方言与专用 resolve：Bedrock、Vertex、Cloudflare、Azure、Gemini、Mistral |
| 4 动态与本地 | Radius、llama.cpp、`models.json` |
| 5 产品面 | 只读 TUI 或 IDE ACP 客户端、插件目录、子代理 worktree、OS 沙箱收紧 |

阶段 0–1 结束后应能：`lato login openai --api-key` + `lato -p "fix the tests"` 在仓库里改代码并跑测试。

## 12. 测试

- Auth 解析：存盘 oauth / 存盘 key / 仅 env / override / 刷新失败不回落，用假 `CredentialStore`。
- Loop：fixture 流（模型先 tool 后文本）；取消时历史含未完成 call。
- 工具：同文件并行编辑串行化。
- Provider：每个 stream 方言至少一条录制的 HTTP fixture（VCR），不在 CI 打真网。
- OAuth：device code / PKCE 用 mock token endpoint。
- ACP：stdio 往返：prompt → tool permission → 完成。

## 13. 建议补充（对话里没拍板，建议写入范围或明确推迟）

下面这些不影响「像 Codex 一样转、像 Pi 一样登录」，但自主跑和可维护性差很多。建议 **采纳带 \*** 的项进 v1，其余标明推迟。

1. **\* Replay / eval 夹具。** 把 turn 输入、工具结果、最终 diff 存成 case，防止换模型或改 prompt 时回归。没有这项，40 家 provider 无法持续保证。
2. **\* 输出截断策略。** 工具结果进模型有字节上限（Grok 20k 字符 / MCP 20k 字节量级），溢出落盘路径，模型只看到摘要。否则长日志会撑死 context。
3. **\* 静止检测。** 同一工具同一参数连打 N 次则 EndTurn，避免 hook/goal 空转。
4. **模型目录新鲜度。** v1 嵌入静态表即可；建议预留 `models-store.json` 缓存形状，阶段 4 再联网刷新。不预留的话 Radius 会逼着改存储。
5. **Pi `auth.json` 只读导入。** 启动时若本仓库还没有凭证、但存在 `~/.pi/agent/auth.json`，可提示导入。方便迁移，不做默认同步双写。
6. **代理与企业网。** Pi 认 `HTTP_PROXY` / `HTTPS_PROXY` / 凭证 `env`。国内厂商（ZAI CN、Moonshot CN、Qwen CN、Xiaomi CN）几乎必用。建议 v1 就读这些变量。
7. **请求级缓存头。** Anthropic / OpenAI prompt cache 能明显降成本。可在对应 stream 方言里跟 Pi 一样做，不单开项目。
8. **子代理隔离。** v1 允许同仓串行子代理；**worktree 隔离建议作为 v1.1 硬需求**，否则并行改代码会互相覆盖。
9. **可观测性。** 至少：session id、provider、模型、input/output tokens、工具名、耗时，打到 stderr JSON 或本地日志。不上完整 OTEL 也可以。
10. **安全默认。** `always` 审批不得作为交互默认；CI 显式开。`.env`、私钥 glob 在 workspace 沙箱 deny。
11. **许可证流程。** 按 Pi 契约重写的 OAuth/API 代码在 `NOTICE` 列出 MIT 来源与日期。Codex `apply_patch` 若移植需保留 Apache 声明（Grok 已这样标注）。
12. **产品名与协议前缀。** 已定为 `lato` / `lato/` / `~/.lato/`，客户端与 crate 从一开始就用此 id，禁止再写 `harness` 占位。

明确推迟：跨会话 memory、语音、图像生成、插件市场、多用户服务端、把 Codex 当 MCP server 再包一层。

## 14. 仓库布局

代码只写在 `lato` 仓库。现有根 `Cargo.toml`（`[package] name = "lato"`）保留为 **CLI 二进制**；在同仓库加 workspace members：

```text
lato/                          # 本 git 仓库根；CLI package name = lato
  Cargo.toml                   # workspace + lato bin
  crates/
    lato-protocol/             # ACP 类型与 JSON-RPC
    lato-ai/                   # Provider、auth、stream 方言、内建 catalog
    lato-agent/                # SessionActor、turn loop、compaction
    lato-tools/                # 内建工具、ToolRegistry
    lato-workspace/            # 文件系统、沙箱包装、审批
    lato-mcp/                  # MCP 客户端
```

`lato-ai` 不得依赖 `lato-agent`。`lato-agent` 只通过 `get_auth` 与 `stream` 使用 `lato-ai`。CLI 通过 in-process ACP channel 调 host，不直连 `run_turn`。

## 15. 开放问题

已关闭：

1. **产品短名**：`lato`（状态目录、ACP 扩展前缀、CLI 二进制、工具命名空间）。
2. **语言**：Rust。代码目录 `innovation/lato`。

仍待确认（未拍板则用默认）：

3. **第 13 节带 \* 的三项**（replay、截断、静止检测）是否进 v1。默认按「进 v1」写。
4. **是否只读导入 `~/.pi/agent/auth.json`。** 默认：提示导入，不自动。

---

审阅时请重点看：第 2 节非目标是否砍够、第 8.4 节 40 家是否都要出现在 v1 注册表、Rust workspace 布局是否接受。通过后下一份文档才是分阶段实现计划。
