"""Local protocol fixture, NOT agentic measurement. Never contacts commercial endpoint.
Capture request tool schemas only; discard headers, reasoning and other payload fields.
"""
import base64
import http.server
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
from evaluate import REPO, RESULTS, FLAGS, ROOT, env, dump

captured = []
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        captured.append(dict(tools=body.get('tools', []), model=body.get('model'),
                             reasoning=body.get('reasoning'), request_path=self.path))
        response = json.dumps({'error': {'message': 'offline schema probe complete; intentional stop', 'type': 'invalid_request_error'}}).encode()
        self.send_response(400)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        self.wfile.write(response)

if __name__ == '__main__':
    work = Path(tempfile.mkdtemp(prefix='slim-schema-probe-'))
    shutil.copytree(ROOT/'initial/C', work, dirs_exist_ok=True)
    server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    child_env = env()
    claims = base64.urlsafe_b64encode(json.dumps({'https://api.openai.com/auth': {'chatgpt_account_id': 'offline-probe'}}).encode()).decode().rstrip('=')
    child_env['SLIM_API_KEY'] = 'e30.' + claims + '.offline-dummy-not-a-credential'
    # Same native Codex route/config/schema, local endpoint, dummy credential.
    args = [str(REPO/'target/release/slim.exe'), '--headless', '--jsonl', '--provider', 'openai-codex',
            '--model', 'gpt-5.6-luna', '--effort', 'high', '--normal',
            '--endpoint', f'http://127.0.0.1:{server.server_port}', '--prompt', (ROOT/'prompt-C.txt').read_text(encoding='utf-8')]
    try:
        run = subprocess.run(args, cwd=work, env=child_env, capture_output=True, timeout=60, creationflags=FLAGS)
    finally:
        server.shutdown()
        thread.join()
        server.server_close()
    dump(RESULTS/'schema-probe.json', dict(kind='offline protocol fixture, not a model run',
         workspace=str(work), exit=run.returncode, requests=captured,
         stdout=run.stdout.decode(errors='replace'), stderr=run.stderr.decode(errors='replace')))
    print('exit', run.returncode, 'requests', len(captured))
    for request in captured:
        print([t.get('name', t.get('function',{}).get('name')) for t in request['tools']])
