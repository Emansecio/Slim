"""Adaptive localhost fixture for the Slim/Pi/Pit benchmark.

The runner starts one fixture process per campaign and resets it before each
scenario/run/agent arm. Every reset removes that arm's previous artifacts, so a
short or failed rerun cannot inherit stale requests.
"""

import argparse
import json
import os
import re
import shutil
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

BASE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_OUT = os.path.join(BASE, "campaigns")
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

SCENARIOS = {
    "s1_read": ["read"],
    "s2_codegen": ["write"],
    "s3_multistep": ["write", "readback"],
    "s4_long": ["read", "read", "write", "readback"],
}
EXPECTED_REQUESTS = {name: len(actions) + 1 for name, actions in SCENARIOS.items()}

state_lock = threading.Lock()
state = {"scenario": "s1_read", "step": 0, "tag": "agent", "run": "1"}
OUT = DEFAULT_OUT


def _props(tool):
    fn = tool.get("function") or {}
    params = fn.get("parameters") or tool.get("input_schema") or tool.get("parameters") or {}
    name = fn.get("name") or tool.get("name") or ""
    return name, set((params.get("properties") or {}).keys())


def _path_key(props):
    return next((key for key in PATH_KEYS if key in props), None)


def discover_tool(tools, name_re, required_groups=()):
    """Return the first matching tool satisfying every required key group."""
    for tool in tools or []:
        name, props = _props(tool)
        if not name_re.search(name) or not _path_key(props):
            continue
        if all(any(key in props for key in group) for group in required_groups):
            return name, props
    return None, None


def build_read_call(tools, path):
    name, props = discover_tool(tools, READ_NAME_RE)
    if not name:
        return None, None
    args = {_path_key(props): path}
    limit = next((key for key in LIMIT_KEYS if key in props), None)
    if limit:
        args[limit] = 400
    return name, args


def build_write_call(tools, path):
    # Pi advertises `edit` before `write`. Consider every compatible schema and
    # prefer a dedicated writer instead of stopping at the first name match.
    candidates = []
    for tool in tools or []:
        name, props = _props(tool)
        content_key = next((key for key in CONTENT_KEYS if key in props), None)
        if WRITE_NAME_RE.search(name) and _path_key(props) and content_key:
            priority = 0 if name.lower() in ("write", "create") else 1
            candidates.append((priority, name, props, content_key))
    if not candidates:
        return None, None
    _, name, props, content_key = min(candidates, key=lambda candidate: candidate[0])
    return name, {_path_key(props): path, content_key: FIZZBUZZ_CODE}


def find_reasoning_effort(body):
    if isinstance(body.get("reasoning_effort"), str):
        return body["reasoning_effort"]
    reasoning = body.get("reasoning")
    if isinstance(reasoning, dict) and isinstance(reasoning.get("effort"), str):
        return reasoning["effort"]
    return "(missing)"


def safe_component(value, fallback):
    value = re.sub(r"[^A-Za-z0-9_.-]", "_", str(value))
    return value or fallback


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def _json(self, status, payload):
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _sse(self, events):
        payload = "".join(f"data: {json.dumps(event)}\n\n" for event in events)
        payload += "data: [DONE]\n\n"
        raw = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self._json(200, {"ok": True})

    def do_POST(self):
        parsed_path = urlparse(self.path)
        length = int(self.headers.get("Content-Length", "0"))
        body_bytes = self.rfile.read(length)

        if parsed_path.path.endswith("/reset"):
            query = parse_qs(parsed_path.query)
            scenario = (query.get("scenario") or ["s1_read"])[0]
            if scenario not in SCENARIOS:
                self._json(400, {"error": "unknown scenario"})
                return
            tag = safe_component((query.get("tag") or ["agent"])[0], "agent")
            run = safe_component((query.get("run") or ["1"])[0], "1")
            arm_dir = os.path.join(OUT, scenario, f"run{run}", tag)
            shutil.rmtree(arm_dir, ignore_errors=True)
            os.makedirs(arm_dir, exist_ok=True)
            with state_lock:
                state.update(scenario=scenario, step=0, tag=tag, run=run)
            self._json(200, {"ok": True, "expected_requests": EXPECTED_REQUESTS[scenario]})
            return

        try:
            body = json.loads(body_bytes)
        except json.JSONDecodeError:
            self._json(400, {"error": "invalid JSON"})
            return

        with state_lock:
            scenario = state["scenario"]
            step = state["step"]
            state["step"] += 1
            tag = state["tag"]
            run = state["run"]

        arm_dir = os.path.join(OUT, scenario, f"run{run}", tag)
        os.makedirs(arm_dir, exist_ok=True)
        now_mono = time.perf_counter()
        now_epoch_ms = int(time.time() * 1000)
        with open(os.path.join(arm_dir, f"req_{step}.json"), "wb") as output:
            output.write(body_bytes)
        with open(os.path.join(arm_dir, f"req_{step}.meta.json"), "w", encoding="utf-8") as output:
            json.dump({"req": step, "epoch_ms": now_epoch_ms, "perf_counter": now_mono}, output)

        record = {
            "scenario": scenario,
            "run": run,
            "agent": tag,
            "req": step,
            "model": body.get("model"),
            "reasoning_effort": find_reasoning_effort(body),
            "expected_model": MODEL,
            "expected_effort": EXPECTED_EFFORT,
        }
        record["ok"] = (
            record["model"] == record["expected_model"]
            and record["reasoning_effort"] == record["expected_effort"]
        )
        with open(os.path.join(arm_dir, "compliance.jsonl"), "a", encoding="utf-8") as output:
            output.write(json.dumps(record) + "\n")

        script = SCENARIOS[scenario]
        action = script[step] if step < len(script) else "stop"
        tools = body.get("tools") or []

        if action in ("read", "readback"):
            target = TARGET_WRITE if action == "readback" else TARGET_READ
            name, args = build_read_call(tools, target)
            if name:
                self._sse([
                    {"choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": f"call-bench-{step}",
                        "function": {"name": name, "arguments": json.dumps(args)},
                    }]}}]},
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                ])
                return

        if action == "write":
            name, args = build_write_call(tools, TARGET_WRITE)
            if name:
                self._sse([
                    {"choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": f"call-bench-{step}",
                        "function": {"name": name, "arguments": json.dumps(args)},
                    }]}}]},
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                ])
                return

        self._sse([
            {"choices": [{"delta": {"content": "bench-done"}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
        ])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8931)
    parser.add_argument("--out", default=DEFAULT_OUT)
    args = parser.parse_args()
    OUT = os.path.abspath(args.out)
    os.makedirs(OUT, exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(
        f"capture_server v3 listening on 127.0.0.1:{args.port} "
        f"(out={OUT}, model={MODEL}, expected effort={EXPECTED_EFFORT})",
        flush=True,
    )
    server.serve_forever()
