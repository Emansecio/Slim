"""Small varied native-agent comparison; fixed scenarios, alternating order, all runs retained."""
import argparse
import json
import run

SCENARIOS = {
    'merge_ranges': dict(),
    'repair_catalog': dict(
        prompt='Read SPEC.md, fix the existing implementation, and run python check.py. Do not change SPEC.md or check.py. Do not install dependencies. Finish with a brief summary.',
        spec_text='''Fix catalog.py. visible_products(products, query) returns new dictionaries for active
products whose name contains query, case-insensitively. Strip surrounding whitespace
from query. Sort by price ascending, then name case-insensitively. Each result has
only name and price. Do not mutate input. Empty query matches every active product.
Keep format_price in formatting.py unchanged and use it only for presentation.
''',
        files={
            'catalog.py': '''from formatting import format_price

def visible_products(products, query):
    products.sort(key=lambda product: product['price'], reverse=True)
    return [p for p in products if query in p['name']]

def product_labels(products, query):
    return [p['name'] + ': ' + format_price(p['price'])
            for p in visible_products(products, query)]
''',
            'formatting.py': "def format_price(price):\n    return f'{price:.2f}'\n",
        },
        check='''import copy
from pathlib import Path
from catalog import visible_products, product_labels

source = [dict(name='Beta', price=20, active=True), dict(name='alpha', price=10, active=True),
          dict(name='ALPINE', price=10, active=False), dict(name='Alps', price=10, active=True)]
before = copy.deepcopy(source)
assert visible_products(source, ' AL ') == [dict(name='alpha', price=10), dict(name='Alps', price=10)]
result = visible_products(source, '')
assert result == [dict(name='alpha', price=10), dict(name='Alps', price=10), dict(name='Beta', price=20)]
assert source == before
result[0]['name'] = 'changed'
assert source == before
assert visible_products([], '') == []
assert visible_products(source, 'missing') == []
assert product_labels(source, 'beta') == ['Beta: 20.00']
assert Path('formatting.py').read_text() == "def format_price(price):\\n    return f'{price:.2f}'\\n"
print('PASS: filtering, sorting, copying, caller and preserved formatting')
''',
    ),
    'json_cli': dict(
        prompt='Read SPEC.md and implement the requested CLI. Run python check.py and fix failures. Do not change SPEC.md, check.py or data. Do not install dependencies. Finish with a brief summary.',
        spec_text='''Create tools/report.py using only Python standard library. It takes two positional
arguments: input JSON path and output JSON path. Input is a list of objects with
category (string), cents (integer) and paid (boolean). Sum cents of paid entries
by category. Output is a JSON object mapping categories to totals, with keys
sorted and UTF-8 characters preserved. Create output parent directories.
Invalid input must exit nonzero, explain the error on stderr, and leave an
existing output file unchanged. Reject malformed records, bool/noninteger cents
and negative cents. Support file paths with spaces and non-ASCII characters.
''',
        files={'data/itens de ação.json': json.dumps([
            dict(category='ação', cents=150, paid=True),
            dict(category='livros', cents=250, paid=True),
            dict(category='ação', cents=50, paid=True),
            dict(category='livros', cents=999, paid=False),
        ], ensure_ascii=False)},
        check='''import json
from pathlib import Path
import subprocess
import sys
import tempfile

def call(src, dst):
    return subprocess.run([sys.executable, 'tools/report.py', str(src), str(dst)], capture_output=True, timeout=5)

source = Path('data/itens de ação.json')
before = source.read_bytes()
with tempfile.TemporaryDirectory(prefix='report check ') as directory:
    root = Path(directory)
    output = root / 'ação extra' / 'resumo.json'
    result = call(source, output)
    assert result.returncode == 0, result.stderr
    text = output.read_text(encoding='utf-8')
    assert json.loads(text) == {'ação': 200, 'livros': 250}
    assert 'ação' in text and text.index('ação') < text.index('livros')
    for invalid in [{}, [None], [dict(category='x', cents=True, paid=True)],
                    [dict(category='x', cents=-1, paid=True)],
                    [dict(category=1, cents=2, paid=True)], [dict(category='x', cents=2, paid='yes')]]:
        bad = root / 'bad.json'
        bad.write_text(json.dumps(invalid), encoding='utf-8')
        output.write_text('preserved', encoding='utf-8')
        result = call(bad, output)
        assert result.returncode != 0 and result.stderr
        assert output.read_text(encoding='utf-8') == 'preserved'
    bad.write_text('{', encoding='utf-8')
    result = call(bad, output)
    assert result.returncode != 0 and result.stderr
    assert output.read_text(encoding='utf-8') == 'preserved'
assert source.read_bytes() == before
print('PASS: CLI, Unicode paths, grouping, errors and output preservation')
''',
    ),
}

