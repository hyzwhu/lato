#!/usr/bin/env python3
"""Native, bounded SWE-bench-derived Lato baseline. See README.md for scope."""
from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import urllib.request

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
COMMON = ['setuptools==68.2.2', 'wheel==0.41.2', 'pytest==7.4.4',
          'mpmath==1.2.1', 'numpy==1.24.4', 'hypothesis==6.82.6',
          'packaging==23.1', 'attrs==23.1.0', 'pluggy==1.0.0', 'py==1.11.0',
          'toml==0.10.2', 'iniconfig==2.0.0', 'exceptiongroup==1.1.3',
          'typing_extensions==4.7.1', 'tomli==2.0.1', 'pexpect==4.8.0',
          'nose==1.3.7', 'xmlschema==2.4.0', 'more-itertools==9.1.0']


def read_json(path):
    return json.loads(Path(path).read_text())


def write_json(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')
    temporary.replace(path)


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(args, cwd, log, timeout=180, env=None):
    """No shell interpolation; retain partial output and kill the process group."""
    log = Path(log)
    log.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    timed_out = False
    with log.open('wb') as output:
        proc = subprocess.Popen([str(x) for x in args], cwd=cwd, env=env,
                                stdout=output, stderr=subprocess.STDOUT,
                                start_new_session=True)
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
        finally:
            # Also clean up background descendants after a successful parent exit.
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            if proc.poll() is None:
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
    return {'exit_code': proc.returncode, 'timed_out': timed_out,
            'duration_seconds': round(time.monotonic() - started, 3)}


def checked(args, cwd, log, timeout=180, env=None):
    result = command(args, cwd, log, timeout, env)
    if result['exit_code'] != 0 or result['timed_out']:
        raise RuntimeError(f'Command failed: {args[0]} (see {log})')
    return result


def git_output(cwd, *args):
    return subprocess.check_output(['git', *args], cwd=cwd)


def patch_files(patch):
    return re.findall(r'^diff --git a/(.+?) b/.+$', patch, flags=re.M)


def protected(path):
    parts = Path(path).parts
    return (any(x in ('tests', 'testing') for x in parts)
            or Path(path).name in ('conftest.py', 'pytest.ini', 'tox.ini',
                                   'setup.cfg', 'pyproject.toml'))


def capture_patch(workspace, base_revision='HEAD'):
    # Include untracked files and both staged/unstaged changes, including new tests.
    git_output(workspace, 'add', '-A', '--', '.')
    return git_output(workspace, 'diff', '--cached', '--binary', base_revision)


def outcomes(events):
    grouped = collections.defaultdict(list)
    collection_errors = []
    for event in events:
        if 'collection_error' in event:
            collection_errors.append(event['collection_error'])
        else:
            grouped[event['nodeid']].append(event)
    result = {}
    for nodeid, reports in grouped.items():
        if any(x['outcome'] == 'failed' for x in reports):
            result[nodeid] = 'failed'
        elif any(x['outcome'] == 'skipped' or x.get('wasxfail') for x in reports):
            result[nodeid] = 'skipped'
        elif any(x['when'] == 'call' and x['outcome'] == 'passed' for x in reports):
            result[nodeid] = 'passed'
        else:
            result[nodeid] = 'missing'
    return result, collection_errors


def expected_outcomes(required, actual, bare_names=False):
    result = {}
    for name in required:
        matches = [state for node, state in actual.items()
                   if (node.split('::')[-1] == name if bare_names else node == name)]
        if not matches and not bare_names and '[' in name and not name.endswith(']'):
            # Upstream console parsers truncated parametrized IDs at whitespace.
            # Require ALL concrete variants for that recorded prefix, never pick
            # a convenient passing variant or accept a missing one.
            variants = [state for node, state in actual.items() if node.split()[0] == name]
            if variants:
                result[name] = ('passed' if all(x == 'passed' for x in variants)
                                else 'failed' if any(x == 'failed' for x in variants)
                                else 'skipped')
                continue
        # An ambiguous bare function name cannot silently count as passing.
        result[name] = matches[0] if len(matches) == 1 else ('missing' if not matches else 'ambiguous')
    return result


def assess(row, result, reference=False):
    bare = row['repo'] == 'sympy/sympy'
    f2p = expected_outcomes(json.loads(row['FAIL_TO_PASS']), result['outcomes'], bare)
    p2p = expected_outcomes(json.loads(row['PASS_TO_PASS']), result['outcomes'], bare)
    expected = 'passed' if reference else 'failed'
    ok = (bool(f2p) and all(x == expected for x in f2p.values())
          and all(x == 'passed' for x in p2p.values())
          and not result['collection_errors'] and not result['timed_out'])
    return {'valid': ok, 'fail_to_pass': f2p, 'pass_to_pass': p2p}


def dataset(root, manifest):
    path = root / 'dataset.json'
    if not path.exists():
        rows = []
        for offset in range(0, 500, 100):
            url = ('https://datasets-server.huggingface.co/rows?dataset='
                   'princeton-nlp/SWE-bench_Verified&config=default&split=test'
                   f'&length=100&offset={offset}')
            with urllib.request.urlopen(url, timeout=90) as response:
                rows.extend(item['row'] for item in json.load(response)['rows'])
        write_json(path, rows)
    rows = {row['instance_id']: row for row in read_json(path)}
    for case in manifest['cases']:
        row = rows[case['instance_id']]
        for key in ('repo', 'base_commit', 'problem_statement'):
            if case[key] != row[key]:
                raise ValueError(f'Manifest/source mismatch: {case["instance_id"]}: {key}')
    return rows


def fresh_snapshot(root, row, destination):
    if destination.exists():
        raise ValueError(f'Refusing to overwrite existing workspace: {destination}')
    archive = root / 'archives' / (row['instance_id'] + '.tar.gz')
    if not archive.exists():
        archive.parent.mkdir(parents=True, exist_ok=True)
        url = f'https://codeload.github.com/{row["repo"]}/tar.gz/{row["base_commit"]}'
        with urllib.request.urlopen(url, timeout=120) as response:
            archive.write_bytes(response.read())
    destination.mkdir(parents=True)
    with tarfile.open(archive) as package:
        for member in package.getmembers():
            parts = Path(member.name).parts
            if len(parts) < 2:
                continue
            member.name = str(Path(*parts[1:]))
            package.extract(member, destination, filter='data')
    log = destination.parent / (destination.name + '-git.log')
    for args in (['init', '-q'], ['config', 'user.email', 'lato-eval@example.invalid'],
                 ['config', 'user.name', 'Lato Evaluation'], ['add', '-A'],
                 ['commit', '-q', '-m', f'Source snapshot {row["base_commit"]}']):
        checked(['git', *args], destination, log)
    # Local environment/agent scratch files must never become candidate patches.
    with (destination / '.git/info/exclude').open('a') as f:
        f.write('\n.venv\n.lato/\n__pycache__/\n.pytest_cache/\n.hypothesis/\n')
    return destination


def apply_patch(workspace, patch, path):
    path.write_text(patch)
    checked(['git', 'apply', '--whitespace=nowarn', str(path)], workspace,
            path.with_suffix('.log'))


def environment(root, row, workspace, logs):
    venv = root / 'envs' / row['instance_id']
    python = venv / 'bin/python'
    if not (venv / 'lato-ready.json').exists():
        if not python.exists():
            py_version = '3.9' if row['repo'] == 'pytest-dev/pytest' else '3.11'
            checked(['uv', 'venv', '--python', py_version, str(venv)], root, logs / 'venv.log')
        deps = list(COMMON)
        if row['repo'] == 'pallets/flask':
            deps += ['Flask==2.3.2', 'Werkzeug==2.3.7', 'Jinja2==3.1.2',
                     'itsdangerous==2.1.2', 'click==8.1.7', 'blinker==1.6.2',
                     'asgiref==3.7.2']
        elif row['repo'] == 'sphinx-doc/sphinx':
            deps += ['Sphinx==7.2.6', 'docutils==0.18.1', 'Jinja2==3.1.2',
                     'Pygments==2.16.1', 'sphinxcontrib-applehelp==1.0.7',
                     'sphinxcontrib-devhelp==1.0.5', 'sphinxcontrib-htmlhelp==2.0.4',
                     'sphinxcontrib-qthelp==1.0.6', 'sphinxcontrib-serializinghtml==1.1.9',
                     'sphinxcontrib-jsmath==1.0.1', 'alabaster==0.7.13',
                     'cython==0.29.36', 'html5lib==1.1', 'defusedxml==0.7.1']
        if row['instance_id'] == 'sympy__sympy-23262':
            deps += ['scipy==1.10.1']
        checked(['uv', 'pip', 'install', '--python', str(python), *deps],
                root, logs / 'install.log', 300)
        write_json(venv / 'lato-ready.json', {'dependencies': deps})
    return python


def test_environment(python, workspace):
    env = os.environ.copy()
    env.update({'PATH': str(python.parent) + os.pathsep + env.get('PATH', ''),
                'PYTHONPATH': os.pathsep.join([str(workspace / 'src'), str(workspace), str(HERE)]),
                'PYTHONDONTWRITEBYTECODE': '1', 'PYTEST_DISABLE_PLUGIN_AUTOLOAD': '1',
                'PYTHONHASHSEED': '0', 'MPLBACKEND': 'Agg',
                'TMPDIR': str(workspace.parent / 'os-tmp')})
    (workspace.parent / 'os-tmp').mkdir(parents=True, exist_ok=True)
    return env


def source_setup(python, workspace, row):
    # pytest generates this module at package-build time. Keep the generated file
    # out of the candidate patch and supply identical version data in every phase.
    if row['repo'] == 'pytest-dev/pytest':
        version = row['version'] + '.0'
        version_file = workspace / 'src/_pytest/_version.py'
        if not version_file.exists():
            version_file.write_text(f'version = {version!r}\nversion_tuple = tuple(map(int, version.split(".")))\n')
            with (workspace / '.git/info/exclude').open('a') as f:
                f.write('\nsrc/_pytest/_version.py\n')
    (workspace / '.venv').symlink_to(python.parent.parent, target_is_directory=True)


def grade(row, workspace, python, outdir, timeout):
    outdir.mkdir(parents=True, exist_ok=True)
    events_path = outdir / 'test-events.jsonl'
    events_path.write_text('')
    env = test_environment(python, workspace)
    env['LATO_EVAL_EVENTS'] = str(events_path)
    files = sorted({p for p in patch_files(row['test_patch'])
                    if Path(p).name.startswith('test_') and p.endswith('.py')})
    if not files:
        raise ValueError('No test modules in upstream test patch')
    args = [python, '-m', 'pytest', '-p', 'lato_eval_plugin', '-o', 'addopts=',
            '-o', 'cache_dir=.pytest_cache', '--basetemp=' + str(workspace.parent / 'pytest-tmp')]
    if row['repo'] == 'pytest-dev/pytest':
        args += ['-p', 'pytester']
    args += ['-q', *files]
    result = command(args, workspace, outdir / 'tests.log', timeout, env)
    parsed = [json.loads(line) for line in events_path.read_text().splitlines()]
    result['outcomes'], result['collection_errors'] = outcomes(parsed)
    write_json(outdir / 'tests.json', result)
    return result


def preflight(root, manifest, rows, selected):
    for case in selected:
        row = rows[case['instance_id']]
        case_dir = root / 'preflight' / row['instance_id']
        result_file = case_dir / 'result.json'
        if result_file.exists():
            print(f'preflight cached {row["instance_id"]}', flush=True)
            continue
        case_dir.mkdir(parents=True, exist_ok=True)
        result = {'instance_id': row['instance_id'], 'valid': False}
        try:
            workspace = fresh_snapshot(root, row, case_dir / 'workspace')
            python = environment(root, row, workspace, case_dir)
            source_setup(python, workspace, row)
            apply_patch(workspace, row['test_patch'], case_dir / 'test.patch')
            base = grade(row, workspace, python, case_dir / 'base', manifest['test_timeout_seconds'])
            result['base'] = assess(row, base)
            apply_patch(workspace, row['patch'], case_dir / 'reference.patch')
            reference = grade(row, workspace, python, case_dir / 'reference', manifest['test_timeout_seconds'])
            result['reference'] = assess(row, reference, reference=True)
            result['valid'] = result['base']['valid'] and result['reference']['valid']
            result['archive_sha256'] = sha256(root / 'archives' / (row['instance_id'] + '.tar.gz'))
            checked(['uv', 'pip', 'freeze', '--python', python], root, case_dir / 'requirements.lock')
        except Exception as error:
            result['error'] = str(error)
        write_json(result_file, result)
        print(f'preflight {row["instance_id"]}: {"VALID" if result["valid"] else "INVALID"}', flush=True)


def prompt_for(row):
    return (
        'Fix the issue below in this repository. Inspect the code, implement the fix, '
        'run relevant tests, and report what changed and what you verified. '
        'Preserve unrelated behavior. Do not weaken or delete existing tests. '
        'Use only this workspace: do not search the internet, other local workspaces, '
        'upstream history, reference solutions or evaluation artifacts. '
        'The repository is in repo/. Dependencies are installed in repo/.venv. '
        'From repo/, run tests with '
        '`TMPDIR=../os-tmp PYTEST_DISABLE_PLUGIN_AUTOLOAD=1 PYTHONDONTWRITEBYTECODE=1 '
        'PYTHONHASHSEED=0 PYTHONPATH=src:. .venv/bin/python -m pytest -o addopts= '
        '--basetemp=../pytest-tmp <test paths>`. For pytest repository tests also '
        'pass `-p pytester`. Temporary directories are outside repo/ to avoid '
        'inheriting the repository test configuration in nested test projects. '
        'You may add regression tests in new files. Do not edit existing tests '
        'or test configuration. Do not commit your changes.\n\n'
        + row['problem_statement'])


def collect_journal(home, workspace, started, target):
    matching = []
    for path in (home / 'sessions').glob('*/events.jsonl'):
        if path.stat().st_mtime < started - 2:
            continue
        contents = path.read_text(errors='replace')
        if str(workspace) in contents:
            matching.append((path, contents))
    if len(matching) != 1:
        return {'journal_error': f'Expected one attributable journal; found {len(matching)}',
                'tool_calls': None}
    path, contents = matching[0]
    target.write_text(contents)
    records = []
    for line in contents.splitlines():
        try:
            records.append(json.loads(line).get('record', {}))
        except json.JSONDecodeError:
            pass
    calls = [r for r in records if r.get('type') == 'tool_call_requested']
    return {'session_id': path.parent.name, 'tool_calls': len(calls),
            'tool_names': dict(collections.Counter(r.get('name', 'unknown') for r in calls))}


def freeze_run(root, manifest, binary):
    metadata_file = root / 'run.json'
    wanted = {'manifest_sha256': sha256(HERE / 'manifest.json'),
              'binary_sha256': sha256(binary), 'model': manifest['model'],
              'runner_sha256': sha256(__file__),
              'collector_sha256': sha256(HERE / 'lato_eval_plugin.py')}
    if metadata_file.exists():
        existing = read_json(metadata_file)
        if any(existing.get(k) != v for k, v in wanted.items()):
            raise ValueError('Run inputs changed; use a new output directory, do not mix baselines')
        return
    (root / 'lato-source.patch').write_bytes(git_output(REPO, 'diff', '--binary', 'HEAD'))
    source_hashes = {}
    with tarfile.open(root / 'lato-source.tar.gz', 'w:gz') as archive:
        names = git_output(REPO, 'ls-files', '-c', '-o', '--exclude-standard', '-z').decode().split('\0')
        for name in sorted(set(names) - {''}):
            path = REPO / name
            if path.is_file() and not path.is_symlink():
                source_hashes[name] = sha256(path)
                archive.add(path, arcname=name, recursive=False)
    write_json(root / 'lato-source-hashes.json', source_hashes)
    wanted.update({'created_at': time.strftime('%Y-%m-%dT%H:%M:%S%z'),
                   'platform': platform.platform(), 'python': sys.version,
                   'lato_commit': git_output(REPO, 'rev-parse', 'HEAD').decode().strip(),
                   'lato_dirty': bool(git_output(REPO, 'status', '--porcelain')),
                   'binary': str(binary), 'dataset_sha256': sha256(root / 'dataset.json'),
                   'tokens': None, 'cost': None})
    write_json(metadata_file, wanted)


def run_trials(root, manifest, rows, selected, binary):
    freeze_run(root, manifest, binary)
    home = Path(os.environ.get('LATO_HOME', str(Path.home() / '.lato')))
    for case in selected:
        row = rows[case['instance_id']]
        pre = root / 'preflight' / row['instance_id'] / 'result.json'
        if not pre.exists() or not read_json(pre)['valid']:
            print(f'skipping invalid/unprepared case {row["instance_id"]}', flush=True)
            continue
        if sha256(root / 'archives' / (row['instance_id'] + '.tar.gz')) != read_json(pre)['archive_sha256']:
            raise ValueError(f'Source archive changed after preflight: {row["instance_id"]}')
        for trial in range(1, manifest['trials'] + 1):
            trial_dir = root / 'trials' / row['instance_id'] / str(trial)
            result_file = trial_dir / 'result.json'
            if result_file.exists():
                continue
            # An interrupted attempt must be audited, never silently restarted.
            if trial_dir.exists():
                raise ValueError(f'Incomplete attempt needs inspection: {trial_dir}')
            trial_dir.mkdir(parents=True)
            result = {'instance_id': row['instance_id'], 'trial': trial,
                      'model': manifest['model'], 'passed': False,
                      'failure_class': None, 'tokens': None, 'cost': None}
            print(f'start {row["instance_id"]} trial {trial}', flush=True)
            try:
                sandbox = trial_dir / 'workspace'
                workspace = fresh_snapshot(root, row, sandbox / 'repo')
                base_revision = git_output(workspace, 'rev-parse', 'HEAD').decode().strip()
                python = root / 'envs' / row['instance_id'] / 'bin/python'
                source_setup(python, workspace, row)
                (sandbox / 'os-tmp').mkdir()
                prompt = prompt_for(row)
                (trial_dir / 'prompt.txt').write_text(prompt)
                env = os.environ.copy()
                env['LATO_HOME'] = str(home)
                env['PATH'] = str(python.parent) + os.pathsep + env.get('PATH', '')
                env['PYTHONDONTWRITEBYTECODE'] = '1'
                env['PYTEST_DISABLE_PLUGIN_AUTOLOAD'] = '1'
                started = time.time()
                agent = command([binary, '-p', '--sandbox', 'workspace', '--model',
                                 manifest['model'], prompt], sandbox,
                                trial_dir / 'agent.log', manifest['agent_timeout_seconds'], env)
                result['agent'] = agent
                result.update(collect_journal(home, sandbox, started, trial_dir / 'journal.jsonl'))
                candidate = capture_patch(workspace, base_revision)
                (trial_dir / 'candidate.patch').write_bytes(candidate)
                changed = git_output(workspace, 'diff', '--cached', '--name-status', base_revision).decode()
                (trial_dir / 'changed-files.txt').write_text(changed)
                # New tests are allowed; edits to existing test/config files are
                # flagged separately, and upstream test files are restored to grade.
                protected_changes = [line for line in changed.splitlines()
                                     if protected(line.split('\t')[-1])
                                     and (line[:1] != 'A' or Path(line.split('\t')[-1]).name
                                          in ('conftest.py', 'pytest.ini', 'tox.ini',
                                              'setup.cfg', 'pyproject.toml'))]
                result['protected_changes'] = protected_changes
                if agent['timed_out']:
                    result['failure_class'] = 'agent_timeout'
                elif agent['exit_code']:
                    log = (trial_dir / 'agent.log').read_text(errors='replace').lower()
                    provider_terms = ('rate_limit', 'rate limit', '429', '401', '403',
                                      'quota', 'usage limit', 'websocket', 'http request')
                    result['failure_class'] = ('provider' if any(t in log for t in provider_terms)
                                               else 'lato_harness')
                grader = fresh_snapshot(root, row, trial_dir / 'grader-workspace')
                source_setup(python, grader, row)
                if candidate:
                    checked(['git', 'apply', '--whitespace=nowarn', str(trial_dir / 'candidate.patch')],
                            grader, trial_dir / 'candidate-apply.log')
                for path in patch_files(row['test_patch']):
                    # Reset existing hidden test files and remove upstream-added
                    # paths before applying the immutable evaluation patch.
                    target = grader / path
                    try:
                        original = git_output(grader, 'show', f'HEAD:{path}')
                    except subprocess.CalledProcessError:
                        if target.exists():
                            target.unlink()
                    else:
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.write_bytes(original)
                apply_patch(grader, row['test_patch'], trial_dir / 'test.patch')
                grading = grade(row, grader, python, trial_dir / 'grading', manifest['test_timeout_seconds'])
                result['grading'] = assess(row, grading, reference=True)
                result['passed'] = (result['grading']['valid'] and not protected_changes
                                    and not result['failure_class'])
                if not result['passed'] and not result['failure_class']:
                    result['failure_class'] = 'scope_violation' if protected_changes else 'agent'
            except Exception as error:
                result['failure_class'] = result['failure_class'] or 'infrastructure'
                result['error'] = str(error)
            write_json(result_file, result)
            report(root, manifest)
            print(f'finish {row["instance_id"]} trial {trial}: '
                  f'{"PASS" if result["passed"] else result["failure_class"]}', flush=True)


def report(root, manifest):
    results = []
    cases = []
    for case in manifest['cases']:
        cid = case['instance_id']
        trials = [read_json(p) for p in sorted((root / 'trials' / cid).glob('*/result.json'))]
        results.extend(trials)
        pre = root / 'preflight' / cid / 'result.json'
        cases.append({'instance_id': cid, 'repo': case['repo'],
                      'preflight': read_json(pre)['valid'] if pre.exists() else None,
                      'completed': len(trials), 'passed': sum(x['passed'] for x in trials),
                      'first_trial_passed': any(x['trial'] == 1 and x['passed'] for x in trials),
                      'failures': [x['failure_class'] for x in trials if not x['passed']]})
    summary = {'planned_tasks': len(cases), 'planned_trials': len(cases) * manifest['trials'],
               'valid_tasks': sum(x['preflight'] is True for x in cases),
               'invalid_tasks': sum(x['preflight'] is False for x in cases),
               'completed_trials': len(results), 'passed_trials': sum(x['passed'] for x in results),
               'first_trial_successes': sum(x['first_trial_passed'] for x in cases),
               'tasks_at_least_2_of_3': sum(x['passed'] >= 2 for x in cases),
               'tasks_all_3': sum(x['passed'] == 3 for x in cases),
               'failures': dict(collections.Counter(x['failure_class'] for x in results if not x['passed'])),
               'cases': cases}
    write_json(root / 'summary.json', summary)
    with (root / 'results.jsonl').open('w') as f:
        for result in results:
            f.write(json.dumps(result, ensure_ascii=False) + '\n')
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['prepare', 'run', 'report'])
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--case', action='append', default=[])
    parser.add_argument('--binary', type=Path, default=Path.home() / '.cargo/bin/lato')
    args = parser.parse_args()
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    root.chmod(0o700)
    manifest = read_json(HERE / 'manifest.json')
    selected = [x for x in manifest['cases'] if not args.case or x['instance_id'] in args.case]
    if args.case and set(args.case) - {x['instance_id'] for x in selected}:
        parser.error('Unknown case ID')
    if args.command == 'report':
        print(json.dumps(report(root, manifest), ensure_ascii=False, indent=2))
        return
    rows = dataset(root, manifest)
    if args.command == 'prepare':
        preflight(root, manifest, rows, selected)
    else:
        run_trials(root, manifest, rows, selected, args.binary.resolve())
    report(root, manifest)


if __name__ == '__main__':
    main()
