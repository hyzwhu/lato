# Lato Phase 6C MCP Runtime — 产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **A-level gate passed**（2026-09-11；见 `docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md`） |
| 日期 | 2026-09-11 |
| 基线 commit | `master @ 2a66a019`（Phase 6B2 hooks 已合入） |
| 上游边界 | Phase 6A §16；Phase 6B §19 Follow-on；可扩展 Agent 设计 §12.3 |
| 平行缺陷 | SenseNova exit0/无产物（见同目录 bug ticket；**不挡** 6C） |

---

## 1. 背景

Phase 6A 已冻结 `PluginSnapshot`：信任、启用、generation、以及 MCP 组件描述符路径（`.mcp.json` / inline metadata）。Phase 6B 已把 Skills 与 Hooks 接到同一快照与会话生命周期，并明确 **不** 启动 MCP。

当前 `lato-mcp` 仅有最小 transport stub：`call_stdio` / `call_streamable_http`（一次性 JSON-RPC、固定 30s 超时；HTTP 允许 `http`+`https`，尚无 SSRF / no-redirect 硬化）。产品需要完整的 MCP **运行时**：配置契约、有界进程生命周期、工具发现与 schema 缓存、渐进发现 UX、以及与现有 `ToolRuntime` 膜完全统一的安全路径。

## 2. 目标

交付 Phase 6C MCP runtime，使可信且启用的插件 MCP 服务器成为 **ToolRegistry 的新 provider**，而不是第二条执行通道。模型默认只看到 `search_tool` / `use_tool`；真实 MCP 调用必须经过：

`PreToolUse → schema/scope → PolicyEngine → approval → execute → PostToolUse`

并覆盖 stdio 与 streamable HTTP、有界生命周期、结果/故障边界、reload 与父子会话能力收窄、以及可安装命令 smoke / 门禁。

## 3. 完成定义（Definition of Done）

1. 仅从 turn 冻结的 `PluginSnapshot` 消费 **trusted + enabled** MCP 描述符；未信任/未启用插件零启动。
2. stdio 与 streamable HTTP fixture 均可完成 `initialize` → `tools/list` → 受控 `tools/call`。
3. MCP 工具名、参数、权限、批准、Hook rewrite、sandbox/scope 全部走现有统一边界；`McpManager` **不得** 绕过 `ToolRuntime`。
4. 默认渐进发现：模型仅暴露 `search_tool` / `use_tool`；少数服务器直扩为显式选项。
5. SessionEnd / cancel / timeout / 崩溃路径均能 process-tree reap；单服务器失败隔离。
6. 父子会话：子会话只能收窄 MCP server/tool 能力，不能恢复父会话已移除权限；N→N+1 reload 按 turn 快照隔离。
7. 门禁：workspace tests、Clippy、`cargo install --path .`、installed-command smoke、README、release report 全部通过。
8. SenseNova bug **不** 作为本阶段阻塞项（可并行跟踪）。

## 4. 范围与非范围

### 4.1 范围内（Phase 6C）

| # | 工作流 | 摘要 |
| --- | --- | --- |
| 1 | MCP config & descriptor contract | 冻结快照 → 解析 `.mcp.json`/inline；stdio / streamable HTTP；`server__tool` 限定名 |
| 2 | Bounded server lifecycle | start / initialize / health / timeout / cancel / process-tree reap / SessionEnd shutdown / 失败隔离 |
| 3 | Tool discovery & schema cache | `initialize`、`tools/list`、稳定缓存、命名碰撞处理 |
| 4 | Progressive discovery | 默认 `search_tool` / `use_tool`；少数服务器直扩为显式选项 |
| 5 | Unified tool safety membrane | MCP → ToolRegistry provider → ToolRuntime；禁止旁路 |
| 6 | Result & fault boundaries | 输出上限、超大结果落盘、cancel、协议错误、崩溃、HTTP SSRF、无 redirect、敏感审计 |
| 7 | Reload & parent/child | N/N+1 turn 快照隔离；子会话只能收窄 |
| 8 | Release gate | stdio/HTTP fixture、installed smoke、workspace tests、Clippy、README、release report |

### 4.2 明确非范围

