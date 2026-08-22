"""Adaptive localhost fixture for cross-agent token-economy benchmarks.

Serves an OpenAI-compatible SSE API on 127.0.0.1:8931. For the FIRST request
after a /reset it inspects the advertised `tools`, discovers the agent's
file-reading tool (name + path/limit property names) and replies with a real
tool call for `TARGET_FILE`. Every subsequent request receives a plain stop
response. All request bodies are saved for offline analysis.

Endpoints:
    POST /reset        -> restarts the per-run request counter
    POST /*            -> captured + answered (see above)
"""
import json
import os
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "captures")
os.makedirs(OUT, exist_ok=True)

TARGET_FILE = "bench-target.txt"
PATH_KEYS = ("path", "file_path", "filePath", "abs_path", "filename")
LIMIT_KEYS = ("max_lines", "maxLines", "limit", "max_tokens_lines")

state = {"n": 0}
lock = threading.Lock()

READ_NAME_RE = re.compile(r"(read|view|cat|open)", re.IGNORECASE)


def discover_read_tool(tools):
    """Return (tool_name, arguments_dict) for the agent's own reader."""
    candidates = []
    for tool in tools or []:
        fn = tool.get("function") or {}
        name = fn.get("name") or tool.get("name") or ""
        params = (fn.get("parameters")
                  or tool.get("input_schema")
                  or tool.get("parameters")
                  or {})
        props = set((params.get("properties") or {}).keys())
        candidates.append((name, props))

    def path_key(props):
        return next((k for k in PATH_KEYS if k in props), None)

    # Prefer a tool whose name looks like a reader AND takes a path.
    for name, props in candidates:
        if READ_NAME_RE.search(name) and path_key(props):
            args = {path_key(props): TARGET_FILE}
            limit = next((k for k in LIMIT_KEYS if k in props), None)
            if limit:
                args[limit] = 400
            return name, args
    # Fallback: any tool taking a path.
    for name, props in candidates:
        if path_key(props):
            args = {path_key(props): TARGET_FILE}
            limit = next((k for k in LIMIT_KEYS if k in props), None)
            if limit:
                args[limit] = 400
            return name, args
    return None, None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _sse(self, events, done=True):
        payload = "".join(f"data: {json.dumps(e)}\n\n" for e in events)
        if done:
            payload += "data: [DONE]\n\n"
        raw = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self):
        if self.path.endswith("/reset"):
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with lock:
                state["n"] = 0
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
            return

        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        with lock:
            state["n"] += 1
            n = state["n"]
        tag = re.sub(r"[^A-Za-z0-9_.-]", "_", self.headers.get("X-Bench-Agent", "agent"))
        with open(os.path.join(OUT, f"{tag}_req_{n}.json"), "wb") as f:
            f.write(body)

        parsed = json.loads(body)
        if n == 1:
            name, args = discover_read_tool(parsed.get("tools"))
            if name:
                events = [{"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call-bench-1", "function": {
                        "name": name, "arguments": json.dumps(args)}}]}}]},
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}]
                self._sse(events)
                return
        self._sse([
            {"choices": [{"delta": {"content": "bench-done"}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
        ])


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", 8931), Handler)
    server.serve_forever()
