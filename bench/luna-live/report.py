"""Comparativo rastreavel Slim x Pi: chamadas, erros, tokens e tempo.

Melhoria sobre analyze.py: aceita N campanhas (daily.py/run.py), classifica
erros de ferramenta por taxonomia com trecho do erro, abre tempo em
wall/provider/tools/residuo e emite markdown + json com ponteiros de
rastreabilidade (arquivos, seq, manifest com versoes/hashes).

Uso:
  python report.py CAMPAIGN [CAMPAIGN ...] --output-md RELATORIO.md --output-json report.json
"""
import argparse
import hashlib
import json
from pathlib import Path
from datetime import datetime, timezone
from collections import Counter
from statistics import median
import re


def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def records(path):
    return [json.loads(line) for line in Path(path).read_text(encoding="utf-8").splitlines() if line.strip()]


def excerpt(text, limit=200):
    if text is None:
        return ""
    flat = str(text).replace("\r", " ").replace("\n", " ")
    flat = " ".join(flat.split())
    return flat if len(flat) <= limit else flat[:limit] + "..."


def short_args(value, limit=120):
    if value is None:
        return ""
    raw = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)
    return excerpt(raw, limit)


def classify_error(tool, text):
    t = (text or "").lower()
    name = (tool or "").lower()
    if any(k in t for k in ("timed out", "timeout", "timedout")):
        return "timeout"
    if name in ("shell", "bash"):
        if "jsondecodeerror" in t:
            return "invalid-json"
        if "assertionerror" in t or re.search(r"\b[1-9]\d* failed\b", t):
            return "validation-failed"
        if "not a git repository" in t:
            return "git-no-repository"
        if "usage: git diff" in t:
            return "git-command-usage"
        if "traceback (most recent call last)" in t:
            return "program-error"
        if any(k in t for k in ("parsererror", "unexpected token", "syntax error near", "was unexpected at this time")):
            return "shell-syntax"
        if "not recognized" in t or "command not found" in t:
            return "command-not-found"
    if any(k in t for k in ("enoent", "no such file", "arquivo especificado", "not found", "does not exist", "cannot find", "missing")):
        if name in ("read", "list", "bash", "shell"):
            return "read-missing"
        return "missing-target"
    if "expected" in t and any(k in t for k in ("exist", "precond", "empty", "ausente", "vazio")):
        return "write-precondition"
    if name in ("patch", "edit") and any(k in t for k in ("patch", "match", "hunk", "apply", "conflict", "find", "replace")):
        return "patch-reject"
    if "todo" in t or "transition" in t or "in_progress" in t or "inprogress" in t:
        return "todo-transition"
    if "timeout" in t or "timed out" in t:
        return "timeout"
    return "other-error"


def pi_request_components(request):
    """Match Slim's disjoint JSON item byte accounting, before payload redaction."""
    keys = ("system_bytes", "tool_schema_bytes", "history_bytes", "tool_result_bytes")
    if request.get("component_schema") == "slim-components-v1":
        return {key: request[key] for key in keys}
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return dict.fromkeys(keys)
    size = lambda value: len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
    result = dict.fromkeys(keys, 0)
    for key in ("instructions", "system"):
        if key in payload:
            result["system_bytes"] = size(payload[key])
            break
    if "tools" in payload:
        result["tool_schema_bytes"] = size(payload["tools"])
    for field in ("messages", "input"):
        if field not in payload:
            continue
        items = payload[field]
        if not isinstance(items, list):
            result["history_bytes"] += size(items)
            continue
        for item in items:
            role = item.get("role")
            content = item.get("content")
            result_types = ("function_call_output", "tool_result")
            tool_result = item.get("type") in result_types or role == "tool" or (
                isinstance(content, list) and any(c.get("type") in result_types for c in content if isinstance(c, dict)))
            key = "system_bytes" if role in ("system", "developer") else "tool_result_bytes" if tool_result else "history_bytes"
            result[key] += size(item)
    return result


def pi_text_of_result(result):
    if not isinstance(result, dict):
        return excerpt(result)
    parts = []
    content = result.get("content")
    if isinstance(content, list):
        for item in content:
            if isinstance(item, dict) and "text" in item:
                parts.append(str(item["text"]))
            else:
                parts.append(json.dumps(item, ensure_ascii=False))
    elif content is not None:
        parts.append(str(content))
    if result.get("details"):
        parts.append(json.dumps(result["details"], ensure_ascii=False))
    return " ".join(p for p in parts if p)


