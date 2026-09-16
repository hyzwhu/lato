# Lato Phase 7C：AgentField 远程执行适配器产品设计

| 字段 | 值 |
| --- | --- |
| 状态 | **v1.1 修订稿，待设计评审；禁止施工** |
| 日期 | 2026-09-16 |
| 基线 | `origin/master@0611e26`（Phase 7B7 已合入） |
| 目标版本 | Phase 7C；具体发行版本待确认 |
| 依赖 | Phase 5 task/runtime、Phase 6 信任与配置、Phase 7B workflow/journal/model tool、现有 `ToolRuntime`/policy/approval membrane；7C3 另硬依赖 WIN-26 合入后的 master SHA |
| 外部基线 | AgentField `v0.1.138` / `0aba9d6de1ef2c473070fc329ac7ac63e5d096b9`；脱敏 fixture SHA-256 `1bac46c4ce5c20d165bb88128a68ae056a70c4aabea3310b3e229210e094736a` |
| 后续 | AgentField shared memory、DID/VC、实时 session、harness、远程 workflow DAG UI 均另立规格 |

## 1. 决策摘要

Phase 7C v1 把 AgentField 定义为一个**可选的、出站的远程执行适配器**：Lato 主会话模型可发现被本地管理员明确允许的 AgentField capability，异步启动一次执行，并查询或取消该执行。所有调用仍先经过 Lato 的工具 schema、policy、approval、sandbox/network 和审计边界；AgentField 控制面的策略是第二道边界，不替代 Lato 授权。

v1 不把 AgentField 变成第二个 Lato turn loop，不替换本地 `SubagentCoordinator`、`WorkflowManager` 或模型 provider，也不允许远端 capability 回调本机任意工具。适配器注册为主会话专属内建工具 `agentfield`，复用现有 `ToolRuntime` 与 session journal；本地 `spawn/send/wait/cancel/inspect` 和 `workflow` 工具保持原合同。

核心选择：

1. **显式 allowlist，不做全控制面直通。** 远端 target 必须在本地配置中声明，不能由模型拼 URL 或任意 `node.function`。
2. **异步执行，不在一次 tool call 中等待长任务。** `start` 返回本地 run ID；`status` 轮询远端。
3. **本地 journal 只保存关联和有界快照。** AgentField 是远端执行记录的权威来源；Lato 不复制远端 DAG、memory 或完整输出。
4. **不自动重试非幂等 start。** 锁定版本不接受 idempotency key。传输结果不确定且未收到 execution ID 时永久记录 `outcome_unknown`；Lato 不自动查询、对账或重放，只提供人工控制面核对指引。
5. **断网不伪造终态。** 已知远端执行在控制面不可达时显示 `unavailable`，而非 `failed` 或 `cancelled`。
6. **单一静态 policy。** 现有 `ToolRuntime` metadata 是工具级而非 action 级；`list/start/status/cancel` 四个 action 全部冻结为 `external_mutation`，不在 `invoke` 内旁路或降级 policy。

## 2. 已确认事实、合理推断与 Lato 建议

### 2.1 已确认事实

- AgentField 官方架构由 control plane 与 agent nodes 构成；control plane 负责发现、路由、执行、policy 与 audit。
- capability 使用 `node.function` 形态；官方 REST API 默认位于 `/api/v1`，JSON 请求/响应，受保护端点使用 Bearer token。
- 锁定版 `v0.1.138` 的 async start 为 `POST /api/v1/execute/async/{target}`，成功返回 HTTP 202 与 `execution_id/run_id/status/target/type/created_at`；status 为 `GET /api/v1/executions/{execution_id}`，cancel 为 `POST /api/v1/executions/{execution_id}/cancel`。
- 锁定版 execute 路由**不接受 idempotency key**，官方明确警告重试可能创建另一 execution。
- discovery 返回 capability metadata 与可选 schema，但没有稳定 schema/version digest。
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

