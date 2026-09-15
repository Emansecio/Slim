"""Audit frozen oracles and all attempts, including interrupted/unknown requests."""
import collections
import json
import math
from pathlib import Path
import subprocess
import sys

from run_cycle import HERE, normalize, records, save, sha


def audit():
    m=json.loads((HERE/'manifest.json').read_text(encoding='utf-8'))
    rows=[]
    skipped=[]
    for folder in sorted((HERE/'runs').glob('*/*/*')):
        if (folder/'skipped.json').exists():
            skipped.append(dict(folder=str(folder.relative_to(HERE)),**json.loads((folder/'skipped.json').read_text(encoding='utf-8'))))
            continue
        condition=json.loads((folder/'condition.json').read_text(encoding='utf-8'))
        case=m['scenarios'][condition['scenario']]
        validation=[]
        for index in range(len(case['turns'])):
            if not (folder/f'turn{index}.spec.json').exists(): break
            target=folder/f'turn{index}.audit-validation.json'
            if target.exists():
                v=json.loads(target.read_text(encoding='utf-8'))
            else:
                cwd=folder/'workspace'
                # Original frozen oracle, correctly decoded. No change to model
                # output, fixture, requirements or original validation record.
                preserved=all((cwd/p).exists() and sha(cwd/p)==digest for p,digest in case['file_hashes'].items() if p not in case['mutable'])
                try:
                    checked=subprocess.run([sys.executable,'-c',case['checks'][index]],cwd=cwd,capture_output=True,text=True,timeout=25)
                    v=dict(exit_code=checked.returncode,stdout=checked.stdout,stderr=checked.stderr,preserved=preserved,
                           oracle_sha256=__import__('hashlib').sha256(case['checks'][index].encode('utf-8')).hexdigest())
                except Exception as error: v=dict(exit_code=None,error=str(error),preserved=preserved)
                save(target,v)
            validation.append(v)
        result=normalize(folder,condition['arm'],m,case,{k:condition[k] for k in ['provider','model']},persist=False)
        result['artifact_pass']=len(validation)==len(case['turns']) and all(v['exit_code']==0 and v['preserved'] for v in validation)
        result['quality_pass']=result['artifact_pass'] and result['native_completed']
        result['host_interrupted']=any(not (folder/f'turn{i}.timing.json').exists() for i in range(len(case['turns'])) if (folder/f'turn{i}.spec.json').exists())
        if result['host_interrupted']:
            result['wall_ms']=None
            if condition['arm']=='pi':
                events=records(folder/'pi.audit.jsonl')
                ended=bool(events) and events[-1]['type']=='agent_end'
                if not ended:
                    result['usage_complete']=False
                    result['total_tokens']=None
                    result['issues'].append('host interrupted with request still in flight')
        # Add semantics checks beyond numeric oracle: citations must support the
        # stated constant and matching claim, not merely point to any source line.
        if condition['scenario']=='source_audit' and result['quality_pass']:
            answer=json.loads((folder/'workspace/answer.json').read_text(encoding='utf-8'))
            markers=dict(default_hits='DEFAULT_MAX_HITS',max_hits='MAX_HITS_CAP',max_patterns='MAX_SEARCH_PATTERNS',snapshot_ttl_seconds='SEARCH_SNAPSHOT_TTL')
            semantics={fact:any(e['fact']==fact and marker in e['snippet'] for e in answer['evidence']) for fact,marker in markers.items()}
            semantics['literal_test']=any(e['fact']=='literal_test' and '/tests/' in e['path'] for e in answer['evidence'])
            result['citation_support']=semantics
            result['quality_pass'] &= all(semantics.values())
        save(folder/'audited-result.json',result)
        rows.append(dict(**condition,folder=str(folder.relative_to(HERE)),**{k:v for k,v in result.items() if k not in ['arm','requests','finals']}))
    def aggregate(group):
        complete=all(r['usage_complete'] for r in group)
        return dict(attempts=len(group),quality_pass=sum(r['quality_pass'] for r in group),
            unknown=sum(not r['usage_complete'] for r in group),host_interrupted=sum(r['host_interrupted'] for r in group),
            total=sum(r['total_tokens'] for r in group) if complete else None,
            known_reported_tokens=sum(r['known_reported_tokens'] for r in group if r['known_reported_tokens'] is not None),
            input_uncached=sum(r['uncached_input_tokens'] for r in group) if complete else None,
            cache_read=sum(r['cache_read_tokens'] for r in group) if complete else None,
            cache_write=sum(r['cache_write_tokens'] for r in group) if complete else None,
            output=sum(r['output_tokens'] for r in group) if complete else None,
            tool_failures=sum(r['tool_failures'] for r in group) if all(r['tool_failures'] is not None for r in group) else None,
            model_calls=sum(r['model_calls'] for r in group) if all(r['model_calls'] is not None for r in group) else None)
    groups={}
    for model in m['routes'].values():
        for arm in m['binaries']:
            group=[r for r in rows if r['model']==model['model'] and r['arm']==arm]
            if group: groups[model['model']+'/'+arm]=aggregate(group)
    per_case={}
    for key in sorted({(r['model'],r['scenario']) for r in rows}):
        per_case['/'.join(key)]={arm:aggregate([r for r in rows if (r['model'],r['scenario'])==key and r['arm']==arm]) for arm in m['binaries']}
    pairs=[]
    for key in sorted({(r['model'],r['scenario'],r['round']) for r in rows}):
        group={r['arm']:r for r in rows if (r['model'],r['scenario'],r['round'])==key}
        if not {'before','after'} <= group.keys(): continue
        a,b=group['before'],group['after']
        pairs.append(dict(model=key[0],scenario=key[1],round=key[2],
            before=a['total_tokens'],after=b['total_tokens'],quality_before=a['quality_pass'],quality_after=b['quality_pass'],
            delta_percent=100*(b['total_tokens']/a['total_tokens']-1) if a['total_tokens'] and b['total_tokens'] is not None else None))
    out=dict(rows=rows,skipped=skipped,aggregates=groups,by_scenario=per_case,pairs=pairs)
    save(HERE/'summary.json',out)
    print(json.dumps(groups,indent=2))


if __name__=='__main__': audit()
