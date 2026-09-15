"""One real Codex coding task per native CLI; no proxy or model-output fixtures."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / 'token-economy' / 'v2'))
from process_runner import main as run_process

MODEL = 'gpt-5.6-luna'
PROMPT = ('Read SPEC.md and implement ranges.py. Run python check.py and fix any failures. '
          'Do not change SPEC.md or check.py. Do not install dependencies. '
          'Finish with a brief summary of the result.')
SPEC = '''Implement merge_ranges(ranges) in ranges.py using only the Python standard library.
Input: a list of two-element lists or tuples containing integers (bool is invalid).
Reject an invalid outer container, malformed pair, non-integer endpoint, or start > end
with ValueError. Return a new list of [start, end] lists, sorted by start, merging
overlapping or touching closed intervals (next start <= current end). Do not merge
merely adjacent integers: [1, 2] and [3, 4] stay separate. Do not mutate the input.
Empty input returns []. Target O(n log n) time. No command-line interface is needed.
'''
CHECK = '''import copy
from ranges import merge_ranges

cases = [([], []), ([(3, 5), [1, 3], [8, 9], [2, 2]], [[1, 5], [8, 9]]),
         ([[1, 2], [3, 4]], [[1, 2], [3, 4]]),
         ([[-5, -1], [-3, 0], [0, 0]], [[-5, 0]])]
for source, expected in cases:
    before = copy.deepcopy(source)
    result = merge_ranges(source)
    assert result == expected, (source, result, expected)
    assert source == before and result is not source
    assert all(type(pair) is list for pair in result)
    if result:
        result[0][0] = 999
        assert source == before
invalid = [None, (), [None], [[1]], [[1, 2, 3]], [[2, 1]], [[True, 2]], [[1, 2.0]], [['1', 2]]]
for source in invalid:
    try:
        merge_ranges(source)
    except ValueError:
        continue
    raise AssertionError(('accepted invalid input', source))
print('PASS: 4 valid scenarios, 9 invalid inputs, immutability and output shape')
'''

def write(path, value):
    path.write_text(value, encoding='utf-8')

def main(arms=None, *, prompt=PROMPT, spec_text=SPEC, check=CHECK, files=None, scenario='merge_ranges', provider='openai-codex', model=MODEL):
    route_suffix = '' if provider == 'openai-codex' else '-' + provider
    campaign = HERE / (datetime.now(timezone.utc).strftime('%Y%m%d-%H%M%SZ') + route_suffix + ('' if scenario == 'merge_ranges' else '-' + scenario))
    campaign.mkdir(parents=True, exist_ok=False)
    workspace = Path(tempfile.mkdtemp(prefix='slim-pi-luna-'))
    write(campaign / 'prompt.txt', prompt)
    write(campaign / 'SPEC.md', spec_text)
    write(campaign / 'check.py', check)
    write(campaign / 'slim.toml', '')
    node = shutil.which('node.exe')
    pi = Path(node).parent / 'node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js'
    slim = Path.home() / 'bin/Slim.exe'
    arms = (sys.argv[1:] or ['pi', 'slim']) if arms is None else arms
    if any(arm not in ('pi', 'slim') for arm in arms):
        raise SystemExit('usage: run.py [pi] [slim]')
    manifest = {'model': model, 'provider': provider, 'effort': 'high', 'speed': 'normal', 'order': arms,
                'workspace': str(workspace), 'prompt': prompt, 'scenario': scenario, 'executables': {},
                'route': f'native {provider}; each CLI uses its own configured authentication'}
    for arm, executable in [('pi', pi), ('slim', slim)]:
        version_cmd = [node, str(pi), '--version'] if arm == 'pi' else [str(slim), '--version']
        manifest['executables'][arm] = {'path': str(executable),
            'sha256': hashlib.sha256(executable.read_bytes()).hexdigest(),
            'version': subprocess.check_output(version_cmd, text=True).strip()}
    write(campaign / 'manifest.json', json.dumps(manifest, indent=2))
    print(campaign, flush=True)
    failed = False
    for arm in manifest['order']:
        cwd = workspace / arm
        cwd.mkdir()
        write(cwd / 'SPEC.md', spec_text)
        write(cwd / 'check.py', check)
        for name, content in (files or {}).items():
            target = cwd / name
            target.parent.mkdir(parents=True, exist_ok=True)
            write(target, content)
        common_env = {key: None for key in os.environ if key.startswith('SLIM_') or
                      key in ('CODEX_ACCESS_TOKEN', 'CODEX_ACCOUNT_ID', 'OPENAI_API_KEY',
                              'NODE_OPTIONS', 'PI_CODING_AGENT_DIR', 'PI_CODING_AGENT_SESSION_DIR')}
        if arm == 'pi':
            arguments = [str(pi), '--provider', provider, '--model', model, '--thinking', 'high',
                         '--mode', 'json', '--print', '--no-extensions', '--no-skills',
                         '--no-prompt-templates', '--no-context-files', '--no-themes', '--no-approve',
                         '-e', str(HERE / 'pi-audit.ts'), '--session', str(campaign / 'pi.session.jsonl'), prompt]
            common_env.update({'LUNA_AUDIT_FILE': str(campaign / 'pi.audit.jsonl'), 'PI_TELEMETRY': '0'})
            executable = node
        else:
            arguments = ['--headless', '--provider', provider, '--model', model,
                         '--effort', 'high', '--jsonl',
                         '--session', str(campaign / 'slim.session.jsonl'), '--prompt', prompt]
            if provider == 'openai-codex':
                arguments.append('--normal')
            common_env['SLIM_CONFIG_FILE'] = str(campaign / 'slim.toml')
            executable = str(slim)
        spec = {'agent': arm, 'scenario': scenario, 'run': 1, 'executable': executable,
                'arguments': arguments, 'cwd': str(cwd), 'environment': common_env, 'timeout_seconds': 240}
        paths = [campaign / f'{arm}.{suffix}' for suffix in ('spec.json', 'timing.json', 'stdout.jsonl', 'stderr.log')]
        write(paths[0], json.dumps(spec, indent=2))
        run_process(*(str(path) for path in paths))
        timing = json.loads(paths[1].read_text())
        unchanged = all((cwd / name).read_bytes() == (campaign / name).read_bytes()
                        for name in ('check.py', 'SPEC.md'))
        # Execute the original oracle outside the model's editable check.py.
        validation = subprocess.run([sys.executable, '-c', check], cwd=cwd, capture_output=True, text=True, timeout=15)
        write(campaign / f'{arm}.validation.json', json.dumps({'fixtures_unchanged': unchanged,
              'exit_code': validation.returncode, 'stdout': validation.stdout, 'stderr': validation.stderr}, indent=2))
        shutil.copytree(cwd, campaign / f'{arm}.workspace', ignore=shutil.ignore_patterns('__pycache__'))
        print(arm, json.dumps(timing), 'validation=', validation.returncode, 'fixtures_unchanged=', unchanged, flush=True)
        failed |= timing['exit_code'] != 0 or timing['timed_out'] or validation.returncode != 0 or not unchanged
    if failed:
        raise SystemExit('Benchmark arm failed; inspect timing, native events and validation records.')
    return campaign

if __name__ == '__main__':
    main()