# Added before measuring workflow v1.1: an unseen task in a different language.
SCENARIOS['js_pagination'] = dict(
    prompt='Read SPEC.md, fix the existing JavaScript module, and run python check.py. Do not change SPEC.md or check.py. Do not install dependencies. Finish with a brief summary.',
    spec_text='''Fix paginate(items, page, pageSize) in pager.cjs; keep its CommonJS export.
items must be an array. page and pageSize must be positive safe integers.
Invalid arguments throw TypeError. Pages are one-based. Return an object with
items (a new shallow array), page, pageSize, total (input length), and pages
(ceiling of total/pageSize, zero for empty input). Beyond the final page returns
an empty items array. Do not mutate the input. Existing callers must work.
''',
    files={
        'pager.cjs': '''function paginate(items, page, pageSize) {
  const start = page * pageSize;
  return { items: items.splice(start, pageSize), page, pageSize,
           total: items.length, pages: Math.floor(items.length / pageSize) };
}
module.exports = { paginate };
''',
        'listing.cjs': '''const { paginate } = require('./pager.cjs');
exports.firstPage = (items) => paginate(items, 1, 2).items;
''',
    },
    check='''import subprocess

script = r"""
const assert = require('node:assert/strict');
const { paginate } = require('./pager.cjs');
const { firstPage } = require('./listing.cjs');
const source = ['a', 'b', 'c', 'd', 'e'];
assert.deepEqual(paginate(source, 2, 2), {items:['c','d'],page:2,pageSize:2,total:5,pages:3});
assert.deepEqual(source, ['a','b','c','d','e']);
assert.deepEqual(paginate(source, 3, 2).items, ['e']);
assert.deepEqual(paginate(source, 4, 2).items, []);
assert.deepEqual(paginate([], 1, 2), {items:[],page:1,pageSize:2,total:0,pages:0});
const all = paginate(source, 1, 10).items;
all.push('new');
assert.equal(source.length, 5);
assert.deepEqual(firstPage(source), ['a','b']);
for (const args of [[null,1,2],[{},1,2],[[],0,2],[[],1,0],[[],1.5,2],[[],1,Infinity],
                    [[],true,2],[[],1,'2'],[[],Number.MAX_SAFE_INTEGER+1,2]]) {
  assert.throws(() => paginate(...args), TypeError);
}
console.log('PASS: pagination, validation, immutability and caller');
"""
result = subprocess.run(['node', '-e', script], capture_output=True, text=True, timeout=5)
assert result.returncode == 0, result.stdout + result.stderr
print(result.stdout.strip())
''',
)


# Held-out workloads fixed before the observed-write/catalog measurements.
SCENARIOS['ledger_audit'] = dict(
    prompt='Read SPEC.md and audit the ledger. Write audit.json, then run python check.py. Do not change the input CSV, SPEC.md or check.py. Do not install dependencies. Finish with a brief summary.',
    spec_text='''Audit ledger.csv and write audit.json. Keep only the last row for each transaction
id (file order). From these rows, include only status=settled. Sum integer cents
by account, retaining negative refunds and zero totals. The JSON object must
have totals (account to sum), settled_count, and duplicate_rows (all discarded
earlier versions, regardless of status). Preserve account names exactly.
Do not modify the source CSV. No implementation file is required.
''',
    files={'ledger.csv': 'id,account,cents,status\n' + '\n'.join(
        f't{i},"{["ação", "Main, East", "reserve"][i % 3]}",{(i % 9 - 3) * 101},{"pending" if i % 5 == 0 else "settled"}'
        for i in range(90)
    ) + '\n' + '\n'.join(
        f't{i},"{["ação", "Main, East", "reserve"][i % 3]}",{-i * 7},{"void" if i % 4 == 0 else "settled"}'
        for i in range(0, 90, 3)
    ) + '\n'},
    check='''import csv
import json
import os
from pathlib import Path
rows = list(csv.DictReader(Path('ledger.csv').open(encoding='utf-8', newline='')))
assert len(rows) == 120
latest = {row['id']: row for row in rows}
totals = {}
count = 0
for row in latest.values():
    if row['status'] == 'settled':
        count += 1
        totals[row['account']] = totals.get(row['account'], 0) + int(row['cents'])
assert json.loads(Path('audit.json').read_text(encoding='utf-8')) == {
    'totals': totals, 'settled_count': count, 'duplicate_rows': len(rows) - len(latest)}
expected = 'id,account,cents,status\\n' + '\\n'.join(
    f't{i},"{["ação", "Main, East", "reserve"][i % 3]}",{(i % 9 - 3) * 101},{"pending" if i % 5 == 0 else "settled"}'
    for i in range(90)) + '\\n' + '\\n'.join(
    f't{i},"{["ação", "Main, East", "reserve"][i % 3]}",{-i * 7},{"void" if i % 4 == 0 else "settled"}'
    for i in range(0, 90, 3)) + '\\n'
assert Path('ledger.csv').read_bytes() == expected.replace('\\n', os.linesep).encode('utf-8')
print('PASS: last version, status, refunds, quoted CSV, Unicode and preserved source')
''',
)

