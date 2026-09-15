"""Comparativo rastreavel Slim x Pi: chamadas, erros, tokens e tempo.

Melhoria sobre analyze.py: aceita N campanhas (daily.py/run.py), classifica
erros de ferramenta por taxonomia com trecho do erro, abre tempo em
wall/provider/tools/residuo e emite markdown + json com ponteiros de
rastreabilidade (arquivos, seq, manifest com versoes/hashes).

Uso:
  python report.py CAMPAIGN [CAMPAIGN ...] --output-md RELATORIO.md --output-json report.json
"""
import argparse
import json
from pathlib import Path
from datetime import datetime, timezone
from collections import Counter
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
    if any(k in t for k in ("enoent", "no such file", "arquivo especificado", "not found", "does not exist", "cannot find", "missing")):
        if name in ("read", "list", "bash", "shell"):
            return "read-missing"
        return "missing-target"
    if "expected" in t and any(k in t for k in ("exist", "precond", "empty", "ausente", "vazio")):
        return "write-precondition"
    if any(k in t for k in ("not recognized", "unexpected token", "parse", "syntax", "powershell", "cmd ", "exit 1", "was unexpected")):
        if name in ("shell", "bash"):
            return "shell-syntax"
    if any(k in t for k in ("patch", "quote", "aspas", "hunk", "apply", "conflict")):
        return "patch-reject"
    if "todo" in t or "transition" in t or "in_progress" in t or "inprogress" in t:
        return "todo-transition"
    if "timeout" in t or "timed out" in t:
        return "timeout"
    return "other-error"


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
    pair_count = min(len(requests), len(messages))
    for index in range(pair_count):
        request, end = requests[index], messages[index]
        message = end.get("message", {})
        usage = message.get("usage", {})
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
        })
    ends_sorted = sorted(list(messages), key=lambda e: e.get("time_ms", 0))
    def turn_of(ts):
        for i, m in enumerate(ends_sorted, 1):
            if ts <= m.get("time_ms", 0):
                return i
        return len(ends_sorted) if ends_sorted else 0
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
            "turn": turn_of(tool.get("time_ms", 0)),
            "success": ok,
            "duration_ms": tool.get("duration_ms", 0),
            "args": short_args(tool.get("args")),
            "error_kind": classify_error(name, err_text) if ok is False else "",
            "error": excerpt(err_text),
            "trace": "pi.audit.jsonl:" + str(call_id)[-12:],
        })
    return {"rows": rows, "tools": tool_list, "request_count": len(requests), "message_count": len(messages)}


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
                "usage_complete": slim.get("usage_complete"), "usage_unknown": slim.get("usage_unknown")}
    calls, tool_outputs = [], {}
    turn = 0
    for r in recs:
        if r.get("type") != "entry":
            continue
        entry = r.get("entry", {})
        if entry.get("tool_calls"):
            turn += 1
            for call in entry["tool_calls"]:
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
        if isinstance(decided, dict) and isinstance(decided.get("success"), bool):
            ok = decided["success"]
            err = "" if ok else content
        tool_list.append({
            "call_id": c["call_id"],
            "name": c["name"] or "?",
            "turn": c["turn"],
            "seq": None,
            "success": ok,
            "duration_ms": None,
            "args": short_args(c["arguments"]),
            "error_kind": classify_error(c["name"], err) if ok is False else "",
            "error": excerpt(err),
            "trace": "slim.session.jsonl:call=..." + str(c["call_id"])[-8:],
        })
    tool_ms = sum(u.get("tool_latency_ms", 0) or 0 for u in usage_requests)
    return {"rows": rows, "tools": tool_list, "tool_ms_sum": tool_ms,
            "provider_turns": usage.get("provider_turns"),
            "usage_complete": slim.get("usage_complete"), "usage_unknown": slim.get("usage_unknown")}

