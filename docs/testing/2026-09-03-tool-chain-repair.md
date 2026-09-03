# 工具调用链修复与验收（2026-09-03）

## 已确认的问题与修复

1. CLI 用关键词包含关系识别本地目录/模型查询，导致“请在当前目录创建文件”直接返回路径、跳过模型与工具。现在只匹配完整事实查询，复合任务交给模型；这个问题由真实配置验收发现，并有修复前失败的回归测试。

2. Responses 的 `response.output_item.done` 没有对应 added 事件时，工具调用被丢弃；`response.completed` 中的完整 output 也被忽略。现在 HTTP、Codex SSE 和 WebSocket 共用的 Responses 映射器接受这些完成信息，并按 call ID 去重。
3. HTTP 和 Codex SSE 在服务端已经发出完成事件后仍等待连接关闭。现在按协议完成事件结束本轮读取，工具结果可以进入下一轮请求。
4. 完整 JSON 响应按行解析，导致格式化 JSON、Responses output、Anthropic content 中的文字和工具调用丢失。现在区分完整 JSON 文档与 SSE，支持多行 data、分块 UTF-8，并增量处理 SSE 事件。
5. 服务端 error、Responses incomplete 和尚未完成的 Responses 工具调用可能表现为空成功。现在返回明确错误；参数 JSON 无效时不构造虚假参数。
6. 多个 Chat Completions 工具调用按字符串索引排序，出现 0、1、10、11、2 的次序。现在按数值索引排序。
7. Responses / Anthropic 的请求历史转换丢弃带工具调用的助手消息中的文字。现在保留文字及调用 ID、参数、结果。

## 参考实现

- `/Users/huangyongzhao/Documents/work/rustproject/codex/codex-rs/codex-api/src/sse/responses.rs`：`process_responses_event` 直接消费已完成的 output item。
- `/Users/huangyongzhao/Documents/work/grok-build/crates/codegen/xai-grok-sampler/src/stream/responses.rs`：`observe_for_recovery` 保留已完成 item 和 terminal output。
- `/Users/huangyongzhao/Documents/work/grok-build/crates/codegen/xai-grok-sampler/src/stream/chat_completions.rs`：使用数值索引累计工具调用。

## 测试证据

- 首批 7 项回归测试在修复前全部失败，在修复后通过。
- 新增共 12 项解析、请求历史和传输回归测试、1 项 CLI 入口误判测试，以及 4 项 CLI 端到端测试。
- 4 项 CLI 测试分别覆盖 Chat Completions、Responses、Anthropic JSON、Codex SSE。每项都实际执行写入、替换、读取，并检查下一轮请求中的结果 ID 和文件内容。
- SSE 测试刻意保持 HTTP 分块响应未关闭，验证完成事件能推动下一轮请求；Responses 重复完成快照不会再次执行替换。
- `cargo test --workspace --no-fail-fast`：423 passed，0 failed，0 ignored。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。

全量补测还发现会话日志并发导入的临时文件名碰撞：同一进程内时间戳可能相同，`create_new` 返回 File exists。文件名已追加原子序号，原有并发导入测试加强为 16 路并发，保留不得覆盖既有日志的检查。

真实配置验证使用 `openai-codex/gpt-5.6-luna`，在临时目录执行文件创建任务，进程退出码 0，文件内容精确为 `lato-tool-chain-ok`，模型回复 `VERIFIED`。临时目录验证后自动清理。原有未提交改动保留。

## 本地交付

- 已运行 `cargo install --path .`，更新 `/Users/huangyongzhao/.cargo/bin/lato`。
- 使用 `LATO_TEST_BINARY=/Users/huangyongzhao/.cargo/bin/lato cargo test --test tool_chain_repair` 对安装后的 release 二进制重跑 4 项端到端测试，全部通过。
- 安装后的 release 二进制也通过上述真实模型文件创建验证。
- `lato doctor`：status ok，9 个工具已注册，策略及沙箱自检通过。
