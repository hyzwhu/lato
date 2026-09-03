#!/usr/bin/env python3
"""Create a Chinese evidence report from completed, immutable trial artifacts."""
import argparse
import collections
import json
from pathlib import Path
import re
import statistics

from runner import HERE, read_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    manifest = read_json(HERE / 'manifest.json')
    summary = read_json(root / 'summary.json')
    run = read_json(root / 'run.json')
    trials = []
    error_kinds = collections.Counter()
    tools = collections.Counter()
    dev_null_trials = 0
    test_command_trials = 0
    for path in sorted((root / 'trials').glob('*/*/result.json')):
        result = read_json(path)
        trials.append(result)
        journal = path.parent / 'journal.jsonl'
        if not journal.exists():
            continue
        records = []
        for line in journal.read_text().splitlines():
            try:
                records.append(json.loads(line)['record'])
            except (json.JSONDecodeError, KeyError):
                pass
        calls = {r['call_id']: r for r in records if r['type'] == 'tool_call_requested'}
        tools.update(r['name'] for r in calls.values())
        dev_null = False
        test_command = False
        for r in records:
            if r['type'] != 'tool_call_completed':
                continue
            call = calls.get(r['call_id'], {})
            outcome = r.get('result', {})
            failure = outcome.get('Err')
            if failure:
                error_kinds[failure.get('code', 'unknown')] += 1
                message = failure.get('message', '')
                dev_null |= ('/dev/null' in message and bool(re.search(
                    r'Operation not permitted|PermissionError|[Pp]ermission denied', message)))
            if ('run_terminal_command' in call.get('name', '')
                    and ' -m pytest' in str(call.get('arguments', {}))
                    and re.search(r'\b\d+ passed\b', outcome.get('Ok', {}).get('content', ''))):
                test_command = True
        dev_null_trials += dev_null
        test_command_trials += test_command
    passed = summary['passed_trials']
    completed = summary['completed_trials']
    candidate_passed = sum(bool(r.get('grading', {}).get('valid')) for r in trials)
    total = summary['planned_trials']
    cases = summary['cases']
    any_success = sum(case['passed'] > 0 for case in cases)
    no_success = sum(case['completed'] == manifest['trials'] and case['passed'] == 0
                     for case in cases)
    durations = [r['agent']['duration_seconds'] for r in trials if 'agent' in r]
    fully_completed = completed == total
    lines = [
        '# Lato 真实 issue 修复能力基线（2026-09-03）', '',
        f'状态：{"已完成" if fully_completed else "进行中"}，{completed}/{total} 次正式尝试已产生结果。', '',
        '本报告是本机适配评测，不是官方 SWE-bench 排行榜成绩。任务来自 '
        '[SWE-bench Verified](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified)，'
        '采用固定基础提交与上游 FAIL_TO_PASS / PASS_TO_PASS 测试。'
        '[官方评测](https://www.swebench.com/SWE-bench/guides/evaluation/)使用 Docker；本轮使用隔离的本机 Python 环境。', '',
    ]
    if fully_completed:
        lines += ['## 判断', '',
                  '当前版本已有处理真实仓库缺陷的实用能力，可以作为有人审查和独立测试把关的 coding agent Beta。'
                  '本轮不足以支持把复杂任务长期无人监督地交给它。这个判断是基于以下工程证据，'
                  '不是预先定义的行业认证或通用合格分数。', '',
                  f'固定模型下，{passed}/{total} 次完整尝试通过，{candidate_passed}/{total} 个候选补丁通过指定测试；'
                  f'{summary["tasks_all_3"]}/20 题三次全部成功，{no_success}/20 题三次均未成功。', '',
                  '优先修复工作区沙箱对 `/dev/null` 的兼容性，使常规 pytest 和 Git 验证可以执行。'
                  '随后加强完成前核查：逐项对应需求、检查最终 diff 中的无关删除、保留必要回归断言，'
                  '并运行实际修改模块的测试。还应处理步数和时间预算内的收尾，避免正确补丁伴随任务失败退出。'
                  '这些改进尚未实施，也尚未证明能提升本轮成绩。', '',
                  '自动上下文压缩、中断恢复和长时间自主开发仍需另设评测；本轮没有验证这些能力。', '']
    lines += [
        '## 汇总', '',
        '| 指标 | 结果 |', '|---|---:|',
        f'| 最终任务集预校验通过 | {summary["valid_tasks"]}/20 |',
        f'| 候选补丁通过全部指定上游测试 | {candidate_passed}/{completed} |',
        f'| 已完成尝试通过 | {passed}/{completed}'
        + (f'（{passed / completed:.1%}） |' if completed else ' |'),
        f'| 首次尝试成功的任务 | {summary["first_trial_successes"]}/20 |',
        f'| 至少一次成功的任务 | {any_success}/20 |',
        f'| 至少 2/3 次成功的任务 | {summary["tasks_at_least_2_of_3"]}/20 |',
        f'| 3/3 次均成功的任务 | {summary["tasks_all_3"]}/20 |',
        f'| 已完成且 0/3 次成功的任务 | {no_success}/20 |',
        f'| 有成功 pytest 命令返回的尝试 | {test_command_trials}/{completed} |',
        f'| 遇到 /dev/null 工具错误的尝试 | {dev_null_trials}/{completed} |',
        f'| 总工具调用 | {sum(tools.values())} |',
        f'| 工具非成功返回（含预期复现失败） | {sum(error_kinds.values())} |',
    ]
    if durations:
        lines += [f'| Agent 用时中位数 | {statistics.median(durations):.1f} 秒 |',
                  f'| Agent 累计用时 | {sum(durations) / 60:.1f} 分钟 |']
    lines += ['', '未完成尝试不会算作通过。首轮成功率是固定第一轮结果；尝试通过率使用全部已完成尝试，'
              '不能直接当成独立的 60 个任务样本。“候选补丁通过”只检查上游测试；“尝试通过”还要求 Agent 正常退出且没有范围违规。'
              '因此正确补丁遇到运行时失败仍会单独显示。“成功 pytest 命令”仅表示调用正常返回，'
              '且输出包含通过的测试，不保证它覆盖了所有需求；正确性仍以独立测试为准。'
              '工具非成功返回包含主动复现 bug 时预期的测试失败，不能全部归为 Lato 故障。', '',
              '## 按仓库汇总', '', '| 仓库 | 完成尝试 | 通过尝试 | 3/3 成功任务 |',
              '|---|---:|---:|---:|']
    for repo in dict.fromkeys(case['repo'] for case in cases):
        group = [case for case in cases if case['repo'] == repo]
        lines.append(f'| {repo} | {sum(case["completed"] for case in group)} | '
                     f'{sum(case["passed"] for case in group)} | '
                     f'{sum(case["passed"] == manifest["trials"] for case in group)}/{len(group)} |')
    lines += ['',
              '## 每题结果', '', '| 任务 | 难度标签 | 第 1 次 | 第 2 次 | 第 3 次 |',
              '|---|---|---|---|---|']
    for case in manifest['cases']:
        cid = case['instance_id']
        cells = []
        for index in range(1, 4):
            path = root / 'trials' / cid / str(index) / 'result.json'
            if path.exists():
                result = read_json(path)
                label = '通过' if result['passed'] else result['failure_class']
                cells.append(f'[{label}]({path})')
            else:
                cells.append('待完成')
        lines.append(f'| `{cid}` | {case["difficulty"]} | ' + ' | '.join(cells) + ' |')
    lines += ['', '难度标签来自数据集的人类修复时间标注，不是本轮模型耗时。', '', '## 失败类别', '']
    if summary['failures']:
        lines += [f'- `{kind}`：{count} 次。' for kind, count in summary['failures'].items()]
    else:
        lines += ['已完成的正式尝试尚无最终失败。']
    diagnoses = sorted((root / 'trials').glob('*/*/diagnosis.json'))
    if diagnoses:
        lines += ['', '## 已复查的失败', '']
        for path in diagnoses:
            diagnosis = read_json(path)
            label = f'{path.parent.parent.name} / 第 {path.parent.name} 次'
            finding = diagnosis.get('finding_zh', diagnosis['finding'])
            lines += [f'- [{label}]({path})：{finding}']
    if dev_null_trials:
        lines += ['', '## 已观察到的工具问题', '',
                  '工作区沙箱拒绝对 `/dev/null` 的写入，影响 pytest 默认日志处理或 Git 命令。'
                  '这是实际会话中的工具失败记录；独立判分在模型执行结束后运行，因此工具失败与最终修复通过可以同时发生。'
                  '本轮没有修改 Lato 来消除该问题。']
    lines += ['', '## 实验条件与局限', '',
              f'- 模型：`{manifest["model"]}`；每题 {manifest["trials"]} 次；'
              f'单次 Agent 上限 {manifest["agent_timeout_seconds"]} 秒；测试上限 {manifest["test_timeout_seconds"]} 秒。',
              f'- Lato 提交：`{run["lato_commit"]}`，含未提交改动；二进制 SHA-256：`{run["binary_sha256"]}`。'
              '完整源码快照和哈希另存，避免仅凭提交号无法复现。',
              '- 10 个 SymPy、6 个 pytest、3 个 Sphinx、1 个 Flask 任务；这是目的性选择的 Python 缺陷修复样本。'
              '不能据此推断其他语言、从零实现功能、大规模重构或长期自主开发的水平。',
              '- 每次使用干净源码快照；本轮不覆盖保留用户未提交修改、长上下文压缩或任务中断恢复。',
              '- Python / 依赖版本逐题锁定。每题原代码必须呈现预期失败，参考修复必须通过全部指定测试。'
              '上游日志中因空白截断的参数化测试 ID 映射到全部匹配变体，不能只挑一个通过的变体。',
              '- Agent 只收到原始问题和统一的环境说明。隐藏测试与参考修复不放入 Agent 工作区。'
              '判分恢复上游测试；允许新增测试，禁止修改已有测试和测试配置。',
              '- 正式通过只保证指定的上游测试通过，不代表整个仓库无回归。失败复查中的额外模块测试'
              '单独标注，未事后更改正式评分规则；没有对所有候选补丁运行所有仓库测试。',
              '- 工作区沙箱并不是强化的隐藏测试或网络隔离边界；本轮没有安全通过率结论。'
              '任务公开也意味着无法排除模型训练数据污染。',
              '- 模型名称是服务端别名，无法固定未公开的模型修订。token 与费用字段为 null，不冒充 0。',
              '- 成绩描述当前 Lato 与固定模型的组合。没有同模型、不同 Agent 的对照实验，'
              '因此不能把所有修复失败归因于 Lato，也不能从本轮推断其他模型的成绩。',
              '- 环境校准时排除了 Sphinx 11510：其参考修复在尝试的本机依赖版本下仍失败。'
              '正式模型调用前替换为通过预校验的 Sphinx 10673，保留了替换记录。',
              '- 一次尚未完成的启动试跑因未显式设置 LATO_HOME 而缺失持久日志，被中止并保留。'
              '它不属于正式 60 次结果；正式批次显式使用现有凭据目录，不复制凭据。', '',
              '## 证据', '',
              f'- [运行元数据]({root / "run.json"})',
              f'- [汇总 JSON]({root / "summary.json"})',
              f'- [逐次结果 JSONL]({root / "results.jsonl"})',
              f'- [源码哈希]({root / "lato-source-hashes.json"})',
              f'- [工具和硬件环境]({root / "environment-tools.json"})',
              f'- [最终完整性核查]({root / "integrity-audit.json"})',
              f'- [证据持久化与路径说明]({root / "relocation.json"})',
              f'- [固定任务清单]({HERE / "manifest.json"})',
              f'- [执行说明]({HERE / "README.md"})', '']
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text('\n'.join(lines))
    print(args.output)


if __name__ == '__main__':
    main()
