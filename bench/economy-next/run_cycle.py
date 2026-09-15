"""Native three-arm, two-round experiment. Retains every attempt and unknown usage."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from datetime import datetime, timezone

from scenarios import HERE, ROOT, scenarios
sys.path.insert(0, str(HERE.parent / 'token-economy/v2'))
from process_runner import main as run_process


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding='utf-8')


def save(path, value):
    write(path, json.dumps(value, indent=2, ensure_ascii=False))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def records(path):
    if not path.exists(): return []
    return [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line.strip()]


def prepare():
    target = HERE / 'frozen'
    if target.exists(): raise SystemExit('Frozen inputs already exist; do not replace a measured experiment.')
    data = scenarios()
    for name, case in data.items():
        for path, content in case['files'].items(): write(target / name / path, content)
        case['file_hashes'] = {path: sha(target / name / path) for path in case['files']}
        case['file_bytes'] = sum((target / name / p).stat().st_size for p in case['files'])
        case['file_lines'] = sum(len(v.splitlines()) for v in case['files'].values())
        del case['files']
    node = Path(shutil.which('node.exe'))
    pi = node.parent / 'node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js'
    binaries = {'before': HERE/'slim-before.exe', 'after': HERE/'slim-after.exe', 'pi':pi}
    manifest = dict(created=datetime.now(timezone.utc).isoformat(), scenarios=data,
                    node=str(node), binaries={k:dict(path=str(p),sha256=sha(p)) for k,p in binaries.items()},
                    source_head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
                    source_dirty=True, rounds=2, effort='high', speed='normal',
                    routes={'luna':dict(provider='openai-codex',model='gpt-5.6-luna'),
                            'deepseek':dict(provider='opencode-go',model='deepseek-v4-flash')})
    for arm, item in manifest['binaries'].items():
        cmd = [str(node), item['path']] if arm=='pi' else [item['path']]
        item['version'] = subprocess.check_output(cmd+['--version'],text=True).strip()
    save(HERE/'manifest.json',manifest)
    print(json.dumps({k:dict(files=len(c['file_hashes']),bytes=c['file_bytes'],lines=c['file_lines'],turns=len(c['turns'])) for k,c in data.items()}),flush=True)


def normalize(folder, arm, manifest, case, route, *, persist=True):
    requests, tools, finals, issues = [], [], [], []
    verified_preflight_rejections = 0
    durable_tool_calls = 0
    completed_turns = 0
    if arm != 'pi':
        for index in range(len(case['turns'])):
            items = records(folder / f'turn{index}.stdout.jsonl')
            if len(items)!=1 or 'usage' not in items[0]:
                error_file=folder/f'turn{index}.stderr.log'
                if error_file.exists() and error_file.read_text(encoding='utf-8').strip()=='resume requires durable schema v2; legacy session schema v1 is not migrated':
                    # cli.rs validates this before credential/provider dispatch.
                    # This is a proven no-request failure, not unknown usage.
                    verified_preflight_rejections += 1
                    continue
                issues.append(f'turn{index}: missing usage result'); continue
            output=items[0]; finals.append(output.get('text',''))
            completed_turns += output.get('stop')=='provider_completed'
            durable_tool_calls += output['usage'].get('tool_calls_executed',0)
            if not output.get('usage_complete') or output.get('usage_unknown'):
                issues.append(f'turn{index}: incomplete usage')
            requests.extend(output['usage']['requests'])
        for item in records(folder/'session.jsonl'):
            event=item.get('event',{}).get('kind',{})
            if event.get('type')=='ToolFinished': tools.append(event)
    else:
        audit=records(folder/'pi.audit.jsonl')
        wire=[e for e in audit if e['type']=='request']
        messages=[e['message'] for e in audit if e['type']=='message_end' and e['message'].get('role')=='assistant']
        if len(wire)!=len(messages): issues.append('request/response count mismatch; retry/unknown request retained')
        for i,msg in enumerate(messages):
            usage=msg.get('usage')
            if msg.get('stopReason')=='error' or not isinstance(usage,dict) or any(usage.get(k) is None for k in ['input','cacheRead','cacheWrite','output']):
                issues.append(f'response{i}: unknown usage'); continue
            w=wire[i] if i<len(wire) else {}
            if msg.get('model')!=route['model'] or w.get('model')!=route['model']:
                issues.append(f'response{i}: model mismatch')
            if route['provider']=='openai-codex' and ((w.get('reasoning') or {}).get('effort')!='high' or w.get('service_tier') is not None):
                issues.append(f'response{i}: effort/tier mismatch')
            if route['provider']=='opencode-go' and w.get('thinking')!={'type':'enabled'}:
                issues.append(f'response{i}: thinking not verified')
            requests.append(dict(uncached_input_tokens=usage['input'],cache_read_tokens=usage['cacheRead'],
                cache_write_tokens=usage['cacheWrite'],output_tokens=usage['output'],
                reasoning_tokens=usage.get('reasoning'),usage_unknown=False,
                system_bytes=w.get('system_bytes'),tool_schema_bytes=w.get('tool_schema_bytes'),
                history_bytes=w.get('history_bytes'),tool_result_bytes=w.get('tool_result_bytes')))
            finals.append(''.join(c.get('text','') for c in msg.get('content',[]) if c.get('type')=='text'))
        tools=[dict(success=not e['isError']) for e in audit if e['type']=='tool_execution_end']
        completed_turns=sum(e['type']=='agent_end' for e in audit)
    if not requests: issues.append('no known requests')
    if any(r.get('usage_unknown') for r in requests): issues.append('unknown request usage')
    known={k:sum(r[k] for r in requests if r.get(k) is not None) for k in ['uncached_input_tokens','cache_read_tokens','cache_write_tokens','output_tokens']}
    usage_complete=not issues
    validations=[json.loads((folder/f'turn{i}.validation.json').read_text(encoding='utf-8')) for i in range(len(case['turns'])) if (folder/f'turn{i}.validation.json').exists()]
    timings=[json.loads((folder/f'turn{i}.timing.json').read_text(encoding='utf-8')) for i in range(len(case['turns'])) if (folder/f'turn{i}.timing.json').exists()]
    passed=(len(validations)==len(case['turns']) and all(v['exit_code']==0 and v['preserved'] for v in validations)
            and len(timings)==len(case['turns']) and all(t['exit_code']==0 and not t['timed_out'] for t in timings))
    result=dict(arm=arm,passed=passed,usage_complete=usage_complete,issues=issues,
        native_completed=completed_turns==len(case['turns']),
        total_tokens=sum(known.values()) if usage_complete else None,known_reported_tokens=sum(known.values()) if requests else None,
        **{k:v if requests else None for k,v in known.items()},requests=requests,
        model_calls=len(wire) if arm=='pi' else len(requests) if usage_complete else None,
        tool_calls=durable_tool_calls if case.get('durable') and arm!='pi' else len(tools),
        verified_preflight_rejections=verified_preflight_rejections,
        tool_failures=None if case.get('durable') and arm!='pi' else sum(not t['success'] for t in tools),
        wall_ms=sum(t['total_ms'] for t in timings),finals=finals)
    if persist: save(folder/'result.json',result)
    return result


def run(label):
    manifest=json.loads((HERE/'manifest.json').read_text(encoding='utf-8')); route=manifest['routes'][label]
    for item in manifest['binaries'].values(): assert sha(Path(item['path']))==item['sha256']
    for repeat in range(2):
        for si,(name,case) in enumerate(manifest['scenarios'].items()):
            # Rotate first position, then reverse the same order in round two.
            base=['before','after','pi']; order=base[si%3:]+base[:si%3]
            if repeat: order=list(reversed(order))
            for arm in order:
                folder=HERE/'runs'/label/f'r{repeat+1}-{name}'/arm
                if folder.exists():
                    print(f'RETAIN existing attempt, no new model call: {folder}',flush=True)
                    continue
                folder.mkdir(parents=True)
                cwd=folder/'workspace'; shutil.copytree(HERE/'frozen'/name,cwd)
                config=folder/'empty.toml'; write(config,'')
                session=folder/'session.jsonl'
                if case.get('durable') and arm!='pi':
                    write(session,json.dumps(dict(type='session',schema_version=2,id=f'{label}-{repeat}-{name}-{arm}',
                        timestamp=datetime.now(timezone.utc).isoformat(),cwd=str(cwd),parent_id=None,cutoff_seq=None))+'\n')
                save(folder/'condition.json',dict(arm=arm,round=repeat+1,scenario=name,order=order,**route,effort='high'))
                for index,prompt in enumerate(case['turns']):
                    common={key:None for key in os.environ if key.startswith('SLIM_') or key in (
                        'CODEX_ACCESS_TOKEN','CODEX_ACCOUNT_ID','OPENAI_API_KEY','NODE_OPTIONS',
                        'PI_CODING_AGENT_DIR','PI_CODING_AGENT_SESSION_DIR')}
                    if manifest.get('git_ceiling'):
                        common['GIT_CEILING_DIRECTORIES']=str(cwd.parent)
                    if arm=='pi':
                        executable=manifest['node']
                        argv=[manifest['binaries'][arm]['path'],'--provider',route['provider'],'--model',route['model'],
                            '--thinking','high','--mode','json','--print','--no-extensions','--no-skills',
                            '--no-prompt-templates','--no-context-files','--no-themes','--no-approve',
                            '-e',str(ROOT/'bench/luna-live/pi-audit.ts'),'--session',str(session),prompt]
                        common.update(LUNA_AUDIT_FILE=str(folder/'pi.audit.jsonl'),PI_TELEMETRY='0')
                    else:
                        executable=manifest['binaries'][arm]['path']
                        argv=['--headless','--provider',route['provider'],'--model',route['model'],'--effort','high',
                              '--jsonl','--session' if index==0 and not case.get('durable') else '--resume',str(session),'--prompt',prompt]
                        if route['provider']=='openai-codex': argv+=['--normal']
                        common['SLIM_CONFIG_FILE']=str(config)
                    spec=dict(agent=arm,scenario=name,run=repeat+1,executable=executable,arguments=argv,
                              cwd=str(cwd),environment=common,timeout_seconds=360)
                    paths=[folder/f'turn{index}.{suffix}' for suffix in ['spec.json','timing.json','stdout.jsonl','stderr.log']]
                    save(paths[0],spec)
                    print(f'START {label} r{repeat+1} {name} {arm} turn{index+1}',flush=True)
                    try: run_process(*(str(p) for p in paths))
                    except Exception as error:
                        save(folder/f'turn{index}.runner-error.json',dict(error=str(error)))
                    preserved=all((cwd/p).exists() and sha(cwd/p)==digest for p,digest in case['file_hashes'].items() if p not in case['mutable'])
                    try:
                        checked=subprocess.run([sys.executable,'-c',case['checks'][index]],cwd=cwd,capture_output=True,text=True,timeout=25)
                        validation=dict(exit_code=checked.returncode,stdout=checked.stdout,stderr=checked.stderr,preserved=preserved)
                    except Exception as error: validation=dict(exit_code=None,error=str(error),preserved=preserved)
                    save(folder/f'turn{index}.validation.json',validation)
                    print(f'VALIDATION {label} r{repeat+1} {name} {arm} turn{index+1}: {validation["exit_code"]} preserved={preserved}',flush=True)
                    # Never replace failed attempts. Remaining user turns stay unexecuted.
                    if validation['exit_code']!=0 or not preserved: break
                result=normalize(folder,arm,manifest,case,route)
                print(json.dumps({k:result[k] for k in ['arm','passed','usage_complete','total_tokens','model_calls','tool_failures','issues']}),flush=True)


if __name__=='__main__':
    parser=argparse.ArgumentParser(); parser.add_argument('mode',choices=['prepare','luna','deepseek'])
    args=parser.parse_args()
    if args.mode=='prepare': prepare()
    else: run(args.mode)
