"""One real Codex coding task per native CLI; no proxy or model-output fixtures."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import traceback
import difflib
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / 'token-economy' / 'v2'))
from process_runner import main as run_process

MODEL = 'gpt-6.1-sol'
# Pi names the Codex/ChatGPT OAuth route `openai`; Slim calls it `openai-codex`.
PI_PROVIDERS = {'openai-codex': 'openai'}
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

PI_BUNDLE = Path('@earendil-works/pi-coding-agent/dist/bundle/cli.js')


class BenchmarkFailure(RuntimeError):
    def __init__(self, campaign):
        self.campaign = campaign
        super().__init__(f'Benchmark failed; inspect evidence in {campaign}')


def write(path, value):
    path.write_text(value, encoding='utf-8')


def digest_text(value):
    return hashlib.sha256(value.encode('utf-8')).hexdigest()


def digest_file(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def npm_global_root():
    try:
        output = subprocess.check_output('npm root -g', shell=True, text=True, timeout=20)
        return Path(output.strip())
    except (OSError, subprocess.SubprocessError):
        return None


def resolve_pi(node):
    candidates = []
    root = npm_global_root()
    if root:
        candidates.append(root / PI_BUNDLE)
    candidates.append(Path(node).parent / 'node_modules' / PI_BUNDLE)
    appdata = os.environ.get('APPDATA')
    if appdata:
        candidates.append(Path(appdata) / 'npm' / 'node_modules' / PI_BUNDLE)
    # Managed install (`~/.pi/agent/bin/pi` launcher) keeps releases by version.
    managed = Path.home() / '.pi' / 'agent' / 'install'
    try:
        version = (managed / 'current-version').read_text(encoding='utf-8').strip()
        candidates.append(managed / 'releases' / version / 'node_modules' / PI_BUNDLE)
    except OSError:
        pass
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise SystemExit('Pi bundle not found; checked: ' + '; '.join(str(c) for c in candidates))


def resolve_slim():
    candidates = [Path.home() / 'bin' / 'Slim.exe']
    on_path = shutil.which('Slim.exe') or shutil.which('slim')
    if on_path:
        candidates.append(Path(on_path))
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise SystemExit('Slim executable not found; checked: ' + '; '.join(str(c) for c in candidates))


def version_of(command):
    try:
        return subprocess.check_output([*command, '--version'], text=True, timeout=20,
                                       stderr=subprocess.STDOUT).strip()
    except (OSError, subprocess.SubprocessError) as error:
        raise SystemExit('version probe failed for {}: {}'.format(command[0], error))


def python_shim(campaign, oracle_python):
    """Directory with `python` for both arms when the host only has `python3`."""
    if shutil.which('python'):
        return None
    shim = campaign / 'bin'
    shim.mkdir()
    # A script, not a symlink: macOS /usr/bin/python3 is an xcrun stub that dispatches on argv[0].
    script = shim / 'python'
    script.write_text('#!/bin/sh\nexec "{}" "$@"\n'.format(oracle_python), encoding='utf-8')
    script.chmod(0o755)
    return shim


def text_files(root):
    files = {}
    for path in sorted(root.rglob('*')):
        if path.is_file() and '__pycache__' not in path.parts and path.stat().st_size <= 512 * 1024:
            try:
                files[path.relative_to(root).as_posix()] = path.read_text(encoding='utf-8').splitlines(keepends=True)
            except UnicodeDecodeError:
                continue
    return files


def write_code_diff(campaign, arms):
    """Unified diff of the final workspaces, first arm as the base."""
    roots = [campaign / f'{arm}.workspace' for arm in arms]
    if len(arms) != 2 or not all(root.is_dir() for root in roots):
        return None
    left, right = (text_files(root) for root in roots)
    chunks = []
    for name in sorted(set(left) | set(right)):
        chunks.extend(difflib.unified_diff(left.get(name, []), right.get(name, []),
                                           f'{arms[0]}/{name}', f'{arms[1]}/{name}'))
    write(campaign / 'code.diff', ''.join(line if line.endswith('\n') else line + '\n' for line in chunks))
    return campaign / 'code.diff'


def fresh_campaign(base):
    for suffix in ['', *('-{}'.format(n) for n in range(2, 100))]:
        candidate = HERE / (base + suffix)
        try:
            candidate.mkdir(parents=True)
            return candidate
        except FileExistsError:
            continue
    raise SystemExit('could not allocate campaign directory for ' + base)


def harness_files():
    names = [HERE / name for name in ('run.py', 'daily.py', 'holdouts.py', 'visual.py', 'analyze.py', 'report.py', 'pi-audit.ts')]
    names.append(HERE.parent / 'token-economy' / 'v2' / 'process_runner.py')
    return {path.name: digest_file(path) for path in names if path.exists()}


def main(arms=None, *, prompt=PROMPT, spec_text=SPEC, check=CHECK, files=None, scenario='merge_ranges',
         provider='openai-codex', model=MODEL, timeout_seconds=240, validation_timeout=30,
         audit=True, keep_workspace=False, round_number=1, pi_provider=None, visual=None):
    node = shutil.which('node') or shutil.which('node.exe')
    if not node:
        raise SystemExit('node not found on PATH; the Pi arm requires it')
    pi = resolve_pi(node)
    slim = resolve_slim()
    oracle_python = shutil.which('python') or shutil.which('python3') or sys.executable
    pi_provider = pi_provider or PI_PROVIDERS.get(provider, provider)
    arms = (sys.argv[1:] or ['pi', 'slim']) if arms is None else arms
    if any(arm not in ('pi', 'slim') for arm in arms):
        raise SystemExit('usage: run.py [pi] [slim]')
    executables = {}
    for arm, executable, command in [('pi', pi, [node, str(pi)]), ('slim', slim, [str(slim)])]:
        executables[arm] = {'path': str(executable),
            'sha256': digest_file(executable), 'version': version_of(command)}
    route_suffix = '' if provider == 'openai-codex' else '-' + provider
    base = datetime.now(timezone.utc).strftime('%Y%m%d-%H%M%SZ') + route_suffix + ('' if scenario == 'merge_ranges' else '-' + scenario)
    campaign = fresh_campaign(base)
    workspace = Path(tempfile.mkdtemp(prefix='slim-pi-luna-'))
    write(campaign / 'prompt.txt', prompt)
    write(campaign / 'SPEC.md', spec_text)
    write(campaign / 'check.py', check)
    write(campaign / 'slim.toml', '')
    shim = python_shim(campaign, oracle_python)
    fixtures = {'SPEC.md': spec_text, 'check.py': check, **(files or {})}
    for arm in arms:
        for name, content in fixtures.items():
            target = workspace / arm / name
            target.parent.mkdir(parents=True, exist_ok=True)
            write(target, content)
    fixture_info = {name: {'sha256': digest_file(workspace / arms[0] / name),
                           'bytes': (workspace / arms[0] / name).stat().st_size}
                    for name in fixtures}
    manifest = {'model': model, 'provider': provider, 'pi_provider': pi_provider, 'effort': 'high', 'speed': 'normal', 'order': arms,
                'round': round_number, 'fixture_hash_kind': 'materialized-bytes-v1',
                'workspace': str(workspace), 'prompt': prompt, 'scenario': scenario,
                'executables': executables,
                'oracle_python': oracle_python,
                'python_shim': str(shim) if shim else None,
                'visual': visual,
                'skills': 'both arms discover the shared ~/.agents/skills',
                'fixtures': fixture_info,
                'harness': {'python': sys.version.split()[0], 'platform': sys.platform,
                            'files': harness_files()},
                'route': f'native slim={provider} pi={pi_provider}; each CLI uses its own configured authentication'}
    write(campaign / 'manifest.json', json.dumps(manifest, indent=2))
    print(campaign, flush=True)
    failed = False
    for arm in manifest['order']:
        try:
            cwd = workspace / arm
            assert all(digest_file(cwd / name) == info['sha256'] for name, info in fixture_info.items()), 'initial fixtures differ'
            common_env = {key: None for key in os.environ if key.startswith('SLIM_') or
                          key in ('CODEX_ACCESS_TOKEN', 'CODEX_ACCOUNT_ID', 'OPENAI_API_KEY',
                                  'NODE_OPTIONS', 'PI_CODING_AGENT_DIR', 'PI_CODING_AGENT_SESSION_DIR')}
            if shim:
                common_env['PATH'] = str(shim) + os.pathsep + os.environ.get('PATH', '')
            if arm == 'pi':
                arguments = [str(pi), '--provider', pi_provider, '--model', model, '--thinking', 'high',
                             # Skills stay on: both CLIs read the shared ~/.agents/skills.
                             '--mode', 'json', '--print', '--no-extensions',
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
            spec = {'agent': arm, 'scenario': scenario, 'run': round_number, 'executable': executable,
                    'arguments': arguments, 'cwd': str(cwd), 'environment': common_env,
                    'timeout_seconds': timeout_seconds}
            paths = [campaign / f'{arm}.{suffix}' for suffix in ('spec.json', 'timing.json', 'stdout.jsonl', 'stderr.log')]
            write(paths[0], json.dumps(spec, indent=2))
            run_process(*(str(path) for path in paths))
            timing = json.loads(paths[1].read_text())
            unchanged = all((cwd / name).read_bytes() == (campaign / name).read_bytes()
                            for name in ('check.py', 'SPEC.md'))
            # Execute the original oracle outside the model's editable check.py.
            try:
                validation = subprocess.run([oracle_python, '-c', check], cwd=cwd, capture_output=True,
                                            text=True, timeout=validation_timeout)
                record = {'fixtures_unchanged': unchanged, 'exit_code': validation.returncode,
                          'stdout': validation.stdout, 'stderr': validation.stderr}
            except (OSError, subprocess.SubprocessError) as error:
                record = {'fixtures_unchanged': unchanged, 'exit_code': None, 'oracle_error': str(error)}
            write(campaign / f'{arm}.validation.json', json.dumps(record, indent=2))
            shutil.copytree(cwd, campaign / f'{arm}.workspace', ignore=shutil.ignore_patterns('__pycache__'))
            if visual:
                import visual as visual_scenario
                visual_scenario.capture(campaign / f'{arm}.workspace', campaign / 'visual', arm, visual)
            print(arm, json.dumps(timing), 'validation=', record['exit_code'],
                  'fixtures_unchanged=', unchanged, flush=True)
            failed |= (timing['exit_code'] != 0 or timing['timed_out']
                       or record['exit_code'] != 0 or not unchanged)
        except Exception as error:
            failed = True
            write(campaign / f'{arm}.error.json', json.dumps(
                {'arm': arm, 'type': type(error).__name__, 'error': str(error),
                 'traceback': traceback.format_exc()}, indent=2))
            print(arm, 'harness_error', repr(error), flush=True)
    write_code_diff(campaign, manifest['order'])
    if visual:
        import visual as visual_scenario
        print('visual:', visual_scenario.gallery(campaign, manifest['order'], visual), flush=True)
    if audit and all((campaign / f'{arm}.timing.json').exists() for arm in ('pi', 'slim')):
        try:
            spec_mod = importlib.util.spec_from_file_location('luna_live_analyze', HERE / 'analyze.py')
            analyze = importlib.util.module_from_spec(spec_mod)
            spec_mod.loader.exec_module(analyze)
            analyze.audit(campaign, campaign, campaign / 'summary.json')
        except Exception as error:
            print('audit_failed:', repr(error), flush=True)
            failed = True
            write(campaign / 'audit.error.json', json.dumps({'error': str(error)}))
    if failed:
        raise BenchmarkFailure(campaign)
    if not keep_workspace:
        shutil.rmtree(workspace, ignore_errors=True)
    return campaign

if __name__ == '__main__':
    main()