def summarize_pi(cdir):
    audit_path = cdir / "pi.audit.jsonl"
    if not audit_path.exists():
        return None
    audit = records(audit_path)
    requests = [e for e in audit if e.get("type") == "request"]
    messages = [e for e in audit if e.get("type") == "message_end" and (e.get("message") or {}).get("role") == "assistant"]
    tools = {}
    for event in audit:
        if event.get("type") == "tool_execution_start":
            tools[event["toolCallId"]] = dict(event)
        elif event.get("type") == "tool_execution_end":
            tool = tools.get(event.get("toolCallId"))
            if tool is None:
                tools[event.get("toolCallId")] = {"toolCallId": event.get("toolCallId"), "orphan_end": True, **event}
                tool = tools[event.get("toolCallId")]
            tool.update(duration_ms=event.get("time_ms", tool.get("time_ms", 0)) - tool.get("time_ms", event.get("time_ms", 0)),
                        success=not event.get("isError"), result=event.get("result"),
                        toolName=tool.get("toolName") or event.get("toolName"))
    rows = []
    call_turns = {call["id"]: index for index, event in enumerate(messages, 1)
                  for call in event["message"].get("content", []) if call.get("type") == "toolCall"}
    pair_count = min(len(requests), len(messages))
    for index in range(pair_count):
        request, end = requests[index], messages[index]
        message = end.get("message", {})
        usage = message.get("usage", {})
        components = pi_request_components(request)
        rows.append({
            "turn": index + 1,
            "input": usage.get("input", 0) + usage.get("cacheRead", 0) + usage.get("cacheWrite", 0),
            "uncached": usage.get("input", 0),
            "cache": usage.get("cacheRead", 0),
            "cache_write": usage.get("cacheWrite", 0),
            "output": usage.get("output", 0),
            "reasoning": usage.get("reasoning", 0),
            "provider_ms": end.get("time_ms", 0) - request.get("time_ms", 0),
            "tools": [c.get("name") for c in message.get("content", []) if isinstance(c, dict) and c.get("type") == "toolCall"],
            "model": message.get("model") or request.get("model"),
            "system_bytes": components["system_bytes"],
            "schema_bytes": components["tool_schema_bytes"],
            "history_bytes": components["history_bytes"],
            "tool_result_bytes": components["tool_result_bytes"],
            "component_source": "wire" if request.get("component_schema") else "redacted-payload",
        })
    tool_list = []
    for call_id, tool in tools.items():
        if "toolName" not in tool and "toolCallId" in tool:
            continue
        name = tool.get("toolName") or tool.get("name") or "?"
        ok = bool(tool.get("success", False)) if "success" in tool else None
        err_text = ""
        if ok is False:
            err_text = pi_text_of_result(tool.get("result"))
        tool_list.append({
            "call_id": call_id,
            "name": name,
            "turn": call_turns.get(call_id),
            "success": ok,
            "duration_ms": tool.get("duration_ms", 0),
            "args": short_args(tool.get("args")),
            "error_kind": classify_error(name, err_text) if ok is False else "",
            "error": excerpt(err_text),
            "trace": "pi.audit.jsonl:" + str(call_id)[-12:],
        })
    complete = bool(requests) and len(requests) == len(messages) and all(
        all(isinstance(e["message"].get("usage", {}).get(key), (int, float))
            for key in ("input", "output", "cacheRead", "cacheWrite"))
        and e["message"].get("stopReason") != "error" for e in messages)
    return {"rows": rows, "tools": tool_list, "request_count": len(requests),
            "message_count": len(messages), "metrics_complete": complete}


def infer_new_failure(name, content):
    text = content or ""
    low = text.lower()
    if (name or "").lower() in ("shell", "bash"):
        m = re.search(r"^exit\s+(\d+)", text.strip())
        if m:
            return (m.group(1) == "0", "" if m.group(1) == "0" else text)
        if any(k in low for k in ("not recognized", "was unexpected", "cannot be loaded")):
            return (False, text)
        return (True, "")
    if any(k in low for k in ("os error", "does not exist", "cannot find", "not found",
                              "no such file", "arquivo especificado", "failed to",
                              "precondition", "rejected", "conflict", "traceback")):
        return (False, text)
    return (True, "")


