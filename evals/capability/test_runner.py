import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

import runner


class GradingTests(unittest.TestCase):
    def test_missing_skipped_ambiguous_never_pass(self):
        actual = {'a.py::test_a': 'passed', 'b.py::test_a': 'passed',
                  'a.py::test_b': 'skipped'}
        result = runner.expected_outcomes(['test_a', 'test_b', 'test_c'], actual, True)
        self.assertEqual(result, {'test_a': 'ambiguous', 'test_b': 'skipped', 'test_c': 'missing'})

    def test_teardown_failure_overrides_call_success(self):
        result, errors = runner.outcomes([
            {'nodeid': 'test_a', 'when': 'call', 'outcome': 'passed'},
            {'nodeid': 'test_a', 'when': 'teardown', 'outcome': 'failed'},
            {'collection_error': 'broken_module'},
        ])
        self.assertEqual(result['test_a'], 'failed')
        self.assertEqual(errors, ['broken_module'])

    def test_upstream_truncated_parameter_ids_require_every_variant(self):
        name = 'testing/test_mark.py::test_mark[not'
        actual = {name + ' one-result1]': 'passed', name + ' two-result2]': 'passed'}
        self.assertEqual(runner.expected_outcomes([name], actual)[name], 'passed')
        actual[name + ' two-result2]'] = 'failed'
        self.assertEqual(runner.expected_outcomes([name], actual)[name], 'failed')

    def test_base_requires_real_failure_and_regression_pass(self):
        row = {'repo': 'example/repo', 'FAIL_TO_PASS': '["test_a"]',
               'PASS_TO_PASS': '["test_b"]'}
        result = {'outcomes': {'test_a': 'failed', 'test_b': 'passed'},
                  'collection_errors': [], 'timed_out': False}
        self.assertTrue(runner.assess(row, result)['valid'])
        self.assertFalse(runner.assess(row, result, reference=True)['valid'])
        result['outcomes']['test_a'] = 'passed'
        self.assertFalse(runner.assess(row, result)['valid'])
        self.assertTrue(runner.assess(row, result, reference=True)['valid'])
        result['outcomes'].pop('test_b')
        self.assertFalse(runner.assess(row, result, reference=True)['valid'])

    def test_report_keeps_invalid_and_unattempted_denominators(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = {'trials': 3, 'cases': [
                {'instance_id': 'a', 'repo': 'x'}, {'instance_id': 'b', 'repo': 'y'}]}
            runner.write_json(root / 'preflight/a/result.json', {'valid': True})
            runner.write_json(root / 'preflight/b/result.json', {'valid': False})
            runner.write_json(root / 'trials/a/1/result.json',
                              {'trial': 1, 'passed': False, 'failure_class': 'provider'})
            report = runner.report(root, manifest)
            self.assertEqual(report['planned_trials'], 6)
            self.assertEqual(report['completed_trials'], 1)
            self.assertEqual(report['invalid_tasks'], 1)
            self.assertEqual(report['failures'], {'provider': 1})


class ProcessAndPatchTests(unittest.TestCase):
    def test_timeout_is_bounded_and_preserves_partial_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'out.log'
            started = time.monotonic()
            result = runner.command([sys.executable, '-u', '-c',
                                     'import time; print("started"); time.sleep(30)'], tmp, log, .15)
            self.assertTrue(result['timed_out'])
            self.assertLess(time.monotonic() - started, 5)
            self.assertIn('started', log.read_text())

    def test_patch_retains_new_tests_untracked_and_agent_commits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            def git(*args):
                return subprocess.check_output(['git', *args], cwd=root)
            git('init', '-q')
            git('config', 'user.name', 'Test')
            git('config', 'user.email', 'test@example.invalid')
            (root / 'source.py').write_text('old\n')
            git('add', '-A')
            git('commit', '-qm', 'base')
            base = git('rev-parse', 'HEAD').decode().strip()
            (root / 'source.py').write_text('changed\n')
            git('commit', '-qam', 'agent accidentally committed')
            (root / 'test_new.py').write_text('new test\n')
            patch = runner.capture_patch(root, base).decode()
            self.assertIn('+changed', patch)
            self.assertIn('b/test_new.py', patch)


if __name__ == '__main__':
    unittest.main()
