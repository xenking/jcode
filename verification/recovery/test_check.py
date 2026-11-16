import subprocess
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

import check


class ExportTests(unittest.TestCase):
    def test_missing_source_anchor_fails(self):
        with self.assertRaises(ValueError):
            check.between("body", "start", "end")

    def test_ambiguous_source_anchor_fails(self):
        with self.assertRaises(ValueError):
            check.between("start start end", "start", "end")

    def test_missing_end_anchor_fails(self):
        with self.assertRaises(ValueError):
            check.between("start body", "start", "end")

    def test_statement_preserves_real_expression(self):
        source = "before; let should_resume = owner_live || pending; after;"
        self.assertEqual(
            check.statement(source, "should_resume"),
            "let should_resume = owner_live || pending;",
        )


class ResultTests(unittest.TestCase):
    def test_success_requires_completed_model_check(self):
        with tempfile.TemporaryDirectory() as folder:
            with patch.object(check.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "truncated output", "")):
                with self.assertRaises(RuntimeError):
                    check.check(Path(folder), Path("unused.jar"))

    def test_syntax_error_is_not_a_finding(self):
        with tempfile.TemporaryDirectory() as folder:
            with patch.object(check.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "Parse Error", "")):
                with self.assertRaises(RuntimeError):
                    check.check(Path(folder), Path("unused.jar"))

    def test_each_rule_is_checked_independently(self):
        replies = [
            subprocess.CompletedProcess([], 12, f"Invariant {rule} is violated.", "")
            for rule in check.RULES
        ]
        with tempfile.TemporaryDirectory() as folder:
            with patch.object(check.subprocess, "run", side_effect=replies) as run, patch("builtins.print"):
                self.assertEqual(check.check(Path(folder), Path("unused.jar")), set(check.RULES))
                self.assertEqual(run.call_count, len(check.RULES))

    def test_checksum_mismatch_fails_without_network(self):
        with tempfile.TemporaryDirectory() as folder:
            jar = Path(folder) / "wrong.jar"
            jar.write_bytes(b"wrong tool")
            with patch.dict(check.os.environ, {"TLA2TOOLS": str(jar)}):
                with self.assertRaises(ValueError):
                    check.tool_jar()


if __name__ == "__main__":
    unittest.main()