- SenseNova / GLM `-p` 工具环退出码 0 但无产物缺陷的根因修复（平行 ticket；**不挡 6C**）。
- MCP Resources / Prompts / Sampling / Roots 全协议面（本阶段以 Tools 为主；其他能力可诊断为 unsupported）。
- SSE-only 遗留 HTTP transport（仅 streamable HTTP）。
- 任意远程 MCP marketplace / 自动安装不可信服务器。
- 动态改写 `tool` 名以绕过 policy（与 Hooks PreToolUse 一致：工具名不可变）。
- WASM / dylib 插件 ABI。
- Computer Use GUI 自动化本身（仅向测试提供 computer-use / PTY smoke **提示**）。

## 5. 架构边界（文字图）

```text
PluginSnapshot (gen N, trusted+enabled only)
        │
        ▼
   McpRegistry / McpManager          ← lato-mcp (+ lato-extensions 描述符物化)
   - parse .mcp.json / inline
   - start stdio | streamable HTTP
   - initialize + tools/list
   - schema cache, health, shutdown
        │
        ▼
   ToolRegistry provider             ← lato-tools
   - 注册 search_tool / use_tool
   - 可选：少数 server 直扩为 server__tool
   - 每个 MCP tool 仍是普通 Tool 描述符
        │
        ▼
   ToolRuntime membrane              ← 现有 prepare_scoped / authorize / execute
   PreToolUse → schema/scope → PolicyEngine → approval → execute → PostToolUse
        │
        ▼
   Hooks (Phase 6B2)                 ← updatedMCPToolOutput 在 6C 结果类型就位后可应用
   Journal / audit（敏感字段 redact）
```

**安全不变量：** MCP 是新的 ToolRegistry provider，**不是** 第二条执行通道。工具名、参数、权限、批准、Hook rewrite、sandbox/scope 必须全部穿过现有统一边界。`McpManager` 只负责 transport 与服务器生命周期；不得直接向模型返回未膜化的调用结果，也不得在 agent 中另开 `tools/call` 旁路。

## 6. 配置契约摘要

### 6.1 来源与激活

- 唯一输入：当前 turn 冻结 `PluginSnapshot` 中 **active**（trusted ∧ enabled）插件的 `mcp_config_path` / `inline_mcp_servers`。
- 项目插件在 folder 未信任时不得启动任何 MCP 进程或 HTTP 客户端会话。
- 单插件解析失败 → 诊断 + 隔离；不得拖垮同代其他插件。

### 6.2 `.mcp.json` / inline 形状（摘要）

兼容常见 MCP 客户端配置子集：

```json
{
  "mcpServers": {
    "demo-stdio": {
      "command": "node",
      "args": ["server.js"],
      "env": { "DEMO": "1" },
      "cwd": "."
    },
    "demo-http": {
      "url": "https://127.0.0.1:9443/mcp",
      "headers": { "Authorization": "Bearer …" },
      "transport": "streamable-http"
    }
  }
}
```

| 字段 | stdio | streamable HTTP |
| --- | --- | --- |
| 标识 | `command` + `args` | `url`（或等价 endpoint） |
| 环境 | `env` 合并；保留身份 env 覆盖防伪造 | `headers` 仅用于该服务器；不注入 ambient 会话凭据 |
| cwd | 解析后必须仍在插件/工作区允许边界内 | n/a |
| 超时 | 可配置；有默认与硬顶 | 同左 |
| 安全 | process group / tree reap | SSRF 校验；**禁止 redirect**；方案与 hooks HTTPS 同类（允许 loopback；拒绝 private/link-local/CGNAT/unspecified 等，评审确认是否允许 plain `http` 仅限 loopback） |

### 6.3 命名

- 服务器名：插件内稳定、规范化（建议 `[a-z0-9][a-z0-9_-]*`）。
- 工具 wire 名：`{server}__{tool}`（双下划线），与既有 stub `qualified_tool_name` 方向一致。
- 跨插件/跨服务器碰撞：后注册失败并诊断，或按稳定优先级保留先到者；**禁止** 静默覆盖。

## 7. 有界服务器生命周期

每个服务器实例由 `McpManager` 持有，绑定 snapshot generation：

