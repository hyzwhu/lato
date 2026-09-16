# Lato Phase 7C：AgentField 远程执行适配器产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **v1 草案，待设计评审；禁止施工** |
| 日期 | 2026-09-16 |
| 基线 | `origin/master@0611e26`（Phase 7B7 已合入） |
| 目标版本 | Phase 7C；具体发行版本待确认 |
| 依赖 | Phase 5 task/runtime、Phase 6 信任与配置、Phase 7B workflow/journal/model tool、现有 `ToolRuntime`/policy/approval membrane |
| 外部基线 | AgentField 官方文档与 REST API（2026-09-16 查阅）；本机无 `af` CLI，具体兼容版本待集成门禁锁定 |
| 后续 | AgentField shared memory、DID/VC、实时 session、harness、远程 workflow DAG UI 均另立规格 |

## 1. 决策摘要

Phase 7C v1 把 AgentField 定义为一个**可选的、出站的远程执行适配器**：Lato 主会话模型可发现被本地管理员明确允许的 AgentField capability，异步启动一次执行，并查询或取消该执行。所有调用仍先经过 Lato 的工具 schema、policy、approval、sandbox/network 和审计边界；AgentField 控制面的策略是第二道边界，不替代 Lato 授权。

v1 不把 AgentField 变成第二个 Lato turn loop，不替换本地 `SubagentCoordinator`、`WorkflowManager` 或模型 provider，也不允许远端 capability 回调本机任意工具。适配器注册为主会话专属内建工具 `agentfield`，复用现有 `ToolRuntime` 与 session journal；本地 `spawn/send/wait/cancel/inspect` 和 `workflow` 工具保持原合同。

核心选择：

1. **显式 allowlist，不做全控制面直通。** 远端 target 必须在本地配置中声明，不能由模型拼 URL 或任意 `node.function`。
2. **异步执行，不在一次 tool call 中等待长任务。** `start` 返回本地 run ID；`status` 轮询远端。
3. **本地 journal 只保存关联和有界快照。** AgentField 是远端执行记录的权威来源；Lato 不复制远端 DAG、memory 或完整输出。
4. **不自动重试非幂等 start。** 传输结果不确定时记录 `outcome_unknown`，后续只做查询/对账，禁止盲重放。
5. **断网不伪造终态。** 已知远端执行在控制面不可达时显示 `unavailable`，而非 `failed` 或 `cancelled`。

## 2. 已确认事实、合理推断与 Lato 建议

### 2.1 已确认事实

- AgentField 官方架构由 control plane 与 agent nodes 构成；control plane 负责发现、路由、执行、policy 与 audit。
- capability 使用 `node.function` 形态；官方 REST API 默认位于 `/api/v1`，JSON 请求/响应，受保护端点使用 Bearer token。
- 官方支持同步与异步执行、执行查询、控制面 discovery，以及跨 agent workflow tracing。
- Lato 已有主会话工具膜、三态 PolicyMode、一次性 grant、network/sandbox 边界、session journal、跨进程恢复、本地 subagent 与 workflow runtime。
- 当前构建环境中未安装 `af`，因此旧规格所述 `2026-03-24-v1` 不能作为本轮实机兼容性证据。

官方参考：

- <https://agentfield.ai/docs/learn/how-it-works>
- <https://www.agentfield.ai/docs/reference/sdks/rest-api>
- <https://agentfield.ai/docs/build/coordination/workflow-tracing>
- <https://github.com/Agent-Field/agentfield>

### 2.2 合理推断

- AgentField 版本之间可能出现状态名、取消语义、错误 envelope 或 discovery 字段变化；Lato 需要版本探测和严格解码，不能对未知字段静默猜测。
- 远端执行可能在 Lato 进程退出后继续；本地取消、超时或断网不等于远端已经停止。
- 远端 policy PASS 不证明 Lato 用户已授权；两端 policy 必须同时通过。

### 2.3 Lato 建议

- v1 只支持一个配置的 control plane、静态 allowlist 和四个动作 `list/start/status/cancel`。
- v1 不依赖 `af` 子进程；生产路径使用 Rust HTTP client 与版本化 adapter trait，`af` 仅可用于人工诊断。
- 第一刀先交付 client/config/fixture 合同，第二刀再注册模型工具和 journal 恢复，降低外部协议漂移风险。

## 3. 用户价值与用户故事

### 3.1 用户价值

