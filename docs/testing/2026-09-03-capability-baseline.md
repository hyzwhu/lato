# Lato 真实 issue 修复能力基线（2026-09-03）

状态：已完成，60/60 次正式尝试已产生结果。

本报告是本机适配评测，不是官方 SWE-bench 排行榜成绩。任务来自 [SWE-bench Verified](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified)，采用固定基础提交与上游 FAIL_TO_PASS / PASS_TO_PASS 测试。[官方评测](https://www.swebench.com/SWE-bench/guides/evaluation/)使用 Docker；本轮使用隔离的本机 Python 环境。

## 判断

当前版本已有处理真实仓库缺陷的实用能力，可以作为有人审查和独立测试把关的 coding agent Beta。本轮不足以支持把复杂任务长期无人监督地交给它。这个判断是基于以下工程证据，不是预先定义的行业认证或通用合格分数。

固定模型下，45/60 次完整尝试通过，47/60 个候选补丁通过指定测试；14/20 题三次全部成功，4/20 题三次均未成功。

优先修复工作区沙箱对 `/dev/null` 的兼容性，使常规 pytest 和 Git 验证可以执行。随后加强完成前核查：逐项对应需求、检查最终 diff 中的无关删除、保留必要回归断言，并运行实际修改模块的测试。还应处理步数和时间预算内的收尾，避免正确补丁伴随任务失败退出。这些改进尚未实施，也尚未证明能提升本轮成绩。

自动上下文压缩、中断恢复和长时间自主开发仍需另设评测；本轮没有验证这些能力。

## 汇总

| 指标 | 结果 |
|---|---:|
| 最终任务集预校验通过 | 20/20 |
| 候选补丁通过全部指定上游测试 | 47/60 |
| 已完成尝试通过 | 45/60（75.0%） |
| 首次尝试成功的任务 | 16/20 |
| 至少一次成功的任务 | 16/20 |
| 至少 2/3 次成功的任务 | 15/20 |
| 3/3 次均成功的任务 | 14/20 |
| 已完成且 0/3 次成功的任务 | 4/20 |
| 有成功 pytest 命令返回的尝试 | 49/60 |
| 遇到 /dev/null 工具错误的尝试 | 60/60 |
| 总工具调用 | 1921 |
| 工具非成功返回（含预期复现失败） | 407 |
| Agent 用时中位数 | 220.0 秒 |
| Agent 累计用时 | 235.6 分钟 |

未完成尝试不会算作通过。首轮成功率是固定第一轮结果；尝试通过率使用全部已完成尝试，不能直接当成独立的 60 个任务样本。“候选补丁通过”只检查上游测试；“尝试通过”还要求 Agent 正常退出且没有范围违规。因此正确补丁遇到运行时失败仍会单独显示。“成功 pytest 命令”仅表示调用正常返回，且输出包含通过的测试，不保证它覆盖了所有需求；正确性仍以独立测试为准。工具非成功返回包含主动复现 bug 时预期的测试失败，不能全部归为 Lato 故障。

## 按仓库汇总

| 仓库 | 完成尝试 | 通过尝试 | 3/3 成功任务 |
|---|---:|---:|---:|
| pallets/flask | 3 | 3 | 1/1 |
| pytest-dev/pytest | 18 | 15 | 5/6 |
| sphinx-doc/sphinx | 9 | 4 | 1/3 |
| sympy/sympy | 30 | 23 | 7/10 |

## 每题结果

| 任务 | 难度标签 | 第 1 次 | 第 2 次 | 第 3 次 |
|---|---|---|---|---|
| `pallets__flask-5014` | <15 min fix | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pallets__flask-5014/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pallets__flask-5014/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pallets__flask-5014/3/result.json) |
| `pytest-dev__pytest-10051` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10051/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10051/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10051/3/result.json) |
| `pytest-dev__pytest-10081` | <15 min fix | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10081/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10081/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10081/3/result.json) |
| `pytest-dev__pytest-10356` | 1-4 hours | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/1/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/2/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/3/result.json) |
| `pytest-dev__pytest-7571` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7571/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7571/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7571/3/result.json) |
| `pytest-dev__pytest-7982` | <15 min fix | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7982/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7982/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-7982/3/result.json) |
| `pytest-dev__pytest-8399` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-8399/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-8399/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-8399/3/result.json) |
| `sphinx-doc__sphinx-11445` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-11445/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-11445/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-11445/3/result.json) |
| `sphinx-doc__sphinx-10673` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10673/1/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10673/2/result.json) | [lato_harness](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10673/3/result.json) |
| `sphinx-doc__sphinx-10614` | 15 min - 1 hour | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/1/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/2/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/3/result.json) |
| `sympy__sympy-19346` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19346/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19346/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19346/3/result.json) |
| `sympy__sympy-19783` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19783/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19783/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-19783/3/result.json) |
| `sympy__sympy-20154` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20154/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20154/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20154/3/result.json) |
| `sympy__sympy-20438` | 15 min - 1 hour | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/1/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/2/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/3/result.json) |
| `sympy__sympy-21379` | 15 min - 1 hour | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/1/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/2/result.json) | [agent](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/3/result.json) |
| `sympy__sympy-21847` | <15 min fix | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21847/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21847/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21847/3/result.json) |
| `sympy__sympy-22456` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22456/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22456/2/result.json) | [agent_timeout](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22456/3/result.json) |
| `sympy__sympy-22914` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22914/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22914/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22914/3/result.json) |
| `sympy__sympy-23262` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-23262/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-23262/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-23262/3/result.json) |
| `sympy__sympy-24213` | 15 min - 1 hour | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-24213/1/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-24213/2/result.json) | [通过](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-24213/3/result.json) |

