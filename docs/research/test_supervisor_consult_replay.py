"""Run with python3 -B -m unittest discover -s docs/research -p 'test_supervisor_consult_replay.py'."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "supervisor-consult-replay.py"
SECRET = "do-not-export-private-content"
START, END = "2026-11-21T09:05:00Z", "2026-11-21T09:11:00Z"


def message(key, timestamp, role, *blocks, **fields):
    return {"id": key, "timestamp": timestamp, "role": role, "content": blocks, **fields}


def run(*args):
    return subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True)


class ReplayTest(unittest.TestCase):
    def test_capture_exports_only_ordered_metadata_and_deduplicates(self):
        text = {"type": "text", "text": SECRET}
        question = message("private-message-id", "2026-11-21T09:04:59Z", "assistant", {
            "type": "tool_use", "name": "ask", "id": "private-call-id", "input": {"query": SECRET},
        })
        answer = message("result", START, "user", {
            "type": "tool_result", "tool_use_id": "private-call-id", "content": [text],
        })
        direct = message("candidate", "2026-11-21T09:06:00Z", "user", text)
        routed = message("dm", "2026-11-21T09:07:00Z", "assistant", {
            "type": "tool_use", "name": "functions.swarm", "id": "private-dm-id",
            "input": {"action": "message", "message": SECRET},
        })
        boundary = message("end", END, "user", text)
        with tempfile.TemporaryDirectory() as root:
            snapshot, journal = Path(root) / "snapshot.json", Path(root) / "journal.jsonl"
            snapshot.write_text(json.dumps({"messages": [question, answer, direct], "title": SECRET}))
            journal.write_text(json.dumps({"append_messages": [answer, routed, boundary], "meta": SECRET}))
            result = run("capture", "--snapshot", str(snapshot), "--journal", str(journal),
                         "--since", START, "--until", END)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {
                "schema": 1, "since": START, "until": END, "scope": "primary",
                "events": ["tool_result", "ask_return", "user_candidate", "assistant_with_tools",
                           "tool_call", "swarm_message"],
            })
            self.assertNotIn(SECRET, result.stdout)
            self.assertNotIn("private-", result.stdout)
            journal.write_text('not JSON ' + SECRET)
            failed = run("capture", "--snapshot", str(snapshot), "--journal", str(journal),
                         "--since", START, "--until", END)
            self.assertEqual(failed.returncode, 2)
            self.assertEqual(failed.stdout, "")
            self.assertNotIn(SECRET, failed.stderr)

    def test_committed_trace_separates_counts_from_hypotheses(self):
        for scope, tool_calls in (("primary", 12), ("worker", 25)):
            trace = str(HERE / f"supervisor-consult-{scope}.json")
            for consultations in (0, 3):
                result = run("replay", trace, "--consultations", str(consultations))
                self.assertEqual(result.returncode, 0, result.stderr)
                payload = json.loads(result.stdout)
                self.assertEqual(payload["observed"]["tool_call"], tool_calls)
                self.assertEqual(payload["observed"]["user_candidate"], 1)
                counts = payload["counterfactual"]
                self.assertEqual(counts["direct_candidate_wakes_in_either_proposal"], 0)
                self.assertEqual(counts["stateless"], {"dispatches": consultations, "logical_contexts": consultations})
                self.assertEqual(counts["persistent_one_scope"],
                                 {"dispatches": consultations, "logical_contexts": int(consultations > 0)})
                self.assertIn("quota", payload["not_measured"])
                self.assertIn("approved_actions", payload["not_measured"])