用户可以在 Lato 对话中调用组织已经部署、治理和审计的远程 agent capability，而不必把 provider key、远端实现或控制面管理权限暴露给模型，也不必把远端 agent 重写成 Lato 本地 subagent。

### 3.2 用户故事

1. 作为管理员，我能配置 control plane 和允许的 capability 别名，模型只能看到这些别名。
2. 作为用户，我能要求 Lato 启动一个允许的远程任务，并立刻得到可追踪的本地 run ID。
3. 作为用户，我能在稍后或 session resume 后查询任务状态和有界结果。
4. 作为用户，我能请求取消任务，并区分“取消已确认”“远端已终态”“取消结果未知”。
5. 作为安全负责人，我能证明 URL、token、远端 ID、输入输出和授权没有越过既有 Lato policy 与隐私边界。

## 4. 范围

### 4.1 范围内

- 一个可选的 AgentField control-plane 配置与凭据引用。
- 静态 capability allowlist、稳定本地别名、输入 JSON Schema、输出上限与风险分类。
- `AgentFieldClient` trait、HTTP adapter、严格协议解码、版本/健康探测。
- 主会话专属 `agentfield` 工具：`list`、`start`、`status`、`cancel`。
- 与 session 绑定的 `AgentFieldManager`；每个 session 最多 32 个 retained runs、最多 4 个非终态 runs。
- 本地 journal 关联、跨进程 resume 后对账、关闭时不隐式取消远端执行。
- 稳定错误码、结构化日志、敏感字段脱敏、离线 fixture/contract tests。
- README、doctor 状态与迁移/回滚说明。

### 4.2 范围外

- 自动安装或启动 `af` / AgentField control plane。
- AgentField shared memory、vector search、DID/VC、credentials 管理、human approval UI。
- realtime audio/multimodal session、webhook listener、SSE 常驻订阅。
- `app.harness`、AForge 或由 AgentField 反向启动 Codex/Lato。
- 导入 ARD 公共目录、动态远程 registry、自动信任发现结果。
- 将远端 reasoner 映射成本地 `explorer/worker/reviewer`，或替换 `SubagentBackend`。
- 将 AgentField DAG 合并为 Lato task tree/workflow journal。
- 自动 retry、failover、多 control-plane 路由、远端结果自动写文件或执行工具。
- 新 TUI 页面；v1 只复用普通 tool call/结果展示。

## 5. 架构与所有权

```text
main-session model
        |
        v
ToolRuntime -> PolicyEngine -> approval/grant -> network obligation
        |
        v
agentfield Tool (schema + stable errors)
        |
        v
SessionAgentFieldHandle -> AgentFieldManager (session single writer)
        |                         |
        |                         +-> Session journal (mapping + bounded snapshot)
        v
AgentFieldClient trait -> HttpAgentFieldClient -> configured control plane
```

权威边界：

- Lato 对“是否允许发起调用”、输入上限、本地 run ID、journal 与 UI 投影负责。
- AgentField 对远端 capability 健康、execution ID、远端状态、远端 policy/audit 与结果负责。
- 同一远端 execution 只能绑定一个本地 run；本地 run ID 不作为远端权限凭据。
- manager 是 session 内状态单写者；HTTP future 只能通过有界消息回传，不可直接写 journal。

建议文件：

```text
crates/lato-agent/src/agentfield/
  mod.rs client.rs config.rs manager.rs journal.rs tool.rs types.rs
crates/lato-agent/tests/agentfield_contract.rs
crates/lato-agent/tests/agentfield_tool.rs
crates/lato-agent/tests/agentfield_resume.rs
```

不得在 `SessionActor` 中散落 AgentField 特判；工具注册沿用 7B7 主会话专属 extra-tool 接线。

## 6. 配置与发现合同

### 6.1 配置

建议配置形态：

```json
{
  "agentfield": {
    "enabled": true,
    "baseUrl": "https://agents.example.internal",
    "credential": "agentfield:primary",
    "capabilities": {
      "contract-review": {
        "target": "legal.review_contract",
        "description": "Review one contract and return structured findings",
        "inputSchema": {"type":"object","additionalProperties":false},
        "risk": "remote_read",
        "timeoutSeconds": 900,
        "maxOutputBytes": 65536
      }
    }
  }
}
```

约束：

