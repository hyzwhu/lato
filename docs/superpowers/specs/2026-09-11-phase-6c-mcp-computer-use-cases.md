# Lato Phase 6C MCP Runtime — Computer-use / Smoke 用例

| 字段 | 值 |
| --- | --- |
| 状态 | **A-level gate passed**（2026-09-11；对齐产品设计验收矩阵；见 release gate report） |
| 日期 | 2026-09-11 |
| 基线 | `master @ 2a66a019` |
| 设计来源 | `2026-09-11-phase-6c-mcp-product-design.md` §12–§14 |
| 实施来源 | `2026-09-11-phase-6c-mcp-implementation-plan.md` |
| 平行缺陷 | SenseNova exit0/无产物 — **不挡** 6C 放行 |

---

## 0. 用法

- 本文件给 **lato测试** 手工/半自动 computer-use、PTY smoke，以及回归门禁对照。
- 自动化等级：`A` = cargo/fixture 全自动；`H` = 需 PTY / 人工观察；`S` = installed-binary smoke。
- 每条用例记录：lato commit、`lato --version`、OS、fixture 哈希、stdout/stderr、退出码、进程残留检查、journal 是否 redact。
- 不得把 grader / 隐藏断言原文喂给被测 Agent；只给「测试输入」。
- SenseNova 相关复测仅记结果，**不**作为 6C PASS 条件。

基础命令（实现到位后）：

```bash
cargo install --path .
# Prefer the cargo-installed binary (~/.cargo/bin/lato). An older ~/.local/bin/lato
# may shadow PATH — pin explicitly if needed:
#   LATO_SMOKE_BINARY="$HOME/.cargo/bin/lato"
lato --version
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6c_mcp_smoke -- --nocapture
```

PTY / computer-use 入口（需要批准、SessionEnd、信任边界时）：

```bash
# 无参数 TUI / 真实 PTY；或 -p headless
lato -p --sandbox workspace --model <provider/model> '<测试输入>'
```

---

## 1. Fixture 约定

| Fixture | 说明 |
| --- | --- |
| `F-stdio` | 临时可信插件：`.mcp.json` + 本地脚本 MCP server（固定 `echo`/`ping` 工具，返回可预测 JSON） |
| `F-http` | loopback streamable HTTP MCP（评审允许的 `http://127.0.0.1` 或 TLS）；含私网/redirect 负例 endpoint |
| `F-untrusted` | 同配置放在 **未信任** 项目插件目录 |
| `F-bad-json` | 损坏 `.mcp.json` / 非法 cwd 逃逸路径 |
| `F-collide` | 两服务器同名 tool，或跨插件 `server__tool` 碰撞 |
| `F-big-out` | MCP tool 故意返回 > ~20 KiB 负载 |
| `F-crash` | 服务器 A 启动后自杀；服务器 B 正常 |
| `F-child` | 父会话允许 `demo__ping`；子会话 profile 收窄去掉该 tool/server |

---

## 2. 验收矩阵对照用例

### 2.1 Config & descriptor（M-1…M-3）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-CFG-01 | M-1 | A/H | `F-untrusted` | 在未信任项目下启动会话；尝试 discovery/use | **零** MCP 子进程；无 HTTP 客户端会话；诊断提示未信任；内置工具仍可用 |
| CU-CFG-02 | M-1 | A | `F-bad-json` + 正常插件同代 | 加载快照 | 坏 JSON **隔离**；正常插件仍物化；会话不崩溃 |
| CU-CFG-03 | M-2 | A | `F-stdio` + `F-http` | 解析两种描述符 | stdio/HTTP 均可解析；`cwd`/`command` 逃逸插件根被拒 |
| CU-CFG-04 | M-2 | A | 非法 `cwd: ../../..` | 物化描述符 | 拒绝；诊断含路径策略；无进程 |
| CU-CFG-05 | M-3 | A | `F-collide` | 注册两套同限定名工具 | `server__tool` 稳定；**不**静默覆盖；后到者失败并诊断 |

### 2.2 Lifecycle（M-4…M-6）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-LC-01 | M-4 | A/S | `F-stdio` | start → initialize → tools/list | initialize 成功；记录 serverInfo；可列出 fixture 工具 |
| CU-LC-02 | M-4 | A | 故意卡住的 command | 启动超时 | 超时后 process group **reap**；无僵尸 |
| CU-LC-03 | M-5 | H/S | `F-stdio` 已启动 | cancel turn / 退出会话（SessionEnd） | `pgrep -P` / 进程组检查：**无残留** MCP 子进程 |
| CU-LC-04 | M-5 | H | TUI 打开后 Ctrl-C / `/quit` | 同上 | 同 CU-LC-03；SessionEnd 有界截止、幂等 |
| CU-LC-05 | M-6 | A | `F-crash`（A 崩、B 活） | 调用 A 失败后再调 B + 内置工具 | A 标记 unhealthy；B 与内置工具仍成功；失败隔离 |