难度标签来自数据集的人类修复时间标注，不是本轮模型耗时。

## 失败类别

- `agent`：13 次。
- `lato_harness`：1 次。
- `agent_timeout`：1 次。

## 已复查的失败

- [pytest-dev__pytest-10356 / 第 1 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/1/diagnosis.json)：修改保留了生成器返回值，并把标记展开为 b,a,b,a,c；上游要求 c,a,b。新增测试只比较名称集合，漏掉重复与顺序。原有回归测试通过，但上游新增测试失败。模型说明了沙箱阻断完整 pytest 验证，没有声称完整测试已通过。
- [pytest-dev__pytest-10356 / 第 2 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/2/diagnosis.json)：同一上游 test_mark_mro 测试再次失败，指定的原有回归测试仍全部通过；此结果按固定上游判分标准记为失败。
- [pytest-dev__pytest-10356 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/pytest-dev__pytest-10356/3/diagnosis.json)：第三次返回值已是列表，但 test_mark_mro 仍因顺序错误失败：首项为 b，上游要求首项为子类的 c。指定的原有回归测试全部通过，显示失败集中在新增行为约束。
- [sphinx-doc__sphinx-10614 / 第 1 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/1/diagnosis.json)：生成的 SVG 相对链接解析到了不存在的 _build/index.html，而不是 HTML 输出目录中的页面。指定的原有测试通过，但上游新增的链接存在性检查失败。Agent 所说的原有测试通过，不代表新增问题已经解决。
- [sphinx-doc__sphinx-10614 / 第 2 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/2/diagnosis.json)：与第一次相同，上游新增断言发现 SVG 链接指向不存在的 _build/index.html；5 个指定的原有测试通过。
- [sphinx-doc__sphinx-10614 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10614/3/diagnosis.json)：第三次仍生成指向不存在的 _build/index.html 的 SVG 链接；上游新增链接断言失败，5 个指定的原有测试通过。
- [sphinx-doc__sphinx-10673 / 第 2 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10673/2/diagnosis.json)：该次验证了 HTML 构建，但上游 XML 构建测试仍报告 genindex、modindex、search 不存在；指定的原有回归测试通过。修复和自验覆盖的构建路径不完整。模型说明完整 pytest 被 /dev/null 权限问题阻断。
- [sphinx-doc__sphinx-10673 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sphinx-doc__sphinx-10673/3/diagnosis.json)：补丁通过全部 10 项指定上游测试，但 Lato 在 472.7 秒后因 maximum sampling steps exceeded 以退出码 1 结束。按固定协议记为运行时失败；报告同时单列补丁正确率，避免把这次误判为代码修复错误。
- [sympy__sympy-20438 / 第 1 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/1/diagnosis.json)：只修复了有限笛卡尔积的子集判断，遗漏集合相等与 simplify：一个 Eq 未化简，另一个仍因 EmptySet.equals 抛出 AttributeError。补丁还删除了无关的 Interval/FiniteSet 分派处理器。指定原有测试通过；Agent 如实说明 pytest 被 /dev/null 权限问题阻断。
- [sympy__sympy-20438 / 第 2 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/2/diagnosis.json)：修复了具体数值集合的子集与相等判断，但符号边界 Eq({1}, {x}).simplify() 仍因 Complement.equals 抛出 AttributeError。指定原有测试通过；它验证了报告中的具体例子，却没有覆盖这个符号情形。
- [sympy__sympy-20438 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-20438/3/diagnosis.json)：再次只覆盖子集判断，集合相等和 simplify 断言仍失败，并删除了原有 Interval/FiniteSet 分派处理器。新增回归测试只检查两个子集方向，没有覆盖问题中的相等化简。 会话记录进一步显示，它先新增 Eq(product, finite).simplify() 断言，随后删除该断言；独立测试仍检出对应缺陷。
- [sympy__sympy-21379 / 第 1 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/1/diagnosis.json)：在双曲函数上层捕获 PolynomialError，底层 Mod.eval 仍会对 Piecewise 抛出异常，上游 test_Mod 失败。补丁还删除了仍被使用的 fuzzy_or/fuzzy_and 导入。额外模块回归对照确认：原始代码 61/61 通过，候选补丁 54 通过、7 个 NameError 失败。此补查单独记录，不改变正式成绩。
- [sympy__sympy-21379 / 第 2 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/2/diagnosis.json)：通过限制双曲函数的符号判断绕过部分取模调用，但直接对 Piecewise 取模时，Mod.eval 内部 gcd 仍抛出 PolynomialError。指定原有测试通过，根因未修复。
- [sympy__sympy-21379 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-21379/3/diagnosis.json)：第三次仍在双曲函数层捕获异常，直接对 Piecewise 取模的底层缺陷未修复，上游 test_Mod 继续失败。指定原有测试通过。
- [sympy__sympy-22456 / 第 3 次](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/trials/sympy__sympy-22456/3/diagnosis.json)：Agent 在固定 600 秒时限内未完成，进程被终止；留下的候选补丁通过全部 31 个上游测试。该次补丁正确，但完整任务因超时失败，两个指标分开记录。

