#!/usr/bin/env python3
"""Safety-contract tests for the verification helper. Real-app proof is separate."""
import importlib.machinery
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("verify-jcode")
LOADER = importlib.machinery.SourceFileLoader("verify_jcode", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
HELPER = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(HELPER)


class VerificationSafetyTests(unittest.TestCase):
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
            "runtime": str(self.root), "env": {}, "processes": {},
        })
        self.harness = HELPER.Harness(self.evidence)

    def tearDown(self):
        self.temp.cleanup()

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

    def test_digest_works_on_system_python(self):
        path = self.root / "bytes"
        path.write_bytes(b"abc")
        self.assertEqual(HELPER.digest(path), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")


if __name__ == "__main__":
    unittest.main()
