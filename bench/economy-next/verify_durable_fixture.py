"""Offline native CLI check of the v2 setup, not a token benchmark."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

from run_cycle import HERE, save

requests=[]


class Handler(BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        chunks=[dict(id='offline',model='fixture',choices=[dict(index=0,delta=dict(content='offline durable response'),finish_reason=None)]),
                dict(id='offline',model='fixture',choices=[dict(index=0,delta={},finish_reason='stop')],
                     usage=dict(prompt_tokens=10,completion_tokens=2,total_tokens=12))]
        payload=''.join('data: '+json.dumps(chunk)+'\n\n' for chunk in chunks)+'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Content-Length',str(len(payload.encode())))
        self.end_headers(); self.wfile.write(payload.encode())


server=HTTPServer(('127.0.0.1',0),Handler)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
try:
    with tempfile.TemporaryDirectory(prefix='slim-durable-bench-check-') as tmp:
        root=Path(tmp); workspace=root/'workspace';workspace.mkdir()
        session=root/'session.jsonl'; config=root/'empty.toml';config.write_text('',encoding='utf-8')
        session.write_text(json.dumps(dict(type='session',schema_version=2,id='offline',timestamp='2026-09-05T00:00:00Z',
            cwd=str(workspace),parent_id=None,cutoff_seq=None))+'\n',encoding='utf-8')
        env={k:v for k,v in os.environ.items() if not k.startswith('SLIM_')}
        env.update(SLIM_CONFIG_FILE=str(config),SLIM_API_KEY='fixture-only')
        outputs=[]
        for prompt in ['first durable prompt','second durable prompt']:
            result=subprocess.run([str(HERE/'slim-before.exe'),'--headless','--provider','openai-compatible','--model','fixture',
                '--endpoint',f'http://127.0.0.1:{server.server_port}/v1/chat/completions','--effort','high',
                '--resume',str(session),'--prompt',prompt,'--jsonl'],cwd=workspace,env=env,capture_output=True,text=True,timeout=25)
            assert result.returncode==0,(result.returncode,result.stderr)
            output=json.loads(result.stdout);assert output['usage_complete'],output
            outputs.append(output['text'])
        assert len(requests)==2
        history=requests[1]['messages']
        serialized=json.dumps(history)
        assert 'first durable prompt' in serialized and 'second durable prompt' in serialized and 'offline durable response' in serialized
        records=[json.loads(line) for line in session.read_text(encoding='utf-8').splitlines()]
        save(HERE/'durable-fixture-check.json',dict(passed=True,offline_only=True,requests=len(requests),
            outputs=outputs,history_roles=[m['role'] for m in history],durable_records=len(records)))
        print('PASS: original native CLI, two durable v2 turns, retained user/assistant history; localhost fixture only.')
finally:
    server.shutdown();server.server_close();thread.join()