### 2.3 Discovery & cache（M-7…M-8）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-DIS-01 | M-7 | A | `F-stdio` | 同 generation 两次 `search_tool` | 索引稳定；描述符不变 |
| CU-DIS-02 | M-7 | A/H | 热更新插件 MCP 工具列表后 reload | turn N 中途 reload，再开 turn N+1 | 旧 turn 仍见 gen N；新 turn 见 gen N+1 更新 |
| CU-DIS-03 | M-8 | A | 服务器返回一个坏 schema + 一个好 tool | tools/list | 坏 tool 剔除并诊断；好 tool 仍可用；整 server 不丢（除非 initialize 失败） |

### 2.4 Progressive discovery（M-9…M-11）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-PD-01 | M-9 | A/H | 默认配置 + `F-stdio` | 检查模型可见 tool 列表 / `-p` transcript | 默认 **仅** `search_tool`/`use_tool`（+ 非 MCP 内置）；**无**海量 `server__tool` |
| CU-PD-02 | M-10 | H/S | `F-stdio` 可信启用 | 输入：`用 search_tool 找 ping，再用 use_tool 调用并返回 JSON` | 能检索到 fixture 工具；`use_tool` 成功；返回固定 JSON |
| CU-PD-03 | M-10 | A | 单元/集成 | 直接调 provider | 同 CU-PD-02 的契约断言 |
| CU-PD-04 | M-11 | A/H | 配置白名单直扩少数 server | 打开会话 | 仅白名单 `server__tool` 出现在模型列表；其余仍走 search/use |

### 2.5 Safety membrane（M-12…M-14, M-20）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-SAFE-01 | M-12 | A | fixture + hook/journal 探针 | 一次成功 MCP 调用 | 轨迹顺序：PreToolUse → prepare_scoped → PolicyEngine → approval → execute → PostToolUse |
| CU-SAFE-02 | M-13 | A | 代码/API 审查 + 负测 | 试图经 `McpManager` 无 grant 直接 `tools/call` 交付模型 | **无**公共旁路 API；调用失败或不可达模型结果 |
| CU-SAFE-03 | M-14 | A/H | PreToolUse hook 改写 args | 调用 MCP 写类工具 | 改写后 **重新** schema/policy/approval；工具名不可变；改名企图被拒 |
| CU-SAFE-04 | M-20 | A | 无 `ExtensionInvoke` / 无 MCP ceiling | 尝试 `use_tool` | 与内置工具同等强制；拒绝且无执行副作用 |
| CU-SAFE-05 | M-12/M-20 | H | 需批准的 MCP 写/外网工具 | TUI 触发 approval | 未批准前不执行；拒绝后无副作用 |

### 2.6 Results & faults（M-15…M-17）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-FLT-01 | M-15 | A/S | `F-big-out` | `use_tool` 拉超大结果 | 内联截断；完整内容落 `.lato/tool-output/`；journal **无**完整大体/密钥 |
| CU-FLT-02 | M-16 | A | `F-http` 指向 link-local/私网 | 连接 | **拒绝**；稳定错误；错误文案不回显完整凭据 URL |
| CU-FLT-03 | M-16 | A | HTTP 302 redirect endpoint | 调用 | **不跟随** redirect |
| CU-FLT-04 | M-17 | A | 协议 error / RPC 超时 | 调用 | 映射稳定 `ToolError` 码；可诊断；无半包敏感体泄漏 |
| CU-FLT-05 | M-15 | A | PostToolUse `updatedMCPToolOutput`（若已接线） | hook 改写 MCP 输出 | 再经 `bound_tool_output`；审计 redact |

### 2.7 Reload & parent/child（M-18…M-19）

| Case ID | 映射 | 等级 | 前置 | 测试输入 / 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-REL-01 | M-18 | A/H | 进行中 turn 持有 gen N MCP | 中途 reload 插件 MCP | 旧 turn 仍用 gen N 连接/缓存；下一 turn 用 N+1；旧 gen 引用清零后 shutdown |
| CU-REL-02 | M-19 | A/H | `F-child` | 子会话调用父已移除的 `demo__ping` | **失败**；不能恢复父已禁用插件/已收窄工具 |
| CU-REL-03 | M-19 | A | 子会话仅收窄 server allowlist | 调用未允许 server | 拒绝；父会话不受影响 |

### 2.8 Release gate（M-21…M-22）

| Case ID | 映射 | 等级 | 前置 | 步骤 | 通过条件 |
| --- | --- | --- | --- | --- | --- |
| CU-GATE-01 | M-21 | S | 实现完成 | `LATO_SMOKE_BINARY=… cargo test --test phase6c_mcp_smoke` | stdio + streamable HTTP fixture 全绿 |
| CU-GATE-02 | M-21 | S | `cargo install --path .` | installed binary smoke | `lato --version` 正常；smoke 用安装态 binary |
| CU-GATE-03 | M-22 | A | 干净树 | `cargo fmt --check`；`cargo test --workspace`；`clippy -D warnings`；`cargo install --path .` | 全绿 |
| CU-GATE-04 | M-22 | H | docs | README MCP 段更新；release report 落盘 | 文档不再写「MCP 仅 catalog / 仍属 Phase 6C 未执行」类过期表述 |

