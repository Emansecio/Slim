"""Per-agent payload + timing + compliance analysis for benchmark v2.

Usage: python analyze.py [bench_dir]   (default: this file's directory)

Reads:
    captures/<scenario>/run<N>/<agent>/req_<k>.json      (payloads)
    captures/<scenario>/run<N>/<agent>/req_<k>.meta.json (server timestamps)
    runs/<scenario>_run<N>_<agent>_timing.json           (process timings)
    captures/<scenario>/compliance.jsonl                 (model/effort gate)

Writes summary_v2.json and prints markdown tables:
  1. Compliance  - model + reasoning effort actually on the wire
  2. Tokens      - per scenario: T1 total, system, tools, Tlast total/history,
                   sum of all request bytes, growth ratio
  3. Speed       - per scenario: startup_ms median/min, ttfc_ms median/min,
                   turn gaps, total task time

Timing definitions (all from server-side meta files, monotonic):
    startup_ms = arrival of req_1 - process start
    ttfc_ms    = arrival of req_2 - process start
                 (req_2 only happens after the agent wrote the code)
    gap_k_ms   = arrival req_k - arrival req_(k-1)   (agent loop overhead)
"""
import json
import os
import statistics
import sys


def size(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


def payload_metrics(path):
    raw = open(path, "rb").read()
    body = json.loads(raw)
    messages = body.get("messages") or []
    system = sum(
        size(m.get("content"))
        for m in messages
        if m.get("role") in ("system", "developer")
    )
    history = sum(size(m) for m in messages if m.get("role") not in ("system", "developer"))
    tools = body.get("tools") or []
    biggest_tool_result = 0
    for m in messages:
        if m.get("role") == "tool":
            content = m.get("content")
            if isinstance(content, str):
                biggest_tool_result = max(biggest_tool_result, len(content.encode("utf-8")))
    return {
        "total_bytes": len(raw),
        "system_bytes": system,
        "tools_bytes": size(tools),
        "tools_count": len(tools),
        "history_bytes": history,
        "messages": len(messages),
        "biggest_tool_result_bytes": biggest_tool_result,
        "est_tokens_total": len(raw) // 4,
    }


def load_timing(bench, agent, scenario, run):
    path = os.path.join(bench, "runs", f"{scenario}_run{run}_{agent}_timing.json")
    if not os.path.exists(path):
        return {}
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run_dir(bench, scenario, run, agent):
    return os.path.join(bench, "captures", scenario, f"run{run}", agent)


def collect_run(bench, scenario, run, agent):
    """Return list of {metrics, mono} per request for one run."""
    d = run_dir(bench, scenario, run, agent)
    if not os.path.isdir(d):
        return []
    entries = []
    for name in sorted(os.listdir(d)):
        if name.endswith(".meta.json"):
            continue
        if not name.startswith("req_") or not name.endswith(".json"):
            continue
        k = int(name[len("req_"):-len(".json")])
        meta_path = os.path.join(d, f"req_{k}.meta.json")
        mono = None
        if os.path.exists(meta_path):
            with open(meta_path, encoding="utf-8") as f:
                mono = json.load(f).get("perf_counter")
        entries.append({"req": k, "metrics": payload_metrics(os.path.join(d, name)), "mono": mono})
    entries.sort(key=lambda e: e["req"])
    return entries


def timing_rows(bench, scenario, run, agent, entries):
    t = load_timing(bench, agent, scenario, run)
    start_epoch = t.get("start_ms")
    total_ms = t.get("total_ms")

    # Server-side arrival times are perf_counter values from the same process,
    # so differences between them are valid; anchoring to process start uses
    # the runner wall clock only when both exist.
    monos = [e["mono"] for e in entries if e["mono"] is not None]
    startup_ms = None
    ttfc_ms = None
    # Server-side arrivals are perf_counter values from one process, so their
    # differences are valid. Anchor: assume the last request arrives ~at the
    # end of the measured process window (runner stops the clock right after
    # Wait-Job returns). Then:
    #   startup_ms  = total_ms - span(last - first)
    #   ttfc_ms     = startup_ms + gap(first -> second)
    if monos and len(entries) >= 2 and total_ms is not None:
        span_ms = int((monos[-1] - monos[0]) * 1000)
        startup_ms = max(total_ms - span_ms, 0)
        gap01 = int((monos[1] - monos[0]) * 1000)
        ttfc_ms = max(startup_ms + gap01, 0)
    gaps = [
        int((b - a) * 1000)
        for a, b in zip(monos, monos[1:])
    ]
    return {
        "turns_seen": len(entries),
        "startup_ms": startup_ms,
        "ttfc_ms": ttfc_ms,
        "gap_ms": gaps,
        "process_total_ms": total_ms,
    }


def median_or_none(values):
    clean = [v for v in values if v is not None]
    return round(statistics.median(clean)) if clean else None


def main(bench):
    scenarios_root = os.path.join(bench, "captures")
    runs_root = os.path.join(bench, "runs")

    # Discover agents/scenarios/runs present on disk.
    scenarios = sorted(
        d for d in os.listdir(scenarios_root)
        if os.path.isdir(os.path.join(scenarios_root, d)) and d.startswith("s")
    )
    agents = set()
    max_run = 0
    for sc in scenarios:
        for d in os.listdir(os.path.join(scenarios_root, sc)):
            if d.startswith("run"):
                max_run = max(max_run, int(d[3:]))
                for sub in os.listdir(os.path.join(scenarios_root, sc, d)):
                    if os.path.isdir(os.path.join(scenarios_root, sc, d, sub)):
                        agents.add(sub)

    # ---------------- Compliance ----------------
    print("## Compliance\n")
    print("| agente | modelo no fio | reasoning_effort | ok? |")
    print("|---|---|---|---|")
    compliance = {}
    comp_path = os.path.join(scenarios_root, "compliance.jsonl")
    # v2 writes per-scenario compliance files; merge all.
    merged = []
    for root, _dirs, files in os.walk(scenarios_root):
        for fn in files:
            if fn == "compliance.jsonl":
                p = os.path.join(root, fn)
                with open(p, encoding="utf-8") as f:
                    merged.extend(json.loads(line) for line in f if line.strip())
    by_agent = {}
    for rec in merged:
        by_agent.setdefault(rec["agent"], []).append(rec)
    for agent, recs in sorted(by_agent.items()):
        models = sorted({str(r["model"]) for r in recs})
        efforts = sorted({r["reasoning_effort"] for r in recs})
        ok = all(r["model"] == r["expected_model"] and
                 r["reasoning_effort"] == r["expected_effort"] for r in recs)
        compliance[agent] = {"models": models, "efforts": efforts, "ok": ok, "requests": len(recs)}
        print(f"| {agent} | {', '.join(models)} | {', '.join(efforts)} | "
              f"{'✅' if ok else '❌ VIOLAÇÃO — não comparar'} |")

    # ---------------- Tokens ----------------
    print("\n## Tokens (medianas entre runs)\n")
    token_summary = {}
    for sc in scenarios:
        print(f"### {sc}\n")
        header = ("| agente | T1 total | T1 system | T1 tools (n) | Tn total "
                  "| Tn histórico | maior tool result | soma todos requests | crescimento | ~tokens Tn |")
        print(header)
        print("|---|---|---|---|---|---|---|---|---|---|")
        for agent in sorted(agents):
            runs_data = []
            for run in range(1, max_run + 1):
                entries = collect_run(bench, sc, run, agent)
                if entries:
                    runs_data.append(entries)
            if not runs_data:
                continue
            def med(key, pick):
                vals = []
                for rd in runs_data:
                    e = pick(rd)
                    if e is not None:
                        vals.append(e[key])
                return median_or_none(vals) if vals else None
            t1_total = med("total_bytes", lambda rd: rd[0]["metrics"])
            t1_sys = med("system_bytes", lambda rd: rd[0]["metrics"])
            t1_tools = med("tools_bytes", lambda rd: rd[0]["metrics"])
            tools_n = med("tools_count", lambda rd: rd[0]["metrics"])
            tn_total = med("total_bytes", lambda rd: rd[-1]["metrics"])
            tn_hist = med("history_bytes", lambda rd: rd[-1]["metrics"])
            big_tool = med("biggest_tool_result_bytes", lambda rd: rd[-1]["metrics"])
            sums = [sum(e["metrics"]["total_bytes"] for e in rd) for rd in runs_data]
            sum_all = median_or_none(sums)
            growths = [rd[-1]["metrics"]["total_bytes"] / rd[0]["metrics"]["total_bytes"]
                       for rd in runs_data]
            growth = round(statistics.median(growths), 2)
            est_toks = med("est_tokens_total", lambda rd: rd[-1]["metrics"])
            token_summary.setdefault(sc, {})[agent] = {
                "t1_total": t1_total, "t1_system": t1_sys, "t1_tools": t1_tools,
                "tools_count": tools_n, "tn_total": tn_total, "tn_history": tn_hist,
                "biggest_tool": big_tool, "sum_all_requests_median": sum_all,
                "growth_ratio_median": growth, "est_tokens_tn": est_toks,
            }
            print(f"| {agent} | {t1_total} B | {t1_sys} B | {t1_tools} B ({tools_n}) "
                  f"| {tn_total} B | {tn_hist} B | {big_tool} B | {sum_all} B "
                  f"| x{growth} | ~{est_toks} |")
        print()

    # ---------------- Speed ----------------
    print("## Velocidade (medianas entre runs)\n")
    speed_summary = {}
    for sc in scenarios:
        print(f"### {sc}\n")
        print("| agente | startup (mediana/min) | TTFC (mediana/min) | gaps entre turnos | tempo total do processo |")
        print("|---|---|---|---|---|")
        for agent in sorted(agents):
            rows = []
            for run in range(1, max_run + 1):
                entries = collect_run(bench, sc, run, agent)
                if not entries:
                    continue
                rows.append(timing_rows(bench, sc, run, agent, entries))
            if not rows:
                continue
            startups = [r["startup_ms"] for r in rows]
            ttfcs = [r["ttfc_ms"] for r in rows]
            totals = [r["process_total_ms"] for r in rows]
            all_gaps = [g for r in rows for g in r["gap_ms"]]
            speed_summary.setdefault(sc, {})[agent] = {
                "startup_median_ms": median_or_none(startups),
                "startup_min_ms": min((v for v in startups if v is not None), default=None),
                "ttfc_median_ms": median_or_none(ttfcs),
                "ttfc_min_ms": min((v for v in ttfcs if v is not None), default=None),
                "gap_median_ms": median_or_none(all_gaps),
                "process_total_median_ms": median_or_none(totals),
            }
            s = speed_summary[sc][agent]
            print(f"| {agent} | {s['startup_median_ms']} / {s['startup_min_ms']} ms "
                  f"| {s['ttfc_median_ms']} / {s['ttfc_min_ms']} ms "
                  f"| {s['gap_median_ms']} ms | {s['process_total_median_ms']} ms |")
        print()

    out = os.path.join(bench, "summary_v2.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump({"compliance": compliance, "tokens": token_summary, "speed": speed_summary},
                  f, indent=2, ensure_ascii=False)
    print(f"summary: {out}")


if __name__ == "__main__":
    default_bench = os.path.dirname(os.path.abspath(__file__))
    main(sys.argv[1] if len(sys.argv) > 1 else default_bench)
