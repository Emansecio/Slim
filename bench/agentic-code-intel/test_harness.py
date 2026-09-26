"""Offline counter/parser tests, not synthetic agentic results."""
import json
from pathlib import Path
import tempfile
import unittest
from analyze import analyze
from evaluate import dump, cases, normalized


class Counts(unittest.TestCase):
    def test_parallel_calls_are_one_round_and_results_join_by_id(self):
        out = Path(tempfile.mkdtemp(prefix="slim-counter-test-"))
        dump(out / "run.json", {"wall_ms": 20})
        dump(out / "result.jsonl", {"usage": {"requests": []}})
        dump(out / "check.json", {"correctness": "sucesso", "checks": {"ok": True}})
        entries = [
            {"role": "assistant", "tool_calls": [
                {"id": "a", "name": "search", "arguments": '{"query":"x","context_lines":2}'},
                {"id": "b", "name": "read", "arguments": '{"path":"x"}'}]},
            {"role": "tool", "tool_call_id": "b", "content": "é"},
            {"role": "tool", "tool_call_id": "a", "content": "x"},
            {"role": "assistant", "content": "done"},
        ]
        (out / "session.jsonl").write_text("\n".join(json.dumps({"type": "entry", "entry": e}) for e in entries), encoding="utf-8")
        result = analyze(out)
        self.assertEqual((result["rounds"], result["calls"]), (1, 2))
        self.assertEqual(result["result_bytes"], 3)
        self.assertEqual(result["call_details"][0]["result"], "x")
        self.assertEqual(result["call_details"][1]["result_bytes"], 2)
        self.assertIsNone(result["call_details"][0]["duration_ms"])
        self.assertFalse(result["request_alignment"])
        # Pending calls are not successful tool rounds or zero-byte results.
        (out / "session.jsonl").write_text(json.dumps({"type": "entry", "entry": entries[0]}), encoding="utf-8")
        result = analyze(out)
        self.assertEqual(result["rounds"], 0)
        self.assertEqual(result["missing_results"], 2)
        self.assertIsNone(result["call_details"][0]["result_bytes"])
        self.assertIsNone(result["adjacent_edit_cargo_validation_pairs"])
        self.assertIsNone(result["large_tool_results_gt_10k"])

    def test_opportunity_counts_use_complete_durable_results(self):
        with tempfile.TemporaryDirectory(prefix="slim-opportunity-test-") as root:
            out = Path(root)
            dump(out / "run.json", {"wall_ms": 20})
            dump(out / "result.jsonl", {"usage": {"requests": []}})
            dump(out / "check.json", {"correctness": "sucesso", "checks": {"ok": True}})
            tools = [
                ("patch", '{"path":"a.rs","expected":"a","replacement":"b"}'),
                ("shell", '{"command":"cargo test -p fixture"}'),
                ("write", '{"path":"b.rs","content":"b"}'),
                ("shell", '{"command":"cargo fmt --check"}'),
                ("read", '{"path":"a.rs"}'),
            ]
            entries = [{"role": "assistant", "tool_calls": [
                {"id": str(i), "name": name, "arguments": arguments}
                for i, (name, arguments) in enumerate(tools)
            ]}]
            entries.extend({"role": "tool", "tool_call_id": str(i),
                            "content": "x" * (10 * 1024 + 1) if i == 4 else "ok"}
                           for i in range(len(tools)))
            session = out / "session.jsonl"
            session.write_text("\n".join(json.dumps({"type": "entry", "entry": e}) for e in entries), encoding="utf-8")
            result = analyze(out)
            self.assertEqual(result["adjacent_edit_cargo_validation_pairs"], 2)
            self.assertEqual(result["cross_round_edit_cargo_validation_pairs"], 0)
            self.assertEqual(result["fused_edit_cargo_validation_calls"], 0)
            self.assertEqual(result["large_tool_results_gt_10k"], 1)
            session.write_text("\n".join(json.dumps({"type": "entry", "entry": e}) for e in entries[:-1]), encoding="utf-8")
            result = analyze(out)
            self.assertIsNone(result["adjacent_edit_cargo_validation_pairs"])
            self.assertIsNone(result["fused_edit_cargo_validation_calls"])
            self.assertIsNone(result["large_tool_results_gt_10k"])

    def test_shell_loss_and_cross_round_opportunity_use_process_facts(self):
        with tempfile.TemporaryDirectory(prefix="slim-shell-metrics-") as root:
            out = Path(root)
            dump(out / "run.json", {"wall_ms": 20})
            dump(out / "result.jsonl", {"tool_process_facts": [
                {"name": "shell", "process": {
                    "stdout_bytes": 12000, "stderr_bytes": 9000,
                    "stdout_discarded_bytes": 100, "stderr_discarded_bytes": 0,
                }},
                {"name": "patch", "process": {
                    "stdout_bytes": 9000, "stderr_bytes": 0,
                    "stdout_discarded_bytes": 0, "stderr_discarded_bytes": 0,
                }},
            ], "usage": {"requests": []}})
            dump(out / "check.json", {"correctness": "sucesso", "checks": {"ok": True}})
            entries = [
                {"role": "assistant", "tool_calls": [{"id": "edit", "name": "patch",
                    "arguments": '{"path":"a.rs","expected":"a","replacement":"b"}'}]},
                {"role": "tool", "tool_call_id": "edit", "content": "ok"},
                {"role": "assistant", "tool_calls": [{"id": "test", "name": "shell",
                    "arguments": '{"command":"cargo test"}'}]},
                {"role": "tool", "tool_call_id": "test", "content": "ok"},
                {"role": "assistant", "tool_calls": [{"id": "fused", "name": "patch",
                    "arguments": '{"path":"a.rs","expected":"b","replacement":"c","then_run":{"command":"cargo check"}}'}]},
                {"role": "tool", "tool_call_id": "fused", "content": "ok"},
            ]
            (out / "session.jsonl").write_text("\n".join(
                json.dumps({"type": "entry", "entry": e}) for e in entries), encoding="utf-8")
            result = analyze(out)
            self.assertEqual(result["cross_round_edit_cargo_validation_pairs"], 1)
            self.assertEqual(result["fused_edit_cargo_validation_calls"], 1)
            self.assertEqual(result["shell_truncated_calls"], 2)
            self.assertEqual(result["shell_preview_omitted_bytes"], 5424)
            self.assertEqual(result["shell_capture_discarded_bytes"], 100)

    def test_task_prompts_do_not_prescribe_semantic_tools(self):
        for spec in cases().values():
            self.assertNotIn("code_intel", spec["prompt"])
            self.assertNotIn("context_lines", spec["prompt"])
            self.assertNotIn("LSP", spec["prompt"])
            self.assertNotIn("use search", spec["prompt"].lower())

    def test_symbol_target_is_after_default_limit(self):
        source = cases()["C"]["files"]["src/catalog.rs"]
        prefix = source.split("zeta_export_window")[0]
        self.assertEqual(prefix.count("pub fn noise_"), 160)
        self.assertIn("impl ExportPolicy", prefix)


if __name__ == "__main__":
    unittest.main()