门禁命令清单（与设计 §14 一致）：

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path .
lato --version
LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6c_mcp_smoke -- --nocapture
# 建议附加
cargo test -p lato-mcp
cargo test -p lato-extensions --test mcp_config
cargo test -p lato-tools
cargo test -p lato-agent --test mcp_runtime
```

---

## 3. Computer-use / PTY 剧本（优先手工）

> 对应产品设计 §13；实现落地后由测试在真实桌面/PTY 执行。

### Playbook P1 — stdio 端到端（映射 M-9/M-10/M-12/M-21）

1. 准备可信插件 `F-stdio`（`.mcp.json` + 本地 server 脚本）。
2. `cargo install --path .`；确认 `lato --version`。
3. PTY 启动 `lato`（或 `-p`），确认模型工具列表默认只有 `search_tool`/`use_tool`。
4. 提示：`搜索 MCP 里的 ping 工具并调用，把返回 JSON 原样打印。`
5. **Pass：** 检索命中 → `use_tool` 成功 → 固定 JSON；journal 有膜路径；退出后无残留进程。

### Playbook P2 — HTTP SSRF / no-redirect（映射 M-16）

1. 起 `F-http` loopback 正例 + 私网/redirect 负例。
2. 正例 `use_tool` 成功。
3. 指向 `169.254.0.0/16` 或 RFC1918 目标应失败。
4. 302 目标应失败且不跟随。
5. **Pass：** 正例绿、负例稳定错误、无凭据回显。

### Playbook P3 — 信任边界（映射 M-1）

1. 同配置先放 `F-untrusted`：不得拉起 MCP。
2. 标记信任并启用后：同配置可启动并 `search_tool`。
3. **Pass：** 信任前后行为对照清晰；`pgrep` 未信任阶段为零。

### Playbook P4 — SessionEnd 残留（映射 M-5）

1. 启动 stdio MCP 并完成一次调用。
2. `/quit` 或杀会话。
3. 立刻检查进程树 / `pgrep -af mcp|node|…`（按 fixture）。
4. **Pass：** 无残留；可重复开关 3 次仍干净。

### Playbook P5 — 子会话收窄（映射 M-19）

1. 父会话能 `use_tool` `demo__ping`。
2. 派生子会话（去掉该 server/tool）。
3. 子会话再调应失败；父会话仍成功。
4. **Pass：** 只收窄、不恢复。

### Playbook P6 — SenseNova 平行记录（不挡 6C）

1. 若同环境复测 SenseNova「exit 0 无产物」：只记 ticket，不纳入 6C gate。
2. 参考：`2026-09-11-bug-sensenova-exit0-no-artifact.md`。

---

## 4. 建议执行顺序（等开发落地后）

1. **A 级契约先绿：** M-1…M-3, M-7…M-8, M-13, M-16…M-17 unit/integration。
2. **膜与发现：** M-9…M-14, M-20。
3. **生命周期 / 残留：** M-4…M-6 + Playbook P4。
4. **父子 / reload：** M-18…M-19 + P5。
5. **Installed smoke + workspace gate：** M-21…M-22 + P1/P2。
6. 出 `docs/testing/reports/phase-6c-mcp-release-gate-YYYY-MM-DD.md`。

---

## 5. 开放问题对测试的影响（跟设计 §15）

| # | 问题 | 测试侧影响 |
| --- | --- | --- |
| 1 | loopback `http://` 是否允许 | P2 正例 URL scheme 以评审结论为准 |
| 2 | 子会话收窄粒度 server vs server+tool | CU-REL-02/03 fixture 字段名待定 |
| 3 | 惰性启动 vs SessionStart 预热 | CU-LC-01 计时点与「首次 discovery 才有进程」断言 |
| 4 | `use_tool` 默认 approval | CU-SAFE-05 是否强制 ask |
| 5 | `CapabilityCeiling` vs `McpCapabilityCeiling` | CU-SAFE-04 断言 API |
| 6 | inline `mcpServers` 是否完整支持 | 增 CU-CFG-inline 或标 N/A |

---

## 6. 交付物清单

- [x] 本用例草稿合入：`docs/superpowers/specs/2026-09-11-phase-6c-mcp-computer-use-cases.md`
- [x] 合入 `docs/superpowers/specs/`（及实施计划入 `plans/`）
- [x] 执行记录 / release gate report：`docs/testing/reports/phase-6c-mcp-release-gate-2026-09-11.md`
- [x] SenseNova 平行 bug 独立跟踪：`docs/superpowers/specs/2026-09-11-bug-sensenova-exit0-no-artifact.md`

---

## 7. 给开发的对接备注

- 自动化优先实现：`tests/phase6c_mcp_smoke.rs`、`lato-extensions` `mcp_config`、`lato-agent` `mcp_runtime`。
- computer-use Playbook 依赖：**可信插件安装路径、SessionEnd 可观察、approval UI/PTY**。
- 关键不变量断言建议在集成测试里硬编码路径探针（PreToolUse…PostToolUse），避免只靠自然语言 transcript。
