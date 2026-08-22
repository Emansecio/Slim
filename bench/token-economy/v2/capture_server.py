"""Adaptive localhost fixture v2 for cross-agent token+speed benchmarks.

Serves an OpenAI-compatible SSE API on 127.0.0.1:8931 (override with --port).
Improvements over v1:

- Multi-scenario state machine. POST /reset?scenario=s2_codegen selects the
  script; the server drives the agent through it by inspecting the agent's own
  advertised tools each turn (no hardcoded tool names):
      s1_read      : read -> stop                          (2 turns)
      s2_codegen   : write fizzbuzz.py (~25 lines) -> stop (2 turns)
      s3_multistep : write -> read-back -> stop            (3 turns)
      s4_long      : read -> read -> write -> read -> stop (5 turns)
  Tool discovery matches by regex on tool name AND required schema keys, so
  each agent runs its OWN read/write tools.
- Timing: every request is timestamped (epoch ms + perf_counter) into
  captures/<scenario>/<tag>/req_<n>.meta.json so startup latency and
  turn gaps are measurable offline.
- Compliance gate: records model + reasoning field found per request into
  compliance.jsonl; analyze.py flags any request whose reasoning effort
  is not "high".

Endpoints:
    POST /reset?scenario=<id>  -> restart counters for that scenario
    POST /*                    -> captured + answered per the scenario script
"""

import json
import os
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

BASE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(BASE, "captures")
os.makedirs(OUT, exist_ok=True)

MODEL = "gpt-5.6-luna"
EXPECTED_EFFORT = "high"

TARGET_READ = "bench-target.txt"
TARGET_WRITE = "fizzbuzz.py"

FIZZBUZZ_CODE = (
    "def fizzbuzz(n: int) -> list[str]:\n"
    "    out = []\n"
    "    for i in range(1, n + 1):\n"
    "        if i % 15 == 0:\n"
    "            out.append('FizzBuzz')\n"
    "        elif i % 3 == 0:\n"
    "            out.append('Fizz')\n"
    "        elif i % 5 == 0:\n"
    "            out.append('Buzz')\n"
    "        else:\n"
    "            out.append(str(i))\n"
    "    return out\n"
    "\n"
    "\n"
    "if __name__ == '__main__':\n"
    "    for line in fizzbuzz(25):\n"
    "        print(line)\n"
)

READ_NAME_RE = re.compile(r"(read|view|cat|open)", re.IGNORECASE)
WRITE_NAME_RE = re.compile(r"(write|create|edit|patch|apply)", re.IGNORECASE)
PATH_KEYS = ("path", "file_path", "filePath", "abs_path", "filename")
LIMIT_KEYS = ("max_lines", "maxLines", "limit", "max_tokens_lines")
CONTENT_KEYS = ("content", "contents", "text", "file_text", "new_string", "value")

# Scenario scripts: sequence of actions the fixture performs.
# ("read", file) -> answer with a real tool call to the agent's reader
# ("write", file)-> answer with a real tool call to the agent's writer
# ("readback")   -> like read, but the file was written in a previous turn;
#                   discovery prefers the reader again
# ("stop",)      -> plain final answer
SCENARIOS = {
    "s1_read": ["read"],
    "s2_codegen": ["write"],
    "s3_multistep": ["write", "readback"],
    "s4_long": ["read", "read", "write", "read"],
}

state_lock = threading.Lock()
state = {"scenario": "s1_read", "step": 0, "tag": "agent"}


def _props(tool):
    fn = tool.get("function") or {}
    params = fn.get("parameters") or tool.get("input_schema") or {}
    return fn.get("name") or tool.get("name") or "", set((params.get("properties") or {}).keys())


def _path_key(props):
    return next((k for k in PATH_KEYS if k in props), None)


def discover_tool(tools, name_re):
    """Return (tool_name, args) for the agent's own tool matching name_re."""
    candidates = [_props(t) for t in (tools or [])]
    for name, props in candidates:
        if name_re.search(name) and _path_key(props):
            return name, props
    return None, None


def build_read_call(tools, path):
    name, props = discover_tool(tools, READ_NAME_RE)
    if not name:
        return None, None
    args = {_path_key(props): path}
    limit = next((k for k in LIMIT_KEYS if k in props), None)
    if limit:
        args[limit] = 400
    return name, args


