"""Audit native records. Pass the completed Pi campaign, then the Slim campaign."""
import argparse
import json
from pathlib import Path

def records(path):
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line.strip()]

def load(path):
    return json.loads(path.read_text(encoding='utf-8'))

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('pi_dir', type=Path)
parser.add_argument('slim_dir', type=Path)
parser.add_argument('--output', type=Path, default=Path(__file__).with_name('summary.json'))
args = parser.parse_args()
pi_dir, slim_dir = args.pi_dir, args.slim_dir
manifest = load(slim_dir / 'manifest.json')
pi_manifest = load(pi_dir / 'manifest.json')
assert manifest['model'] == pi_manifest['model']
assert manifest.get('provider', 'openai-codex') == pi_manifest.get('provider', 'openai-codex')
audit = records(pi_dir / 'pi.audit.jsonl')
pi_requests = [e for e in audit if e['type'] == 'request']
pi_messages = [e for e in audit if e['type'] == 'message_end' and e['message']['role'] == 'assistant']
assert len(pi_requests) == len(pi_messages)
pi_tools = {}
for event in audit:
    if event['type'] == 'tool_execution_start':
        pi_tools[event['toolCallId']] = dict(event)
    elif event['type'] == 'tool_execution_end':
        tool = pi_tools[event['toolCallId']]
        tool.update(duration_ms=event['time_ms'] - tool['time_ms'], success=not event['isError'], result=event['result'])
pi_rows = []
for index, (request, end) in enumerate(zip(pi_requests, pi_messages), 1):
    message = end['message']
    assert request['model'] == message['model'] == manifest['model']
    assert request['reasoning']['effort'] == manifest['effort'] == 'high' and request['service_tier'] is None
    assert message['stopReason'] != 'error'
    usage = message['usage']
    pi_rows.append({'turn': index, 'input': usage['input'] + usage['cacheRead'] + usage['cacheWrite'],
        'uncached': usage['input'], 'cache': usage['cacheRead'], 'output': usage['output'],
        'reasoning': usage.get('reasoning', 0), 'provider_ms': end['time_ms'] - request['time_ms'],
        'tools': [c['name'] for c in message['content'] if c['type'] == 'toolCall'],
        'system_bytes': request['system_bytes'], 'schema_bytes': request['tool_schema_bytes'],
        'history_bytes': request['history_bytes'], 'payload_bytes': request['payload_bytes']})
slim = load(slim_dir / 'slim.stdout.jsonl')
events = [e['event'] for e in records(slim_dir / 'slim.session.jsonl') if e['type'] == 'event']
if manifest['model'].startswith('deepseek-v4-'):
    assert all(request.get('thinking') == {'type': 'enabled'} for request in pi_requests), 'Pi thinking mode not verified'
    assert any(e['kind']['type'] == 'ReasoningDelta' and e['kind'].get('text') for e in events), 'Slim thinking mode not verified; high effort alone is insufficient'
slim_rows, slim_tools = [], {}
for event in events:
    k = event['kind']
    if k['type'] == 'ContextSnapshot':
        usage = slim['usage']['requests'][len(slim_rows)]
        slim_rows.append({'turn': len(slim_rows) + 1, 'seq': event['seq'],
            'input': usage['uncached_input_tokens'] + usage['cache_read_tokens'] + usage['cache_write_tokens'],
            'uncached': usage['uncached_input_tokens'], 'cache': usage['cache_read_tokens'],
            'output': usage['output_tokens'], 'reasoning': usage['reasoning_tokens'],
            'provider_ms': usage['provider_latency_ms'], 'tools': [],
            'system_bytes': usage['system_bytes'], 'schema_bytes': usage['tool_schema_bytes'],
            'history_bytes': usage['history_bytes']})
    elif k['type'] == 'ToolStarted':
        slim_rows[-1]['tools'].append(k['name'])
        slim_tools[k['call_id']] = {'seq': event['seq'], **k, 'turn': len(slim_rows)}
    elif k['type'] in ['ToolOutput', 'ToolFinished']:
        slim_tools[k['call_id']].update(k)
assert len(slim_rows) == slim['usage']['provider_turns'] == len(slim['usage']['requests'])
assert slim['usage_complete'] and not slim['usage_unknown']
summary = {}
for arm, directory, rows, tools in [('pi', pi_dir, pi_rows, list(pi_tools.values())),
                                    ('slim', slim_dir, slim_rows, list(slim_tools.values()))]:
    timing, validation = load(directory / f'{arm}.timing.json'), load(directory / f'{arm}.validation.json')
    assert timing['exit_code'] == validation['exit_code'] == 0 and validation['fixtures_unchanged']
    totals = {key: sum(row[key] for row in rows) for key in ['input', 'uncached', 'cache', 'output', 'reasoning', 'provider_ms']}
    summary[arm] = {'campaign': str(directory), 'wall_ms': timing['total_ms'], 'model_calls': len(rows),
        'tool_calls': len(tools), 'tool_failures': sum(not t['success'] for t in tools),
        'tool_ms_sum': sum(t['duration_ms'] for t in tools), 'total_tokens': totals['input'] + totals['output'],
        **totals, 'requests': rows, 'tools': tools}
out = args.output
out.write_text(json.dumps(summary, indent=2, ensure_ascii=False), encoding='utf-8')
for arm, result in summary.items():
    print(arm, json.dumps({k: v for k, v in result.items() if k not in ['requests', 'tools']}))