- alias 匹配 `[a-z0-9][a-z0-9_-]{0,63}`，target 匹配受限 `node.function`，长度不超过 129 bytes。
- capability 最多 64 个；输入序列化后不超过 64 KiB；输出默认/硬顶 64 KiB。
- `baseUrl` 必须是预配置的绝对 URL，无 userinfo/query/fragment；生产只允许 HTTPS。回环地址可在显式 development mode 使用 HTTP。
- 禁止重定向到不同 origin；DNS 解析后应用现有 SSRF/private-network policy，连接复用不能绕过复核。
- token 只通过 Lato credential store 或环境变量引用解析；不得写入 settings、journal、tool output 或日志。
- 配置中的 description/schema 是模型可见事实源；远端 discovery 只做健康和 target 存在性校验，不能静默扩大 allowlist 或改写 schema。

### 6.2 冻结 catalog 与 TOCTOU

每个 model turn 冻结 `AgentFieldCatalogRevision`：

```text
SHA-256(canonical base origin + adapter version + sorted alias/target/schema/risk/limits)
```

`list` 返回 alias、description、risk、inputSchema 和 revision，不返回 base URL、token、原始远端 metadata。`start` 必须携带 alias 与 revision。approval 后、发送 HTTP 前再次比较 revision；不一致返回 `agentfield.catalog_changed`，grant 已消费，远端请求为零。

远端健康探测不得成为每 turn 的硬依赖：最近 30 秒成功快照可用于 list；过期且不可达时 list 标记 `available:false`，start fail closed。

## 7. 模型工具协议

唯一工具名：`agentfield`。仅注册到主会话；child/subagent、headless capability-filtered runtime 和 workflow Rhai host 默认不可见。

### 7.1 `list`

输入：

```json
{"action":"list"}
```

输出最多 64 条、64 KiB：

```json
{"revision":"sha256:...","capabilities":[{"name":"contract-review","description":"...","risk":"remote_read","available":true}],"truncated":false}
```

### 7.2 `start`

输入：

```json
{"action":"start","name":"contract-review","revision":"sha256:...","input":{}}
```

处理顺序：schema → local allowlist → Lato policy/approval → revision recheck → reserve local slot/journal intent → exactly one HTTP attempt → bind remote execution ID → return。

成功：

```json
{"runId":"afrun_...","name":"contract-review","status":"queued","createdAt":"..."}
```

`start` 不等待远端完成。若响应在确认 execution ID 前中断，记录 `outcome_unknown`，返回同码；禁止自动重发。实现门禁必须证明可用 correlation/idempotency 字段能供后续查询；若锁定 AgentField 版本不支持对账，ambiguous run 只能人工在控制面核对。

### 7.3 `status`

输入为 `runId`，或省略以列出最近 20 条 owned runs。只允许当前 session journal 拥有的 run；未知与他会话 ID 均返回同一个 `agentfield.not_found`。

规范状态：

| Lato 状态 | 含义 |
| --- | --- |
| `queued` | 远端已接受但未运行 |
| `running` | 远端执行中 |
| `paused` | 明确等待 human approval；仅在锁定版本可证明时映射 |
| `completed` | 远端成功终态 |
| `failed` | 远端失败终态或协议结果不可解码 |
| `cancelled` | 远端明确确认取消 |
| `outcome_unknown` | start 结果不确定，尚未对账 |
| `unavailable` | 控制面暂时不可达；不是终态 |

未知远端状态返回 `agentfield.remote_protocol`，保留最后已知状态，不猜测映射。终态结果必须是严格 JSON、64 KiB 内；超限返回 metadata + `truncated:true`，完整内容不落 journal。

### 7.4 `cancel`

只接受 owned run ID。结果分为：`cancelled`、`already_terminal`、`cancel_requested`、`unavailable`。网络超时不能报告 cancelled；再次 cancel 必须幂等，不重复制造远端 execution。

## 8. Policy、安全与隐私模型

### 8.1 Policy

`list/status` 为只读，但仍受 session ownership 与输出边界约束。`start/cancel` 为有副作用操作：

- Ask：展示 alias、target、risk、输入字段名、序列化字节数；不显示 token/secret value。拒批返回 `policy.approval_denied`，远端请求和本地 active run 均为零。
- Auto/Always：只在现有 policy 明确允许 `network + agentfield:<alias>` 时签发一次性 grant。
- Deny 是 policy decision，不是第四种 PolicyMode。
- grant 绑定 session、turn、call、action、alias、catalog revision、input digest；任何字段变化都不可复用。

AgentField 远端 PASS 不能升级 Lato 权限；远端返回的 URL、命令、文件路径或 tool request 一律作为不可信数据展示，不自动执行。

