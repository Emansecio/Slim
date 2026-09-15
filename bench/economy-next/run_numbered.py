"""Second bounded cycle: only numbered read differs between the Slim binaries."""
import argparse
import copy
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
import subprocess

import run_cycle as cycle
import analyze_cycle as analyzer

BASE=cycle.HERE
HERE=BASE/'numbered'


def prepare():
    if (HERE/'manifest.json').exists(): raise SystemExit('Refusing to replace frozen cycle')
    previous=json.loads((BASE/'manifest.json').read_text(encoding='utf-8'))
    cases={k:copy.deepcopy(previous['scenarios'][k]) for k in ['source_audit','cache_conversation']}
    for case in cases.values(): case['holdout']=False
    cases['cache_conversation']['durable']=True
    for name in cases:
        shutil.copytree(BASE/'frozen'/name,HERE/'frozen'/name)
    source='''from copy import deepcopy
from datetime import datetime, timezone


def parse_timestamp(value):
    if not isinstance(value, str):
        raise ValueError('timestamp must be text')
    parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
    if parsed.tzinfo is None:
        raise ValueError('timezone required')
    return parsed.astimezone(timezone.utc)


def group_by_kind(events):
    result = {}
    for event in events:
        result.setdefault(event['kind'], []).append(event)
    return result


def latest_events(events):
    latest = {}
    for event in events:
        latest.setdefault(event['id'], event)
    return [latest[key] for key in sorted(latest)]


def total_value(events):
    return sum(event.get('value', 0) for event in events)
'''
    spec='''Fix latest_events(events) in events.py. Input must be a list of dicts with a
nonempty string id; reject invalid outer containers, records or IDs with ValueError.
Return the latest record per id, ordered by the position of its LAST occurrence in
the input. Deep-copy returned records including nested data; do not mutate input.
Keep every other function and client.py unchanged. Use only the standard library.
Create report.json containing citations for the fixed function: keys function,
validation, ordering, copying. Each value has line (1-based current events.py line)
and snippet (exact source line, trimming indentation allowed). Each cited line must
support its named claim; function must cite the def latest_events line. No invented
line numbers. Run python check.py and fix failures. Do not change SPEC.md/check.py.
'''
    check='''import copy, json
from pathlib import Path
from events import latest_events, parse_timestamp, group_by_kind, total_value
from client import current_total
src=[{'id':'z','value':1,'nested':{'x':[1]}},{'id':'a','value':2},
     {'id':'z','value':5,'nested':{'x':[9]}},{'id':'b','value':3}]
before=copy.deepcopy(src)
out=latest_events(src)
assert [e['id'] for e in out]==['a','z','b'] and current_total(src)==10
assert src==before
out[1]['nested']['x'].append(8)
assert src==before
assert latest_events([])==[]
for value in [None,(),{},[None],[{}],[{'id':''}],[{'id':1}],[{'id':True}]]:
    try: latest_events(value)
    except ValueError: pass
    else: raise AssertionError(('accepted invalid',value))
assert parse_timestamp('2020-01-01T00:00:00Z').year==2020
assert group_by_kind([{'kind':'x'}])=={'x':[{'kind':'x'}]}
assert total_value([{'value':2},{}])==2
lines=Path('events.py').read_text(encoding='utf-8').splitlines()
report=json.loads(Path('report.json').read_text(encoding='utf-8'))
assert set(report)=={'function','validation','ordering','copying'}
for key,item in report.items():
    assert type(item['line']) is int and 1<=item['line']<=len(lines)
    assert item['snippet'].strip()==lines[item['line']-1].strip() and item['snippet'].strip()
assert report['function']['snippet'].strip().startswith('def latest_events(')
print('PASS: last occurrence order, deep copy, invalid input, caller and exact citations')
'''
    check += "\nimport ast\noriginal=ast.parse("+repr(source)+")\ncurrent=ast.parse(Path('events.py').read_text(encoding='utf-8'))\n"
    check += "for node in original.body:\n    if isinstance(node,ast.FunctionDef) and node.name!='latest_events':\n        other=next(n for n in current.body if isinstance(n,ast.FunctionDef) and n.name==node.name)\n        assert ast.dump(node)==ast.dump(other),('changed unrelated function',node.name)\n"
    files={'events.py':source,'client.py':"from events import latest_events, total_value\n\ndef current_total(events):\n    return total_value(latest_events(events))\n",'SPEC.md':spec,'check.py':check}
    folder=HERE/'frozen/event_repair_citations'
    for path,value in files.items(): cycle.write(folder/path,value)
    cases['event_repair_citations']=dict(turns=['Read SPEC.md; implement the fix and supply verified citations.'],checks=[check],
        file_hashes={path:cycle.sha(folder/path) for path in files},file_bytes=sum((folder/path).stat().st_size for path in files),
        file_lines=sum(len(v.splitlines()) for v in files.values()),mutable=['events.py'],outputs=['report.json'],holdout=True)
    m=copy.deepcopy(previous)
    m.update(created=datetime.now(timezone.utc).isoformat(),scenarios=cases,git_ceiling=True,
        comparison='original Slim versus original Slim plus optional numbered read; shell candidate discarded')
    m['binaries']['before']=dict(previous['binaries']['before'])
    candidate=HERE/'slim-numbered.exe'
    m['binaries']['after']=dict(path=str(candidate),sha256=cycle.sha(candidate),
        version=subprocess.check_output([str(candidate),'--version'],text=True).strip())
    cycle.save(HERE/'manifest.json',m)
    print('Frozen numbered cycle: 36 attempts, 48 processes maximum; two rounds, three scenarios, two native routes.')


if __name__=='__main__':
    p=argparse.ArgumentParser(); p.add_argument('mode',choices=['prepare','luna','deepseek','analyze']); args=p.parse_args()
    if args.mode=='prepare': prepare()
    else:
        cycle.HERE=HERE
        analyzer.HERE=HERE
        if args.mode=='analyze': analyzer.audit()
        else: cycle.run(args.mode)
