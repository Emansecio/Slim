"""Collect local audit evidence without reading configuration or credentials."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib


def main() -> None:
    destination = Path(__file__).resolve().parent
    root = destination.parent.parent
    log = (destination / "probe-run.log").read_text(encoding="utf-8-sig")
    start = log.index("\n{\n") + 1
    reproduction, _ = json.JSONDecoder().raw_decode(log[start:])
    tests = (destination / "core-tests.log").read_text(encoding="utf-8-sig")
    counts = re.search(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored", tests)
    if counts is None or "EXIT=0" not in log or "EXIT=0" not in tests:
        raise RuntimeError("Evidence is incomplete or an audit command failed")
    sources = [
        "crates/slim-core/src/tools/write.rs",
        "crates/slim-core/src/tools/patch.rs",
        "crates/slim-core/src/tools/read.rs",
        "crates/slim-core/src/tools/mod.rs",
        "crates/slim-core/src/tools/execution.rs",
        "crates/slim-core/src/runtime/mod.rs",
        "crates/slim-core/src/runtime/loop_guard.rs",
        "crates/slim-core/src/provider.rs",
        "crates/slim-core/src/mcp/spec.rs",
        "crates/slim-core/src/mcp/manager.rs",
        "crates/slim-core/src/session/manual_journal.rs",
        "crates/slim-cli/src/headless.rs",
        "crates/slim-cli/src/mcp.rs",
        "crates/slim-cli/src/tui.rs",
    ]
    hashes = {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources}
    def packages(path: Path) -> set[tuple[str, str, str]]:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        return {(p["name"], p["version"], p["source"]) for p in data["package"] if "source" in p}
    differences = sorted(packages(destination / "probe/Cargo.lock") - packages(root / "Cargo.lock"))
    evidence = {
        "audit_date": "2026-09-13",
        "scope": "Bugs in current source; isolated native tools and loopback provider",
        "production_edits_performed": False,
        "live_model_requests": 0,
        "source_state": {
            "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
            "working_tree_already_modified_at_audit_start": True,
            "hash_capture": "end_of_audit; not a before/after baseline",
            "sha256": hashes,
            "probe_registry_dependencies_not_in_project_lock": differences,
        },
        "unit_tests": {
            "command": "cargo test -p slim-core --lib --offline",
            "passed": int(counts.group(1)), "failed": int(counts.group(2)),
            "ignored": int(counts.group(3)), "exit_code": 0,
            "workspace_suite_executed": False,
        },
        "probe": {
            "command": "cargo run --offline --manifest-path analysis_outputs/AUDIT-BUGS-2026-09-13/probe/Cargo.toml --target-dir target",
            "profile": "dev",
            "exit_code": 0,
            "assertions_characterize_defects_not_correctness": True,
            "results": reproduction,
        },
    }
    encoded = json.dumps(evidence, ensure_ascii=False, indent=2)
    (destination / "EVIDENCIAS.json").write_text(encoded, encoding="utf-8")
    print(encoded)


if __name__ == "__main__":
    main()
