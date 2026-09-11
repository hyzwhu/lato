# Bug: SenseNova `glm-5.2` `-p` 工具任务 exit 0 但无产物

| 字段 | 值 |
| --- | --- |
| Title | SenseNova exit0 / no artifact on tool tasks (`-p`) |
| Severity | High（影响商汤模型工具环可用性；不阻断内置默认模型路径） |
| Status | **fixed** on `fix/sensenova-exit0`（**不挡** 6C；未重做 MCP） |
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

## Investigation findings

三条假设都成立，且会叠成 PTY 报告里的「exit 0 / 无产物 / `</think>` 泄漏」：

1. **model / think tags** — `glm-5.2` 默认思考。网关常把思维链放进 `reasoning_content`，`content` 只剩 `</think>` 或空。思考块里的 GLM `<tool_call>` XML 从未变成用户可见文本，也从未被当成 tool call。
2. **provider adapter** — 解析器丢弃 `reasoning_content`（正确：不应打印），但没有再扫描其中的 GLM XML；`</think>` 当普通 content 输出；`finish_reason: "tool_call"`（单数）和扁平 `tool_calls`（无 `function` 包装）未覆盖。GLM 思考还可能吃掉过小的默认 `max_tokens`。
3. **tool loop** — 工作区写入任务若零次 tool call，最多 retry 一次 `tool_choice=required`，然后 `TurnOutcome::Complete` → headless **exit 0**。TB-002「生成 report.txt」甚至进不了 retry（原先只匹配「生成文件」）。

无工具的 `LATO_SMOKE_OK` 仍走纯文本路径，因此不能证明工具环可用。未使用真实 SenseNova 凭据；修复用 offline fixture/replay 锁定。

## Fix

- 剥离 `<think>…</think>` / 残留 `</think>`，不作为助手可见文本。
- 从 `reasoning_content` 与 think 块中提取 GLM `<tool_call>` XML。
- 兼容扁平 tool_calls 与 `finish_reason: "tool_call"`；GLM/SenseNova 带 tools 时发送 `max_tokens=16384`。
- 工作区变更任务在 retry 后仍未执行工具 → 显式错误（headless 非 0）。
- 「生成 report.txt」一类产出文件意图计入工作区变更检测。

## Relationship to Phase 6C

- **Does not block Phase 6C MCP runtime.**
- 6C 验收不得把 SenseNova 用例列为放行门禁。
- 修复可与 MCP 工作并行；若调查显示与 MCP 无关（预期如此，因故障在内置 write 路径），勿塞进 6C 范围。

## References

- `docs/testing/reports/2026-09-03-pty.md`
- 相关 computer-use 通道说明见同目录历史报告（图形终端策略另案）
