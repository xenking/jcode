#!/usr/bin/env python3
"""Safety-contract tests for the verification helper. Real-app proof is separate."""
import importlib.machinery
import importlib.util
import json
import plistlib
from pathlib import Path
import shlex
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("verify-jcode")
LOADER = importlib.machinery.SourceFileLoader("verify_jcode", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
HELPER = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(HELPER)


class VerificationSafetyTests(unittest.TestCase):
    def test_terminal_journey_rejects_missing_duplicate_and_late_output(self):
        good = "STAGEA_fresh STAGEB_fresh"
        HELPER.check_journey_capture(good, "fresh", True)
        HELPER.check_journey_capture("STAGEA_fresh", "fresh", False)
        for text, completed in [("STAGEA_fresh", True), (good + " STAGEB_fresh", True),
                                (good, False), ("STAGEA_fresh STAGEA_fresh", False)]:
            with self.subTest(text=text, completed=completed), self.assertRaises(RuntimeError):
                HELPER.check_journey_capture(text, "fresh", completed)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)
        self.root = self.base / "runtime"
        self.root.mkdir()
        (self.root / "owner").write_text("test-owner")
        self.evidence = self.base / "evidence"
        self.evidence.mkdir()
        (self.evidence / "proof.txt").write_text("proof survives\n")
        HELPER.dump(self.evidence / "run.json", {
            "schema": 1, "owner": "test-owner", "evidence": str(self.evidence),
            "runtime": str(self.root), "env": {}, "processes": {}, "hotkey_path": HELPER.hotkey_path(),
        })
        self.harness = HELPER.Harness(self.evidence)

    def tearDown(self):
        self.temp.cleanup()

    def test_only_the_tui_pane_gets_file_debug_control(self):
        evidence = self.base / "launched"
        binary = self.base / "fake-jcode"
        binary.write_text("#!/bin/sh\necho jcode-fixture\n")
        binary.chmod(0o700)
        args = SimpleNamespace(binary=binary, cols=160, rows=48, evidence=evidence,
                               loopback_exec_worker=False)
        with patch.object(HELPER.tempfile, "mkdtemp", return_value=str(self.base / "fixture")), \
                patch.object(HELPER.shutil, "which", return_value="/usr/bin/tmux"), \
                patch.object(HELPER.Harness, "spawn", autospec=True) as spawn, \
                patch.object(HELPER.Harness, "cli"), \
                patch.object(HELPER.Harness, "tmux", return_value="123\n") as tmux, \
                patch.object(HELPER.Harness, "frame"), \
                patch.object(HELPER.Harness, "doctor"), \
                patch.object(HELPER, "wait_for", return_value="1234"), \
                patch.object(HELPER, "identity", return_value="original"):
            (self.base / "fixture").mkdir()
            HELPER.launch(args)

        harness, role, _ = spawn.call_args.args
        self.assertEqual(role, "server")
        command = shlex.split(tmux.call_args_list[0].args[-1])
        for name, path in [("JCODE_DEBUG_CMD_PATH", "debug.cmd"),
                           ("JCODE_DEBUG_RESPONSE_PATH", "debug.response")]:
            self.assertNotIn(name, harness.env)
            self.assertIn(f"{name}={harness.root / path}", command)

    def test_cleanup_preserves_proof_and_sessions_and_is_idempotent(self):
        sessions = self.root / "data/sessions"
        sessions.mkdir(parents=True)
        HELPER.dump(sessions / "session_test.json", {"messages": []})
        self.assertEqual(self.harness.cleanup()["status"], "cleaned")
        self.assertFalse(self.root.exists())
        self.assertEqual((self.evidence / "proof.txt").read_text(), "proof survives\n")
        self.assertEqual(json.loads((self.evidence / "sessions/session_test.json").read_text()), {"messages": []})
        self.assertTrue((self.evidence / "cleanup.json").is_file())
        self.assertEqual(HELPER.Harness(self.evidence).cleanup()["status"], "already-cleaned")

    def test_cleanup_refuses_reused_pid_and_keeps_runtime(self):
        self.harness.state["processes"]["server"] = {"pid": 123456, "identity": "original"}
        with patch.object(HELPER, "alive", return_value=True), \
                patch.object(HELPER, "identity", return_value="different"), \
                patch.object(HELPER.os, "killpg") as kill:
            with self.assertRaisesRegex(RuntimeError, "PID identity mismatch"):
                self.harness.cleanup()
            kill.assert_not_called()
        self.assertTrue(self.root.exists())
        self.assertTrue((self.evidence / "proof.txt").exists())

    def test_cleanup_stops_runtime_helpers_and_owned_launch_agents(self):
        with patch.object(HELPER, "stop_runtime_helpers") as companions, \
                patch.object(HELPER, "cleanup_runtime_launch_agents") as agents, \
                patch.object(HELPER, "hotkey_path", return_value="/original.plist"):
            self.harness.state["hotkey_path"] = "/original.plist"
            self.harness.cleanup()
        companions.assert_called_once_with(self.root)
        agents.assert_called_once_with(self.root, "/original.plist")

    def test_cleanup_rejects_changed_global_hotkey_and_keeps_runtime(self):
        self.harness.state["hotkey_path"] = "/original.plist"
        with patch.object(HELPER, "stop_runtime_helpers"), \
                patch.object(HELPER, "cleanup_runtime_launch_agents"), \
                patch.object(HELPER, "hotkey_path", return_value="/other.plist"):
            with self.assertRaisesRegex(RuntimeError, "hotkey.*changed"):
                self.harness.cleanup()
        self.assertTrue(self.root.exists())

    def test_runtime_process_matching_does_not_claim_other_runs(self):
        listing = (f"123 {self.root}/jcode companion\n"
                   f"456 {self.root}-other/jcode companion\n"
                   f"789 {self.root}/jcode serve\n"
                   f"321 {self.root}/jcode setup-hotkey --listen-macos-hotkey\n")
        process = HELPER.subprocess.CompletedProcess([], 0, listing, "")
        with patch.object(HELPER.subprocess, "run", return_value=process), \
                patch.object(HELPER, "identity", return_value="start 123"):
            self.assertEqual(HELPER.runtime_helpers(self.root),
                             [{"pid": 123, "identity": "start 123"}, {"pid": 321, "identity": "start 123"}])

    def test_agent_cleanup_never_boots_out_unowned_or_original_service(self):
        directory = self.root / "home/Library/LaunchAgents"
        directory.mkdir(parents=True)
        for label in ("com.jcode.hotkey", "com.example.owned", "com.example.other"):
            (directory / f"{label}.plist").write_bytes(plistlib.dumps({"Label": label}))
        paths = {"com.jcode.hotkey": str(directory / "com.jcode.hotkey.plist"),
                 "com.example.owned": str(directory / "com.example.owned.plist"),
                 "com.example.other": "/unrelated/other.plist"}
        with patch.object(HELPER.sys, "platform", "darwin"), \
                patch.object(HELPER, "launch_agent_path", side_effect=paths.get), \
                patch.object(HELPER, "run") as run:
            HELPER.cleanup_runtime_launch_agents(self.root, paths["com.jcode.hotkey"])
        run.assert_called_once_with(["/bin/launchctl", "bootout",
                                     f"gui/{HELPER.os.getuid()}/com.example.owned"])

    def test_failed_or_interrupted_drive_cleans_runtime(self):
        for error in (RuntimeError("drive failed"), KeyboardInterrupt()):
            with self.subTest(error=type(error).__name__), \
                    patch.object(HELPER.sys, "argv", ["verify-jcode", "drive", "--evidence", str(self.evidence), "tui"]), \
                    patch.object(HELPER, "Harness", return_value=self.harness), \
                    patch.object(self.harness, "drive", side_effect=error), \
                    patch.object(self.harness, "cleanup") as cleanup:
                with self.assertRaises(type(error)):
                    HELPER.main()
                cleanup.assert_called_once_with()
                self.assertFalse((self.evidence / ".lock").exists())

    def test_bad_owner_refuses_loading(self):
        (self.root / "owner").write_text("someone-else")
        with self.assertRaisesRegex(RuntimeError, "ownership mismatch"):
            HELPER.Harness(self.evidence)

    def test_moved_manifest_refuses_loading(self):
        self.harness.state["evidence"] = str(self.base / "different")
        self.harness.save()
        with self.assertRaisesRegex(RuntimeError, "Moved manifest"):
            HELPER.Harness(self.evidence)

    def test_nested_evidence_is_rejected_before_cleanup(self):
        self.harness.state["runtime"] = str(self.evidence)
        self.harness.save()
        with self.assertRaisesRegex(RuntimeError, "outside disposable runtime"):
            HELPER.Harness(self.evidence)
        self.assertTrue((self.evidence / "proof.txt").is_file())

    def test_observation_disallows_state_injection(self):
        with self.assertRaisesRegex(RuntimeError, "Not an observation"):
            self.harness.observe("inject:assistant:looks good")
        with self.assertRaisesRegex(RuntimeError, "Not a read-only"):
            self.harness.debug("set_provider:openai")

    def test_frame_label_cannot_escape_evidence(self):
        with self.assertRaisesRegex(RuntimeError, "basename"):
            self.harness.frame("../overwritten")

    def test_old_ack_cannot_complete_a_new_turn(self):
        user = {"role": "user", "content": [{"type": "text", "text": "fresh-token"}]}
        ack = {"role": "assistant", "content": [{"type": "text", "text": HELPER.ACK}]}
        self.assertFalse(HELPER.completed_turn([ack, user], "fresh-token"))
        self.assertTrue(HELPER.completed_turn([user, ack], "fresh-token"))
        self.assertFalse(HELPER.completed_turn([user, ack], "absent-token"))
        self.assertFalse(HELPER.completed_turn([user, ack, user], "fresh-token"))

    def test_live_journal_proves_reply_when_snapshot_is_stale(self):
        path = self.root / "session_test.json"
        user = {"id": "user-1", "role": "user", "content": "fresh-token"}
        ack = {"id": "assistant-1", "role": "assistant", "content": HELPER.ACK}
        HELPER.dump(path, {"messages": [user]})
        self.assertFalse(HELPER.completed_turn(HELPER.load_persisted(path)["messages"], "fresh-token"))
        journal = path.with_suffix(".journal.jsonl")
        journal.write_text(json.dumps({"meta": {}, "append_messages": [user, ack]}) + "\n" + '{"partial":')
        loaded = HELPER.load_persisted(path)
        self.assertEqual(loaded["messages"], [user, ack])
        self.assertTrue(HELPER.completed_turn(loaded["messages"], "fresh-token"))
        self.assertEqual(json.loads(path.read_text())["messages"], [user])

    def test_plain_cli_accepts_real_token_usage_footer(self):
        output = HELPER.ACK + "\n[Tokens] upload: 1 download: 1\n\n"
        process = HELPER.subprocess.CompletedProcess([], 0, output, "")
        with patch.object(HELPER.subprocess, "run", return_value=process):
            self.assertEqual(self.harness.cli("plain", "prompt", "plain"), output)

    def test_plain_cli_rejects_unexpected_output(self):
        output = HELPER.ACK + "\nunexpected\n[Tokens] upload: 1 download: 1\n"
        process = HELPER.subprocess.CompletedProcess([], 0, output, "")
        with patch.object(HELPER.subprocess, "run", return_value=process):
            with self.assertRaisesRegex(RuntimeError, "CLI reply mismatch"):
                self.harness.cli("plain", "prompt", "plain")

    def test_digest_works_on_system_python(self):
        path = self.root / "bytes"
        path.write_bytes(b"abc")
        self.assertEqual(HELPER.digest(path), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")

    def test_cancel_releases_provider_gate_after_failed_drive(self):
        (self.root / "control").mkdir()
        with patch.object(self.harness, "doctor"), \
                patch.object(self.harness, "records", return_value=[]), \
                patch.object(self.harness, "frame"), \
                patch.object(self.harness, "observe", return_value={"remote_session_id": "session_test"}), \
                patch.object(self.harness, "send"), \
                patch.object(self.harness, "request_after", side_effect=RuntimeError("provider wait failed")):
            with self.assertRaisesRegex(RuntimeError, "provider wait failed"):
                self.harness.drive("cancel", "json")
        receipt = json.loads((self.evidence / "drive-001-cancel.json").read_text())
        self.assertEqual(receipt["status"], "failed")
        self.assertTrue((self.root / "control" / (receipt["token"] + ".release")).is_file())

    def test_restart_refuses_changed_daemon_identity_before_signal(self):
        self.harness.state["processes"] = {
            "tmux": {"pid": 111, "identity": "terminal"},
            "server": {"pid": 222, "identity": "original-daemon"},
        }
        with patch.object(self.harness, "doctor"), \
                patch.object(self.harness, "records", return_value=[]), \
                patch.object(self.harness, "frame"), \
                patch.object(self.harness, "observe", return_value={"remote_session_id": "session_test"}), \
                patch.object(self.harness, "send"), \
                patch.object(self.harness, "request_after"), \
                patch.object(self.harness, "idle"), \
                patch.object(self.harness, "persisted", return_value={"messages": []}), \
                patch.object(self.harness, "tmux"), \
                patch.object(self.harness, "debug", return_value={"id": "old-daemon"}), \
                patch.object(HELPER, "alive", side_effect=lambda pid: pid == 222), \
                patch.object(HELPER, "identity", return_value="different-daemon"), \
                patch.object(HELPER.os, "killpg") as kill:
            with self.assertRaisesRegex(RuntimeError, "ownership changed"):
                self.harness.drive("restart", "json")
            kill.assert_not_called()
        receipt = json.loads((self.evidence / "drive-001-restart.json").read_text())
        self.assertEqual(receipt["status"], "failed")


if __name__ == "__main__":
    unittest.main()