### 8.2 数据最小化

- 默认只发送 `input`，不发送完整 transcript、system prompt、workspace path、journal、provider credential 或 Lato tool catalog。
- 用户若需附加上下文，必须显式放进 schema 字段并经过 approval 摘要。
- 日志记录 alias、本地/远端 execution ID 的哈希、状态、latency、字节数和稳定错误码；不记录 token 或原始输入输出。
- journal 只保存有界结果摘要；完整远端结果按需查询，不长期复制。

### 8.3 网络与凭据

- 使用现有 HTTP transport 的 timeout、proxy、TLS 与 no-live-network 测试边界。
- Bearer token 仅在最终请求构造时注入，Debug/Display 必须脱敏。
- 响应 Content-Type 必须为 JSON；拒绝 HTML 登录页、压缩炸弹、超长 header/body 和跨 origin redirect。
- 401/403 映射 `agentfield.unauthorized`，不得回显服务端 body 中可能包含的凭据线索。

## 9. 运行时、持久化与恢复边界

### 9.1 Journal 记录

新增版本化事件（名称可在实现评审时调整，但语义不得变）：

- `AgentFieldRunIntentRecorded`
- `AgentFieldExecutionBound`
- `AgentFieldStatusObserved`
- `AgentFieldCancelRequested`
- `AgentFieldRunTerminal`

最小字段：schema version、local run ID、session ID、alias、catalog revision、input digest、remote execution ID（可选）、normalized status、timestamps、bounded result summary、last error code。禁止 token、base URL userinfo、原始输入、完整结果。

append 必须沿用 session journal 单一 sequence owner；不得再引入 host/manager 双写。每次状态转换单调，终态不可被晚到状态覆盖。

### 9.2 跨进程 resume

`session/resume` 重放映射：

1. 已终态 run 直接恢复本地快照，不主动联网。
2. 有 remote execution ID 的非终态 run 在首次 `status` 或后台一次性 reconcile 时查询远端；失败显示 `unavailable`，不改终态。
3. `outcome_unknown` 按 correlation 能力对账；不能证明唯一远端 execution 时禁止重试 start。
4. session close/进程退出只关闭本地 client，不隐式 cancel 远端。用户显式 cancel 才发送取消请求。
5. 配置删除或 credential 缺失时保留历史 run，但查询返回 `agentfield.unconfigured`，不丢 journal。

### 9.3 Retention

- 每 session 最多 32 条 run；只在新增时淘汰最老终态摘要，非终态不可淘汰。
- 结果摘要每条最大 8 KiB；完整 tool response 总计最大 64 KiB。
- v1 不建立全局数据库，不跨 session 搜索，不复制 AgentField workflow DAG。

## 10. 兼容、迁移、降级与回滚

### 10.1 兼容

- `agentfield.enabled` 缺省 false；未配置时工具不注册，现有 CLI/TUI/headless/ACP 输出不变。
- 本地 task/workflow 工具 schema 与行为零变化。
- journal 新事件使用未知事件安全跳过/版本门禁；旧 session 无 AgentField 事件可正常 resume。
- Linux/macOS/Windows 均支持；系统证书或 proxy 差异必须进入矩阵。

### 10.2 迁移

- 不自动迁移 secret；管理员显式写 credential store。
- 首次启用先通过 `lato doctor` 校验配置、TLS、凭据存在性、版本兼容与 allowlist target 存在性。
- 配置变更产生新 catalog revision；已有 run 继续按 journal 中 alias/remote ID 查询，但不能用旧 revision 启动新 run。

### 10.3 降级与回滚

- 设置 `enabled:false` 或构建时关闭 adapter 即可撤销工具注册；不会影响本地 task/workflow。
- 回滚不得删除 journal 事件或远端 execution；旧二进制应忽略未知 7C 事件并提示历史能力不可用。
- control plane 故障时只禁用新 start；历史 status 返回 unavailable，用户仍可在 AgentField 控制面处理。
- 若锁定版本缺少可验证 async status/cancel/correlation 合同，7C2 不开工；只交付 7C1 client/doctor，不以同步调用降级冒充完整 v1。

## 11. 稳定错误码