def summarize_arm(cdir, arm):
    timing_path = cdir / (arm + ".timing.json")
    validation_path = cdir / (arm + ".validation.json")
    timing = load(timing_path) if timing_path.exists() else {}
    validation = load(validation_path) if validation_path.exists() else {}
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
        "model_calls": len(rows), "tool_calls": len(tool_list), "tool_failures": len(failures),
        "tool_ms_sum": tool_ms, "provider_ms_sum": totals.get("provider_ms", 0), "residual_ms": residual,
        "total_tokens": totals.get("input", 0) + totals.get("output", 0), **totals,
        "error_breakdown": dict(Counter(t.get("error_kind", "") for t in failures if t.get("error_kind"))),
        "rows": rows, "tools": tool_list,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaigns", nargs="+", type=Path)
    parser.add_argument("--output-md", type=Path, required=True)
    parser.add_argument("--output-json", type=Path, required=True)
    args = parser.parse_args()
    dirs = [p.resolve() for p in args.campaigns]
    for d in dirs:
        if not (d / "manifest.json").exists():
            raise SystemExit("manifest ausente: " + str(d))
    manifests = {d.name: load(d / "manifest.json") for d in dirs}
    models = {m.get("model") for m in manifests.values()}
    providers = {m.get("provider", "openai-codex") for m in manifests.values()}
    pairs, problems = [], []
    for d in dirs:
        manifest = manifests[d.name]
        for arm in ("pi", "slim"):
            if not (d / (arm + ".timing.json")).exists():
                continue
            try:
                summary = summarize_arm(d, arm)
            except Exception as error:
                problems.append(d.name + "/" + arm + ": " + str(error))
                continue
            if summary is None:
                continue
            summary["scenario"] = manifest.get("scenario", "?")
            summary["order"] = manifest.get("order")
            pairs.append(summary)
            timing_ok = summary["exit_code"] == 0 and not summary["timed_out"]
            validation_ok = summary["validation_exit"] == 0 and summary["fixtures_unchanged"]
            if not (timing_ok and validation_ok):
                problems.append(d.name + "/" + arm + ": gate falhou (exit=" + str(summary["exit_code"]) + ", valid=" + str(summary["validation_exit"]) + ", fixtures=" + str(summary["fixtures_unchanged"]) + ")")
    if not pairs:
        raise SystemExit("nenhum braco aproveitavel")
    by_agent = {"pi": [p for p in pairs if p["campaign_path"].endswith(p["campaign"]) and True], "slim": []}
    # pairs ja trazem arm implícito? recuperar pelo timing file existente
    aggs = {}
    for arm in ("pi", "slim"):
        arms = []
        for d in dirs:
            s = summarize_arm(d, arm)
            if s is not None:
                s["scenario"] = manifests[d.name].get("scenario", "?")
                s["order"] = manifests[d.name].get("order")
                arms.append(s)
        if not arms:
            continue
        tot = {}
        for key in ("wall_ms", "model_calls", "tool_calls", "tool_failures", "tool_ms_sum", "provider_ms_sum", "residual_ms", "total_tokens", "input", "uncached", "cache", "output", "reasoning"):
            tot[key] = sum(a.get(key, 0) for a in arms)
        kinds = Counter()
        for a in arms:
            kinds.update(a.get("error_breakdown", {}))
        aggs[arm] = {"arms": arms, "total": tot, "errors": dict(kinds), "pairs": len(arms)}
    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = []
    lines.append("# Slim x Pi — comparativo rastreavel (" + generated + ")")
    lines.append("")
    lines.append("Modelo: `" + "|".join(sorted(models)) + "`; provider: `" + "|".join(sorted(providers)) + "`; effort high.")
    lines.append("")
    lines.append("## Resumo (soma das campanhas aproveitadas)")
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
        row("Tempo wall soma (ms)", "wall_ms", " ms")
        row("Tempo provider soma (ms)", "provider_ms_sum", " ms")
        row("Tempo tools soma (ms)", "tool_ms_sum", " ms")
    lines.append("")
    lines.append("Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes; residuo = wall-provider-tools (startup/teardown).")
    lines.append("")
    lines.append("## Por campanha")
    lines.append("")
    lines.append("| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall) | Pi (chamadas/falhas/tokens/wall) |")
    lines.append("|---|---|---|---|")
    for d in dirs:
        manifest = manifests[d.name]
        scen = manifest.get("scenario", "?")
        sm = next((a for a in aggs.get("slim", {}).get("arms", []) if a["campaign"] == d.name), None)
        pm = next((a for a in aggs.get("pi", {}).get("arms", []) if a["campaign"] == d.name), None)
        def cell(a):
            return "-" if a is None else str(a["model_calls"]) + "/" + str(a["tool_failures"]) + "/" + str(a["total_tokens"]) + "/" + str(a["wall_ms"]) + "ms"
        lines.append("| `" + d.name + "` | " + scen + " | " + cell(sm) + " | " + cell(pm) + " |")
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
            lines.append("| turno | in | out | reasoning | provider_ms | tools |")
            lines.append("|---|---:|---:|---:|---:|---|")
            for r in a["rows"]:
                lines.append("| " + str(r.get("turn")) + " | " + str(r.get("input")) + " | " + str(r.get("output")) + " | " + str(r.get("reasoning")) + " | " + str(r.get("provider_ms")) + " | " + ",".join(r.get("tools") or []) + " |")
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
    lines.append("- Instrumentacao assimetrica: Pi via extensao observadora, Slim via ledger/sessao; contagens sao turnos do modelo, nao TCP/retries de transporte.")
    lines.append("- Tokens sao usos informados pelos providers; sem inferencia de custo monetario.")
    lines.append("- Outcomes Slim: facts estruturados tool.v1 quando presentes; senao heuristica sobre o texto da tool.")
    if problems:
        lines.append("- Gates com problema: " + "; ".join(problems))
    else:
        lines.append("- Todos os bracos aproveitaram passaram no gate (exit 0, validacao externa PASS, fixtures intactos).")
    args.output_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    args.output_json.write_text(json.dumps({"generated_utc": generated, "manifests": {k: manifests[k] for k in manifests}, "aggregate": {arm: {"total": aggs[arm]["total"], "errors": aggs[arm]["errors"], "pairs": aggs[arm]["pairs"]} for arm in aggs}, "arms": pairs, "problems": problems}, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print("md: " + str(args.output_md))
    print("json: " + str(args.output_json))
    for arm in sorted(aggs):
        print(arm + " " + json.dumps(aggs[arm]["total"], ensure_ascii=False))


if __name__ == "__main__":
    main()