"""Per-agent payload metrics for captured benchmark requests.

Usage: python analyze.py [captures_dir]
Prints a markdown table and writes summary.json next to the captures.
"""
import json
import os
import sys


def size(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


def metrics(path):
    raw = open(path, "rb").read()
    body = json.loads(raw)
    messages = body.get("messages") or []
    system = sum(
        size(m.get("content")) for m in messages if m.get("role") in ("system", "developer")
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


def main(captures_dir):
    runs = {}
    for name in sorted(os.listdir(captures_dir)):
        if not name.endswith(".json"):
            continue
        agent = name.split("_req_")[0]
        runs.setdefault(agent, []).append(os.path.join(captures_dir, name))

    rows = []
    summary = {}
    for agent, files in runs.items():
        turns = [metrics(f) for f in files]
        t1, tlast = turns[0], turns[-1]
        row = {
            "agent": agent,
            "turns": len(turns),
            "t1_total": t1["total_bytes"],
            "t1_system": t1["system_bytes"],
            "t1_tools": t1["tools_bytes"],
            "tools_count": t1["tools_count"],
            "tlast_total": tlast["total_bytes"],
            "tlast_history": tlast["history_bytes"],
            "tlast_biggest_tool": tlast["biggest_tool_result_bytes"],
            "tlast_est_tokens": tlast["est_tokens_total"],
            "growth_ratio": round(tlast["total_bytes"] / t1["total_bytes"], 2),
        }
        rows.append(row)
        summary[agent] = {"turns": turns, "row": row}

    header = ("| agente | turnos | T1 total | T1 system | T1 tools (n) | "
              "Tn total | Tn histórico | maior tool result | ~tokens Tn | crescimento |")
    sep = "|---|---|---|---|---|---|---|---|---|---|"
    print(header)
    print(sep)
    for r in rows:
        print(f"| {r['agent']} | {r['turns']} | {r['t1_total']} B | {r['t1_system']} B | "
              f"{r['t1_tools']} B ({r['tools_count']}) | {r['tlast_total']} B | "
              f"{r['tlast_history']} B | {r['tlast_biggest_tool']} B | "
              f"~{r['tlast_est_tokens']} | x{r['growth_ratio']} |")

    out = os.path.join(captures_dir, "summary.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2, ensure_ascii=False)
    print(f"\nsummary: {out}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else
         os.path.join(os.path.dirname(os.path.abspath(__file__)), "captures"))