def summarize_slim(cdir):
    session_path = cdir / "slim.session.jsonl"
    stdout_path = cdir / "slim.stdout.jsonl"
    if not session_path.exists() or not stdout_path.exists():
        return None
    slim = load(stdout_path)
    usage = slim.get("usage", {}) or {}
    usage_requests = usage.get("requests") or []
    recs = records(session_path)
    legacy = [r for r in recs if r.get("type") == "event" and r.get("event")]
    if legacy:
        events = [r["event"] for r in legacy]
        rows, tools = [], {}
        outputs = {}
        for event in events:
            kind = event.get("kind", {})
            ktype = kind.get("type")
            if ktype == "ContextSnapshot":
                idx = len(rows)
                u = usage_requests[idx] if idx < len(usage_requests) else {}
                rows.append({
                    "turn": idx + 1, "seq": event.get("seq"),
                    "input": u.get("uncached_input_tokens", 0) + u.get("cache_read_tokens", 0) + u.get("cache_write_tokens", 0),
                    "uncached": u.get("uncached_input_tokens", 0),
                    "cache": u.get("cache_read_tokens", 0),
                    "cache_write": u.get("cache_write_tokens", 0),
                    "output": u.get("output_tokens", 0),
                    "reasoning": u.get("reasoning_tokens", 0),
                    "provider_ms": u.get("provider_latency_ms", 0),
                    "tools": [],
                    "system_bytes": u.get("system_bytes", 0),
                    "schema_bytes": u.get("tool_schema_bytes", 0),
                    "history_bytes": u.get("history_bytes", 0),
                    "tool_result_bytes": u.get("tool_result_bytes", 0),
                })
            elif ktype == "ToolStarted":
                if rows:
                    rows[-1]["tools"].append(kind.get("name"))
                tools[kind.get("call_id")] = {"seq": event.get("seq"), "name": kind.get("name"),
                                              "arguments": kind.get("arguments"), "turn": len(rows)}
            elif ktype == "ToolOutput":
                outputs[kind.get("call_id")] = kind.get("output")
                if kind.get("call_id") in tools:
                    tools[kind.get("call_id")]["output"] = kind.get("output")
            elif ktype == "ToolFinished":
                entry = tools.get(kind.get("call_id"))
                if entry is not None:
                    entry.update(success=kind.get("success"), duration_ms=kind.get("duration_ms", 0),
                                 finished_seq=event.get("seq"))
        tool_list = []
        for call_id, tool in tools.items():
            ok = tool.get("success")
            err_text = ""
            if ok is False:
                err_text = str(tool.get("output") or outputs.get(call_id) or "")
            tool_list.append({
                "call_id": call_id,
                "name": tool.get("name") or "?",
                "turn": tool.get("turn", 0),
                "seq": tool.get("seq"),
                "success": bool(ok) if ok is not None else None,
                "duration_ms": tool.get("duration_ms", 0),
                "args": short_args(tool.get("arguments")),
                "error_kind": classify_error(tool.get("name"), err_text) if ok is False else "",
                "error": excerpt(err_text),
                "trace": "slim.session.jsonl:seq=" + str(tool.get("seq")),
            })
        tool_ms = sum(t.get("duration_ms", 0) or 0 for t in tool_list if isinstance(t.get("duration_ms"), (int, float)))
        return {"rows": rows, "tools": tool_list, "tool_ms_sum": tool_ms,
                "provider_turns": usage.get("provider_turns"),
                "metrics_complete": bool(rows) and len(rows) == len(usage_requests) == usage.get("provider_turns") and bool(slim.get("usage_complete")) and not slim.get("usage_unknown"),
                "usage_complete": slim.get("usage_complete"), "usage_unknown": slim.get("usage_unknown")}
    calls, tool_outputs = [], {}
    turn = 0
    for r in recs:
        if r.get("type") != "entry":
            continue
        entry = r.get("entry", {})
        if entry.get("role") == "assistant":
            turn += 1
            for call in entry.get("tool_calls") or []:
                calls.append({"turn": turn, "call_id": call.get("id"),
                              "name": call.get("name"), "arguments": call.get("arguments")})
        elif entry.get("role") == "tool" and entry.get("tool_call_id"):
            tool_outputs[entry["tool_call_id"]] = entry.get("content", "")
    rows = []
    for i, u in enumerate(usage_requests, 1):
        rows.append({
            "turn": i, "seq": None,
            "input": u.get("uncached_input_tokens", 0) + u.get("cache_read_tokens", 0) + u.get("cache_write_tokens", 0),
            "uncached": u.get("uncached_input_tokens", 0),
            "cache": u.get("cache_read_tokens", 0),
            "cache_write": u.get("cache_write_tokens", 0),
            "output": u.get("output_tokens", 0),
            "reasoning": u.get("reasoning_tokens", 0),
            "provider_ms": u.get("provider_latency_ms", 0),
            "tools": [c["name"] for c in calls if c["turn"] == i],
            "system_bytes": u.get("system_bytes", 0),
            "schema_bytes": u.get("tool_schema_bytes", 0),
            "history_bytes": u.get("history_bytes", 0),
            "tool_result_bytes": u.get("tool_result_bytes", 0),
        })
    tool_facts = {}
    for r in recs:
        if r.get("type") == "fact":
            fact = r.get("fact", {})
            if fact.get("namespace") == "tool.v1":
                tool_facts[fact.get("key")] = fact.get("value", {})
    tool_list = []
    for c in calls:
        content = tool_outputs.get(c["call_id"], "")
        ok, err = infer_new_failure(c["name"], content)
        decided = tool_facts.get(c["call_id"])
        duration = None
        if isinstance(decided, dict):
            if isinstance(decided.get("success"), bool):
                ok = decided["success"]
                err = "" if ok else content
            duration = decided.get("duration_ms")
        tool_list.append({
            "call_id": c["call_id"],
            "name": c["name"] or "?",
            "turn": c["turn"],
            "seq": None,
            "success": ok,
            "duration_ms": duration,
            "args": short_args(c["arguments"]),
            "error_kind": classify_error(c["name"], err) if ok is False else "",
            "error": excerpt(err),
            "trace": "slim.session.jsonl:call=..." + str(c["call_id"])[-8:],
        })
    tool_ms = sum(u.get("tool_latency_ms", 0) or 0 for u in usage_requests)
    return {"rows": rows, "tools": tool_list, "tool_ms_sum": tool_ms,
            "provider_turns": usage.get("provider_turns"),
            "metrics_complete": bool(rows) and turn == len(rows) == usage.get("provider_turns") and bool(slim.get("usage_complete")) and not slim.get("usage_unknown"),
            "usage_complete": slim.get("usage_complete"), "usage_unknown": slim.get("usage_unknown")}