| 错误码 | Retry | 说明 |
| --- | --- | --- |
| `agentfield.invalid_arguments` | Never | 工具 schema/本地 input schema 不合法 |
| `agentfield.not_found` | Never | alias/run 不存在或非当前 session 所有 |
| `agentfield.unconfigured` | After config | 未启用、credential 缺失或历史配置消失 |
| `agentfield.catalog_changed` | Relist | approval 后 revision 变化；grant 已消费、远端零请求 |
| `agentfield.unavailable` | Backoff | DNS/TLS/connect/5xx/health 暂时失败 |
| `agentfield.unauthorized` | After credential fix | 401/403 |
| `agentfield.remote_denied` | Never | AgentField policy 明确拒绝 |
| `agentfield.remote_protocol` | After upgrade/config | 版本、状态或 JSON envelope 不兼容 |
| `agentfield.limit_exceeded` | After terminal/retention | 本地 4 active / 32 retained / 输入输出上限 |
| `agentfield.outcome_unknown` | Reconcile only | start 是否到达远端不可证明，禁止重试 |
| `agentfield.output_too_large` | Never | 单结果无法安全投影 |

所有 pre-policy 参数拒绝必须返回 `agentfield.invalid_arguments`；不能泄漏通用内部错误或远端 body。

## 12. 必要测试

### 12.1 纯离线合同测试

使用受控 fake server/fixture，不访问公网：

1. 配置解析、alias/target/URL/schema/上限和 secret 引用。
2. HTTPS/localhost 规则、redirect、SSRF、DNS rebinding、TLS/timeout/body cap。
3. Bearer token 注入且 Debug/log/journal/tool output 均无 secret。
4. allowlist 与远端 discovery 交集；远端新增 capability 不可见。
5. `list/start/status/cancel` schema 组合与稳定错误码。
6. Ask 拒批、Auto/Always、remote deny；Lato deny 时零 HTTP。
7. revision TOCTOU：approval 后配置变化 → grant consumed、零 HTTP/零 active run。
8. start 成功、明确失败、响应前断线三态；断线不自动重试。
9. 4-active 真并发原子上限；第 5 个零远端请求。
10. unknown/foreign run 同 not_found；晚到状态不能覆盖终态。
11. cancel 四结果与重复 cancel 幂等；超时不可伪报 cancelled。
12. 输出 64 KiB、恶意 JSON、HTML body、压缩炸弹、未知 status/version。
13. journal crash points：intent 前/后、bind 前/后、terminal append 前/后。
14. 跨进程 resume：completed 离线恢复、running 对账、断网 unavailable、outcome_unknown 不重放。
15. session close 后晚到调用 fail closed；远端 run 不被隐式 cancel。
16. registration：主会话唯一可见；subagent/headless-filtered/workflow host 不可见。
17. enabled=false 与回滚：所有既有 task/workflow/ACP/TUI golden 不变。

### 12.2 锁定版本集成测试

实现前必须记录 AgentField server/CLI commit 或 release、OpenAPI/fixture hash，并用隔离控制面执行：

- health/discovery/async start/status/cancel 的真实 round trip；
- 401/403、remote policy deny、404 execution、5xx 与 restart；
- Lato 进程在远端 running 时硬退出，resume 后对账同一 execution；
- 同 correlation 的 ambiguous start 不产生可观察重复执行，或明确证明只能进入人工对账；
- AgentField 升级一版后的兼容测试，未知合同必须 fail closed。

LIVE 测试只使用无敏感数据的 fixture capability，不进入普通 CI；普通 CI 必须通过 `no-live-network`。

### 12.3 仓库门禁

- focused tests 全部通过；
- `cargo fmt --all -- --check`；
- `cargo clippy --workspace --all-targets -- -D warnings`；
- `CARGO_PROFILE_TEST_DEBUG=0 cargo test --workspace -j 2`；
- `cargo install --path . --force`；
- `lato doctor --json`、enabled/disabled CLI smoke；
- GitHub Ubuntu/macOS/Windows/lint/no-live-network 5/5 SUCCESS。

## 13. 量化验收标准