1. **start** — 按需或会话预热（产品默认：首次 discovery/use 时惰性启动；可选预热名单）。
2. **initialize** — MCP `initialize` + `initialized` 通知；记录 serverInfo / capabilities。
3. **health** — 周期或调用前轻量检查；不健康则隔离并诊断。
4. **timeout** — 单 RPC 与整体启动均有界；超时杀进程组 / 取消 HTTP。
5. **cancel** — 会话/turn cancellation token 贯通。
6. **process-tree reap** — Unix 新进程组；超时/取消后 terminate → 短等待 → force-kill；避免僵尸。
7. **SessionEnd shutdown** — 与 hooks SessionEnd 同类有界截止；幂等。
8. **failure isolation** — 单服务器崩溃不影响其他服务器与内置工具环。

## 8. 工具发现与 schema 缓存

- 启动成功后调用 `tools/list`（必要时分页/续拉，若服务器支持）。
- Schema 进入 generation 级稳定缓存；同 generation 内模型可见描述符不变。
- Reload 采用新 generation：旧连接在无 turn 引用后关闭；新 turn 使用新缓存。
- 无效 schema 的单个 tool 被剔除并诊断，不丢弃整个服务器（除非 initialize 失败）。

## 9. 渐进发现 UX

**默认（推荐）：**

| 暴露给模型的工具 | 行为 |
| --- | --- |
| `search_tool` | 在已缓存 MCP 工具索引中按 query 检索；返回限定名、server、短描述、可选 schema 摘要 |
| `use_tool` | 参数：`server`/`tool` 或限定名 + `arguments`；内部解析为真实 MCP tool 调用并走完整膜 |

**显式选项：** 对白名单中的少数服务器，可将 `server__tool` **直接** 扩入模型 tool 列表（仍经同一 ToolRuntime）。默认关闭或仅限配置声明的小集合，避免上下文爆炸。

`search_tool` / `use_tool` 本身也是普通内置工具：需 schema、policy、approval（`use_tool` 对写/外网类 MCP 默认 ask，具体与 PolicyEngine 能力标签对齐）。

## 10. 结果与故障边界

| 类别 | 行为 |
| --- | --- |
| 输出大小 | 复用 / 对齐 `bound_tool_output`（当前 ~20 KiB 内联；超限写入 `.lato/tool-output/`） |
| 取消 | 返回可诊断的 cancel 错误；不泄漏半包敏感体 |
| 协议错误 | JSON-RPC `error` → 稳定 `ToolError` 码；不把原始 headers/env 写入 journal |
| 服务器崩溃 | 标记 server unhealthy；当前调用失败；可按策略惰性重启（有上限） |
| HTTP SSRF | 解析后地址类检查；无 redirect；错误信息不回显完整 URL 凭据 |
| 审计 | 记录 generation、server、tool、时长、outcome、hash；敏感 args/结果 redact |

PostToolUse：6C 提供 MCP 结果类型后，允许应用 hooks 已解析的 `updatedMCPToolOutput`（与 `updatedToolOutput` 同类再经 `bound_tool_output`）。

## 11. Reload 与父子会话

- **N / N+1：** 进行中的 turn 继续使用 generation N 的 MCP 连接与缓存；新 turn 采用 N+1；退休 generation 在引用清零后 shutdown。
- **子会话：** 先 `PluginSnapshot::derive_child`，再物化 MCP 能力。子会话只能 **移除** 父允许的 server/tool（或整体去掉 `ExtensionInvoke`），不得重新启用父已禁用插件，不得恢复父已收窄工具。
- 当前 `derive_child` 以 `ExtensionInvoke` 全有/全无为粗门；6C 需在此之上增加 **server/tool 级** 收窄契约（配置或 profile allowlist），并在验收矩阵中覆盖。

## 12. 验收矩阵