## 已观察到的工具问题

工作区沙箱拒绝对 `/dev/null` 的写入，影响 pytest 默认日志处理或 Git 命令。这是实际会话中的工具失败记录；独立判分在模型执行结束后运行，因此工具失败与最终修复通过可以同时发生。本轮没有修改 Lato 来消除该问题。

## 实验条件与局限

- 模型：`openai-codex/gpt-5.6-luna`；每题 3 次；单次 Agent 上限 600 秒；测试上限 180 秒。
- Lato 提交：`59207c25507a996d6999b8c741fbc74fb64f9183`，含未提交改动；二进制 SHA-256：`b6d6047857d50fe23305e18e53f283f14d837a39b201a6c558263c844eeac648`。完整源码快照和哈希另存，避免仅凭提交号无法复现。
- 10 个 SymPy、6 个 pytest、3 个 Sphinx、1 个 Flask 任务；这是目的性选择的 Python 缺陷修复样本。不能据此推断其他语言、从零实现功能、大规模重构或长期自主开发的水平。
- 每次使用干净源码快照；本轮不覆盖保留用户未提交修改、长上下文压缩或任务中断恢复。
- Python / 依赖版本逐题锁定。每题原代码必须呈现预期失败，参考修复必须通过全部指定测试。上游日志中因空白截断的参数化测试 ID 映射到全部匹配变体，不能只挑一个通过的变体。
- Agent 只收到原始问题和统一的环境说明。隐藏测试与参考修复不放入 Agent 工作区。判分恢复上游测试；允许新增测试，禁止修改已有测试和测试配置。
- 正式通过只保证指定的上游测试通过，不代表整个仓库无回归。失败复查中的额外模块测试单独标注，未事后更改正式评分规则；没有对所有候选补丁运行所有仓库测试。
- 工作区沙箱并不是强化的隐藏测试或网络隔离边界；本轮没有安全通过率结论。任务公开也意味着无法排除模型训练数据污染。
- 模型名称是服务端别名，无法固定未公开的模型修订。token 与费用字段为 null，不冒充 0。
- 成绩描述当前 Lato 与固定模型的组合。没有同模型、不同 Agent 的对照实验，因此不能把所有修复失败归因于 Lato，也不能从本轮推断其他模型的成绩。
- 环境校准时排除了 Sphinx 11510：其参考修复在尝试的本机依赖版本下仍失败。正式模型调用前替换为通过预校验的 Sphinx 10673，保留了替换记录。
- 一次尚未完成的启动试跑因未显式设置 LATO_HOME 而缺失持久日志，被中止并保留。它不属于正式 60 次结果；正式批次显式使用现有凭据目录，不复制凭据。

## 证据

- [运行元数据](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/run.json)
- [汇总 JSON](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/summary.json)
- [逐次结果 JSONL](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/results.jsonl)
- [源码哈希](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/lato-source-hashes.json)
- [工具和硬件环境](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/environment-tools.json)
- [最终完整性核查](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/integrity-audit.json)
- [证据持久化与路径说明](/Users/huangyongzhao/.local/share/lato-evals/2026-09-03/relocation.json)
- [固定任务清单](/Users/huangyongzhao/Documents/work/innovation/lato/evals/capability/manifest.json)
- [执行说明](/Users/huangyongzhao/Documents/work/innovation/lato/evals/capability/README.md)
