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
import random
from fractions import Fraction
from math import comb
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


def tool_outcomes(tool_list):
    """Unica definicao da contagem de resultado de ferramenta usada pelos normalizadores.

    `success=None` significa resultado nao comprovado (fact/evento ausente), nao falha.
    """
    failed = sum(1 for t in tool_list if t.get("success") is False)
    unknown = sum(1 for t in tool_list if t.get("success") is None)
    return {"succeeded": len(tool_list) - failed - unknown, "failed": failed, "unknown": unknown}


def slim_usage_identity(usage_requests, manifest=None):
    """Provider/modelo observados por request Slim e consistencia interna do par.

    O rotulo do produto nao e assumido igual ao nome do provider no manifesto: a
    divergencia e registrada e reprova o braco no gate, porque o numero so vale
    para a rota rotulada. Requests do mesmo par tambem devem concordar entre si
    (fallback silencioso de modelo/rota no meio da tarefa).
    """
    pairs = [((r.get("provider") or ""), (r.get("model") or "")) for r in usage_requests]
    known = [pair for pair in pairs if pair[0] or pair[1]]
    counts = Counter(known)
    dominant = counts.most_common(1)[0][0] if counts else ("", "")
    expected_model = (manifest or {}).get("model")
    expected_provider = (manifest or {}).get("provider")
    return {
        "provider": dominant[0] or None, "model": dominant[1] or None,
        "turns": len(pairs), "turns_with_identity": len(known),
        "distinct": [{"provider": p, "model": m, "turns": n} for (p, m), n in counts.most_common()],
        "inconsistent_turns": [i for i, pair in enumerate(pairs, 1) if pair in known and pair != dominant],
        "manifest_model": expected_model, "manifest_provider": expected_provider,
        "model_matches_manifest": (dominant[1] == expected_model) if (dominant[1] and expected_model) else None,
        "provider_matches_manifest": (dominant[0] == expected_provider) if (dominant[0] and expected_provider) else None,
    }


def slim_request_counters(usage_requests):
    """Campos por request que o ledger Slim ja entrega e o harness ignorava."""
    def total(key):
        return sum(r.get(key) or 0 for r in usage_requests)
    return {"retries": total("retry_count"), "cancelled_requests": total("cancelled"),
            "cache_hits": total("response_cache_hit"), "unknown_requests": total("usage_unknown"),
            "failed_requests": total("failed"), "ttfb_ms_sum": total("time_to_first_byte_ms"),
            "ttfs_ms_sum": total("time_to_first_semantic_ms"),
            "estimation_error_tokens_sum": total("estimation_error_tokens")}


def sign_test_p(wins, trials):
    """p bilateral exato (metodo minlike) do teste de sinais, sem dependencias externas."""
    if not trials or wins < 0 or wins > trials:
        return None
    probabilities = [Fraction(comb(trials, k), 2 ** trials) for k in range(trials + 1)]
    observed = probabilities[wins]
    return float(sum(p for p in probabilities if p <= observed))


