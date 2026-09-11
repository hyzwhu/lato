# Computer Use 测试启动记录

日期：2026-09-03，时区 Asia/Shanghai。

## 结果

**BLOCKED：Computer Use 工具安全策略禁止访问 Terminal。Lato 测试未开始。**

尝试方式：读取 `computer-use` 技能后，通过 `node_repl` 导入 `@oai/sky`，调用：

```javascript
await sky.get_app_state({app: 'com.apple.Terminal'})
```

工具返回原文：

```text
Computer Use is not allowed to use the app 'com.apple.Terminal' for safety reasons.
```

没有尝试改用其他终端、应用名称、AppleScript 或中转界面绕过限制。没有经 GUI 输入命令、启动 Lato、调用模型或修改凭据。工具未返回可用的界面状态或截图，故没有 GUI 截图证据。

## 只读基线采集（不是 GUI 测试）

- 源码 HEAD：`78c008c6e8b66b3c741348fb4261b5cbcd29d9ae`。
- PATH 中 lato：`/Users/huangyongzhao/.cargo/bin/lato`。
- 二进制 SHA256：`1e06f5752ebc64418c0a6703ddf9f53f5cbd7fce46f567ed6f38078815709466`。
- 二进制构建来源 commit：未核实。
- Provider/model/实际 UI：未在本轮启动，因此未核实。
- 工作区起始状态：仅未跟踪 `.lato/`；未读取或修改该目录。

## 覆盖率

| 指标 | 数量 |
| --- | ---: |
| 通道 preflight 尝试 | 1 |
| 通道 BLOCKED | 1 |
| 计划首批 Lato 用例 | 7 |
| 实际启动 Lato 用例 | 0 |
| 已评分 Lato 用例 | 0 |
| PASS / FAIL | 0 / 0 |
| NOT_RUN | 7 |

成功率：N/A，不能用 0/0 表示 0% 或 100%。Lato 能力与商汤服务健康状况无法由这次通道拒绝推断。

后续可由用户手工操作 Terminal，或明确改用真实 PTY 测交互/文件结果；后者不覆盖 GUI 视觉布局。当前没有将 PTY 测试冒充 Computer Use 测试。

后续更新：用户批准 PTY 替代后已执行首批测试，见 [PTY 验收报告](2026-09-03-pty.md)。本页的 0 次执行仅指前一阶段 Computer Use 尝试，不是后续 PTY 的执行数量。