def expected_fixtures(manifest):
    """Original fixture digests: manifest (new campaigns) or scenario registry (old)."""
    expected = {name: info.get("sha256") for name, info in (manifest.get("fixtures") or {}).items()
                if info.get("sha256")}
    if manifest.get("fixture_hash_kind") != "materialized-bytes-v1":
        try:
            import daily
            import holdouts
            scen = daily.SCENARIOS.get(manifest.get("scenario")) or holdouts.SCENARIOS.get(manifest.get("scenario")) or {}
            for name, text in (scen.get("files") or {}).items():
                source_digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
                if name not in expected or expected[name] == source_digest:
                    materialized = text.replace("\n", "\r\n") if manifest.get("harness", {}).get("platform") == "win32" else text
                    expected[name] = hashlib.sha256(materialized.encode("utf-8")).hexdigest()
        except ImportError:
            pass
    return expected


def workspace_diff(cdir, arm, manifest):
    """Files the agent created or modified in its workspace (SPEC.md/check.py included)."""
    ws = cdir / (arm + ".workspace")
    if not ws.is_dir():
        return None
    expected = expected_fixtures(manifest)
    for name in ("SPEC.md", "check.py"):
        base = cdir / name
        if base.exists():
            expected[name] = hashlib.sha256(base.read_bytes()).hexdigest()
    modified, added, seen = [], [], set()
    for f in sorted(ws.rglob("*")):
        if not f.is_file():
            continue
        rel = f.relative_to(ws).as_posix()
        seen.add(rel)
        digest = hashlib.sha256(f.read_bytes()).hexdigest()
        if rel in expected:
            if digest != expected[rel]:
                modified.append(rel)
        else:
            added.append(rel)
    return {"modified": modified, "added": added, "deleted": sorted(set(expected) - seen)}


def summarize_arm(cdir, arm):
    timing_path = cdir / (arm + ".timing.json")
    validation_path = cdir / (arm + ".validation.json")
    timing = load(timing_path) if timing_path.exists() else {}
    validation = load(validation_path) if validation_path.exists() else {}
    manifest = load(cdir / "manifest.json") if (cdir / "manifest.json").exists() else {}
    detail = summarize_pi(cdir) if arm == "pi" else summarize_slim(cdir)
    if detail is None:
        return None
    rows, tool_list = detail["rows"], detail["tools"]
    totals = {}
    for key in ("input", "uncached", "cache", "output", "reasoning", "provider_ms"):
        totals[key] = sum(r.get(key, 0) for r in rows)
    tool_ms = detail.get("tool_ms_sum")
    if tool_ms is None:
        tool_ms = sum(t.get("duration_ms", 0) or 0 for t in tool_list if isinstance(t.get("duration_ms"), (int, float)))
    wall = timing.get("total_ms", 0) or 0
    residual = wall - totals.get("provider_ms", 0) - tool_ms
    failures = [t for t in tool_list if t.get("success") is False]
    return {
        "campaign": cdir.name, "campaign_path": str(cdir),
        "wall_ms": wall, "exit_code": timing.get("exit_code"), "timed_out": timing.get("timed_out"),
        "validation_exit": validation.get("exit_code"), "fixtures_unchanged": validation.get("fixtures_unchanged"),
        "metrics_complete": detail.get("metrics_complete", False),
        "arm": arm,
        "model_calls": len(rows), "tool_calls": len(tool_list), "tool_failures": len(failures),
        "tool_ms_sum": tool_ms, "provider_ms_sum": totals.get("provider_ms", 0), "residual_ms": residual,
        "total_tokens": totals.get("input", 0) + totals.get("output", 0), **totals,
        "error_breakdown": dict(Counter(t.get("error_kind", "") for t in failures if t.get("error_kind"))),
        "workspace_diff": workspace_diff(cdir, arm, manifest),
        "rows": rows, "tools": tool_list,
    }


