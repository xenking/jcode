#!/usr/bin/env python3
"""Test receipt reuse without building Jcode or Desktop."""

from contextlib import redirect_stdout
import fcntl
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "accept_smoke", Path(__file__).with_name("accept-smoke.py")
)
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)
CLI = "a" * 40
DESKTOP = "b" * 40
PASS_FLOWS = dict.fromkeys((
    "send", "queue_while_busy", "btw", "ask_answer", "image_paste",
    "transcript_scroll", "restart_restore",
), "PASS")


class ReceiptReuseTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name).resolve()
        self.path = self.directory / f"{CLI}-{DESKTOP}.json"

    def write_receipt(self, **fields):
        receipt = dict(schema=1, pair=dict(cli=CLI, desktop=DESKTOP),
                       flows=PASS_FLOWS, result="PASS",
                       finished_at="2026-11-25T04:00:00.123456789Z")
        receipt.update(fields)
        text = json.dumps(receipt) + "\n"
        self.path.write_text(text)
        return text

    def run_smoke(self, *flags, during_run=None):
        def fake_smoke(_args, _run, results):
            if during_run is not None:
                during_run()
            results.update(PASS_FLOWS)

        argv = ["accept-smoke.py", "--jcode-sha", CLI, "--desktop-sha", DESKTOP,
                "--receipt-dir", str(self.directory), *flags]
        stdout = io.StringIO()
        with (patch.object(sys, "argv", argv),
              patch.object(sys, "dont_write_bytecode"),
              patch.object(SMOKE, "smoke", side_effect=fake_smoke),
              redirect_stdout(stdout)):
            self.assertEqual(SMOKE.main(), 0)
        self.assertIn(f"PASS receipt: {self.path}", stdout.getvalue())

    def test_reuses_existing_pass_unchanged(self):
        original = self.write_receipt()
        self.run_smoke()
        self.assertEqual(self.path.read_text(), original)
        self.assertEqual(list(self.directory.glob("smoke-*")), [])

    def test_reuses_pass_published_while_waiting_for_lock(self):
        flock = fcntl.flock
        original = None

        def first_run_finishes(lock, operation):
            nonlocal original
            original = self.write_receipt()
            flock(lock, operation)

        with patch.object(fcntl, "flock", side_effect=first_run_finishes):
            self.run_smoke()
        self.assertEqual(self.path.read_text(), original)
        self.assertEqual(list(self.directory.glob("smoke-*")), [])

    def test_force_removes_pass_while_rerunning(self):
        self.write_receipt()
        self.run_smoke("--force", during_run=lambda: self.assertFalse(self.path.exists()))
        self.assert_new_pass()

    def test_blocked_receipt_reruns(self):
        original = self.write_receipt(result="BLOCKED", flows=dict(PASS_FLOWS, send="BLOCKED"))
        self.run_smoke(during_run=lambda: self.assertEqual(self.path.read_text(), original))
        self.assert_new_pass()

    def test_no_receipt_until_run_finishes(self):
        self.run_smoke(during_run=lambda: self.assertFalse(self.path.exists()))
        self.assert_new_pass()

    def test_invalid_pass_receipts_rerun(self):
        missing_flow = {name: status for name, status in PASS_FLOWS.items() if name != "send"}
        for fields in (
            {"schema": True},
            {"pair": {"cli": DESKTOP, "desktop": CLI}},
            {"flows": missing_flow},
            {"flows": dict(PASS_FLOWS, send="BLOCKED")},
            {"flows": dict(PASS_FLOWS, unexpected="PASS")},
            {"finished_at": "2026-11-25T04:00:00"},
            {"finished_at": "2026-02-30T04:00:00Z"},
        ):
            with self.subTest(fields=fields):
                original = self.write_receipt(**fields)
                self.run_smoke()
                self.assertNotEqual(self.path.read_text(), original)
                self.assert_new_pass()

    def test_duplicate_result_reruns(self):
        original = self.write_receipt().replace('"result": "PASS"', '"result": "BLOCKED", "result": "PASS"')
        self.path.write_text(original)
        self.run_smoke()
        self.assert_new_pass()

    def assert_new_pass(self):
        receipt = json.loads(self.path.read_text())
        self.assertEqual(receipt["result"], "PASS")
        self.assertTrue(Path(receipt["evidence"]).is_dir())


class EvidenceSizeTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name).resolve()

    def write_sparse(self, path, size):
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wb") as stream:
            stream.truncate(size)  # sparse: counts its apparent size without using disk

    def test_small_evidence_passes_and_build_output_fails(self):
        self.write_sparse(self.directory / "send/screenshot.png", 4 * 1024 * 1024)
        self.assertIsNone(SMOKE.evidence_oversize(self.directory))
        self.write_sparse(self.directory / "target/release/jcode", SMOKE.MAX_EVIDENCE_BYTES + 1)
        self.assertIn("over the 50 MiB limit", SMOKE.evidence_oversize(self.directory))

    def test_oversize_evidence_blocks_the_receipt(self):
        def build_copy_leaks_into_evidence(_args, run, results):
            self.write_sparse(run / "target/jcode", SMOKE.MAX_EVIDENCE_BYTES + 1)
            results.update(PASS_FLOWS)

        argv = ["accept-smoke.py", "--jcode-sha", CLI, "--desktop-sha", DESKTOP,
                "--receipt-dir", str(self.directory)]
        stdout = io.StringIO()
        with (patch.object(sys, "argv", argv),
              patch.object(sys, "dont_write_bytecode"),
              patch.object(SMOKE, "smoke", side_effect=build_copy_leaks_into_evidence),
              redirect_stdout(stdout)):
            self.assertEqual(SMOKE.main(), 2)
        receipt = json.loads((self.directory / f"{CLI}-{DESKTOP}.json").read_text())
        self.assertEqual(receipt["result"], "BLOCKED")
        self.assertIn("over the 50 MiB limit", stdout.getvalue())

    def test_builds_go_to_the_shared_target_not_the_receipt_dir(self):
        home = {"JCODE_HOME": "/home/j"}
        self.assertEqual(SMOKE.shared_target_dir(home), Path("/home/j/shared-build/target"))
        self.assertEqual(SMOKE.shared_target_dir(dict(home, CARGO_TARGET_DIR="/cache/t")), Path("/cache/t"))


class ToolchainTests(unittest.TestCase):
    def test_explicit_developer_dir_wins(self):
        self.assertEqual(SMOKE.developer_dir({"DEVELOPER_DIR": "/custom"}, xcode="/"), "/custom")

    def test_missing_xcode_uses_command_line_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            absent = str(Path(directory) / "Xcode")
            self.assertEqual(SMOKE.developer_dir({}, xcode=absent), SMOKE.COMMAND_LINE_TOOLS)
            self.assertEqual(SMOKE.developer_dir({"DEVELOPER_DIR": ""}, xcode=directory), directory)

    @unittest.skipUnless(sys.platform == "darwin", "xcrun is a macOS tool")
    def test_directory_without_toolchains_has_no_metal(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertFalse(SMOKE.has_metal(directory))

    def test_runtime_shaders_become_a_desktop_build_feature(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(SMOKE.subprocess, "run") as run:
            SMOKE.build(Path(directory), Path(directory) / "t", "jcode-desktop", "dev", {},
                        Path(directory) / "log", (SMOKE.RUNTIME_SHADERS,))
            SMOKE.build(Path(directory), Path(directory) / "t", "jcode", "dev", {},
                        Path(directory) / "log")
        with_features, without = (call.args[0] for call in run.call_args_list)
        self.assertEqual(with_features[-2:], ["--features", "gpui_platform/runtime_shaders"])
        self.assertNotIn("--features", without)


if __name__ == "__main__":
    unittest.main()
