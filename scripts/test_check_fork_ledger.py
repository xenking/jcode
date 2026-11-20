#!/usr/bin/env python3
"""Exercise the checker with two disposable Git repositories."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_fork_ledger import HEADER

SCRIPT = Path(__file__).resolve().with_name("check_fork_ledger.py")


class ForkLedgerTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name)
        self.cli = root / "cli"
        self.desktop = root / "desktop"
        self.ledger = root / "ledger.md"
        self.cli_sha, self.lost_sha = self.repository(self.cli, "master")
        self.desktop_sha, _ = self.repository(self.desktop, "main")

    def repository(self, path, branch):
        path.mkdir()

        def git(*args):
            return subprocess.check_output(
                ["git", "-C", str(path), "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                 "-c", "commit.gpgsign=false", *args],
                text=True,
            ).strip()

        git("init", "-q")
        git("commit", "--allow-empty", "-qm", f"{branch} ancestor")
        ancestor = git("rev-parse", "--short=12", "HEAD")
        git("update-ref", f"refs/remotes/origin/{branch}", "HEAD")
        git("commit", "--allow-empty", "-qm", f"{branch} local only")
        return ancestor, git("rev-parse", "--short=12", "HEAD")

    def check(self, cli_sha, desktop_sha):
        self.ledger.write_text(
            f"{HEADER}\n| --- | --- | --- | --- | --- | --- |\n"
            f"| jcode | CLI feature | `{cli_sha}` | a | test | origin |\n"
            f"| jcode-desktop | Desktop feature | `{desktop_sha}` | b | test | origin |\n",
            encoding="utf-8",
        )
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--jcode", str(self.cli), "--desktop", str(self.desktop),
             "--ledger", str(self.ledger)],
            capture_output=True,
            text=True,
        )

    def test_checks_both_origin_branches(self):
        result = self.check(self.cli_sha, self.desktop_sha)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 commit references verified", result.stdout)

    def test_lists_lost_commits_even_if_some_exist_locally(self):
        result = self.check(self.lost_sha, "123abcdef00")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"{self.lost_sha} (CLI feature) is not an ancestor of origin/master", result.stderr)
        self.assertIn("123abcdef00 (Desktop feature) missing or ambiguous under origin/main", result.stderr)

    def test_does_not_accept_sha_from_wrong_repository(self):
        result = self.check(self.desktop_sha, self.cli_sha)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"{self.desktop_sha} (CLI feature)", result.stderr)
        self.assertIn(f"{self.cli_sha} (Desktop feature)", result.stderr)

    def test_rejects_invalid_sha_cell(self):
        result = self.check("nonsense", self.desktop_sha)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("ledger line 3", result.stderr)


if __name__ == "__main__":
    unittest.main()
