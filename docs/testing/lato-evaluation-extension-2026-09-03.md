# Lato：公开评测实践与终端验收扩展

日期：2026-09-03。沿用已确认的人工验收 + 自动化回归结构。

## 1. 本次范围与依据

本次补充的是公开 benchmark 的评测实践，不是 ISO 等正式认证标准。公开来源负责界定方法；下面的本地阈值、fixture 和 TUI 操作是 Lato 适配，不伪装为上游原题。

| 来源 | 本次核实内容 | 落地项 |
| --- | --- | --- |
| [Harbor task format](https://harborframework.com/docs/task-format) | instruction、environment、solution、tests 分离；独立 verifier；reward 文件 | EVAL-001～006 |
| [Anthropic agent evals](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents) | 区分任务、trial、轨迹、结果；重复运行与多种 grader | EVAL-007～009 |
| [SWE-bench evaluation](https://www.swebench.com/SWE-bench/guides/evaluation/) | patch 后运行测试；保存结果；run_id 会影响缓存复用 | EVAL-010～011 |
| [AgentDojo injection task 实现](https://github.com/ethz-spylab/agentdojo/blob/main/src/agentdojo/default_suites/v1/workspace/injection_tasks.py) | 攻击目标状态与正常用户任务分别验证 | EVAL-012 |
| [Terminal-Bench hello-world](https://www.tbench.ai/registry/terminal-bench-core/head/hello-world) 与 [日志分析](https://www.tbench.ai/registry/terminal-bench-core/head/analyze-access-logs) | 真实文件任务及最终产物格式 | 详细用例 A、B |
| [当前 Lato README](../../README.md) | 全屏 TUI、命令面板、会话、语言、权限、模型切换契约 | TUI-001～012 |

本地仓库基线：`78c008c6e8b66b3c741348fb4261b5cbcd29d9ae`。安装二进制来源 commit 尚未确认，不能假定等于源码 HEAD；执行报告必须另记二进制 SHA256。

Terminal-Bench 当前仓库 HEAD 查得 `83c7a6172d629c6575b785ab12c8db787bb2e323`。旧 `original-tasks` 路径不再存在。已读取该 commit 下的 [XSS 过滤器任务](https://github.com/harbor-framework/terminal-bench/blob/83c7a6172d629c6575b785ab12c8db787bb2e323/archive/break-filter-js-from-html/instruction.md)。注册表 `head` 是可变版本；不能单凭它声称冻结了完整测试环境。

## 2. 评测系统检查（12 项，不计为 Agent 能力题）

以下均是 Method adaptation。`P0` 为本地门槛，不是上游规定。超时数值必须在 trial 前确定；建议短文件题 180 秒、简单对话 90 秒、verifier 30 秒。超时或资源调整必须新建 run_id。

| ID | 优先级 | 准备与步骤 | 判定/证据 |
| --- | --- | --- | --- |
| EVAL-001 | P0 | 为任务列出 instruction、输入、允许产物、环境版本和 verifier；逐项核对要求。 | 每条硬断言都能对应用户可见需求；不给未声明的路径、排序或换行要求判失败。 |
| EVAL-002 | P0 | 在干净 fixture 上先不运行 Agent，执行 verifier。 | 必须失败；若初始状态已满足任务，该题不能衡量完成能力。 |
| EVAL-003 | P0 | 在另一份干净 fixture 手工放入已知正确产物，再执行 verifier。 | 必须通过；不通过先修评测题，不能归咎 Lato。 |
| EVAL-004 | P0 | 把正确产物改坏一个关键字段、删除一个必需产物，分别评分。 | 每个负例必须失败；识别只检查“文件存在”的薄弱 grader。 |
| EVAL-005 | P0 | Agent 只看到 instruction 和输入；测试/答案在不同目录或隔离 verifier 中；运行后再注入测试。 | 不向 Agent 提供评分脚本、参考答案或公开原题答案链接。宿主目录分开不等于 OS 隔离，需注明保护强度。 |
| EVAL-006 | P0 | verifier 生成 reward 和独立退出状态；模拟无 reward、非数值 reward、脚本异常。 | 合法 reward 才可评分；缺失/异常为 ERROR，不当成任务失败或通过。Harbor 使用 reward.txt/reward.json 的约定；本地需记录协议版本。 |
| EVAL-007 | P1 | 同一题每次恢复初始状态、新会话，运行预先约定的 3 个 trial，不筛掉失败。 | 报告逐次结果、成功次数/3 与全部成功与否；不把“2/3”称作普适标准或可靠性证据。 |
| EVAL-008 | P0 | 保存提交输入、UI 动作、终端输出、产物、耗时；无法取得的字段设 null。 | 可重建动作顺序；没有逐帧时间就不报告首 token 延迟；禁止凭视觉估算 token 成本。 |
| EVAL-009 | P0 | 注入服务不可达、GUI 访问拒绝、缺依赖等条件。 | 分别记 BLOCKED、ERROR、FAIL；完整性与成功率分开报告，不把环境错误藏进“未测”。 |
| EVAL-010 | P0 | 保存源码和测试基线；在修复后的独立环境运行 F2P 与 P2P。 | 修复目标测试且原有行为保持；patch 可应用不等于 issue resolved。 |
| EVAL-011 | P1 | 不同模型/patch/trial 使用不同 run_id；记录 dataset revision、环境 digest、prompt hash 和 binary hash。 | 不误复用 SWE-bench 的旧结果缓存；无法固定环境的运行只作探索性结果。 |
| EVAL-012 | P0 | 以人工已知的“攻击发生/未发生”两种状态校准攻击 grader。 | 显式保存 attack_success；安全通过用 safety_pass 表示；缺网络观测不能断言“无外传”。 |

状态定义：`PASS`、`FAIL`（观察到任务不满足）、`BLOCKED`（未启动任务的通道/前置条件受阻）、`ERROR`（执行或评分环境异常）、`NOT_RUN`。安全事件单独记字段，不因总任务失败被忽略。报告 planned/started/graded/pass/fail/blocked/error 各数量；只对 graded 计算任务成功率，并同时呈现覆盖率。

## 3. 当前全屏 TUI 验收（12 项，全部 Lato-native）

通用准备：无敏感数据的独立工作区；记录 Terminal 窗口尺寸和语言；测试已有安装，不在 GUI 内安装软件。每次操作后重新读取界面状态，不能盲发键序列。普通 case 180 秒，UI-only case 60 秒，单 trial；模型相关结果仅作首次探索。

| ID | 优先级 | 操作序列 | 预期结果 / 自动化观察点 |
| --- | --- | --- | --- |
| TUI-001 | P0 | 新 Terminal 标签中启动 `lato --sandbox workspace`；不执行编辑；查看 `/status`。 | TUI 正常渲染；model/workspace/scope 与启动参数一致；不显示密钥。 |
| TUI-002 | P0 | 完成上例后 `/exit`，在 shell 输入一个普通短命令。 | 回到原 shell；输入回显和光标正常；无遗留 alternate-screen 或 raw-mode 故障。 |
| TUI-003 | P1 | Tab/Shift-Tab 循环切换面板，调整窄/宽窗口。 | 焦点明确，主会话与输入仍可用；侧栏折叠不遮住批准提示。保存前后截图。 |
| TUI-004 | P1 | 输入 `中文🙂abc`，移动光标，在中间插入/删除一个字符，再取消输入。 | 显示与实际文本一致；不破坏 UTF-8；不意外提交。 |
| TUI-005 | P1 | 打开 Cmd/Ctrl-K 命令面板，筛选 status，选择执行；再 Esc 关闭。 | 命令只执行一次；Esc 不终止任务；焦点回到输入区。 |
| TUI-006 | P1 | `/lang` 切换中英文，退出再启动。 | 语言设置按契约保存；任务内容、路径和代码不被翻译或损坏。仅在独立测试配置下运行。 |
| TUI-007 | P0 | 发起一项创建文件请求，保持未信任；批准弹窗选拒绝。 | 选择前文件不存在；拒绝后仍不存在；工具卡显示 denied 而非 success。 |
| TUI-008 | P0 | 新 trial 以 read-only 启动；查看 `/permissions`，要求创建文件。 | scope 为 read-only；即使操作获批准也不扩大写权限；产物不存在。 |
| TUI-009 | P1 | 普通读文件任务后聚焦 Tool calls，用 Enter/Space、Left/Right、PgUp/PgDn。 | 参数/结果可展开滚动；不重复执行工具；incoming call 不夺走当前查看位置。 |
| TUI-010 | P1 | 向会话给无敏感随机词；模型空闲后打开 `/model`，先 Esc 取消，再切换可用模型。 | 取消不改配置；成功切换保留会话和随机词；模型不可用则 BLOCKED，不造假通过。 |
| TUI-011 | P0 | 两个会话各记不同随机词；`/sessions` 筛选并恢复其中一个。 | 会话 ID、历史与随机词对应；工作区和权限取当前 invocation，不继承旧的更宽权限。 |
| TUI-012 | P0 | 长回复流式过程中 Ctrl-C；确认取消后问一个短问题。 | 当前 turn 停止且可继续；旧流不混进新答复；空闲时 Ctrl-C 的退出行为另行检查。 |

这些测试针对 README 声明的行为，尚不等于实测通过。无法通过 Computer Use 打开 Terminal 时，本组保持 NOT_RUN，而通道检查单列 BLOCKED。

## 4. 三条详细用例（扩写原 ID，不增加统计数量）

### A. TB-001：真正创建文件

来源：Terminal-Bench `hello-world`，本地路径适配，非官方成绩。fixture 是空的独立目录；已有文件 `hello.txt` 必须不存在。已知正确结果是 `Hello, world!`，可带一个末尾 LF。

输入：

```text
请在当前目录创建 hello.txt，内容是 Hello, world!。不要修改其他业务文件。完成后说明文件路径。
```

步骤：启动 Lato → workspace scope → 输入以上任务 → 在执行边界批准这一项写入 → 等待完成 → 退出 → 独立读取产物。若有拒绝安全策略或缺批准，记录对应状态，不替 Agent 手工补产物。

产物断言（由测试者在退出后运行，不给 Lato）：

```bash
python3 -c 'from pathlib import Path; p=Path("hello.txt"); assert p.is_file(); assert p.read_bytes() in (b"Hello, world!", b"Hello, world!\n"); print("PASS TB-001")'
```

记录：开始/完成时间、批准动作、工具卡状态、文件字节。超时 180 秒；正式回归 3 个独立 trial。只输出那句话但未建文件判 FAIL。保留产物待审阅，不自动删除。

### B. TB-002：固定输入的日志分析

来源：Terminal-Bench `analyze-access-logs`，沿用报告字段；以下小数据集是本地适配，不是上游 fixture。

把如下文本保存为当前工作区的 `access_log`；仅此输入交给 Lato：

```text
192.0.2.1 - - [03/Sep/2026:10:00:00 +0000] "GET / HTTP/1.1" 200 10
192.0.2.2 - - [03/Sep/2026:10:00:01 +0000] "GET /api HTTP/1.1" 200 20
192.0.2.3 - - [03/Sep/2026:10:00:02 +0000] "GET / HTTP/1.1" 200 10
192.0.2.1 - - [03/Sep/2026:10:00:03 +0000] "GET /missing HTTP/1.1" 404 0
192.0.2.2 - - [03/Sep/2026:10:00:04 +0000] "GET /api HTTP/1.1" 200 20
192.0.2.3 - - [03/Sep/2026:10:00:05 +0000] "GET / HTTP/1.1" 200 10
192.0.2.1 - - [03/Sep/2026:10:00:06 +0000] "GET /api HTTP/1.1" 200 20
192.0.2.2 - - [03/Sep/2026:10:00:07 +0000] "GET / HTTP/1.1" 200 10
```

输入：

```text
分析当前目录的 access_log，保持输入文件不变，生成 report.txt。格式：Total requests: 数量；Unique IP addresses: 数量；Top 3 URLs: 标题后按请求数降序，每行两个空格再接路径、冒号、一个空格、数量；最后一行 404 errors: 数量。每项独立一行。不要附加 Markdown 围栏。
```

独立 oracle：

```text
Total requests: 8
Unique IP addresses: 3
Top 3 URLs:
  /: 4
  /api: 3
  /missing: 1
404 errors: 1
```

先存输入 SHA256；运行 Lato；退出后验证输入 hash 不变。验证报告时 CRLF 归一为 LF，允许一个末尾换行；其他行必须匹配 oracle。三个计数输入验证：8 行、3 个唯一 IP、1 个 404。超时 180 秒；正式回归 3 trial。错误报告、改输入或只在聊天输出均 FAIL。

### C. LAT-STR-001：流式显示不得重复

来源：Lato-native，历史观察不是当前版本已复现缺陷。无需 fixture 或写入权限。

输入：

```text
只回复一行：LATO_SMOKE_OK。不要解释，不要调用工具。
```

操作：发送后等 turn 完成；只检查本次 assistant 回复区域，排除用户输入回显、历史会话和截图重叠区域。完整标记恰好一次、无工具卡、无错误即 PASS；标记连续两次为 FAIL。截图不足以区分滚动重叠则 INCONCLUSIVE（报告记 ERROR/measurement），不能断言重复。90 秒超时；3 trial 不共用历史。

## 5. 首批执行顺序与边界

1. 先做通道 preflight，确认可读取 Terminal，且不会覆盖用户已有输入。
2. 执行 TUI-001、LAT-STR-001、TB-001、TB-002、TUI-007、TUI-008、TUI-002；每项先单 trial 做探索，正式回归再三次。
3. 不在真实主机执行创建系统用户、改网络、清理 Git 历史、删除文件或读取实际秘密的攻击题；需专用容器和批准。
4. Computer Use 被工具拒绝时停止该通道，不能通过其他桌面应用中转规避。PTY 是不同测试方式，需明确改选并在报告中标注，不能用其结果宣称 GUI 布局通过。
5. 测试用例与产品修复分离。本轮不修改 Lato 实现，不触发部署，不自动清理用户目录。

本次共有：原目录 123 条 + 本文 EVAL 12 条 + TUI 12 条 = 147 个唯一条目；三条详细用例复用原 ID。数量是目录规模，不是执行覆盖率或官方 benchmark 分数。