def build_write_call(tools, path):
    name, props = discover_tool(tools, WRITE_NAME_RE)
    if not name:
        return None, None
    pk = _path_key(props)
    ck = next((k for k in CONTENT_KEYS if k in props), None)
    if not ck:
        return None, None
    return name, {pk: path, ck: FIZZBUZZ_CODE}


def find_reasoning_effort(body):
    """Locate the reasoning-effort value wherever the agent put it."""
    if isinstance(body.get("reasoning_effort"), str):
        return body["reasoning_effort"]
    reasoning = body.get("reasoning")
    if isinstance(reasoning, dict) and isinstance(reasoning.get("effort"), str):
        return reasoning["effort"]
    return "(missing)"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _sse(self, events):
        payload = "".join(f"data: {json.dumps(e)}\n\n" for e in events)
        raw = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _reply(self, events):
        # Timing meta is written in do_POST together with the body.
        self._sse(events)

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self):
        parsed_path = urlparse(self.path)
        if parsed_path.path.endswith("/reset"):
            length = int(self.headers.get("Content-Length", "0"))
            self.rfile.read(length)
            qs = parse_qs(parsed_path.query)
            scenario = (qs.get("scenario") or ["s1_read"])[0]
            tag = (qs.get("tag") or ["agent"])[0]
            run = (qs.get("run") or ["1"])[0]
            if scenario not in SCENARIOS:
                self.send_response(400)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            with state_lock:
                state.update(scenario=scenario, step=0, tag=tag, run=run)
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
            return

        length = int(self.headers.get("Content-Length", "0"))
        body_bytes = self.rfile.read(length)
        body = json.loads(body_bytes)

        with state_lock:
            scenario = state["scenario"]
            step = state["step"]
            state["step"] += 1
            # Run directory comes from /reset (?run=N) so every run writes to
            # its own folder — no overwriting between runs.
            tag = state["tag"]
            run = str(state.get("run", "1"))
        d = os.path.join(OUT, scenario, f"run{run}", tag)
        os.makedirs(d, exist_ok=True)
        now_mono = time.perf_counter()
        with open(os.path.join(d, f"req_{step}.json"), "wb") as f:
            f.write(body_bytes)
        with open(os.path.join(d, f"req_{step}.meta.json"), "w", encoding="utf-8") as f:
            json.dump({"req": step, "epoch_ms": int(time.time() * 1000),
                       "perf_counter": now_mono}, f)
        script = SCENARIOS[scenario]
        action = script[step] if step < len(script) else "stop"

        # Compliance record (model + reasoning actually on the wire)
        with state_lock:
            # Prefer the tag registered at /reset; fall back to the header.
            tag = state["tag"] or re.sub(
                r"[^A-Za-z0-9_.-]", "_", self.headers.get("X-Bench-Agent", "agent")
            )
        comp = os.path.join(OUT, scenario, "compliance.jsonl")
        with open(comp, "a", encoding="utf-8") as f:
            f.write(json.dumps({
                "scenario": scenario,
                "agent": tag,
                "req": step,
                "model": body.get("model"),
                "reasoning_effort": find_reasoning_effort(body),
                "expected_model": MODEL,
                "expected_effort": EXPECTED_EFFORT,
            }) + "\n")

        tools = body.get("tools") or []

        if action in ("read", "readback"):
            name, args = build_read_call(tools, TARGET_READ)
            if name:
                events = [
                    {"choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": f"call-bench-{step}", "function": {
                            "name": name, "arguments": json.dumps(args)}}]}}]},
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                ]
                self._reply(events)
                return

        if action == "write":
            name, args = build_write_call(tools, TARGET_WRITE)
            if name:
                events = [
                    {"choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": f"call-bench-{step}", "function": {
                            "name": name, "arguments": json.dumps(args)}}]}}]},
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                ]
                self._reply(events)
                return

        # stop / fallback
        self._reply([
            {"choices": [{"delta": {"content": "bench-done"}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
        ])


if __name__ == "__main__":
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8931)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"capture_server v2 listening on 127.0.0.1:{args.port} "
          f"(model={MODEL}, expected effort={EXPECTED_EFFORT})")
    server.serve_forever()