- v1 只支持一个配置的 control plane、静态 allowlist 和四个统一按 `external_mutation` 授权的动作 `list/start/status/cancel`。
- v1 不依赖 `af` 子进程；生产路径使用 Rust HTTP client 与版本化 adapter trait，`af` 仅可用于人工诊断。
- 第一刀先交付 client/config/fixture 合同，第二刀再注册模型工具和 journal 恢复，降低外部协议漂移风险。

## 3. 用户价值与用户故事

### 3.1 用户价值

用户可以在 Lato 对话中调用组织已经部署、治理和审计的远程 agent capability，而不必把 provider key、远端实现或控制面管理权限暴露给模型，也不必把远端 agent 重写成 Lato 本地 subagent。

### 3.2 用户故事

1. 作为管理员，我能配置 control plane 和允许的 capability 别名，模型只能看到这些别名。
2. 作为用户，我能要求 Lato 启动一个允许的远程任务，并立刻得到可追踪的本地 run ID。
3. 作为用户，我能在收到 remote execution ID 后，于稍后或 session resume 后查询任务状态和有界结果；未收到 ID 的 `outcome_unknown` 只能人工核对。
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
- 自动 retry、自动 ambiguous-run 对账、failover、多 control-plane 路由、远端结果自动写文件或执行工具。
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

- alias 匹配 `[a-z0-9][a-z0-9_-]{0,63}`。配置 target 不是自由字符串：只能由经验证的 discovery `agent_id` 与 reasoner `id` 派生。
- capability 最多 64 个；输入序列化后不超过 64 KiB；输出默认/硬顶 64 KiB。
- `baseUrl` 必须是预配置的绝对 URL，无 userinfo/query/fragment；生产只允许 HTTPS。回环地址可在显式 development mode 使用 HTTP。
- 禁止重定向到不同 origin；DNS 解析后应用现有 SSRF/private-network policy，连接复用不能绕过复核。
- token 只通过 Lato credential store 或环境变量引用解析；不得写入 settings、journal、tool output 或日志。
- 配置中的 description/schema 是模型可见事实源；远端 discovery 只做健康和 target 存在性校验，不能静默扩大 allowlist 或改写 schema。

### 6.2 冻结 catalog 与 TOCTOU

每个 model turn 冻结 `AgentFieldCatalogRevision`。canonical bytes 使用 UTF-8 JSON、对象 key 递归字典序、数组按 alias 字典序、整数十进制、无空白；`inputSchema` 先按同一 JSON canonicalization 处理：

```text
SHA-256({"adapter":"agentfield-v0.1.138","origin":"<scheme://lowercase-host:effective-port>","capabilities":[{"alias", "target", "description", "inputSchema", "risk", "timeoutSeconds", "maxOutputBytes"}...]})
```

`list` 返回 alias、description、risk、inputSchema 和 revision，不返回 base URL、token、原始远端 metadata。`start` 必须携带 alias 与 revision。approval 后、发送 HTTP 前再次比较 revision；不一致返回 `agentfield.catalog_changed`，grant 已消费，远端请求为零。

锁定版存在必须显式适配的上游不一致：discovery 生成 `agent_id:reasoner_id`，而 execute handler 只接受 `agent_id.reasoner_id`。唯一允许的转换合同如下：

1. 仅把 discovery 的 `agent_id` 与 reasoner `id` 当作原子字段；二者分别严格匹配 ASCII `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`。
2. 原子字段中的 `.`、`:`、`/`、`%`、非 ASCII 与 Unicode 混淆字符一律拒绝；不做 percent decode、Unicode normalization 或分隔符替换。
3. discovery 的 `invocation_target` 必须逐字等于 `agent_id + ":" + reasoner_id`，否则 fail closed。
4. Lato 只从已验证原子构造 execute target `agent_id + "." + reasoner_id`；`invocation_target` 永不直接拼入 URL。
5. 本地 allowlist target 必须等于上述派生 execute target；重复或不一致均返回 `agentfield.remote_protocol`。

