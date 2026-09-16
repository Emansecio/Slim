"""Regression checks for measurement boundaries; no provider calls."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import daily
import report
import run


class MeasurementTests(unittest.TestCase):
    def test_error_cause_not_exit_code_or_git_help_text(self):
        cases = [
            ('shell', "exit 1\nTraceback (most recent call last):\nJSONDecodeError: Expecting ',' delimiter", 'invalid-json'),
            ('bash', 'usage: git diff --no-index ... --patch ...', 'git-command-usage'),
            ('bash', 'fatal: not a git repository', 'git-no-repository'),
            ('shell', 'exit 1\nAssertionError: output mismatch', 'validation-failed'),
            ('shell', 'exit 1\nserver refused operation', 'other-error'),
            ('shell', 'ParserError: Unexpected token', 'shell-syntax'),
            ('shell', 'exit 1 timed out', 'timeout'),
        ]
        for tool, text, expected in cases:
            with self.subTest(expected=expected):
                self.assertEqual(report.classify_error(tool, text), expected)

    def test_components_partition_full_items_and_measure_unicode_utf8(self):
        items = [{'role': 'developer', 'content': 'rules'},
                 {'role': 'user', 'content': 'a\u00e7\u00e3o'},
                 {'type': 'function_call_output', 'call_id': 'x', 'output': 'done'}]
        payload = {'instructions': 'system', 'tools': [], 'input': items}
        size = lambda x: len(json.dumps(x, ensure_ascii=False, separators=(',', ':')).encode())
        result = report.pi_request_components({'payload': payload})
        self.assertEqual(result['system_bytes'], size('system') + size(items[0]))
        self.assertEqual(result['history_bytes'], size(items[1]))
        self.assertEqual(result['tool_result_bytes'], size(items[2]))
        wire = dict(result, history_bytes=999, component_schema='slim-components-v1', payload=payload)
        self.assertEqual(report.pi_request_components(wire)['history_bytes'], 999)
        for item in [{'type': 'tool_result', 'content': 'done'},
                     {'role': 'user', 'content': [{'type': 'function_call_output', 'output': 'done'}]},
                     {'role': 'tool', 'content': 'done'}]:
            result = report.pi_request_components({'payload': {'messages': [item]}})
            self.assertEqual(result['tool_result_bytes'], size(item))
            self.assertEqual(result['history_bytes'], 0)

    def test_provider_error_with_zero_usage_is_not_a_complete_measurement(self):
        events = [dict(type='request', time_ms=1, payload={}),
                  dict(type='message_end', time_ms=2, message=dict(role='assistant',
                       usage=dict(input=0, output=0, cacheRead=0, cacheWrite=0),
                       stopReason='error', errorMessage='429: Monthly usage limit reached', content=[]))]
        with tempfile.TemporaryDirectory() as root:
            path = Path(root)
            (path / 'pi.audit.jsonl').write_text('\n'.join(json.dumps(x) for x in events), encoding='utf-8')
            result = report.summarize_pi(path)
            self.assertFalse(result['metrics_complete'])
            self.assertFalse(report.gate_ok(dict(exit_code=0, timed_out=False, validation_exit=0,
                                                fixtures_unchanged=True, metrics_complete=result['metrics_complete'])))

    def test_tool_turn_uses_call_id_not_next_response_timestamp(self):
        usage = dict(input=10, output=2, cacheRead=0, cacheWrite=0)
        events = [dict(type='request', time_ms=1, payload={}),
                  dict(type='message_end', time_ms=10, message=dict(role='assistant', usage=usage,
                       content=[dict(type='toolCall', id='c1', name='read')])),
                  dict(type='tool_execution_start', time_ms=11, toolCallId='c1', toolName='read', args={}),
                  dict(type='tool_execution_end', time_ms=12, toolCallId='c1', isError=False, result={}),
                  dict(type='request', time_ms=13, payload={}),
                  dict(type='message_end', time_ms=20, message=dict(role='assistant', usage=usage, content=[]))]
        with tempfile.TemporaryDirectory() as root:
            path = Path(root)
            (path / 'pi.audit.jsonl').write_text('\n'.join(json.dumps(x) for x in events), encoding='utf-8')
            result = report.summarize_pi(path)
            self.assertEqual(result['tools'][0]['turn'], 1)
            self.assertTrue(result['metrics_complete'])
            events.pop()
            (path / 'pi.audit.jsonl').write_text('\n'.join(json.dumps(x) for x in events), encoding='utf-8')
            self.assertFalse(report.summarize_pi(path)['metrics_complete'])

    def test_materialized_fixture_hash_preserves_line_ending_changes_and_deletions(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root); ws = path / 'slim.workspace'; ws.mkdir()
            data = b'unchanged\r\n'
            manifest = {'fixture_hash_kind': 'materialized-bytes-v1', 'fixtures': {
                'x.txt': {'sha256': hashlib.sha256(data).hexdigest()}}}
            (ws / 'x.txt').write_bytes(data)
            self.assertEqual(report.workspace_diff(path, 'slim', manifest)['modified'], [])
            (ws / 'x.txt').write_bytes(b'unchanged\n')
            self.assertEqual(report.workspace_diff(path, 'slim', manifest)['modified'], ['x.txt'])
            (ws / 'x.txt').unlink()
            self.assertEqual(report.workspace_diff(path, 'slim', manifest)['deleted'], ['x.txt'])

    def test_legacy_windows_fixture_digest_reconstruction(self):
        text = daily.SCENARIOS['repair_catalog']['files']['formatting.py']
        manifest = {'scenario': 'repair_catalog', 'harness': {'platform': 'win32'},
                    'fixtures': {'formatting.py': {'sha256': hashlib.sha256(text.encode()).hexdigest()}}}
        self.assertEqual(report.expected_fixtures(manifest)['formatting.py'],
                         hashlib.sha256(text.replace('\n', '\r\n').encode()).hexdigest())

    def test_daily_continues_and_retains_failed_campaign(self):
        with patch.object(daily, 'SCENARIOS', {'one': {}, 'two': {}}), \
             patch('sys.argv', ['daily.py', '--rounds', '1']), \
             patch.object(run, 'main', side_effect=[run.BenchmarkFailure(Path('failed')), Path('passed')]) as main, \
             patch('builtins.print') as output:
            with self.assertRaises(SystemExit):
                daily.main()
            self.assertEqual(main.call_count, 2)
            result = json.loads(output.call_args.args[0])
            self.assertEqual(result['campaigns'], ['failed', 'passed'])
            self.assertEqual(len(result['failures']), 1)

    def test_shuffle_keeps_each_scenario_balanced_and_reproducible(self):
        calls = []
        for _ in range(2):
            with patch.object(daily, 'SCENARIOS', {'one': {}, 'two': {}, 'three': {}}), \
                 patch('sys.argv', ['daily.py', '--rounds', '4', '--shuffle-seed', '42']), \
                 patch.object(run, 'main', return_value=Path('campaign')) as main, \
                 patch('builtins.print'):
                daily.main()
                calls.append([(c.kwargs['scenario'], c.args[0][0]) for c in main.call_args_list])
        self.assertEqual(calls[0], calls[1])
        for name in ['one', 'two', 'three']:
            self.assertEqual(sum(n == name and first == 'slim' for n, first in calls[0]), 2)
            self.assertEqual(sum(n == name and first == 'pi' for n, first in calls[0]), 2)

    def test_runner_records_actual_fixture_bytes_before_execution(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            exe = root / 'fake-executable'; exe.write_bytes(b'fixture')
            observed = []
            def process(spec_path, timing_path, stdout_path, stderr_path):
                spec = json.loads(Path(spec_path).read_text(encoding='utf-8'))
                cwd = Path(spec['cwd']); campaign = Path(spec_path).parent
                manifest = json.loads((campaign / 'manifest.json').read_text(encoding='utf-8'))
                for name, info in manifest['fixtures'].items():
                    self.assertEqual(hashlib.sha256((cwd / name).read_bytes()).hexdigest(), info['sha256'])
                observed.append(spec['agent'])
                Path(timing_path).write_text(json.dumps(dict(exit_code=0, timed_out=False, total_ms=1)))
                Path(stdout_path).write_text(''); Path(stderr_path).write_text('')
            with patch.object(run, 'HERE', root), patch.object(run, 'resolve_pi', return_value=exe), \
                 patch.object(run, 'resolve_slim', return_value=exe), \
                 patch.object(run, 'version_of', return_value='test'), \
                 patch.object(run, 'run_process', side_effect=process), patch('builtins.print'):
                campaign = run.main(['pi', 'slim'], files={'config/current.json': '{}\n'},
                                    check="print('PASS')", audit=False)
            self.assertEqual(observed, ['pi', 'slim'])
            manifest = json.loads((campaign / 'manifest.json').read_text(encoding='utf-8'))
            self.assertEqual(manifest['fixture_hash_kind'], 'materialized-bytes-v1')
            self.assertFalse(Path(manifest['workspace']).exists())


if __name__ == '__main__':
    unittest.main()
