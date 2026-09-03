"""Out-of-workspace pytest event collector, loaded only during grading."""
import json
import os


def pytest_runtest_logreport(report):
    with open(os.environ['LATO_EVAL_EVENTS'], 'a', encoding='utf-8') as output:
        output.write(json.dumps({
            'nodeid': report.nodeid, 'when': report.when,
            'outcome': report.outcome, 'wasxfail': hasattr(report, 'wasxfail'),
        }) + '\n')


def pytest_collectreport(report):
    if report.failed:
        with open(os.environ['LATO_EVAL_EVENTS'], 'a', encoding='utf-8') as output:
            output.write(json.dumps({'collection_error': report.nodeid}) + '\n')