正例：`legal-agent`、`review_contract`、`legal-agent:review_contract` 派生为 `legal-agent.review_contract`。反例包括任一原子含 `.`/`:`/`/`/`%`/非 ASCII、colon target 与两原子不一致、额外分隔符或编码分隔符。

远端最小兼容检查还要求：版本 endpoint 可识别；discovery entry 与 reasoner envelope 的锁定版必现字段类型正确。远端 description/schema/examples 均不进入 revision、不覆盖本地字段、不扩大输入面。字段缺失、类型错误、重复 target、版本不兼容或 target 不一致均 fail closed 为 `agentfield.remote_protocol`。

远端健康探测不得成为每 turn 的硬依赖：最近 30 秒成功快照可用于 list；过期且不可达时 list 标记 `available:false`，start fail closed。兼容 fixture 位于 `docs/superpowers/fixtures/agentfield-v0.1.138-contract.json`。它是从 pinned source handler/type 提炼并脱敏的 contract fixture，**不是实际隔离服务 LIVE round-trip 证据**；其 envelope 逐字段标注 required/optional/ignored-but-type-checked。SHA-256 为 `1bac46c4ce5c20d165bb88128a68ae056a70c4aabea3310b3e229210e094736a`。复现：

```bash
sha256sum docs/superpowers/fixtures/agentfield-v0.1.138-contract.json
gh release view v0.1.138 --repo Agent-Field/agentfield
gh api repos/Agent-Field/agentfield/commits/v0.1.138 --jq .sha
```

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

`start` 不等待远端完成。`v0.1.138` 无 idempotency key：若 Lato 未完整校验 HTTP 202 body 并持久化非空 execution ID，记录**永久** `outcome_unknown`，返回稳定错误并禁止自动重发、status 查询、resume reconcile 或按 input/time/target 猜测匹配。用户文案必须说明“远端可能已启动；请在 AgentField 控制面按时间、target 与审计记录人工核对；不要再次 start，除非接受重复执行风险”。只有收到并绑定 execution ID 的 run 才进入自动 status/resume。

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
| `outcome_unknown` | start 结果不确定且无 remote execution ID；永久本地终态，仅人工核对 |
| `unavailable` | 控制面暂时不可达；不是终态 |

未知远端状态返回 `agentfield.remote_protocol`，保留最后已知状态，不猜测映射。终态结果必须是严格 JSON、64 KiB 内；超限返回 metadata + `truncated:true`，完整内容不落 journal。

### 7.4 `cancel`

只接受 owned run ID。结果分为：`cancelled`、`already_terminal`、`cancel_requested`、`unavailable`。网络超时不能报告 cancelled；再次 cancel 必须幂等，不重复制造远端 execution。

## 8. Policy、安全与隐私模型

### 8.1 Policy

现有 `ToolRuntime` 的 capability、side-effect 与 sandbox metadata 是工具级静态 descriptor，不能根据 `action` 改变。v1.1 不扩展 runtime，唯一 `agentfield` 工具四个 action 全部声明：`SideEffect::ExternalMutation`、network capability、现有 network sandbox obligation、非幂等。即使 `list/status` 在远端语义上是读取，也必须经过同一 external-mutation policy/approval；这是保守授权，不得在 `invoke` 内自行降级为只读。

- Ask：展示 action、alias/owned run、target、risk、输入字段名、序列化字节数；不显示 token/secret value。拒批返回 `policy.approval_denied`，远端请求和本地 active run 均为零。
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

