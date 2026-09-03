"""Strict analysis for one hermetic Slim/Pi/Pit benchmark campaign."""

import argparse
import json
import os
import re
import shutil
import statistics
import sys
from pathlib import Path

BASE = Path(__file__).resolve().parent
EXPECTED_REQUESTS = {"s1_read": 2, "s2_codegen": 2, "s3_multistep": 3, "s4_long": 5}
POST_WRITE_REQUEST = {"s2_codegen": 1, "s3_multistep": 1, "s4_long": 3}
REQUEST_RE = re.compile(r"req_(\d+)\.json$")


def encoded_size(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


def read_json(path):
    with open(path, encoding="utf-8-sig") as source:
        return json.load(source)


def payload_metrics(path):
    raw = path.read_bytes()
    body = json.loads(raw)
    messages = body.get("messages") or []
    tools = body.get("tools") or []
    biggest_tool_result = 0
    for message in messages:
        if message.get("role") == "tool" and isinstance(message.get("content"), str):
            biggest_tool_result = max(
                biggest_tool_result, len(message["content"].encode("utf-8"))
            )
    return {
        "total_bytes": len(raw),
        "system_content_bytes": sum(
            encoded_size(message.get("content"))
            for message in messages
            if message.get("role") in ("system", "developer")
        ),
        "tools_schema_bytes": encoded_size(tools),
        "tools_count": len(tools),
        "history_bytes": sum(
            encoded_size(message)
            for message in messages
            if message.get("role") not in ("system", "developer")
        ),
        "biggest_tool_result_bytes": biggest_tool_result,
        "rough_tokens_byte_div4": len(raw) // 4,
    }


def median_int(values):
    return round(statistics.median(values))


def min_or_none(values):
    clean = [value for value in values if value is not None]
    return min(clean) if clean else None


def median_or_none(values):
    clean = [value for value in values if value is not None]
    return median_int(clean) if clean else None


def agent_tags(manifest):
    suffix = f"_{manifest['variant_tag']}" if manifest.get("variant_tag") else ""
    return [f"{agent}{suffix}" for agent in manifest["agents"]]


def collect_entries(arm_dir, errors, arm_name):
    entries = []
    for path in arm_dir.glob("req_*.json"):
        match = REQUEST_RE.fullmatch(path.name)
        if not match:
            continue
        request = int(match.group(1))
        meta_path = arm_dir / f"req_{request}.meta.json"
        if not meta_path.exists():
            errors.append(f"{arm_name}: missing {meta_path.name}")
            continue
        entries.append(
            {
                "request": request,
                "payload": payload_metrics(path),
                "meta": read_json(meta_path),
            }
        )
    entries.sort(key=lambda entry: entry["request"])
    return entries


def compliance_records(arm_dir, errors, arm_name):
    path = arm_dir / "compliance.jsonl"
    if not path.exists():
        errors.append(f"{arm_name}: missing compliance.jsonl")
        return []
    records = []
    with open(path, encoding="utf-8") as source:
        for line_number, line in enumerate(source, 1):
            if not line.strip():
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as error:
                errors.append(f"{arm_name}: invalid compliance line {line_number}: {error}")
    return records


def arm_payload_row(run, entries):
    first = entries[0]["payload"]
    last = entries[-1]["payload"]
    return {
        "run": run,
        "requests": len(entries),
        "t1_total_bytes": first["total_bytes"],
        "t1_system_content_bytes": first["system_content_bytes"],
        "t1_tools_schema_bytes": first["tools_schema_bytes"],
        "tools_count": first["tools_count"],
        "tn_total_bytes": last["total_bytes"],
        "tn_history_bytes": last["history_bytes"],
        "biggest_tool_result_bytes": max(
            entry["payload"]["biggest_tool_result_bytes"] for entry in entries
        ),
        "sum_all_request_bytes": sum(
            entry["payload"]["total_bytes"] for entry in entries
        ),
        "growth_ratio": round(last["total_bytes"] / first["total_bytes"], 4),
        "rough_tokens_tn_byte_div4": last["rough_tokens_byte_div4"],
    }


def arm_speed_row(run, scenario, entries, timing, errors, arm_name):
    start_ms = timing.get("start_ms")
    end_ms = timing.get("end_ms")
    total_ms = timing.get("total_ms")
    arrivals = [entry["meta"].get("epoch_ms") for entry in entries]
    monotonic = [entry["meta"].get("perf_counter") for entry in entries]
    if start_ms is None or end_ms is None or total_ms is None:
        errors.append(f"{arm_name}: incomplete process timing")
        return None
    if any(value is None for value in arrivals + monotonic):
        errors.append(f"{arm_name}: incomplete request timing")
        return None
    if arrivals and (arrivals[0] < start_ms - 5 or arrivals[-1] > end_ms + 5):
        errors.append(f"{arm_name}: request timestamps fall outside process interval")
    startup_ms = max(arrivals[0] - start_ms, 0)
    write_index = POST_WRITE_REQUEST.get(scenario)
    write_complete_ms = (
        max(arrivals[write_index] - start_ms, 0)
        if write_index is not None and len(arrivals) > write_index
        else None
    )
    gaps = [round((after - before) * 1000) for before, after in zip(monotonic, monotonic[1:])]
    return {
        "run": run,
        "startup_ms": startup_ms,
        "write_complete_ms": write_complete_ms,
        "turn_gap_ms": gaps,
        "process_total_ms": total_ms,
        "exit_code": timing.get("exit_code"),
        "timed_out": timing.get("timed_out"),
    }


def aggregate_payload(rows):
    return {
        "runs": rows,
        "requests_per_run": sorted({row["requests"] for row in rows}),
        "t1_total_median_bytes": median_int([row["t1_total_bytes"] for row in rows]),
        "t1_system_content_median_bytes": median_int(
            [row["t1_system_content_bytes"] for row in rows]
        ),
        "t1_tools_schema_median_bytes": median_int(
            [row["t1_tools_schema_bytes"] for row in rows]
        ),
        "tools_count_median": median_int([row["tools_count"] for row in rows]),
        "tn_total_median_bytes": median_int([row["tn_total_bytes"] for row in rows]),
        "tn_history_median_bytes": median_int([row["tn_history_bytes"] for row in rows]),
        "biggest_tool_result_median_bytes": median_int(
            [row["biggest_tool_result_bytes"] for row in rows]
        ),
        "sum_all_requests_median_bytes": median_int(
            [row["sum_all_request_bytes"] for row in rows]
        ),
        "growth_ratio_median": round(
            statistics.median(row["growth_ratio"] for row in rows), 2
        ),
        "rough_tokens_tn_byte_div4_median": median_int(
            [row["rough_tokens_tn_byte_div4"] for row in rows]
        ),
    }


def aggregate_speed(rows):
    all_gaps = [gap for row in rows for gap in row["turn_gap_ms"]]
    startups = [row["startup_ms"] for row in rows]
    writes = [row["write_complete_ms"] for row in rows]
    totals = [row["process_total_ms"] for row in rows]
    return {
        "runs": rows,
        "startup_median_ms": median_or_none(startups),
        "startup_min_ms": min_or_none(startups),
        "write_complete_median_ms": median_or_none(writes),
        "write_complete_min_ms": min_or_none(writes),
        "turn_gap_median_ms": median_or_none(all_gaps),
        "process_total_median_ms": median_or_none(totals),
        "process_total_min_ms": min_or_none(totals),
    }


def discover_actual_arms(captures):
    arms = set()
    if not captures.is_dir():
        return arms
    for scenario_dir in captures.iterdir():
        if not scenario_dir.is_dir():
            continue
        for run_dir in scenario_dir.iterdir():
            if not run_dir.is_dir() or not run_dir.name.startswith("run"):
                continue
            try:
                run = int(run_dir.name[3:])
            except ValueError:
                continue
            for agent_dir in run_dir.iterdir():
                if agent_dir.is_dir():
                    arms.add((scenario_dir.name, run, agent_dir.name))
    return arms


def format_value(value, suffix=""):
    return "—" if value is None else f"{value}{suffix}"


def markdown(summary):
    manifest = summary["manifest"]
    lines = [
        f"# Analysis — campaign `{manifest['campaign']}`",
        "",
        "## Gate",
        "",
        f"**PASS** — {summary['gate']['valid_arms']}/{summary['gate']['expected_arms']} arms; "
        f"{summary['gate']['compliant_requests']}/{summary['gate']['expected_requests']} compliant requests.",
        "",
        "Payload values below are UTF-8 bytes in the captured JSON request. "
        "`~tokens` remains only bytes ÷ 4 and is not provider billing usage.",
        "",
        "## Compliance",
        "",
        "| agent | model | effort | requests | ok |",
        "|---|---|---|---:|---|",
    ]
    for agent, row in summary["compliance"].items():
        lines.append(
            f"| {agent} | {', '.join(row['models'])} | {', '.join(row['efforts'])} | "
            f"{row['requests']}/{row['expected_requests']} | {'PASS' if row['ok'] else 'FAIL'} |"
        )
    lines.extend(["", "## Payload (medians across runs)", ""])
    for scenario, agents in summary["payload"].items():
        lines.extend(
            [
                f"### {scenario}",
                "",
                "| agent | requests/run | T1 total | T1 system content | T1 tool schemas (n) | "
                "Tn total | Tn history | sum all requests | growth | ~tokens Tn |",
                "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        for agent, row in agents.items():
            lines.append(
                f"| {agent} | {','.join(map(str, row['requests_per_run']))} | "
                f"{row['t1_total_median_bytes']} B | {row['t1_system_content_median_bytes']} B | "
                f"{row['t1_tools_schema_median_bytes']} B ({row['tools_count_median']}) | "
                f"{row['tn_total_median_bytes']} B | {row['tn_history_median_bytes']} B | "
                f"{row['sum_all_requests_median_bytes']} B | ×{row['growth_ratio_median']} | "
                f"~{row['rough_tokens_tn_byte_div4_median']} |"
            )
        lines.append("")
    lines.extend(["## Speed (direct medians/minima)", ""])
    for scenario, agents in summary["speed"].items():
        lines.extend(
            [
                f"### {scenario}",
                "",
                "| agent | startup median/min | post-write request median/min | turn gap median | process median/min |",
                "|---|---:|---:|---:|---:|",
            ]
        )
        for agent, row in agents.items():
            lines.append(
                f"| {agent} | {format_value(row['startup_median_ms'])}/{format_value(row['startup_min_ms'])} ms | "
                f"{format_value(row['write_complete_median_ms'])}/{format_value(row['write_complete_min_ms'])} ms | "
                f"{format_value(row['turn_gap_median_ms'], ' ms')} | "
                f"{format_value(row['process_total_median_ms'])}/{format_value(row['process_total_min_ms'])} ms |"
            )
        lines.append("")
    lines.extend(
        [
            "## Definitions",
            "",
            "- startup = first request arrival epoch − process start epoch.",
            "- post-write request = first request sent after the write tool returns − process start epoch.",
            "- process total is measured around the agent subprocess only; no PowerShell `Start-Job`.",
            "- all individual run samples are retained in `summary_v2.json` and raw requests in `captures/`.",
            "",
        ]
    )
    return "\n".join(lines)


def analyze(campaign):
    manifest_path = campaign / "manifest.json"
    if not manifest_path.exists():
        raise ValueError(f"missing manifest: {manifest_path}")
    manifest = read_json(manifest_path)
    captures = campaign / "captures"
    runs_dir = campaign / "runs"
    workspaces = campaign / "workspaces"
    scenarios = list(manifest["scenarios"])
    tags = agent_tags(manifest)
    run_count = int(manifest["runs"])
    model = manifest["model"]
    effort = manifest["effort"]
    expected_arms = {
        (scenario, run, agent)
        for scenario in scenarios
        for run in range(1, run_count + 1)
        for agent in tags
    }
    errors = []
    actual_arms = discover_actual_arms(captures)
    for arm in sorted(expected_arms - actual_arms):
        errors.append(f"missing arm: {arm[0]}/run{arm[1]}/{arm[2]}")
    for arm in sorted(actual_arms - expected_arms):
        errors.append(f"unexpected arm: {arm[0]}/run{arm[1]}/{arm[2]}")

    payload_rows = {scenario: {agent: [] for agent in tags} for scenario in scenarios}
    speed_rows = {scenario: {agent: [] for agent in tags} for scenario in scenarios}
    compliance_all = {agent: [] for agent in tags}
    valid_arms = 0

    for scenario, run, agent in sorted(expected_arms):
        arm_name = f"{scenario}/run{run}/{agent}"
        arm_dir = captures / scenario / f"run{run}" / agent
        if not arm_dir.is_dir():
            continue
        expected_requests = EXPECTED_REQUESTS.get(scenario)
        if expected_requests is None:
            errors.append(f"{arm_name}: unknown expected request count")
            continue
        entries = collect_entries(arm_dir, errors, arm_name)
        sequence = [entry["request"] for entry in entries]
        if sequence != list(range(expected_requests)):
            errors.append(
                f"{arm_name}: request sequence {sequence}, expected {list(range(expected_requests))}"
            )
        records = compliance_records(arm_dir, errors, arm_name)
        compliance_all[agent].extend(records)
        if len(records) != expected_requests:
            errors.append(
                f"{arm_name}: {len(records)} compliance records, expected {expected_requests}"
            )
        for record in records:
            if record.get("model") != model or record.get("reasoning_effort") != effort:
                errors.append(
                    f"{arm_name}/req{record.get('req')}: compliance violation "
                    f"model={record.get('model')!r}, effort={record.get('reasoning_effort')!r}"
                )
        timing_path = runs_dir / f"{scenario}_run{run}_{agent}_timing.json"
        if not timing_path.exists():
            errors.append(f"{arm_name}: missing process timing")
            continue
        timing = read_json(timing_path)
        if timing.get("timed_out") or timing.get("exit_code") != 0:
            errors.append(
                f"{arm_name}: process timed_out={timing.get('timed_out')} "
                f"exit_code={timing.get('exit_code')}"
            )
        if scenario != "s1_read" and not (workspaces / scenario / f"run{run}" / agent / "fizzbuzz.py").is_file():
            errors.append(f"{arm_name}: fizzbuzz.py was not created")
        if len(entries) == expected_requests:
            payload_rows[scenario][agent].append(arm_payload_row(run, entries))
            speed = arm_speed_row(run, scenario, entries, timing, errors, arm_name)
            if speed is not None:
                speed_rows[scenario][agent].append(speed)
            valid_arms += 1

    compliance = {}
    expected_by_agent = sum(EXPECTED_REQUESTS[scenario] for scenario in scenarios) * run_count
    for agent, records in compliance_all.items():
        compliance[agent] = {
            "models": sorted({str(record.get("model")) for record in records}),
            "efforts": sorted({str(record.get("reasoning_effort")) for record in records}),
            "requests": len(records),
            "expected_requests": expected_by_agent,
            "ok": len(records) == expected_by_agent
            and all(
                record.get("model") == model and record.get("reasoning_effort") == effort
                for record in records
            ),
        }

    for scenario in scenarios:
        for agent in tags:
            if len(payload_rows[scenario][agent]) != run_count:
                errors.append(
                    f"{scenario}/{agent}: {len(payload_rows[scenario][agent])} valid payload runs, "
                    f"expected {run_count}"
                )
            if len(speed_rows[scenario][agent]) != run_count:
                errors.append(
                    f"{scenario}/{agent}: {len(speed_rows[scenario][agent])} valid timing runs, "
                    f"expected {run_count}"
                )

    expected_request_total = expected_by_agent * len(tags)
    compliant_requests = sum(row["requests"] for row in compliance.values() if row["ok"])
    gate = {
        "ok": not errors,
        "expected_arms": len(expected_arms),
        "valid_arms": valid_arms,
        "expected_requests": expected_request_total,
        "compliant_requests": compliant_requests,
        "errors": errors,
    }
    if errors:
        with open(campaign / "gate_report.json", "w", encoding="utf-8") as output:
            json.dump(gate, output, indent=2, ensure_ascii=False)
            output.write("\n")
        raise ValueError("campaign gate failed:\n- " + "\n- ".join(errors))

    payload = {
        scenario: {
            agent: aggregate_payload(payload_rows[scenario][agent]) for agent in tags
        }
        for scenario in scenarios
    }
    speed = {
        scenario: {agent: aggregate_speed(speed_rows[scenario][agent]) for agent in tags}
        for scenario in scenarios
    }
    summary = {
        "schema_version": 3,
        "manifest": manifest,
        "gate": gate,
        "compliance": compliance,
        "payload": payload,
        "speed": speed,
    }
    summary_path = campaign / "summary_v2.json"
    with open(summary_path, "w", encoding="utf-8") as output:
        json.dump(summary, output, indent=2, ensure_ascii=False)
        output.write("\n")
    analysis_path = campaign / "analysis.md"
    analysis_path.write_text(markdown(summary), encoding="utf-8")
    return summary_path, analysis_path


def latest_campaign():
    root = BASE / "campaigns"
    candidates = [path for path in root.iterdir() if path.is_dir() and (path / "manifest.json").exists()] if root.exists() else []
    if not candidates:
        raise ValueError("no campaign found; run run_benchmark.ps1 first or pass a campaign path")
    return max(candidates, key=lambda path: path.stat().st_mtime)


def main():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser()
    parser.add_argument("campaign", nargs="?", type=Path)
    parser.add_argument(
        "--publish",
        action="store_true",
        help="copy the validated summary/analysis to summary_v2.json and analysis-current.md",
    )
    args = parser.parse_args()
    campaign = (args.campaign or latest_campaign()).resolve()
    summary_path, analysis_path = analyze(campaign)
    if args.publish:
        shutil.copyfile(summary_path, BASE / "summary_v2.json")
        shutil.copyfile(analysis_path, BASE / "analysis-current.md")
    rendered = analysis_path.read_text(encoding="utf-8")
    try:
        print(rendered)
    except UnicodeEncodeError:
        print(rendered.encode(sys.stdout.encoding or "ascii", errors="replace").decode(sys.stdout.encoding or "ascii"))
    print(f"summary: {summary_path}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"analysis failed: {error}", file=sys.stderr)
        raise SystemExit(1)
