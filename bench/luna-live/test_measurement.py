"""Regression checks for measurement boundaries; no provider calls."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import analyze
import daily
import report
import run


MODEL = 'gpt-5.4-high'


def write_campaign(root, name='20260101-000000Z-a', *, slim_exit=0, pi_exit=0, usage_models=None,
                   usage_provider='openai-codex', tool_fact=True, counters=None, session='v2'):
    """Campanha sintetica com os dois bracos (sem chamada a provider)."""
    cdir = Path(root) / name
    cdir.mkdir()
    manifest = {'model': MODEL, 'provider': 'openai-codex', 'effort': 'high', 'scenario': 'merge_ranges',
                'prompt': 'prompt', 'order': ['pi', 'slim'], 'fixture_hash_kind': 'materialized-bytes-v1',
                'fixtures': {}, 'harness': {'platform': 'linux'}}
    (cdir / 'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
    usage = dict(input=10, output=2, cacheRead=0, cacheWrite=0)
    audit = [dict(type='request', time_ms=1, model=MODEL, reasoning={'effort': 'high'}, payload={}),
             dict(type='message_end', time_ms=10, message=dict(role='assistant', model=MODEL, usage=usage,
                  stopReason='endTurn', content=[dict(type='toolCall', id='c1', name='read')])),
             dict(type='tool_execution_start', time_ms=11, toolCallId='c1', toolName='read', args={}),
             dict(type='tool_execution_end', time_ms=12, toolCallId='c1', isError=False, result={})]
    (cdir / 'pi.audit.jsonl').write_text('\n'.join(json.dumps(x) for x in audit), encoding='utf-8')
    models = usage_models if usage_models is not None else [MODEL]
    requests = []
    for model in models:
        request = {'uncached_input_tokens': 10, 'cache_read_tokens': 0, 'cache_write_tokens': 0,
                   'output_tokens': 2, 'reasoning_tokens': 0, 'provider_latency_ms': 5,
                   'provider': usage_provider, 'model': model}
        request.update(counters or {})
        requests.append(request)
    (cdir / 'slim.stdout.jsonl').write_text(json.dumps({'usage': {'requests': requests,
        'provider_turns': len(requests)}, 'usage_complete': True, 'usage_unknown': False}), encoding='utf-8')
    entries = []
    if session == 'v1':
        # sem ToolFinished: desfecho nunca provado, em nenhum dos dois medidores
        entries = [dict(type='event', event=dict(seq=1, kind=dict(type='ContextSnapshot'))),
                   dict(type='event', event=dict(seq=2, kind=dict(type='ToolStarted', call_id='c1',
                                                                 name='read', arguments={}))),
                   dict(type='event', event=dict(seq=3, kind=dict(type='ToolOutput', call_id='c1',
                                                                 output='ok')))]
    else:
        for _ in requests:
            entries.append(dict(type='entry', entry=dict(role='assistant',
                                                         tool_calls=[dict(id='c1', name='read', arguments={})])))
            entries.append(dict(type='entry', entry=dict(role='tool', tool_call_id='c1', content='ok')))
        if tool_fact:
            entries.append(dict(type='fact', fact=dict(namespace='tool.v1', key='c1',
                                                      value=dict(success=True, duration_ms=3))))
    (cdir / 'slim.session.jsonl').write_text('\n'.join(json.dumps(x) for x in entries), encoding='utf-8')
    (cdir / 'pi.timing.json').write_text(json.dumps(dict(exit_code=pi_exit, timed_out=False, total_ms=100)), encoding='utf-8')
    (cdir / 'pi.validation.json').write_text(json.dumps(dict(exit_code=0, fixtures_unchanged=True)), encoding='utf-8')
    (cdir / 'slim.timing.json').write_text(json.dumps(dict(exit_code=slim_exit, timed_out=False, total_ms=200)), encoding='utf-8')
    (cdir / 'slim.validation.json').write_text(json.dumps(dict(exit_code=0, fixtures_unchanged=True)), encoding='utf-8')
    return cdir


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


class MeasurementHonestyTests(unittest.TestCase):
    def test_unproven_tool_outcome_is_unknown_not_failure_in_both_meters(self):
        with tempfile.TemporaryDirectory() as root:
            unproven = report.summarize_arm(write_campaign(root, '1-orphan', session='v1'), 'slim')
            self.assertEqual((unproven['tool_failures'], unproven['tool_unknown'], unproven['tool_succeeded']),
                             (0, 1, 0))
            self.assertEqual(unproven['tools'][0]['evidence'], 'absence')
            self.assertEqual(report.tool_outcomes(unproven['tools']), {'succeeded': 0, 'failed': 0, 'unknown': 1})
            audited = analyze.audit(unproven['campaign_path'], unproven['campaign_path'])['slim']
            self.assertEqual((audited['tool_failures'], audited['tool_unknown'], audited['tool_succeeded']),
                             (0, 1, 0))
            self.assertIsNone(audited['tools'][0].get('success'))
            self.assertEqual(audited['tools'][0]['evidence'], 'absence')
            self.assertEqual(audited['tool_calls'], 1)

            proven = report.summarize_arm(write_campaign(root, '2-fact'), 'slim')
            self.assertEqual((proven['tool_failures'], proven['tool_unknown'], proven['tool_succeeded'],
                              proven['tool_inferred']), (0, 0, 1, 0))
            self.assertEqual(proven['tools'][0]['evidence'], 'fact')

            inferred = report.summarize_arm(write_campaign(root, '3-nofact', tool_fact=False), 'slim')
            self.assertEqual((inferred['tool_failures'], inferred['tool_inferred']), (0, 1))
            self.assertEqual(inferred['tools'][0]['evidence'], 'inferred')
            self.assertEqual(analyze.audit(inferred['campaign_path'], inferred['campaign_path'])['slim']['tools'][0]['success'],
                             None)

    def test_route_contradiction_blocks_gate_and_absence_only_warns(self):
        with tempfile.TemporaryDirectory() as root:
            same = report.summarize_arm(write_campaign(root, '1-same'), 'slim')
            self.assertTrue(report.gate_ok(same))
            self.assertTrue(same['identity']['model_matches_manifest'])
            self.assertTrue(same['identity']['provider_matches_manifest'])

            renamed = report.summarize_arm(write_campaign(root, '2-model', usage_models=['gpt-5.3']), 'slim')
            self.assertFalse(report.gate_ok(renamed))
            self.assertFalse(renamed['identity']['model_matches_manifest'])

            switched = report.summarize_arm(write_campaign(root, '3-swap', usage_models=[MODEL, 'gpt-5.3']), 'slim')
            self.assertEqual(len(switched['identity']['inconsistent_turns']), 1)
            self.assertFalse(report.gate_ok(switched))

            rerouted = report.summarize_arm(write_campaign(root, '4-provider', usage_provider='local'), 'slim')
            self.assertFalse(report.gate_ok(rerouted))
            self.assertFalse(rerouted['identity']['provider_matches_manifest'])

            blind = report.summarize_arm(write_campaign(root, '5-blind', usage_models=[''], usage_provider=''), 'slim')
            self.assertEqual(blind['identity']['turns_with_identity'], 0)
            self.assertTrue(report.gate_ok(blind))

    def test_ledger_counters_reach_the_report(self):
        counters = dict(retry_count=2, cancelled=1, response_cache_hit=3, usage_unknown=1,
                        time_to_first_byte_ms=40, time_to_first_semantic_ms=60,
                        estimation_error_tokens=7)
        with tempfile.TemporaryDirectory() as root:
            cdir = write_campaign(root, '20260101-000000Z-a', counters=counters)
            slim = report.summarize_arm(cdir, 'slim')
            self.assertEqual((slim['retries'], slim['cancelled_requests'], slim['cache_hits'],
                              slim['unknown_requests'], slim['ttfb_ms_sum'], slim['ttfs_ms_sum'],
                              slim['estimation_error_tokens_sum']), (2, 1, 3, 1, 40, 60, 7))
            self.assertTrue(slim['metrics_complete'])
            (Path(root) / 'r.md').write_text('', encoding='utf-8')
            (Path(root) / 'r.json').write_text('', encoding='utf-8')
            with patch('sys.argv', ['report.py', str(cdir), '--output-md', str(Path(root) / 'r.md'),
                                   '--output-json', str(Path(root) / 'r.json')]), patch('builtins.print'):
                report.main()
            payload = json.loads((Path(root) / 'r.json').read_text(encoding='utf-8'))
            self.assertEqual(payload['identity']['20260101-000000Z-a/slim']['estimation_error_tokens_sum'], 7)
            self.assertIn('erro estimativa tokens', (Path(root) / 'r.md').read_text(encoding='utf-8'))

    def test_approved_scope_excludes_reproved_arm_but_keeps_it_visible(self):
        with tempfile.TemporaryDirectory() as root:
            good = write_campaign(root, '20260101-000000Z-a')
            bad = write_campaign(root, '20260101-000001Z-b', slim_exit=1)
            md, js = Path(root) / 'r.md', Path(root) / 'r.json'
            with patch('sys.argv', ['report.py', str(good), str(bad), '--output-md', str(md),
                                   '--output-json', str(js)]), patch('builtins.print'):
                report.main()
            payload = json.loads(js.read_text(encoding='utf-8'))
            self.assertEqual(payload['aggregate']['slim']['total']['campaigns'], 1)
            self.assertEqual(payload['aggregate']['pi']['total']['campaigns'], 2)
            self.assertEqual(payload['aggregate_all']['slim']['total']['campaigns'], 2)
            self.assertEqual(payload['paired']['stats']['pairs'], 1)
            self.assertEqual(payload['identity']['20260101-000001Z-b/slim']['gate_ok'], False)
            stats = payload['paired']['stats']
            self.assertEqual(stats['sign_test_tokens']['trials'], 1)
            self.assertEqual((stats['bootstrap_ci95_token_ratio'] or {}).get('seed'), 20260915)
            text = md.read_text(encoding='utf-8')
            approved = text.split('### Aprovados no gate')[1].split('###')[0]
            # o denominador assimetrico fica explicito na tabela em vez de ser escondido
            self.assertIn('| Campanhas no denominador | 1 | 2 | -1 |', approved)
            # o braco reprovado continua visivel, com o motivo explicito na tabela de campanhas
            self.assertIn('| exit=1 | aprovado |', text)
            self.assertIn('### `20260101-000001Z-b`', text.split('## Rastreabilidade')[1])

    def test_sign_test_and_bootstrap_are_deterministic(self):
        self.assertAlmostEqual(report.sign_test_p(4, 4), 0.125, places=12)
        self.assertAlmostEqual(report.sign_test_p(3, 3), 0.25, places=12)
        self.assertLess(report.sign_test_p(5, 5), report.sign_test_p(4, 5))
        self.assertIsNone(report.sign_test_p(0, 0))
        self.assertIsNone(report.bootstrap_median_ci([]))
        ratios = [0.5, 0.9, 1.1, 2.0]
        first, second = report.bootstrap_median_ci(ratios), report.bootstrap_median_ci(ratios)
        self.assertEqual(first, second)
        self.assertLessEqual(min(ratios), first['low'])
        self.assertLessEqual(first['low'], first['high'])
        self.assertLessEqual(first['high'], max(ratios))
        self.assertEqual(first['resamples'], 10000)


if __name__ == '__main__':
    unittest.main()
