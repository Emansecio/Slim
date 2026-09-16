"""Audit native records. Pass the completed Pi campaign, then the Slim campaign (same dir allowed)."""
import argparse
import json
from pathlib import Path
from report import pi_request_components

def records(path):
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line.strip()]

def load(path):
    return json.loads(path.read_text(encoding='utf-8'))

def require(directory, names):
    missing = [name for name in names if not (directory / name).exists()]
    assert not missing, f'{directory}: missing evidence files: {", ".join(missing)}'

def audit(pi_dir, slim_dir, output=None):
    """Verify one campaign pair and return its summary; write JSON when output is given."""
    pi_dir, slim_dir = Path(pi_dir), Path(slim_dir)
    require(pi_dir, ['manifest.json', 'pi.audit.jsonl', 'pi.timing.json', 'pi.validation.json'])
    require(slim_dir, ['manifest.json', 'slim.stdout.jsonl', 'slim.session.jsonl',
                       'slim.timing.json', 'slim.validation.json'])
    manifest = load(slim_dir / 'manifest.json')
    pi_manifest = load(pi_dir / 'manifest.json')
    assert manifest['model'] == pi_manifest['model'], f'model mismatch: {slim_dir} vs {pi_dir}'
    assert manifest.get('provider', 'openai-codex') == pi_manifest.get('provider', 'openai-codex'), \
        f'provider mismatch: {slim_dir} vs {pi_dir}'
    for key in ('scenario', 'prompt'):
        assert manifest.get(key) == pi_manifest.get(key), \
            f'{key} mismatch between paired campaigns: {slim_dir} vs {pi_dir}'
    audit_events = records(pi_dir / 'pi.audit.jsonl')
    pi_requests = [e for e in audit_events if e['type'] == 'request']
    pi_messages = [e for e in audit_events if e['type'] == 'message_end' and e['message']['role'] == 'assistant']
    assert len(pi_requests) == len(pi_messages), \
        f'{pi_dir}: {len(pi_requests)} requests vs {len(pi_messages)} assistant messages'
    pi_tools = {}
    for event in audit_events:
        if event['type'] == 'tool_execution_start':
            pi_tools[event['toolCallId']] = dict(event)
        elif event['type'] == 'tool_execution_end':
            tool = pi_tools.setdefault(event['toolCallId'],
                                       {'toolCallId': event['toolCallId'], 'orphan_end': True})
            tool.update(duration_ms=event['time_ms'] - tool.get('time_ms', event['time_ms']),
                        success=not event['isError'], result=event['result'])
    pi_rows = []
    for index, (request, end) in enumerate(zip(pi_requests, pi_messages), 1):
        message = end['message']
        assert request.get('model') == message.get('model') == manifest['model'], \
            f'{pi_dir}: model mismatch on turn {index}'
        assert (request.get('reasoning') or {}).get('effort') == manifest.get('effort') == 'high' \
            and request.get('service_tier') is None, f'{pi_dir}: effort/tier mismatch on turn {index}'
        assert message.get('stopReason') != 'error', f'{pi_dir}: stopReason=error on turn {index}'
        usage = message['usage']
        components = pi_request_components(request)
        pi_rows.append({'turn': index, 'input': usage['input'] + usage['cacheRead'] + usage['cacheWrite'],
            'uncached': usage['input'], 'cache': usage['cacheRead'], 'output': usage['output'],
            'reasoning': usage.get('reasoning', 0), 'provider_ms': end['time_ms'] - request['time_ms'],
            'tools': [c['name'] for c in message['content'] if c['type'] == 'toolCall'],
            'system_bytes': components['system_bytes'], 'schema_bytes': components['tool_schema_bytes'],
            'history_bytes': components['history_bytes'], 'tool_result_bytes': components['tool_result_bytes'],
            'payload_bytes': request.get('payload_bytes', 0)})
    slim = load(slim_dir / 'slim.stdout.jsonl')
    recs = records(slim_dir / 'slim.session.jsonl')
    events = [e['event'] for e in recs if e.get('type') == 'event' and e.get('event')]
    usage_requests = slim.get('usage', {}).get('requests') or []
    if manifest['model'].startswith('deepseek-v4-'):
        assert all(request.get('thinking') == {'type': 'enabled'} for request in pi_requests), 'Pi thinking mode not verified'
        thinking = (any(e['kind']['type'] == 'ReasoningDelta' and e['kind'].get('text') for e in events) if events
                    else any(u.get('reasoning_tokens') for u in usage_requests))
        assert thinking, 'Slim thinking mode not verified; high effort alone is insufficient'
    slim_rows, slim_tools = [], {}
    if events:
        for event in events:
            k = event['kind']
            if k['type'] == 'ContextSnapshot':
                assert len(slim_rows) < len(usage_requests), \
                    f'{slim_dir}: ContextSnapshot seq {event.get("seq")} without usage record'
                usage = usage_requests[len(slim_rows)]
                slim_rows.append({'turn': len(slim_rows) + 1, 'seq': event['seq'],
                    'input': (usage.get('uncached_input_tokens') or 0) + (usage.get('cache_read_tokens') or 0)
                             + (usage.get('cache_write_tokens') or 0),
                    'uncached': usage.get('uncached_input_tokens') or 0,
                    'cache': usage.get('cache_read_tokens') or 0,
                    'output': usage.get('output_tokens') or 0,
                    'reasoning': usage.get('reasoning_tokens') or 0,
                    'provider_ms': usage.get('provider_latency_ms') or 0, 'tools': [],
                    'system_bytes': usage.get('system_bytes') or 0,
                    'schema_bytes': usage.get('tool_schema_bytes') or 0,
                    'history_bytes': usage.get('history_bytes') or 0,
                    'tool_result_bytes': usage.get('tool_result_bytes') or 0})
            elif k['type'] == 'ToolStarted':
                if slim_rows:
                    slim_rows[-1]['tools'].append(k['name'])
                slim_tools[k['call_id']] = {'seq': event['seq'], **k, 'turn': len(slim_rows)}
            elif k['type'] in ['ToolOutput', 'ToolFinished']:
                slim_tools.setdefault(k['call_id'], {'call_id': k['call_id'], 'seq': event['seq'],
                                                     'turn': len(slim_rows), 'orphan': True}).update(k)
    else:
        # schema v2: assistant entries align 1:1 with provider turns; outcomes via tool.v1 facts
        for i, usage in enumerate(usage_requests, 1):
            slim_rows.append({'turn': i, 'seq': None,
                'input': (usage.get('uncached_input_tokens') or 0) + (usage.get('cache_read_tokens') or 0)
                         + (usage.get('cache_write_tokens') or 0),
                'uncached': usage.get('uncached_input_tokens') or 0,
                'cache': usage.get('cache_read_tokens') or 0,
                'output': usage.get('output_tokens') or 0,
                'reasoning': usage.get('reasoning_tokens') or 0,
                'provider_ms': usage.get('provider_latency_ms') or 0, 'tools': [],
                'system_bytes': usage.get('system_bytes') or 0,
                'schema_bytes': usage.get('tool_schema_bytes') or 0,
                'history_bytes': usage.get('history_bytes') or 0,
                'tool_result_bytes': usage.get('tool_result_bytes') or 0})
        turn = 0
        for r in recs:
            if r.get('type') != 'entry':
                continue
            entry = r.get('entry', {})
            if entry.get('role') == 'assistant':
                turn += 1
                for call in entry.get('tool_calls') or []:
                    cid = call.get('id')
                    slim_tools[cid] = {'call_id': cid, 'name': call.get('name'), 'turn': turn,
                                       'duration_ms': 0, 'success': None}
                    if 0 < turn <= len(slim_rows):
                        slim_rows[turn - 1]['tools'].append(call.get('name'))
            elif entry.get('role') == 'tool' and entry.get('tool_call_id') in slim_tools:
                slim_tools[entry['tool_call_id']]['result'] = entry.get('content')
        for r in recs:
            if r.get('type') == 'fact':
                fact = r.get('fact', {})
                if fact.get('namespace') == 'tool.v1' and fact.get('key') in slim_tools:
                    value = fact.get('value') or {}
                    slim_tools[fact['key']].update(success=value.get('success'),
                                                   duration_ms=value.get('duration_ms') or 0)
    usage = slim.get('usage', {})
    assert len(slim_rows) == usage.get('provider_turns') == len(usage_requests), (
        f'{slim_dir}: {len(slim_rows)} snapshots vs provider_turns={usage.get("provider_turns")} '
        f'vs {len(usage_requests)} usage records')
    assert slim.get('usage_complete') and not slim.get('usage_unknown'), \
        f'{slim_dir}: slim usage incomplete (usage_complete={slim.get("usage_complete")}, usage_unknown={slim.get("usage_unknown")})'
    summary = {}
    for arm, directory, rows, tools in [('pi', pi_dir, pi_rows, list(pi_tools.values())),
                                        ('slim', slim_dir, slim_rows, list(slim_tools.values()))]:
        timing, validation = load(directory / f'{arm}.timing.json'), load(directory / f'{arm}.validation.json')
        gate = (f'{directory.name}/{arm}: exit={timing.get("exit_code")} timed_out={timing.get("timed_out")} '
                f'oracle={validation.get("exit_code")} fixtures={validation.get("fixtures_unchanged")} '
                f'oracle_error={validation.get("oracle_error")}')
        assert timing.get('exit_code') == 0 and not timing.get('timed_out'), 'arm not clean: ' + gate
        assert validation.get('exit_code') == 0 and validation.get('fixtures_unchanged'), 'oracle/fixtures failed: ' + gate
        totals = {key: sum(row[key] for row in rows) for key in ['input', 'uncached', 'cache', 'output', 'reasoning', 'provider_ms']}
        summary[arm] = {'campaign': str(directory), 'wall_ms': timing['total_ms'], 'model_calls': len(rows),
            'tool_calls': len(tools), 'tool_failures': sum(not t.get('success') for t in tools),
            'tool_ms_sum': sum(t.get('duration_ms') or 0 for t in tools), 'total_tokens': totals['input'] + totals['output'],
            **totals, 'requests': rows, 'tools': tools}
    if output is not None:
        Path(output).write_text(json.dumps(summary, indent=2, ensure_ascii=False), encoding='utf-8')
    return summary

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pi_dir', type=Path)
    parser.add_argument('slim_dir', type=Path)
    parser.add_argument('--output', type=Path, default=None,
                        help='default: SLIM_DIR/summary.json')
    args = parser.parse_args()
    summary = audit(args.pi_dir, args.slim_dir, args.output or args.slim_dir / 'summary.json')
    for arm, result in summary.items():
        print(arm, json.dumps({k: v for k, v in result.items() if k not in ['requests', 'tools']}))

if __name__ == '__main__':
    main()