def gate_ok(summary):
    return (summary["exit_code"] == 0 and not summary["timed_out"]
            and summary["validation_exit"] == 0 and summary["fixtures_unchanged"]
            and summary.get("metrics_complete", False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaigns", nargs="+", type=Path)
    parser.add_argument("--output-md", type=Path, required=True)
    parser.add_argument("--output-json", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, default=None,
                        help="output-json de um relatorio anterior para comparacao de tendencia")
    args = parser.parse_args()
    dirs = [p.resolve() for p in args.campaigns]
    for d in dirs:
        if not (d / "manifest.json").exists():
            raise SystemExit("manifest ausente: " + str(d))
    manifests = {d.name: load(d / "manifest.json") for d in dirs}
    models = {m.get("model") for m in manifests.values()}
    providers = {m.get("provider", "openai-codex") for m in manifests.values()}
    cells, pairs, problems, causes = {}, [], [], Counter()
    for d in dirs:
        manifest = manifests[d.name]
        for arm in ("pi", "slim"):
            if not (d / (arm + ".timing.json")).exists():
                causes["nao-executado"] += 1
                problems.append(d.name + "/" + arm + ": braco sem timing.json (nao executado)")
                continue
            try:
                summary = summarize_arm(d, arm)
            except Exception as error:
                causes["erro-leitura"] += 1
                problems.append(d.name + "/" + arm + ": " + str(error))
                continue
            if summary is None:
                causes["sem-registros"] += 1
                problems.append(d.name + "/" + arm + ": registros de auditoria ausentes ou ilegiveis")
                continue
            summary["scenario"] = manifest.get("scenario", "?")
            summary["order"] = manifest.get("order")
            cells[(d.name, arm)] = summary
            pairs.append(summary)
            if not gate_ok(summary):
                cause = ("timeout" if summary["timed_out"]
                         else "processo" if summary["exit_code"] != 0
                         else "usage-incompleto" if not summary["metrics_complete"]
                         else "fixtures" if not summary["fixtures_unchanged"]
                         else "oracle")
                causes[cause] += 1
                problems.append(d.name + "/" + arm + ": gate falhou (exit=" + str(summary["exit_code"]) + ", valid=" + str(summary["validation_exit"]) + ", fixtures=" + str(summary["fixtures_unchanged"]) + ")")
    if not pairs:
        raise SystemExit("nenhum braco aproveitavel")
    aggs = {}
    for arm in ("pi", "slim"):
        arms = [cells[(d.name, arm)] for d in dirs if (d.name, arm) in cells]
        if not arms:
            continue
        tot = {}
        for key in ("wall_ms", "model_calls", "tool_calls", "tool_failures", "tool_ms_sum", "provider_ms_sum", "residual_ms", "total_tokens", "input", "uncached", "cache", "output", "reasoning"):
            tot[key] = sum(a.get(key, 0) for a in arms)
        tot["files_modified"] = sum(len((a.get("workspace_diff") or {}).get("modified", [])) for a in arms)
        tot["files_added"] = sum(len((a.get("workspace_diff") or {}).get("added", [])) for a in arms)
        kinds = Counter()
        for a in arms:
            kinds.update(a.get("error_breakdown", {}))
        by_tool = {}
        for a in arms:
            for t in a["tools"]:
                e = by_tool.setdefault(t.get("name") or "?", {"calls": 0, "failures": 0, "ms": 0})
                e["calls"] += 1
                e["failures"] += 1 if t.get("success") is False else 0
                e["ms"] += t.get("duration_ms") or 0
        aggs[arm] = {"arms": arms, "total": tot, "errors": dict(kinds), "pairs": len(arms), "by_tool": by_tool}
    paired = []
    for d in dirs:
        s, p = cells.get((d.name, "slim")), cells.get((d.name, "pi"))
        if s is None or p is None or not (gate_ok(s) and gate_ok(p)):
            continue
        turns = []
        for i in range(max(len(s["rows"]), len(p["rows"]))):
            def pick(a, i):
                r = a["rows"][i] if i < len(a["rows"]) else {}
                return {"in": r.get("input"), "out": r.get("output"), "hist_bytes": r.get("history_bytes"),
                        "tools": r.get("tools")}
            turns.append({"turn": i + 1, "slim": pick(s, i), "pi": pick(p, i)})
        paired.append({"campaign": d.name, "scenario": s["scenario"],
                       "slim_tokens": s["total_tokens"], "pi_tokens": p["total_tokens"],
                       "token_ratio": s["total_tokens"] / p["total_tokens"] if p["total_tokens"] else None,
                       "wall_ratio": s["wall_ms"] / p["wall_ms"] if p["wall_ms"] else None,
                       "slim_calls": s["model_calls"], "pi_calls": p["model_calls"],
                       "turns": turns})
    stats = {}
    if paired:
        ratios = [x["token_ratio"] for x in paired if x["token_ratio"] is not None]
        wratios = [x["wall_ratio"] for x in paired if x["wall_ratio"] is not None]
        by_scenario = {}
        for x in paired:
            agg = by_scenario.setdefault(x["scenario"], {"pairs": 0, "slim_tokens": 0, "pi_tokens": 0, "ratios": []})
            agg["pairs"] += 1
            agg["slim_tokens"] += x["slim_tokens"]
            agg["pi_tokens"] += x["pi_tokens"]
            if x["token_ratio"] is not None:
                agg["ratios"].append(x["token_ratio"])
        stats = {"pairs": len(paired),
                 "slim_token_wins": sum(1 for x in paired if x["token_ratio"] is not None and x["token_ratio"] < 1),
                 "median_token_ratio": median(ratios) if ratios else None,
                 "median_wall_ratio": median(wratios) if wratios else None,
                 "per_scenario": by_scenario}
    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = []
    lines.append("# Slim x Pi — comparativo rastreavel (" + generated + ")")
    lines.append("")
    lines.append("Modelo: `" + "|".join(sorted(models)) + "`; provider: `" + "|".join(sorted(providers)) + "`; effort high.")
    lines.append("")
    lines.append("## Resumo (todos os bracos com registros, incluindo falhas; uso pode ser parcial)")
    lines.append("")
    lines.append("| Metrica | Slim | Pi | Delta (Slim-Pi) |")
    lines.append("|---|---:|---:|---:|")
    if "slim" in aggs and "pi" in aggs:
        s, p = aggs["slim"]["total"], aggs["pi"]["total"]
        def row(label, key, suffix=""):
            delta = s[key] - p[key]
            sign = "+" if delta > 0 else ""
            lines.append("| " + label + " | " + str(s[key]) + suffix + " | " + str(p[key]) + suffix + " | " + sign + str(delta) + suffix + " |")
        row("Chamadas ao modelo", "model_calls")
        row("Ferramentas executadas", "tool_calls")
        row("Falhas de ferramenta", "tool_failures")
        row("Tokens totais (in+out)", "total_tokens")
        row("Entrada (inclui cache)", "input")
        row("Cache lido", "cache")
        row("Saida (inclui reasoning)", "output")
        row("Reasoning", "reasoning")
        row("Arquivos modificados", "files_modified")
        row("Arquivos criados", "files_added")
        row("Tempo wall soma (ms)", "wall_ms", " ms")
        row("Tempo provider soma (ms)", "provider_ms_sum", " ms")
        row("Tempo tools soma (ms)", "tool_ms_sum", " ms")
        if s["input"] and p["input"]:
            hs, hp = s["cache"] / s["input"], p["cache"] / p["input"]
            lines.append("| Taxa de acerto de cache | " + format(hs * 100, ".1f") + "% | " + format(hp * 100, ".1f") + "% | " + format((hs - hp) * 100, "+.1f") + " pp |")
    lines.append("")
    lines.append("Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes. Residuo = wall-provider-tools: diferenca aritmetica que nao isola startup, pois duracoes de ferramentas podem se sobrepor. Arquivos = diff do workspace vs fixtures originais.")
    if stats:
        lines.append("")
        lines.append("## Pareamento (somente pares com gate aprovado nos dois bracos)")
        lines.append("")
        lines.append("| Medida | Valor |")
        lines.append("|---|---:|")
        lines.append("| Pares aproveitados | " + str(stats["pairs"]) + " |")
        lines.append("| Slim mais economico em tokens | " + str(stats["slim_token_wins"]) + "/" + str(stats["pairs"]) + " |")
        if stats["median_token_ratio"] is not None:
            lines.append("| Mediana razao tokens Slim/Pi | " + format(stats["median_token_ratio"], ".4f") + " |")
        if stats["median_wall_ratio"] is not None:
            lines.append("| Mediana razao wall Slim/Pi | " + format(stats["median_wall_ratio"], ".4f") + " |")
        lines.append("")
        lines.append("| Cenario | Pares | Tokens Slim | Tokens Pi | Delta Slim | Razao min/med/max |")
        lines.append("|---|---:|---:|---:|---:|---:|")
        for scen, agg in sorted(stats["per_scenario"].items()):
            delta = (agg["slim_tokens"] - agg["pi_tokens"]) / agg["pi_tokens"] * 100 if agg["pi_tokens"] else 0
            spread = "-"
            if agg["ratios"]:
                spread = format(min(agg["ratios"]), ".2f") + "/" + format(median(agg["ratios"]), ".2f") + "/" + format(max(agg["ratios"]), ".2f")
            lines.append("| " + scen + " | " + str(agg["pairs"]) + " | " + str(agg["slim_tokens"]) + " | " + str(agg["pi_tokens"]) + " | " + format(delta, "+.2f") + "% | " + spread + " |")
        lines.append("")
        if args.baseline:
            try:
                prev = (load(args.baseline).get("paired") or {}).get("stats") or {}
                lines.append("Baseline `" + args.baseline.name + "`: mediana razao tokens "
                             + str(prev.get("median_token_ratio")) + " -> " + str(stats.get("median_token_ratio"))
                             + "; mediana razao wall " + str(prev.get("median_wall_ratio"))
                             + " -> " + str(stats.get("median_wall_ratio"))
                             + "; vitorias Slim " + str(prev.get("slim_token_wins")) + "/" + str(prev.get("pairs"))
                             + " -> " + str(stats["slim_token_wins"]) + "/" + str(stats["pairs"]) + ".")
                lines.append("")
            except Exception as error:
                problems.append("baseline ilegivel: " + str(error))
    lines.append("## Por campanha")
    lines.append("")
    lines.append("| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall/arq) | Pi (chamadas/falhas/tokens/wall/arq) |")
    lines.append("|---|---|---|---|")
    for d in dirs:
        manifest = manifests[d.name]
        scen = manifest.get("scenario", "?")
        sm = next((a for a in aggs.get("slim", {}).get("arms", []) if a["campaign"] == d.name), None)
        pm = next((a for a in aggs.get("pi", {}).get("arms", []) if a["campaign"] == d.name), None)
        def cell(a):
            if a is None:
                return "-"
            diff = a.get("workspace_diff") or {}
            arq = str(len(diff.get("modified", []))) + "mod+" + str(len(diff.get("added", []))) + "novos"
            return (str(a["model_calls"]) + "/" + str(a["tool_failures"]) + "/" + str(a["total_tokens"])
                    + "/" + str(a["wall_ms"]) + "ms/" + arq)
        lines.append("| `" + d.name + "` | " + scen + " | " + cell(sm) + " | " + cell(pm) + " |")
    lines.append("")
    lines.append("## Por ferramenta (uso agregado por braco)")
    lines.append("")
    tool_names = sorted(set().union(*[set(aggs[a]["by_tool"]) for a in aggs]),
                      key=lambda n: -(sum(aggs[a]["by_tool"].get(n, {}).get("calls", 0) for a in aggs)))
    if tool_names:
        lines.append("| Ferramenta | Slim calls/falhas/ms | Pi calls/falhas/ms |")
        lines.append("|---|---:|---:|")
        for name in tool_names:
            def tcell(arm):
                e = aggs.get(arm, {}).get("by_tool", {}).get(name)
                return "-" if e is None else str(e["calls"]) + "/" + str(e["failures"]) + "/" + str(e["ms"]) + "ms"
            lines.append("| " + name + " | " + tcell("slim") + " | " + tcell("pi") + " |")
    else:
        lines.append("Nenhuma ferramenta registrada.")
    lines.append("")
    lines.append("## Erros de ferramenta (para achar fraqueza)")
    lines.append("")
    for arm in ("slim", "pi"):
        if arm not in aggs:
            continue
        lines.append("### " + arm + ": " + str(aggs[arm]["total"]["tool_failures"]) + " falha(s) em " + str(aggs[arm]["total"]["tool_calls"]) + " execucoes")
        lines.append("")
        if aggs[arm]["errors"]:
            lines.append("| Classe | Qtd |")
            lines.append("|---|---:|")
            for kind, count in sorted(aggs[arm]["errors"].items(), key=lambda kv: -kv[1]):
                lines.append("| " + kind + " | " + str(count) + " |")
            lines.append("")
        failures = [t for a in aggs[arm]["arms"] for t in a["tools"] if t.get("success") is False]
        if not failures:
            lines.append("Nenhuma falha registrada.")
            lines.append("")
            continue
        lines.append("| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |")
        lines.append("|---|---|---|---|---|---|---|")
        for a in aggs[arm]["arms"]:
            for t in a["tools"]:
                if t.get("success") is False:
                    lines.append("| `" + a["campaign"] + "` | " + str(t.get("turn")) + " | " + str(t.get("name")) + " | `" + excerpt(t.get("args"), 80).replace("|", "/") + "` | " + str(t.get("error_kind")) + " | " + excerpt(t.get("error"), 140).replace("|", "/") + " | " + str(t.get("trace")) + " |")
        lines.append("")
    lines.append("## Por turno (crescimento de contexto)")
    lines.append("")
    for d in dirs:
        lines.append("### `" + d.name + "`")
        lines.append("")
        for arm in ("slim", "pi"):
            if arm not in aggs:
                continue
            a = next((x for x in aggs[arm]["arms"] if x["campaign"] == d.name), None)
            if a is None:
                continue
            lines.append("#### " + arm)
            lines.append("")
            lines.append("| turno | in | out | reasoning | hist_bytes | provider_ms | tools |")
            lines.append("|---|---:|---:|---:|---:|---:|---|")
            for r in a["rows"]:
                lines.append("| " + str(r.get("turn")) + " | " + str(r.get("input")) + " | " + str(r.get("output")) + " | " + str(r.get("reasoning")) + " | " + str(r.get("history_bytes") or "-") + " | " + str(r.get("provider_ms")) + " | " + ",".join(r.get("tools") or []) + " |")
            lines.append("")
    lines.append("## Rastreabilidade")
    lines.append("")
    for d in dirs:
        manifest = manifests[d.name]
        lines.append("### `" + d.name + "` — " + str(manifest.get("scenario", "?")) + " (ordem: " + json.dumps(manifest.get("order"), ensure_ascii=False) + ")")
        lines.append("")
        exes = manifest.get("executables", {})
        for arm in ("slim", "pi"):
            info = exes.get(arm, {})
            if info:
                lines.append("- " + arm + ": `" + str(info.get("version")) + "` sha256 `" + str(info.get("sha256")) + "`")
        lines.append("- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.")
        lines.append("- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.")
        lines.append("")
    lines.append("## Reproducao")
    lines.append("")
    lines.append("```powershell")
    lines.append("python bench/luna-live/daily.py --rounds 1")
    lines.append("python bench/luna-live/report.py <campanhas...> --output-md RELATORIO.md --output-json report.json")
    lines.append("```")
    lines.append("")
    lines.append("## Limitacoes")
    lines.append("")
    lines.append("- Amostra pequena, host nao exclusivo, caches do servidor nao controlados, ordem alternada mas sem randomizacao plena.")
    lines.append("- Instrumentacao assimetrica: Pi via extensao observadora, Slim via ledger/sessao; contagens sao turnos do modelo, nao TCP/retries de transporte. Residuo nao isola startup: duracoes de tools podem se sobrepor.")
    lines.append("- history_bytes exclui resultados de tools nos dois bracos; tool_result_bytes os registra separadamente. Campanhas Pi antigas reconstroem bytes do payload com reasoning opaco omitido, portanto seus bytes de historico sao parciais.")
    lines.append("- Tokens sao usos informados pelos providers; sem inferencia de custo monetario.")
    lines.append("- Outcomes Slim: facts estruturados tool.v1 quando presentes; senao heuristica sobre o texto da tool.")
    if problems:
        if causes:
            lines.append("- Falhas por causa: " + ", ".join(k + "=" + str(v) for k, v in causes.most_common()))
        lines.append("- Gates com problema: " + "; ".join(problems))
    else:
        lines.append("- Todos os bracos passaram no gate (exit 0, validacao externa PASS, fixtures intactas, metricas completas).")
    args.output_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    args.output_json.write_text(json.dumps({"generated_utc": generated, "manifests": {k: manifests[k] for k in manifests}, "aggregate": {arm: {"total": aggs[arm]["total"], "errors": aggs[arm]["errors"], "pairs": aggs[arm]["pairs"], "by_tool": aggs[arm]["by_tool"]} for arm in aggs}, "paired": {"stats": stats, "pairs": paired}, "arms": pairs, "problems": problems, "failure_causes": dict(causes)}, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print("md: " + str(args.output_md))
    print("json: " + str(args.output_json))
    for arm in sorted(aggs):
        print(arm + " " + json.dumps(aggs[arm]["total"], ensure_ascii=False))


if __name__ == "__main__":
    main()
