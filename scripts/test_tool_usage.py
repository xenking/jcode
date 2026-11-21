#!/usr/bin/env python3
"""Regression tests for tool_usage.py using isolated, synthetic sessions."""
import datetime as dt
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


class UsageTest(unittest.TestCase):
    def test_deduplicates_snapshot_and_journal_and_filters_old_messages(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            now = dt.datetime.now(dt.timezone.utc)
            def message(ident, timestamp, name, command=None):
                return {'id': ident, 'timestamp': timestamp.isoformat(), 'content': [
                    {'type': 'tool_use', 'name': name, 'input': {'command': command} if command else {}}
                ]}
            fresh = message('fresh', now, 'bash', 'rg foo src')
            old = message('old', now - dt.timedelta(hours=25), 'bash', 'cat old')
            extra = message('extra', now, 'agentgrep')
            (root / 'session_test.json').write_text(json.dumps({'id': 'session_test', 'messages': [fresh, old]}))
            (root / 'session_test.journal.jsonl').write_text(json.dumps({'append_messages': [fresh, extra]}) + '\n')
            result = subprocess.run([sys.executable, str(pathlib.Path(__file__).with_name('tool_usage.py')),
                                     '--sessions', str(root)], check=True, capture_output=True, text=True)
            lines = result.stdout.splitlines()
            self.assertIn('Window: last 24 hours; 1 sessions with calls', lines)
            self.assertRegex(result.stdout, r'bash\s+1\s+1')
            self.assertRegex(result.stdout, r'agentgrep\s+1\s+1')
            self.assertRegex(result.stdout, r'rg\s+1')
            self.assertRegex(result.stdout, r'cat\s+0')


if __name__ == '__main__':
    unittest.main()
