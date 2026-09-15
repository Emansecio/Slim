"""Mechanical counts only. Redundancy/error labels belong in manual-review.json."""
import collections
import json
from pathlib import Path
from evaluate import RESULTS, dump


def analyze(out):
    run = json.loads((out / "run.json").read_text(encoding="utf-8"))
    result = json.loads((out / "result.jsonl").read_text(encoding="utf-8"))
    check = json.loads((out / "check.json").read_text(encoding="utf-8"))
    usage = result.get("usage", {})
    entries = [d["entry"] for line in (out / "session.jsonl").read_text(encoding="utf-8").splitlines()
               if (d := json.loads(line)).get("type") == "entry"]
    calls, batches, by_id = [], [], {}
    for index, entry in enumerate(entries):
        batch = []
        for call in entry.get("tool_calls", []):
            c = dict(index=len(calls)+1, batch=len(batches)+1, id=call["id"], tool=call["name"],
                     arguments_raw=call["arguments"], arguments=json.loads(call["arguments"]),
                     start=None, end=None, duration_ms=None, result=None, result_bytes=None,
                     bytes_read=None, processes_created=None, lsp_requests=None,
                     error_category=None, freshness=None, classification=None)
            assert c["id"] not in by_id, "duplicate call id"
            by_id[c["id"]] = c
            calls.append(c)
            batch.append(c["index"])
        if batch:
            batches.append(dict(calls=batch, returned_to_model=any(e.get("role") == "assistant" for e in entries[index+1:])))
        if entry.get("role") == "tool":
            c = by_id[entry["tool_call_id"]]
            assert c["result"] is None, "duplicate result"
            c["result"] = entry["content"]
            c["result_bytes"] = len(entry["content"].encode())
    review_path = RESULTS.parent / "manual-review.json"
    if review_path.exists():
        review = json.loads(review_path.read_text(encoding="utf-8"))
        assigned = set()
        for group in review["runs"].get(out.name, []):
            for number in group["calls"]:
                assert number not in assigned and 1 <= number <= len(calls), "invalid manual label"
                assigned.add(number)
                calls[number-1]["classification"] = group["class"]
                calls[number-1]["classification_reason"] = group["reason"]
        if out.name in review["runs"]:
            assert len(assigned) == len(calls), "manual review incomplete"
        for error in review["errors"]:
            if error["run"] == out.name:
                calls[error["call"]-1]["error_category"] = error["category"]
    counts = collections.Counter(c["tool"] for c in calls)
    buckets = {name: counts.get(name, 0) for name in ("code_intel", "search", "read", "shell")}
    buckets["patch/write"] = counts["patch"] + counts["write"]
    buckets["other"] = sum(counts.values()) - sum(buckets.values())
    rounds = sum(b["returned_to_model"] for b in batches)
    requests = usage.get("requests", [])
    aligned = (len(requests) == len(batches)+1 and all(r["request_kind"] == "provider_turn" and not r["retry_count"] and not r["failed"] for r in requests))
    if aligned:
        for i, batch in enumerate(batches):
            batch["next_request"] = {k: requests[i+1][k] for k in ("system_bytes", "tool_schema_bytes", "history_bytes", "tool_result_bytes")}
            batch["tool_latency_ms_aggregate"] = requests[i]["tool_latency_ms"]
    summary = dict(run=out.name, correctness=check["correctness"], failed_checks=[k for k,v in check["checks"].items() if not v],
                   tool_sequence=[[calls[i-1]["tool"] for i in b["calls"]] for b in batches],
                   rounds=rounds, batches=len(batches), calls=len(calls), counts=dict(counts), buckets=buckets,
                   missing_results=sum(c["result"] is None for c in calls),
                   result_bytes=sum(c["result_bytes"] or 0 for c in calls),
                   wall_ms=run["wall_ms"], provider_latency_ms=usage.get("provider_latency_ms"),
                   tool_latency_ms=usage.get("tool_latency_ms"), provider_turns=usage.get("provider_turns"),
                   tool_calls_executed=usage.get("tool_calls_executed"), tool_calls_reused=usage.get("tool_calls_reused"),
                   tool_calls_suppressed=usage.get("tool_calls_suppressed"),
                   request_alignment=aligned, batches_detail=batches, call_details=calls,
                   requests=requests, input_tokens=result.get("input_tokens"), output_tokens=result.get("output_tokens"),
                   usage_complete=result.get("usage_complete"), stop=result.get("stop"),
                   search_with_context=sum(c["tool"] == "search" and c["arguments"].get("context_lines",0)>0 for c in calls))
    dump(out / "metrics.json", summary)
    return summary


if __name__ == "__main__":
    summaries = [analyze(RESULTS/f"{k}-{rep}") for rep in (1,2) for k in "ABCDE"]
    dump(RESULTS / "summary.json", [{k:v for k,v in s.items() if k not in ("call_details", "requests", "batches_detail")} for s in summaries])
    for s in summaries:
        print(s["run"], s["correctness"], 'rounds',s["rounds"], 'calls',s["calls"], s["counts"], 'seconds',round(s["wall_ms"]/1000,2))
    text = []
    for s in summaries:
        text.append(f'## {s["run"]}')
        for c in s["call_details"]:
            text.append(f'\n{c["index"]}. batch {c["batch"]} {c["tool"]} {c["arguments_raw"]}\nRESULT ({c["result_bytes"]} bytes):\n{c["result"]}')
    (RESULTS / "observable-calls.txt").write_text('\n'.join(text), encoding="utf-8")