| ID | 工作流 | 验收标准 |
| --- | --- | --- |
| M-1 | Config & descriptor | 仅 trusted+enabled 快照描述符被物化；未信任项目 `.mcp.json` 零进程；坏 JSON 隔离 |
| M-2 | Config & descriptor | stdio 与 streamable HTTP 描述符均可解析；非法路径逃逸被拒 |
| M-3 | Config & descriptor | 限定名 `server__tool` 稳定；碰撞不静默覆盖 |
| M-4 | Lifecycle | initialize 成功路径记录 serverInfo；启动超时可 reap |
| M-5 | Lifecycle | cancel / SessionEnd 后无残留 MCP 子进程 |
| M-6 | Lifecycle | 服务器 A 崩溃不影响服务器 B 与内置工具 |
| M-7 | Discovery & cache | `tools/list` 结果在同 generation 稳定；reload 后新 generation 可见更新 |
| M-8 | Discovery & cache | 单 tool 坏 schema 被剔除并诊断 |
| M-9 | Progressive discovery | 默认模型定义仅含 `search_tool`/`use_tool`（及非 MCP 内置工具） |
| M-10 | Progressive discovery | `search_tool` 可检索到 fixture 工具；`use_tool` 可成功调用 |
| M-11 | Progressive discovery | 显式直扩选项仅暴露配置允许的少数 `server__tool` |
| M-12 | Safety membrane | MCP 调用路径出现 PreToolUse → prepare_scoped → PolicyEngine → approval → execute → PostToolUse |
| M-13 | Safety membrane | `McpManager` 无公共 API 可在无 grant 时直接 `tools/call` 向模型交付结果 |
| M-14 | Safety membrane | PreToolUse 改写参数后重新 schema/policy/approval；不可改工具名 |
| M-15 | Results & faults | 超大结果截断并落盘；journal 无密钥/完整超大体 |
| M-16 | Results & faults | HTTP 私网目标拒绝；redirect 不跟随 |
| M-17 | Results & faults | 协议错误与超时映射为稳定错误码 |
| M-18 | Reload & child | 中 turn reload：旧 turn 仍用 gen N；下 turn 用 N+1 |
| M-19 | Reload & child | 子会话无法调用父已移除的 MCP server/tool |
| M-20 | Security invariant | MCP 工具注册为 ToolRegistry provider；capability / sandbox 与内置工具同等强制 |
| M-21 | Release gate | stdio fixture + streamable HTTP fixture + installed-command smoke 全绿 |
| M-22 | Release gate | `cargo test --workspace`、Clippy `-D warnings`、`cargo install --path .`、README、release report 完成 |

## 13. Computer-use / Smoke 提示（给测试）

> 非 GUI Computer Use 全量套件；以下为 Phase 6C 建议手工/半自动冒烟提示。

1. **stdio fixture：** 临时插件含 `.mcp.json` + 本地脚本服务器；`-p` 或 TUI 中 `search_tool` → `use_tool` 读回固定 JSON。
2. **streamable HTTP fixture：** loopback TLS 或评审允许的 loopback HTTP；验证 SSRF（指向 link-local/私网应失败）与 no-redirect。
3. **Installed binary：** `cargo install --path .` 后 `LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6c_mcp_smoke`。
4. **信任边界：** 未信任项目插件不得拉起 MCP；信任后同配置可启动。
5. **SessionEnd：** 退出后 `pgrep`/进程组检查无残留。
6. **与 PTY 报告交叉：** SenseNova 用例 **不** 作为 6C 放行条件；若同环境复测，仅记录，不阻塞。

## 14. 门禁命令列表

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path .
lato --version
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6c_mcp_smoke -- --nocapture
```

附加建议（实现计划中展开）：

```bash
cargo test -p lato-mcp
cargo test -p lato-extensions --test mcp_config
cargo test -p lato-tools
cargo test -p lato-agent --test mcp_runtime
```

## 15. 开放问题（评审）

1. Streamable HTTP 是否允许 **loopback-only** 的 `http://`，抑或与 hooks 一样仅 `https`？
2. 子会话 MCP 收窄粒度：仅 server allowlist，还是 server+tool allowlist？profile 字段名？
3. 惰性启动 vs SessionStart 预热的默认产品行为。
4. `use_tool` 的默认 approval 策略：一律 ask，还是按 MCP tool annotations / 推断 side-effect？
5. 现有 `CapabilityCeiling` 仅 `ExtensionInvoke` 粗门，6C API 是扩展 ceiling 还是并行 `McpCapabilityCeiling`？
6. inline `mcpServers` 对象在 6A 为 inert：6C 是否完整支持 inline，还是仅文件路径？

## 16. 参考

- `docs/superpowers/specs/2026-09-07-lato-phase-6a-plugin-runtime-foundation-design.md` §16
- `docs/superpowers/specs/2026-09-07-lato-phase-6b-skills-hooks-design.md` §19、`updatedMCPToolOutput`
- `docs/superpowers/specs/2026-09-01-lato-extensible-agent-design.md` §12.3
- `docs/superpowers/plans/2026-09-07-lato-phase-6b2-hooks-implementation.md`（计划体例）
- 平行缺陷：`2026-09-11-bug-sensenova-exit0-no-artifact.md`
