"""Frozen scenarios for the shell-contract experiment; never imported by Slim."""
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE.parent / 'luna-live'))
import daily


def scenarios():
    result = {}
    for name in ('config_migration', 'ledger_audit'):
        old = daily.SCENARIOS[name]
        result[name] = dict(files={**old['files'], 'SPEC.md': old['spec_text'], 'check.py': old['check']},
                            turns=[old['prompt']], checks=[old['check']],
                            mutable=['config/development.json', 'config/production.json'] if name == 'config_migration' else [],
                            outputs=[] if name == 'config_migration' else ['audit.json'], holdout=False)

    # Snapshot real production sources and their tests, without local configuration,
    # generated files, credentials, or the candidate shell description.
    sources = {p.relative_to(ROOT).as_posix(): p.read_text(encoding='utf-8')
               for p in (ROOT / 'crates').rglob('*.rs') if '.slim' not in p.parts}
    mod = 'crates/slim-core/src/tools/mod.rs'
    sources[mod] = sources[mod].replace(
        'Run in workspace. Program/inline script: command=\\"python\\", args=[\\"-c\\",\\"code\\"] (literal arguments). PowerShell script: command=\\"...\\", omit args or use null, never []; no Bash heredocs or profile/wrapper; separate with ; and check native exit codes. Use file tools for code edits. Raise timeout only for deliberate builds/tests.',
        'Run in workspace. With args: executable + literal arguments, preferred for programs/inline scripts. Otherwise: PowerShell without profile/wrapper; separate with ; and check native exit codes. Use file tools for code edits. Raise timeout only for deliberate builds/tests.')
    audit_spec = '''Audit this Rust source snapshot without modifying any source. Find the native
search tool's literal matching, default/max hits, maximum patterns and snapshot TTL.
Create answer.json with keys default_hits, max_hits, max_patterns, snapshot_ttl_seconds,
matching ("literal" or "regex"), and evidence (array). Each evidence object must have
path, line (1-based), snippet (an exact nonempty source line, whitespace may be trimmed),
and fact (one of those five keys). Give source evidence for every fact, and evidence
from an existing regression test that verifies literal matching (fact="literal_test").
Explain briefly in the final response why paging does not require a new scan while
the snapshot is live. Do not run cargo or install dependencies; this is a source audit.
Run python check.py to validate the answer's facts and citations.
'''
    audit_check = '''import json
from pathlib import Path
d = json.loads(Path('answer.json').read_text(encoding='utf-8'))
expected = dict(default_hits=200, max_hits=500, max_patterns=32, snapshot_ttl_seconds=120, matching='literal')
assert {k:d[k] for k in expected} == expected
facts = set()
for e in d['evidence']:
    p = Path(e['path'])
    assert not p.is_absolute() and '..' not in p.parts and p.suffix == '.rs'
    text = p.read_text(encoding='utf-8').splitlines()[e['line']-1].strip()
    assert text and text == e['snippet'].strip()
    fact = e['fact']
    if fact in expected or fact == 'literal_test': facts.add(fact)
assert facts >= set(expected) | {'literal_test'}
print('PASS: search facts and source/test citations')
'''
    result['source_audit'] = dict(files={**sources, 'SPEC.md': audit_spec, 'check.py': audit_check},
        turns=['Read SPEC.md and perform the source audit. Preserve all source files.'],
        checks=[audit_check], mutable=[], outputs=['answer.json'], holdout=True)

    cache_spec = '''Implement makeCache(capacity) exported by cache.cjs, standard JavaScript only.
Capacity must be a positive safe integer; otherwise throw RangeError. Returned object
has get(key), set(key,value), has(key), clear(), and size getter. Any JavaScript key/value
is supported with Map key semantics. get returns undefined if missing; has distinguishes
stored undefined. get of an existing key refreshes recency, has does not. set updates
recency, evicts the least recently used key above capacity and returns the cache object.
clear empties the cache. Preserve client.cjs. Run node check.cjs and fix failures.
'''
    cache_check = '''const assert = require('node:assert/strict');
const {makeCache} = require('./cache.cjs');
for (const n of [0,-1,1.5,NaN,Infinity,Number.MAX_SAFE_INTEGER+1,'2',true,null,undefined]) assert.throws(()=>makeCache(n),RangeError);
const c=makeCache(2), key={};
assert.equal(c.set('a',undefined),c); c.set(key,2);
assert.equal(c.has('a'),true); assert.equal(c.get('a'),undefined);
c.set('b',3); assert.equal(c.has(key),false);
assert.equal(c.size,2); assert.equal(c.has('a'),true);
c.set('z',4); assert.equal(c.has('a'),false); // has must not refresh
c.set('b',9); c.set('q',8); assert.equal(c.has('z'),false);
assert.equal(c.get('b'),9); c.clear(); assert.equal(c.size,0);
c.set(NaN,1); assert.equal(c.get(NaN),1);
assert.equal(require('./client.cjs').demo(),3);
console.log('PASS: LRU, arbitrary keys, undefined, client, validation and clear');
'''
    ttl_check = '''const assert = require('node:assert/strict');
const {makeCache} = require('./cache.cjs');
let time=100;
const c=makeCache(2,{ttlMs:10,now:()=>time});
c.set('a',1); time=105; assert.equal(c.get('a'),1);
time=110; assert.equal(c.has('a'),false); assert.equal(c.size,0); // fixed from set, not get
c.set('a',undefined); time=115; assert.equal(c.has('a'),true);
c.set('a',2); time=120; assert.equal(c.get('a'),2);
time=125; assert.equal(c.get('a'),undefined);
c.set('a',1); time=130; c.set('b',2); time=135;
c.set('c',3); assert.equal(c.has('b'),true); assert.equal(c.has('c'),true); assert.equal(c.size,2);
time=140; assert.equal(c.size,1); c.clear(); assert.equal(c.size,0);
for (const ttlMs of [0,-1,NaN,Infinity,'1',null]) assert.throws(()=>makeCache(2,{ttlMs}),RangeError);
assert.throws(()=>makeCache(2,{now:123}),TypeError);
const d=makeCache(1); d.set('x',8); assert.equal(d.get('x'),8);
console.log('PASS: TTL boundaries, fixed lifetime, update, expiry before eviction and options');
'''
    py_cache_check = "import subprocess\nsubprocess.run(['node','check.cjs'],check=True,timeout=15)\n"
    py_ttl_check = py_cache_check + "subprocess.run(['node','-e'," + repr(ttl_check) + "],check=True,timeout=15)\n"
    result['cache_conversation'] = dict(
        files={'SPEC.md':cache_spec,'cache.cjs':"exports.makeCache = function () { throw new Error('unimplemented'); };\n",
               'client.cjs':"const {makeCache}=require('./cache.cjs');\nexports.demo=()=>{const c=makeCache(1);c.set('a',3);return c.get('a');};\n",
               'check.cjs':cache_check},
        turns=['Read SPEC.md and implement the cache. Preserve SPEC.md, client.cjs and check.cjs.',
               'Extend makeCache(capacity, options={}) with optional ttlMs and now (default Date.now). '
               'Omitted ttlMs means no expiration. Provided ttlMs must be a finite positive number '
               '(RangeError otherwise); provided now must be a function (TypeError otherwise). '
               'An entry expires when now() >= its set time + ttlMs. get refreshes LRU but never extends TTL; '
               'set refreshes both. get/has/size must hide expired entries. Purge expired entries before '
               'evicting live ones. Preserve all original behavior and files; run the original tests and '
               'add and run your own boundary tests for expiration with an injected clock. No real sleeps.'],
        checks=[py_cache_check,py_ttl_check], mutable=['cache.cjs'], outputs=[],holdout=True)
    return result
