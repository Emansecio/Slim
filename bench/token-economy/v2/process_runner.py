"""Run one benchmark arm without adding shell/job timing overhead."""

import json
import os
import subprocess
import sys
import time
from pathlib import Path


def main(spec_path, result_path, stdout_path, stderr_path):
    with open(spec_path, encoding="utf-8-sig") as source:
        spec = json.load(source)

    env = os.environ.copy()
    for key, value in spec.get("environment", {}).items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = str(value)

    started_epoch_ms = time.time_ns() // 1_000_000
    started = time.perf_counter()
    timed_out = False
    with open(stdout_path, "wb") as stdout, open(stderr_path, "wb") as stderr:
        process = subprocess.Popen(
            [spec["executable"], *spec.get("arguments", [])],
            cwd=spec["cwd"],
            env=env,
            stdout=stdout,
            stderr=stderr,
        )
        try:
            exit_code = process.wait(timeout=float(spec["timeout_seconds"]))
        except subprocess.TimeoutExpired:
            timed_out = True
            process.kill()
            exit_code = process.wait()

    elapsed_ms = round((time.perf_counter() - started) * 1000)
    result = {
        "agent": spec["agent"],
        "scenario": spec["scenario"],
        "run": spec["run"],
        "start_ms": started_epoch_ms,
        "end_ms": time.time_ns() // 1_000_000,
        "total_ms": elapsed_ms,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_bytes": Path(stdout_path).stat().st_size,
        "stderr_bytes": Path(stderr_path).stat().st_size,
    }
    with open(result_path, "w", encoding="utf-8") as output:
        json.dump(result, output, indent=2)
        output.write("\n")


if __name__ == "__main__":
    if len(sys.argv) != 5:
        raise SystemExit("usage: process_runner.py SPEC RESULT STDOUT STDERR")
    main(*sys.argv[1:])
