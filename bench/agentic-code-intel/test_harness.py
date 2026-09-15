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