SCENARIOS['config_migration'] = dict(
    prompt='Read SPEC.md and migrate the configuration files. Run python check.py. Do not change SPEC.md or check.py. Do not install dependencies. Finish with a brief summary.',
    spec_text='''Migrate each JSON file in config to schema_version 2. Replace the root retry
integer with a retry_policy object containing attempts equal to retry, and
backoff_ms equal to 250. Replace each service timeout_seconds with timeout_ms
by multiplying by 1000. Leave every other key/value and list order unchanged,
including nested metadata keys with similar names. Files already at version 2
must remain byte-for-byte unchanged. No migration script is required.
''',
    files={
        'config/development.json': json.dumps({'schema_version': 1, 'retry': 3, 'services': [
            {'name': 'api', 'timeout_seconds': 1.5, 'metadata': {'timeout_seconds': 99}},
            {'name': 'worker', 'timeout_seconds': 0, 'enabled': False}], 'metadata': {'retry': 8}}, indent=2),
        'config/production.json': json.dumps({'schema_version': 1, 'retry': 0, 'services': [
            {'name': 'ação', 'timeout_seconds': 12, 'url': 'https://example.invalid/api'}], 'owner': 'ops'}, ensure_ascii=False, indent=2),
        'config/current.json': '{"schema_version":2,"retry_policy":{"attempts":7,"backoff_ms":900},"services":[]}\n',
    },
    check='''import json
import os
from pathlib import Path
dev = json.loads(Path('config/development.json').read_text(encoding='utf-8'))
prod = json.loads(Path('config/production.json').read_text(encoding='utf-8'))
assert dev == {'schema_version': 2, 'retry_policy': {'attempts': 3, 'backoff_ms': 250}, 'services': [
    {'name': 'api', 'timeout_ms': 1500, 'metadata': {'timeout_seconds': 99}},
    {'name': 'worker', 'timeout_ms': 0, 'enabled': False}], 'metadata': {'retry': 8}}
assert prod == {'schema_version': 2, 'retry_policy': {'attempts': 0, 'backoff_ms': 250}, 'services': [
    {'name': 'ação', 'timeout_ms': 12000, 'url': 'https://example.invalid/api'}], 'owner': 'ops'}
assert Path('config/current.json').read_bytes() == ('{"schema_version":2,"retry_policy":{"attempts":7,"backoff_ms":900},"services":[]}' + os.linesep).encode('utf-8')
print('PASS: migration across files, nested keys, zero/fractional values and current file preserved')
''',
)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rounds', type=int, default=2)
    parser.add_argument('--start-round', type=int, default=1)
    parser.add_argument('--scenario', choices=list(SCENARIOS))
    parser.add_argument('--provider', choices=['openai-codex', 'opencode-go'], default='openai-codex')
    parser.add_argument('--model', default=run.MODEL)
    args = parser.parse_args()
    if args.rounds < 1 or args.start_round < 1:
        parser.error('--rounds and --start-round must be positive')
    selected = [args.scenario] if args.scenario else list(SCENARIOS)
    campaigns = []
    failures = []
    for repeat in range(args.start_round - 1, args.start_round - 1 + args.rounds):
        for scenario in selected:
            index = list(SCENARIOS).index(scenario)
            arms = ['slim', 'pi'] if (repeat + index) % 2 == 0 else ['pi', 'slim']
            try:
                campaign = run.main(arms, scenario=scenario, provider=args.provider, model=args.model, **SCENARIOS[scenario])
            except SystemExit as error:
                failures.append({'round': repeat + 1, 'scenario': scenario, 'error': str(error)})
                print(json.dumps(failures[-1]), flush=True)
                continue
            campaigns.append(str(campaign))
            print(json.dumps({'round': repeat + 1, 'scenario': scenario, 'campaign': str(campaign)}), flush=True)
    print(json.dumps({'campaigns': campaigns, 'failures': failures}), flush=True)
    if failures:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