| ID | 验收标准 |
| --- | --- |
| AC-01 | 未配置/disabled 时 `agentfield` 工具 0 注册，现有 task/workflow schema 与 golden 零变化 |
| AC-02 | 模型仅能看到本地 allowlist；64 个上限、revision 稳定，URL/token/远端原始 metadata 零泄漏 |
| AC-03 | `start` 在远端接受后 2 秒内返回本地 run ID（fixture p95）；不等待远端完成；输入 ≤64 KiB |
| AC-04 | Ask/Auto/Always 与明确 deny 全覆盖；deny/拒批/revision mismatch 均远端请求 0，grant 一次性消费 |
| AC-05 | 模糊网络失败请求自动重试次数为 0，记录 `outcome_unknown`；恢复后不会创建第二 execution |
| AC-06 | 每 session 非终态上限 4、retained 上限 32；两个并发 start 竞争最后槽位时 active 永不超过 4 |
| AC-07 | status 只访问 owned run；foreign/unknown 不可区分；未知远端状态 fail closed，不覆盖最后已知/终态 |
| AC-08 | cancel 超时不报告成功；重复 cancel 幂等；session close 不隐式取消远端 execution |
| AC-09 | running 时硬退出并 resume，能绑定同一 remote execution；断网显示 unavailable，completed 可离线恢复 |
| AC-10 | secret 扫描对日志、journal、tool output、错误、Debug 快照为 0 命中；恶意/超限输出安全拒绝或截断 |
| AC-11 | Linux/macOS/Windows focused tests、全仓 tests、fmt、clippy、install、doctor 与 no-live-network 全通过 |
| AC-12 | README 明确双 policy、远端继续运行风险、手工对账与回滚；锁定 AgentField 版本/fixture hash 可复现 |

## 14. 风险与处置

| 风险 | 严重度 | 处置 |
| --- | --- | --- |
| 外部 API 漂移 | P1 | 版本探测、strict decoder、pinned fixture、未知 fail closed |
| start 超时造成重复远端副作用 | P1 | 单次发送、correlation、outcome_unknown、禁止自动 retry |
| 双 policy 语义被误解 | P1 | Lato 先授权，AgentField 再授权；两者都通过才执行 |
| Lato 退出但远端继续运行 | P1 | 明示合同、journal 绑定、resume 对账、显式 cancel |
| token/输入/结果泄漏 | P1 | credential store、最小输入、redaction、bounded journal/output |
| SSRF/redirect/DNS rebinding | P1 | 固定 base origin、现有 network policy、无跨 origin redirect |
| 与本地 task/workflow 状态混淆 | P2 | 独立 `afrun_` ID、独立工具与 journal event，不伪装 TaskId/WorkflowRunId |
| 控制面不可用拖慢 turn | P2 | 短探测、30s cache、start fail fast、status unavailable |
| 跨平台证书/proxy 差异 | P2 | 三平台 fixture TLS tests 与 doctor 诊断 |

## 15. 实施拆分建议

### 7C1：协议与安全基础（可独立开发/验收）

- 配置、credential reference、URL/SSRF 验证；
- `AgentFieldClient` trait、strict types、fake server、版本/health/discovery；
- doctor；不注册模型工具、不发起生产执行。

完成门槛：锁定一个 AgentField release/commit 与 fixture hash，证明 async start/status/cancel/correlation 合同。若失败，7C 停在此阶段。

### 7C2：模型工具与运行时（依赖 7C1）

- manager、`agentfield` tool、policy/approval、并发/retention、稳定错误码；
- 主会话注册与 UI/ACP 普通 tool 投影；
- 不含跨进程恢复。

### 7C3：journal 与跨进程恢复（依赖 7C2）

- 版本化事件、crash consistency、resume/reconcile、outcome_unknown 人工对账说明；
- 完整三平台与 LIVE gate。

三刀必须各自独立 PR 和验收；不得把 AgentField shared memory、harness 或 DAG UI 混入。

## 16. 完成定义

Phase 7C v1 只有在以下全部满足后才完成：

1. 规格评审冻结，并锁定外部 AgentField 版本与实际合同。
2. 7C1/7C2/7C3 分刀实现和独立验收全部 PASS。
3. P0/P1 为零；P2/P3 有明确处置。
4. AC-01～AC-12 全部有实际执行证据。
5. 不降低本地 task/workflow、policy、journal、CI 和 no-live-network 基线。
6. migration、doctor、运行风险、人工对账与 rollback 文档齐全。

## 17. 评审待确认项

以下问题会实质改变施工合同，规格冻结前必须由评审根据锁定 AgentField 版本回答：

1. async start 的正式 endpoint、execution status envelope、cancel 结果与 correlation/idempotency 支持是否满足 AC-05；若不满足，采用何种唯一对账键。
2. discovery 是否提供稳定的 target schema/version digest；v1 默认仍以本地 schema 为权威，不允许远端动态扩大输入面。

除这两项外，产品范围与安全边界按本 v1 草案执行。