7C3 在 WIN-26 合入前禁止施工。当前已验收 head 为 `17b18f14dfdd32c241a4d2d73cc30f2a8c34eb07`，但 PR #12 尚未合入；7C3 的实际基线必须是“包含该 head 的 `origin/master` merge SHA”，并在开工前以 v1.2 规格勘误写入确切 SHA。append 必须复用 WIN-26 验收后的 SessionLoop 单一 sequence owner；不得让 host、manager 或 HTTP future 直接 append。每次状态转换单调，终态不可被晚到状态覆盖。

### 9.2 跨进程 resume

`session/resume` 重放映射：

1. 已终态 run 直接恢复本地快照，不主动联网。
2. 有 remote execution ID 的非终态 run 在首次 `status` 或后台一次性 reconcile 时查询远端；失败显示 `unavailable`，不改终态。
3. 无 remote execution ID 的 `outcome_unknown` 不自动对账、不查询、不 reconcile，resume 后仍为同一永久本地终态；只呈现人工控制面核对指引。
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
- 当前 `JournalRecord` 是无 `other/unknown` variant 的 serde tagged enum；旧 reader 遇到 7C 新事件会反序列化失败并 fail closed，不能安全跳过。7C3 必须提高 journal schema version并加入显式 reader-version 门禁；旧 session 无 7C 事件仍可正常 resume。
- Linux/macOS/Windows 均支持；系统证书或 proxy 差异必须进入矩阵。

### 10.2 迁移

- 不自动迁移 secret；管理员显式写 credential store。
- 首次启用先通过 `lato doctor` 校验配置、TLS、凭据存在性、版本兼容与 allowlist target 存在性。
- 配置变更产生新 catalog revision；已有 run 继续按 journal 中 alias/remote ID 查询，但不能用旧 revision 启动新 run。

### 10.3 降级与回滚

- 设置 `enabled:false` 或构建时关闭 adapter 即可撤销工具注册；不会影响本地 task/workflow。
- 回滚不得删除 journal 事件或远端 execution。写入首条 7C journal 事件后，旧二进制无法读取该 session；因此代码回滚只支持“升级后尚未产生 7C 事件”的 session。已使用 7C 的 session 必须保留新 reader，或先通过另立并验收的版本化迁移导出；禁止直接降级二进制。
- control plane 故障时只禁用新 start；历史 status 返回 unavailable，用户仍可在 AgentField 控制面处理。
- v1.1 已据 `v0.1.138` 冻结无幂等键降级合同；实现不得虚构 correlation 对账或以同步调用冒充 async v1。

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
| `agentfield.outcome_unknown` | Manual only | 未收到 execution ID；永久本地终态，禁止重试/自动查询/对账 |
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
6. 四个 action 共用 static external-mutation descriptor；Ask 拒批、Auto/Always、remote deny；Lato deny 时零 HTTP，且不存在 invoke 内 action-aware 绕过。
7. revision TOCTOU：approval 后配置变化 → grant consumed、零 HTTP/零 active run。
8. start 成功、明确失败、响应前断线三态；断线不自动重试。
9. 4-active 真并发原子上限；第 5 个零远端请求。
10. unknown/foreign run 同 not_found；晚到状态不能覆盖终态。
11. cancel 四结果与重复 cancel 幂等；超时不可伪报 cancelled。
12. 输出 64 KiB、恶意 JSON、HTML body、压缩炸弹、未知 status/version。
13. journal crash points：intent 前/后、bind 前/后、terminal append 前/后。
14. 跨进程 resume：completed 离线恢复、有 execution ID 的 running 对账、断网 unavailable、无 ID 的 outcome_unknown 永久不查询/不重放。
15. session close 后晚到调用 fail closed；远端 run 不被隐式 cancel。
16. registration：主会话唯一可见；subagent/headless-filtered/workflow host 不可见。
17. enabled=false 与回滚：所有既有 task/workflow/ACP/TUI golden 不变。
18. target 正反例：colon discovery target 只用于一致性校验；由合法原子派生 dot execute target；`.`/`:`/`/`/`%`/非 ASCII、编码分隔符、字段不一致全部在发 HTTP 前拒绝。
19. pinned async/status/cancel/discovery 完整成功 envelope：缺少必现字段、required/optional 字段类型错误均 fail closed；未知字段仅忽略。

