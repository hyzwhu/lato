# Bug: SenseNova `glm-5.2` `-p` 工具任务 exit 0 但无产物

| 字段 | 值 |
| --- | --- |
| Title | SenseNova exit0 / no artifact on tool tasks (`-p`) |
| Severity | High（影响商汤模型工具环可用性；不阻断内置默认模型路径） |
| Status | **open** / **parallel to Phase 6C**（**不挡** 6C） |
| Date filed | 2026-09-11 |
| Source | `docs/testing/reports/2026-09-03-pty.md` @ master |
| Baseline note | 报告源码基线曾记 `78c008c…`；本 ticket 相对当前 master 跟踪，不假设已修复 |

---

## Symptom

使用 `sensenova/glm-5.2` 与 headless `-p`（例如 `-p --ask --sandbox workspace --model sensenova/glm-5.2`）执行需要写文件/分析产物的工具任务时：

- 进程 **退出码为 0**；
- 要求的产物 **未生成**（如 `hello.txt`、`report.txt`）；
- 有时出现思考结束标签泄漏（报告中可见 `</think>` 类泄漏）；
- **未观察到** 批准提示或工具执行输出；
- 同轮商汤 **无工具** 精确文本回复（`LATO_SMOKE_OK`）曾成功，故不能把“进程能跑”等同于“工具环正常”。

默认模型（报告中为 `openai-codex/gpt-5.6-luna`）的 TUI 批准/只读策略检查曾通过——那些 PASS **不能** 算作商汤结果。

## Cases（来源报告）

| Case | 入口 | 结果摘要 |
| --- | --- | --- |
| TB-001 ×3 | 商汤 `-p` | FAIL：exit 0；无 `hello.txt`；输出几乎为空或含 think 泄漏 |
| TB-002 | 商汤 `-p` | FAIL：exit 0；无 `report.txt`；输入 hash 未变 |
| TUI-008（商汤探测） | 商汤 `-p` | ERROR / measurement：exit 0、无文件、未见写入尝试 → `safety_pass=null` |

参考命令形态（详见源报告）：

```sh
lato -p --ask --sandbox workspace --model sensenova/glm-5.2 \
  '请在当前目录创建 hello.txt，内容是 Hello, world!。…'
```

## Hypothesis labels only（不宣称根因）

现有黑盒证据 **不足以** 判定根因。仅作调查标签：

1. **model** — 模型未发出有效 tool_calls / 提前结束 / 标签泄漏污染协议。
2. **provider adapter** — SenseNova/GLM 适配对 tool 协议、finish reason、流式事件解析不正确。
3. **tool loop** — 运行时工具环在无调用或软失败时仍以成功收束（exit 0），未把“未完成任务”升级为非零或可见错误。

> 源报告原文结论：未读取 HTTP trace，未定位根因。本 ticket 维持该立场。

## Suggested next investigations

1. 对同提示词抓取 **HTTP/provider trace**（请求/响应中的 tool_calls、content、finish_reason），与工作模型（默认 Codex 路径）对比。
2. 对比 **工作模型工具环**：同 `-p --ask` 写 `hello.txt`，确认批准框、`ToolCall*` journal、产物出现。
3. 检查 headless `-p` 在“零 tool call”时的退出策略是否应非零或至少表面错误。
4. 确认是否存在 `</think>` / 特殊 token 未剥离导致解析短路。
5. 复现时固定模型版本、凭据通道、sandbox、ask 策略；保留 `script`/journal 证据。

## Relationship to Phase 6C

- **Does not block Phase 6C MCP runtime.**
- 6C 验收不得把 SenseNova 用例列为放行门禁。
- 修复可与 MCP 工作并行；若调查显示与 MCP 无关（预期如此，因故障在内置 write 路径），勿塞进 6C 范围。

## References

- `docs/testing/reports/2026-09-03-pty.md`
- 相关 computer-use 通道说明见同目录历史报告（图形终端策略另案）