def bootstrap_median_ci(values, resamples=10000, seed=20260915, percentiles=(2.5, 97.5)):
    """IC percentil bootstrap da mediana, com semente fixa para repetibilidade."""
    if not values:
        return None
    rng = random.Random(seed)
    count = len(values)
    medians = sorted(median([values[rng.randrange(count)] for _ in range(count)]) for _ in range(resamples))
    def percentile(p):
        position = (len(medians) - 1) * p / 100
        low = int(position)
        high = min(low + 1, len(medians) - 1)
        weight = position - low
        return medians[low] * (1 - weight) + medians[high] * weight
    return {"low": percentile(percentiles[0]), "high": percentile(percentiles[1]),
            "resamples": resamples, "seed": seed, "method": "percentile-bootstrap-median"}


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
    models_seen = Counter(r["model"] for r in rows if r.get("model"))
    dominant = models_seen.most_common(1)[0][0] if models_seen else None
    identity = {"provider": None, "model": dominant, "turns": len(rows),
                "turns_with_identity": sum(models_seen.values()),
                "distinct": [{"provider": None, "model": m, "turns": n} for m, n in models_seen.most_common()],
                "inconsistent_turns": [r["turn"] for r in rows if r.get("model") and r["model"] != dominant],
                "source": "wire"}
    return {"rows": rows, "tools": tool_list, "request_count": len(requests),
            "message_count": len(messages), "metrics_complete": complete,
            "identity": identity, "counters": {}}


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
                "evidence": "tool-finished" if ok is not None else "absence",
                "duration_ms": tool.get("duration_ms", 0),
                "args": short_args(tool.get("arguments")),
                "error_kind": classify_error(tool.get("name"), err_text) if ok is False else "",
                "error": excerpt(err_text),
                "trace": "slim.session.jsonl:seq=" + str(tool.get("seq")),
            })
        tool_ms = sum(t.get("duration_ms", 0) or 0 for t in tool_list if isinstance(t.get("duration_ms"), (int, float)))
        return {"rows": rows, "tools": tool_list, "tool_ms_sum": tool_ms,
                "identity": slim_usage_identity(usage_requests),
                "counters": slim_request_counters(usage_requests),
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
        proven = isinstance(decided, dict) and isinstance(decided.get("success"), bool)
        duration = None
        if isinstance(decided, dict):
            if proven:
                ok = decided["success"]
                err = "" if ok else content
            duration = decided.get("duration_ms")
        tool_list.append({
            "call_id": c["call_id"],
            "name": c["name"] or "?",
            "turn": c["turn"],
            "seq": None,
            "success": ok,
            "evidence": "fact" if proven else "inferred",
            "duration_ms": duration,
            "args": short_args(c["arguments"]),
            "error_kind": classify_error(c["name"], err) if ok is False else "",
            "error": excerpt(err),
            "trace": "slim.session.jsonl:call=..." + str(c["call_id"])[-8:],
        })
    tool_ms = sum(u.get("tool_latency_ms", 0) or 0 for u in usage_requests)
    return {"rows": rows, "tools": tool_list, "tool_ms_sum": tool_ms,
            "identity": slim_usage_identity(usage_requests),
            "counters": slim_request_counters(usage_requests),
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
    outcomes = tool_outcomes(tool_list)
    counters = detail.get("counters") or {}
    identity = detail.get("identity") or {}
    if identity:
        identity["manifest_model"] = manifest.get("model")
        identity["manifest_provider"] = manifest.get("provider", "openai-codex")
        if identity.get("model") and identity["manifest_model"]:
            identity["model_matches_manifest"] = identity["model"] == identity["manifest_model"]
        if identity.get("provider") and identity["manifest_provider"]:
            identity["provider_matches_manifest"] = identity["provider"] == identity["manifest_provider"]
    return {
        "campaign": cdir.name, "campaign_path": str(cdir),
        "wall_ms": wall, "exit_code": timing.get("exit_code"), "timed_out": timing.get("timed_out"),
        "validation_exit": validation.get("exit_code"), "fixtures_unchanged": validation.get("fixtures_unchanged"),
        "metrics_complete": detail.get("metrics_complete", False),
        "arm": arm,
        "model_calls": len(rows), "tool_calls": len(tool_list), "tool_failures": outcomes["failed"],
        "tool_unknown": outcomes["unknown"], "tool_succeeded": outcomes["succeeded"],
        "tool_inferred": sum(1 for t in tool_list if t.get("evidence") == "inferred"),
        "tool_ms_sum": tool_ms, "provider_ms_sum": totals.get("provider_ms", 0), "residual_ms": residual,
        "total_tokens": totals.get("input", 0) + totals.get("output", 0), **totals,
        "retries": counters.get("retries", 0), "cancelled_requests": counters.get("cancelled_requests", 0),
        "cache_hits": counters.get("cache_hits", 0), "unknown_requests": counters.get("unknown_requests", 0),
        "failed_requests": counters.get("failed_requests", 0), "ttfb_ms_sum": counters.get("ttfb_ms_sum", 0),
        "ttfs_ms_sum": counters.get("ttfs_ms_sum", 0),
        "estimation_error_tokens_sum": counters.get("estimation_error_tokens_sum", 0),
        "identity": identity,
        "error_breakdown": dict(Counter(t.get("error_kind", "") for t in failures if t.get("error_kind"))),
        "workspace_diff": workspace_diff(cdir, arm, manifest),
        "rows": rows, "tools": tool_list,
    }


def gate_ok(summary):
    """Gate de execucao + rota: uso so entra na comparacao se a identidade nao se contradiz.

    Rotulo nao observado (`turns_with_identity == 0`) gera aviso, nao reprovacao: ausencia
    de prova nao e prova de troca de rota.
    """
    identity = summary.get("identity") or {}
    if identity.get("inconsistent_turns"):
        return False
    if any(identity.get(field) is False for field in ("model_matches_manifest", "provider_matches_manifest")):
        return False
    return (summary["exit_code"] == 0 and not summary["timed_out"]
            and summary["validation_exit"] == 0 and summary["fixtures_unchanged"]
            and summary.get("metrics_complete", False))


SUMMARY_SUM_KEYS = ("wall_ms", "model_calls", "tool_calls", "tool_failures", "tool_unknown", "tool_succeeded",
                    "tool_inferred",
                    "tool_ms_sum", "provider_ms_sum", "residual_ms", "total_tokens", "input", "uncached",
                    "cache", "cache_write", "output", "reasoning", "retries", "cancelled_requests",
                    "cache_hits", "unknown_requests", "failed_requests", "ttfb_ms_sum", "ttfs_ms_sum",
                    "estimation_error_tokens_sum")


def aggregate_arms(arms_by_arm):
    """Agrega bracos por arm; quem entra no denominador e decisao de quem chama.

    Cada arm agrega as campanhas que passaram no proprio gate, portanto os
    denominadores de Slim e Pi podem diferir quando um arm reprova e o outro nao.
    `campaigns` registra o denominador dentro do proprio total e a tabela principal
    imprime os dois, para nao comparar populacoes diferentes sem aviso; comparacao
    estritamente pareada fica na secao de pareamento, que so usa campanhas com os
    dois bracos aprovados.
    """
    aggs = {}
    for arm, arms in arms_by_arm.items():
        if not arms:
            continue
        tot = {key: sum(a.get(key, 0) or 0 for a in arms) for key in SUMMARY_SUM_KEYS}
        tot["campaigns"] = len(arms)
        tot["files_modified"] = sum(len((a.get("workspace_diff") or {}).get("modified", [])) for a in arms)
        tot["files_added"] = sum(len((a.get("workspace_diff") or {}).get("added", [])) for a in arms)
        kinds = Counter()
        by_tool = {}
        for a in arms:
            kinds.update(a.get("error_breakdown", {}))
            for t in a["tools"]:
                e = by_tool.setdefault(t.get("name") or "?", {"calls": 0, "failures": 0, "unknown": 0, "ms": 0})
                e["calls"] += 1
                e["failures"] += 1 if t.get("success") is False else 0
                e["unknown"] += 1 if t.get("success") is None else 0
                e["ms"] += t.get("duration_ms") or 0
        aggs[arm] = {"arms": arms, "total": tot, "errors": dict(kinds), "pairs": len(arms), "by_tool": by_tool}
    return aggs


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
                identity = summary.get("identity") or {}
                route = (identity.get("inconsistent_turns")
                         or any(identity.get(field) is False for field in ("model_matches_manifest", "provider_matches_manifest")))
                cause = ("timeout" if summary["timed_out"]
                         else "processo" if summary["exit_code"] != 0
                         else "usage-incompleto" if not summary["metrics_complete"]
                         else "fixtures" if not summary["fixtures_unchanged"]
                         else "rota" if route
                         else "oracle")
                causes[cause] += 1
                problems.append(d.name + "/" + arm + ": gate falhou (exit=" + str(summary["exit_code"]) + ", valid=" + str(summary["validation_exit"]) + ", fixtures=" + str(summary["fixtures_unchanged"]) + ")")
    if not cells:
        raise SystemExit("nenhum braco aproveitavel")
    for summary in cells.values():
        label = summary["campaign"] + "/" + summary["arm"]
        identity = summary.get("identity") or {}
        if identity.get("inconsistent_turns"):
            causes["identidade-instavel"] += 1
            problems.append(label + ": provider/modelo mudou no meio da tarefa (turnos "
                            + str(identity["inconsistent_turns"]) + "); uso nao atribuivel a uma rota unica")
        if identity.get("provider_matches_manifest") is False:
            problems.append(label + ": provider do ledger `" + str(identity.get("provider"))
                            + "` difere do rotulado no manifesto `" + str(identity.get("manifest_provider"))
                            + "` (rota nao confirmada pelo registro)")
        if identity.get("model_matches_manifest") is False:
            problems.append(label + ": modelo do registro `" + str(identity.get("model"))
                            + "` difere do manifesto `" + str(identity.get("manifest_model")) + "`")
        if identity and not identity.get("turns_with_identity"):
            problems.append(label + ": nenhum request traz provider/model; a rota rotulada no manifesto nao foi observada no registro")
        if summary.get("tool_unknown"):
            problems.append(label + ": " + str(summary["tool_unknown"])
                            + " ferramenta(s) sem resultado comprovado (success ausente); contadas como incognitas, nao como falha")
    approved = {key: s for key, s in cells.items() if gate_ok(s)}
    aggs = aggregate_arms({arm: [approved[key] for key in sorted(approved) if key[1] == arm] for arm in ("pi", "slim")})
    aggs_all = aggregate_arms({arm: [cells[key] for key in sorted(cells) if key[1] == arm] for arm in ("pi", "slim")})
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
        token_wins = sum(1 for x in paired if x["token_ratio"] is not None and x["token_ratio"] < 1)
        wall_wins = sum(1 for x in paired if x["wall_ratio"] is not None and x["wall_ratio"] < 1)
        stats = {"pairs": len(paired),
                 "slim_token_wins": token_wins,
                 "slim_wall_wins": wall_wins,
                 "median_token_ratio": median(ratios) if ratios else None,
                 "median_wall_ratio": median(wratios) if wratios else None,
                 "sign_test_tokens": {"wins": token_wins, "trials": len(ratios),
                                      "p_two_sided": sign_test_p(token_wins, len(ratios))},
                 "sign_test_wall": {"wins": wall_wins, "trials": len(wratios),
                                    "p_two_sided": sign_test_p(wall_wins, len(wratios))},
                 "bootstrap_ci95_token_ratio": bootstrap_median_ci(ratios),
                 "bootstrap_ci95_wall_ratio": bootstrap_median_ci(wratios),
                 "advisory": ("n<8 pares: teste de sinais e IC sao exploratorios, nao conclusivos" if len(paired) < 8 else None),
                 "per_scenario": by_scenario}
    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = []
    lines.append("# Slim x Pi — comparativo rastreavel (" + generated + ")")
    lines.append("")
    lines.append("Modelo: `" + "|".join(sorted(models)) + "`; provider: `" + "|".join(sorted(providers)) + "`; effort high.")
    lines.append("")
    lines.append("## Resumo (por escopo de gate; o denominador vai na propria tabela)")
    lines.append("")
    lines.append("Aprovado = exit 0, sem timeout, `check.py` externo PASS, fixtures intactas e metricas completas. O braco so entra na comparacao principal se ele proprio passou no gate, entao queda de execucao nao vira vantagem falsa de tokens.")

    def summary_scope(scope, caption):
        lines.append("")
        lines.append("### " + caption)
        lines.append("")
        if "slim" not in scope or "pi" not in scope:
            lines.append("Escopo sem os dois bracos; sem comparacao.")
            return
        s, p = scope["slim"]["total"], scope["pi"]["total"]
        lines.append("| Metrica | Slim | Pi | Delta (Slim-Pi) |")
        lines.append("|---|---:|---:|---:|")
        def row(label, key, suffix=""):
            delta = s.get(key, 0) - p.get(key, 0)
            sign = "+" if delta > 0 else ""
            lines.append("| " + label + " | " + str(s.get(key, 0)) + suffix + " | " + str(p.get(key, 0)) + suffix + " | " + sign + str(delta) + suffix + " |")
        row("Campanhas no denominador", "campaigns")
        row("Chamadas ao modelo", "model_calls")
        row("Ferramentas executadas", "tool_calls")
        row("Falhas de ferramenta", "tool_failures")
        row("Ferramentas sem resultado comprovado", "tool_unknown")
        row("Tokens totais (in+out)", "total_tokens")
        row("Entrada (inclui cache)", "input")
        row("Cache lido", "cache")
        row("Escrita em cache", "cache_write")
        row("Saida (inclui reasoning)", "output")
        row("Reasoning", "reasoning")
        row("Arquivos modificados", "files_modified")
        row("Arquivos criados", "files_added")
        row("Tempo wall soma (ms)", "wall_ms", " ms")
        row("Tempo provider soma (ms)", "provider_ms_sum", " ms")
        row("Tempo tools soma (ms)", "tool_ms_sum", " ms")
        if s.get("input") and p.get("input"):
            hs, hp = s["cache"] / s["input"], p["cache"] / p["input"]
            lines.append("| Taxa de acerto de cache | " + format(hs * 100, ".1f") + "% | " + format(hp * 100, ".1f") + "% | " + format((hs - hp) * 100, "+.1f") + " pp |")
    summary_scope(aggs, "Aprovados no gate (comparacao principal)")
    if any(aggs_all[arm]["pairs"] > aggs.get(arm, {}).get("pairs", 0) for arm in aggs_all):
        summary_scope(aggs_all, "Todos os bracos com registros, incluindo reprovados (uso pode ser parcial)")
    lines.append("")
    lines.append("Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes. Residuo = wall-provider-tools: diferenca aritmetica que nao isola startup, pois duracoes de ferramentas podem se sobrepor. Arquivos = diff do workspace vs fixtures originais.")
    lines.append("")
    lines.append("## Identidade e configuracao observada (o que o registro prova)")
    lines.append("")
    lines.append("Celula `-` significa que o braco nao expoe o campo, nao um zero medido. Robusto = a rota do braco Slim e o modelo efetivamente observados batem com o rotulo do manifesto e nao mudam no meio da tarefa.")
    lines.append("")
    lines.append("| Campanha | Braco | provider | modelo | turnos c/ identidade | turnos divergentes | modelo=manifesto | provider=manifesto | retries | canceladas | cache hit local | usage incognito | requests falhos | TTFB soma (ms) | erro estimativa tokens |")
    lines.append("|---|---|---|---|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|")
    for key in sorted(cells):
        a = cells[key]
        ident = a.get("identity") or {}
        slim_side = a["arm"] == "slim"
        def value(field):
            raw = ident.get(field)
            return "-" if raw is None or raw == "" else str(raw)
        def counter(field):
            return str(a.get(field, 0)) if slim_side else "-"
        divergent = ",".join(str(t) for t in ident.get("inconsistent_turns") or []) or "-"
        lines.append("| `" + a["campaign"] + "` | " + a["arm"] + " | " + value("provider") + " | " + value("model")
                     + " | " + str(ident.get("turns_with_identity", "-")) + " | " + divergent
                     + " | " + value("model_matches_manifest") + " | " + value("provider_matches_manifest")
                     + " | " + counter("retries") + " | " + counter("cancelled_requests")
                     + " | " + counter("cache_hits") + " | " + counter("unknown_requests")
                     + " | " + counter("failed_requests") + " | " + counter("ttfb_ms_sum")
                     + " | " + counter("estimation_error_tokens_sum") + " |")
    lines.append("")
    if stats:
        lines.append("")
        lines.append("## Pareamento (somente pares com gate aprovado nos dois bracos)")
        lines.append("")
        lines.append("| Medida | Valor |")
        lines.append("|---|---:|")
        def ptext(block):
            value = (block or {}).get("p_two_sided")
            return "n/d" if value is None else format(value, ".4f")
        def citext(interval):
            return "n/d" if not interval else "[" + format(interval["low"], ".4f") + ", " + format(interval["high"], ".4f") + "]"
        lines.append("| Pares aproveitados | " + str(stats["pairs"]) + " |")
        lines.append("| Slim mais economico em tokens | " + str(stats["slim_token_wins"]) + "/" + str(stats["pairs"]) + " |")
        lines.append("| Slim mais rapido em wall | " + str(stats["slim_wall_wins"]) + "/" + str(stats["sign_test_wall"]["trials"]) + " |")
        if stats["median_token_ratio"] is not None:
            lines.append("| Mediana razao tokens Slim/Pi | " + format(stats["median_token_ratio"], ".4f") + " |")
        if stats["median_wall_ratio"] is not None:
            lines.append("| Mediana razao wall Slim/Pi | " + format(stats["median_wall_ratio"], ".4f") + " |")
        lines.append("| p bilateral (sinais, tokens) | " + ptext(stats["sign_test_tokens"]) + " |")
        lines.append("| p bilateral (sinais, wall) | " + ptext(stats["sign_test_wall"]) + " |")
        lines.append("| IC95 mediana razao tokens (bootstrap) | " + citext(stats["bootstrap_ci95_token_ratio"]) + " |")
        lines.append("| IC95 mediana razao wall (bootstrap) | " + citext(stats["bootstrap_ci95_wall_ratio"]) + " |")
        if stats.get("advisory"):
            lines.append("| Aviso de amostra | " + stats["advisory"] + " |")
        lines.append("")
        lines.append("Teste de sinais bilateral exato sobre os pares (empates contam como nao-vitoria) e IC percentil bootstrap da mediana com semente fixa `" + str((stats["bootstrap_ci95_token_ratio"] or {}).get("seed", 20260915)) + "`, " + str((stats["bootstrap_ci95_token_ratio"] or {}).get("resamples", 10000)) + " reamostragens; quem nao domina intervalo nao deve reivindicar ganho.")
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
    lines.append("## Por campanha (todas, aprovadas ou nao)")
    lines.append("")
    lines.append("Formato do braco: chamadas/falhas/incognitas/tokens/wall/arquivos; a coluna gate diz aprovado ou o primeiro motivo de reprovacao.")
    lines.append("")
    lines.append("| Campanha | Cenario | Gate Slim | Gate Pi | Slim (cham/falh/incog/tokens/wall/arq) | Pi (cham/falh/incog/tokens/wall/arq) |")
    lines.append("|---|---|---|---|---|---|")
    for d in dirs:
        manifest = manifests[d.name]
        scen = manifest.get("scenario", "?")
        sm, pm = cells.get((d.name, "slim")), cells.get((d.name, "pi"))
        def cell(a):
            if a is None:
                return "-"
            diff = a.get("workspace_diff") or {}
            arq = str(len(diff.get("modified", []))) + "mod+" + str(len(diff.get("added", []))) + "novos"
            return (str(a["model_calls"]) + "/" + str(a["tool_failures"]) + "/" + str(a.get("tool_unknown", 0))
                    + "/" + str(a["total_tokens"]) + "/" + str(a["wall_ms"]) + "ms/" + arq)
        def gate(a):
            if a is None:
                return "nao executado"
            if gate_ok(a):
                return "aprovado"
            if a["timed_out"]:
                return "timeout"
            if a["exit_code"] != 0:
                return "exit=" + str(a["exit_code"])
            if not a["metrics_complete"]:
                return "uso incompleto"
            if not a["fixtures_unchanged"]:
                return "fixtures alteradas"
            identity = a.get("identity") or {}
            if identity.get("inconsistent_turns"):
                return "rota do registro instavel"
            if any(identity.get(field) is False for field in ("model_matches_manifest", "provider_matches_manifest")):
                return "rota do registro != manifesto"
            return "validacao externa"
        lines.append("| `" + d.name + "` | " + scen + " | " + gate(sm) + " | " + gate(pm) + " | " + cell(sm) + " | " + cell(pm) + " |")
    lines.append("")
    lines.append("## Por ferramenta (uso agregado por braco, incluindo bracos reprovados)")
    lines.append("")
    lines.append("Formato: chamadas/falhas/incognitas/ms; incognita = sem resultado comprovado (`success` ausente), nunca somada a falhas.")
    lines.append("")
    tool_names = sorted(set().union(*[set(aggs_all[a]["by_tool"]) for a in aggs_all]),
                      key=lambda n: -(sum(aggs_all[a]["by_tool"].get(n, {}).get("calls", 0) for a in aggs_all)))
    if tool_names:
        lines.append("| Ferramenta | Slim cham/falh/incog/ms | Pi cham/falh/incog/ms |")
        lines.append("|---|---:|---:|")
        for name in tool_names:
            def tcell(arm):
                e = aggs_all.get(arm, {}).get("by_tool", {}).get(name)
                return "-" if e is None else (str(e["calls"]) + "/" + str(e["failures"]) + "/" + str(e["unknown"])
                                              + "/" + str(e["ms"]) + "ms")
            lines.append("| " + name + " | " + tcell("slim") + " | " + tcell("pi") + " |")
    else:
        lines.append("Nenhuma ferramenta registrada.")
    lines.append("")
    lines.append("## Erros de ferramenta (para achar fraqueza; inclui bracos reprovados)")
    lines.append("")
    for arm in ("slim", "pi"):
        if arm not in aggs_all:
            continue
        lines.append("### " + arm + ": " + str(aggs_all[arm]["total"]["tool_failures"]) + " falha(s), "
                     + str(aggs_all[arm]["total"]["tool_unknown"]) + " sem resultado comprovado, "
                     + str(aggs_all[arm]["total"].get("tool_inferred", 0)) + " por heuristica de texto, em "
                     + str(aggs_all[arm]["total"]["tool_calls"]) + " execucoes")
        lines.append("")
        if aggs_all[arm]["errors"]:
            lines.append("| Classe | Qtd |")
            lines.append("|---|---:|")
            for kind, count in sorted(aggs_all[arm]["errors"].items(), key=lambda kv: -kv[1]):
                lines.append("| " + kind + " | " + str(count) + " |")
            lines.append("")
        failures = [t for a in aggs_all[arm]["arms"] for t in a["tools"] if t.get("success") is False]
        if not failures:
            lines.append("Nenhuma falha registrada.")
            lines.append("")
            continue
        lines.append("| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |")
        lines.append("|---|---|---|---|---|---|---|")
        for a in aggs_all[arm]["arms"]:
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
            if arm not in aggs_all:
                continue
            a = next((x for x in aggs_all[arm]["arms"] if x["campaign"] == d.name), None)
            if a is None:
                continue
            lines.append("#### " + arm)
            lines.append("")
            lines.append("| turno | in | in_acum | out | reasoning | hist_bytes | tool_result_bytes | provider_ms | tools |")
            lines.append("|---|---:|---:|---:|---:|---:|---:|---:|---|")
            cumulative = 0
            for r in a["rows"]:
                cumulative += r.get("input") or 0
                lines.append("| " + str(r.get("turn")) + " | " + str(r.get("input")) + " | " + str(cumulative) + " | "
                             + str(r.get("output")) + " | " + str(r.get("reasoning")) + " | "
                             + str(r.get("history_bytes") or "-") + " | " + str(r.get("tool_result_bytes") or "-") + " | "
                             + str(r.get("provider_ms")) + " | " + ",".join(r.get("tools") or []) + " |")
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
    lines.append("- Outcomes Slim (v2): fact estruturado `tool.v1` quando existe (`evidence=fact`); sem ele o desfecho vem da heuristica de texto e fica marcado como `evidence=inferred` no JSON, nao como prova. Execucao sem fact e sem `ToolFinished` (v1) fica `evidence=absence`, com `success=None`: incognita, nunca falha. Separe `tool_failures` de `tool_unknown`/`tool_inferred`: muitas incognitas significam braco subinstrumentado, nao braco perfeito.")
    lines.append("- Denominador: a comparacao principal usa so bracos com gate aprovado. Um braco reprovado sai da comparacao e continua visivel em 'todos' e nas secoes de erro; portanto a reducao de n nao e silenciosa.")
    lines.append("- Estatistica: o teste de sinais bilateral trata empates como nao-vitoria e o IC bootstrap pressupoe pares independentes; repeticoes do mesmo cenario no mesmo host, com cache do servidor, violam essa suposicao em algum grau.")
    lines.append("- Identidade: provider/modelo do Slim vem do ledger por request; no Pi o provider nao consta do payload, entao `provider=manifesto` fica `-` nesse braco. Identidade nao medida nunca e exibida como 0.")
    if problems:
        if causes:
            lines.append("- Falhas por causa: " + ", ".join(k + "=" + str(v) for k, v in causes.most_common()))
        lines.append("- Gates com problema: " + "; ".join(problems))
    else:
        lines.append("- Todos os bracos passaram no gate (exit 0, validacao externa PASS, fixtures intactas, metricas completas).")
    args.output_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    def scope_json(scope):
        return {arm: {"total": scope[arm]["total"], "errors": scope[arm]["errors"],
                      "pairs": scope[arm]["pairs"], "by_tool": scope[arm]["by_tool"]} for arm in scope}
    payload = {"generated_utc": generated,
               "manifests": {k: manifests[k] for k in manifests},
               "gate": {"arms_with_records": len(pairs),
                        "arms_approved": sum(len(scope["arms"]) for scope in aggs.values()),
                        "campaigns": [d.name for d in dirs]},
               "aggregate": scope_json(aggs),
               "aggregate_all": scope_json(aggs_all),
               "identity": {key[0] + "/" + key[1]: {"gate_ok": gate_ok(cells[key]),
                                                     "identity": cells[key].get("identity"),
                                                     "tool_unknown": cells[key].get("tool_unknown"),
                                                     "retries": cells[key].get("retries"),
                                                     "cancelled_requests": cells[key].get("cancelled_requests"),
                                                     "cache_hits": cells[key].get("cache_hits"),
                                                     "unknown_requests": cells[key].get("unknown_requests"),
                                                     "failed_requests": cells[key].get("failed_requests"),
                                                     "ttfb_ms_sum": cells[key].get("ttfb_ms_sum"),
                                                     "ttfs_ms_sum": cells[key].get("ttfs_ms_sum"),
                                                     "estimation_error_tokens_sum": cells[key].get("estimation_error_tokens_sum")} for key in sorted(cells)},
               "paired": {"stats": stats, "pairs": paired},
               "arms": pairs, "problems": problems, "failure_causes": dict(causes)}
    args.output_json.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print("md: " + str(args.output_md))
    print("json: " + str(args.output_json))
    for arm in sorted(aggs_all):
        approved_total = aggs.get(arm, {}).get("total")
        print(arm + " aprovados: " + (json.dumps(approved_total, ensure_ascii=False) if approved_total else "sem bracos aprovados"))
        print(arm + " todos: " + json.dumps(aggs_all[arm]["total"], ensure_ascii=False))


if __name__ == "__main__":
    main()