### 12.2 锁定版本集成测试

实现锁定 AgentField `v0.1.138` / `0aba9d6de1ef2c473070fc329ac7ac63e5d096b9` 与上述 fixture/hash，并用隔离控制面执行：

- health/discovery/async start/status/cancel 的真实 round trip；
- 验证 fixture 只是 source-derived contract；LIVE 结果另存测试证据，不反向改写 fixture；
- 401/403、remote policy deny、404 execution、5xx 与 restart；
- Lato 进程在远端 running 时硬退出，resume 后对账同一 execution；
- response-before-ID 断线后 Lato 自动重试/查询次数均为 0，restart/resume 后仍仅呈现人工核对指引；
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
| AC-04 | 四个 action 共享 static external-mutation descriptor；Ask/Auto/Always 与明确 deny 全覆盖；deny/拒批/revision mismatch 均远端请求 0，grant 一次性消费 |
| AC-05 | 模糊网络失败的自动重试、自动查询、自动对账次数均为 0；无 execution ID 时永久 `outcome_unknown`，resume 后仍只给人工核对指引 |
| AC-06 | 每 session 非终态上限 4、retained 上限 32；两个并发 start 竞争最后槽位时 active 永不超过 4 |
| AC-07 | status 只访问 owned run；foreign/unknown 不可区分；未知远端状态 fail closed，不覆盖最后已知/终态 |
| AC-08 | cancel 超时不报告成功；重复 cancel 幂等；session close 不隐式取消远端 execution |
| AC-09 | running 时硬退出并 resume，能绑定同一 remote execution；断网显示 unavailable，completed 可离线恢复 |
| AC-10 | secret 扫描对日志、journal、tool output、错误、Debug 快照为 0 命中；恶意/超限输出安全拒绝或截断 |
| AC-11 | Linux/macOS/Windows focused tests、全仓 tests、fmt、clippy、install、doctor 与 no-live-network 全通过 |
| AC-12 | README 明确双 policy、远端继续运行风险、手工对账与回滚；锁定 AgentField 版本/fixture hash 可复现 |
| AC-13 | discovery 原子严格 ASCII 校验；colon invocation target 校验后派生 dot execute target，所有分隔符逃逸反例远端请求为 0 |
| AC-14 | async/status/cancel/discovery 的 pinned 完整 envelope 与 required/optional/type 分类可复现；fixture 不冒充 LIVE 证据 |

## 14. 风险与处置

| 风险 | 严重度 | 处置 |
| --- | --- | --- |
| 外部 API 漂移 | P1 | 版本探测、strict decoder、pinned fixture、未知 fail closed |
| start 超时造成重复远端副作用 | P1 | 单次发送、永久 outcome_unknown、仅人工核对、禁止自动 retry/query/reconcile |
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

完成门槛：复现锁定的 `v0.1.138` commit 与 fixture hash，证明 async start/status/cancel/discovery 完整必现 envelope 与字段分类；通过 colon discovery → dot execute 的正反合同测试；明确验证 idempotency key 不受支持。

### 7C2：模型工具与运行时（依赖 7C1）

- manager、`agentfield` tool、policy/approval、并发/retention、稳定错误码；
- 主会话注册与 UI/ACP 普通 tool 投影；
- 不含跨进程恢复。

### 7C3：journal 与跨进程恢复（依赖 7C2 + WIN-26）

- 开工前把 PR #12/WIN-26 合入后的真实 `origin/master` SHA 写回规格；
- 版本化事件、reader-version fail-closed、crash consistency、仅有 execution ID 的 resume/reconcile、outcome_unknown 人工核对说明；
- 完整三平台与 LIVE gate。

三刀必须各自独立 PR 和验收；不得把 AgentField shared memory、harness 或 DAG UI 混入。

## 16. 完成定义

Phase 7C v1 只有在以下全部满足后才完成：

1. 规格评审冻结，并锁定外部 AgentField 版本与实际合同。
2. 7C1/7C2/7C3 分刀实现和独立验收全部 PASS。
3. P0/P1 为零；P2/P3 有明确处置。
4. AC-01～AC-14 全部有实际执行证据。
5. 不降低本地 task/workflow、policy、journal、CI 和 no-live-network 基线。
6. migration、doctor、运行风险、人工对账与 rollback 文档齐全。

## 17. v1.1 审查修订记录与冻结项

| 审查项 | v1.1 处理 | 修改位置 |
| --- | --- | --- |
| P0 ambiguous start | 锁定 `v0.1.138` 无 idempotency key；无 execution ID 永久 outcome_unknown，仅人工核对 | §1、§2.1、§7.2、§9.2、§11～§15 |
| P0 action policy | 不做 action-aware runtime；四 action 统一 static external-mutation | §1、§2.3、§8.1、测试与 AC-04 |
| P1 discovery digest | 冻结 canonical local revision、远端最小字段、strict fail-closed、fixture/hash | §6.2、§12.2 |
| P1 journal/rollback | 7C3 硬依赖 WIN-26 合入 SHA；旧 reader 对未知 enum fail closed，禁止不安全二进制降级 | §9、§10、§15 |
| 二审 P0 target 分隔符 | 冻结 discovery colon 校验、严格原子验证与 execute dot 派生；原始 invocation target 禁止进 URL | §6.2、§12、AC-13 |
| 二审 P1 envelope fixture | async/status/cancel/discovery 记录完整实际 envelope 与字段分类；明确 source-derived、非 LIVE | fixture、§6.2、§12、AC-14 |

仍有一个实施前机械门禁：PR #12 当前 head 为 `736452f866ec66bbf9f208edb0c2df93a3ba67af`，但尚未合入；当前 `origin/master` 仍为 `0611e26`。其 merge SHA 出现后必须以规格勘误替换“待写回”文字；在此之前 7C3 不得开工。这不改变产品选择，只冻结实际代码基线。

## 18. PR #13 CI 失败处置记录

首版文档 head `71e332f` 的 GitHub run `35086976901` 仅 Ubuntu 失败：`crates/lato-mcp/tests/lifecycle.rs:204` 的 `stdio_cancel_mid_call_reaps_child` 在启动 fixture child 时得到 `Err(Spawn)`；同 job 其余 lifecycle 用例通过。

归因证据：

- PR #13 相对 `origin/master@0611e26` 只增加设计文档和脱敏 fixture，对 `crates/lato-mcp`、workspace manifests 与 `Cargo.lock` 的 diff 为零。
- 同一分支本地执行 `cargo test -p lato-mcp --test lifecycle stdio_cancel_mid_call_reaps_child --quiet` 连续 20 次为 20/20 PASS。
- 因此现有证据排除“7C 文档直接改变 MCP 代码/依赖”，但尚不足以把单次 CI 失败永久标记为环境问题；新 SHA 必须触发全套 CI。只有 Ubuntu/macOS/Windows/lint/no-live-network 5/5 SUCCESS 才允许合并。

复现命令：

```bash
git diff --quiet origin/master...HEAD -- crates/lato-mcp Cargo.toml Cargo.lock
for i in $(seq 1 20); do
  cargo test -p lato-mcp --test lifecycle stdio_cancel_mid_call_reaps_child --quiet
done
gh run view 35086976901 --repo hyzwhu/lato --job 104763990022 --log-failed
```

若新 SHA 的 Ubuntu 再现相同 Spawn 失败，则暂停规格合入，建立独立基线缺陷并在 `origin/master` 相同 runner 环境复现；不得用 rerun 绿掩盖可重复 flake。
