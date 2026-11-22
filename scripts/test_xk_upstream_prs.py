#!/usr/bin/env python3
"""Offline CLI and sync tests. The gh stub never accesses the network."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
FIXTURES = SCRIPTS / "fixtures/upstream-prs"
LEDGER = "docs/upstream-pr-decisions.json"

GH_STUB = """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import re
import sys

assert sys.argv[1:4] == ['api', '--method', 'GET'], sys.argv
args = sys.argv[4:]
root = Path(os.environ['PR_FIXTURES'])
if args[:2] == ['--paginate', '--slurp']:
    repo = re.fullmatch(r'repos/1jehuang/(jcode(?:-desktop)?)/pulls\\?state=open&per_page=100', args[2])[1]
    result = json.loads((root / f'{repo}-open.json').read_bytes())
else:
    repo, number = re.fullmatch(r'repos/1jehuang/(jcode(?:-desktop)?)/pulls/([0-9]+)', args[0]).groups()
    path = root / f'{repo}-{number}.json'
    if path.exists():
        result = json.loads(path.read_bytes())
    else:
        pages = json.loads((root / f'{repo}-open.json').read_bytes())
        result = next(row for page in pages for row in page if str(row['number']) == number)
print(json.dumps(result))
"""


class Workflow(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.temp = Path(temp.name)
        self.forks = self.temp / "forks"
        self.fixtures = self.temp / "fixtures"
        shutil.copytree(FIXTURES, self.fixtures)
        self.bin = self.temp / "bin"
        self.bin.mkdir()
        fake = self.bin / "gh"
        fake.write_text(GH_STUB)
        fake.chmod(0o700)
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        PR_FIXTURES=str(self.fixtures), XK_FORKS_DIR=str(self.forks),
                        XK_SYNC_DIR=str(self.temp / "worktrees"), GIT_CONFIG_NOSYSTEM="1",
                        GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
        self.ledger = self.temp / "decisions.json"
        self.ledger.write_text(json.dumps({"version": 1,
                                          "audited_through": {"jcode": 2273, "jcode-desktop": 29},
                                          "decisions": {}}))
        self.fork = self.init_repo("jcode", "master")
        self.desktop = self.init_repo("jcode-desktop", "main")

    def init_repo(self, repo, branch):
        path = self.forks / repo
        path.mkdir(parents=True)
        self.git(path, "init", "-b", branch)
        self.git(path, "config", "user.name", "Fixture User")
        self.git(path, "config", "user.email", "fixture@example.invalid")
        self.git(path, "config", "commit.gpgsign", "false")
        (path / "original").write_text("original\n")
        self.commit(path, "base")
        self.git(path, "update-ref", f"refs/remotes/origin/{branch}", "HEAD")
        return path

    def run_cli(self, args, expected=0):
        proc = subprocess.run(args, env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(proc.returncode, expected, f"{args}\n{proc.stdout}\n{proc.stderr}")
        return proc.stdout

    def git(self, path, *args):
        return self.run_cli(["git", "-C", str(path), *args]).strip()

    def cli(self, *args, expected=0):
        return self.run_cli([sys.executable, str(SCRIPTS / "xk-upstream-prs.py"),
                             "--forks", str(self.forks), "--ledger", str(self.ledger), *args], expected)

    def commit(self, path, message):
        self.git(path, "add", "--all")
        self.git(path, "commit", "--allow-empty", "-m", message)
        return self.git(path, "rev-parse", "HEAD")

    def port(self, number, message=None):
        return self.commit(self.fork, (message or "local port") + f"\n\nUpstream-PR: 1jehuang/jcode#{number}")

    def test_status_groups_and_last_good_report(self):
        self.port(1788)
        self.port(1789)
        self.port(2271)
        self.git(self.fork, "update-ref", "refs/remotes/origin/master", "HEAD")
        ledger = json.loads(self.ledger.read_bytes())
        ledger["decisions"] = {"jcode#2273": {"decision": "skipped", "head": "a" * 40, "reason": "old head"}}
        self.ledger.write_text(json.dumps(ledger))
        report = self.temp / "status.md"
        result = self.cli("status", "--report", str(report))
        self.assertEqual(report.read_text(), result)
        self.assertIn("New since audit (1)\n\n- [jcode#2274]", result)
        self.assertIn("Updated since decision (1)\n\n- [jcode#2273]", result)
        self.assertIn("Ported, merged upstream (1)\n\n- [jcode#1789]", result)
        self.assertIn("Ported, closed without merge (1)\n\n- [jcode#1788]", result)
        self.assertIn("[jcode-desktop#29]", result)
        self.assertNotIn("[jcode#2271]", result)
        self.assertIn("pending in sync", result)
        self.assertIn("01021d63c73e16b3c8f6353336c7392a4f5a56c3", result)
        self.assertIn("macos: detect kitty terminal", result)
        mt = report.stat().st_mtime_ns
        self.cli("status", "--report", str(report))
        self.assertEqual(report.stat().st_mtime_ns, mt)
        (self.fixtures / "jcode-open.json").write_text("invalid JSON")
        self.cli("status", "--report", str(report), expected=1)
        self.assertEqual(report.read_text(), result)

    def test_record_refuses_stale_head_and_requires_ancestral_covering_commit(self):
        sha = "01021d63c73e16b3c8f6353336c7392a4f5a56c3"
        before = self.ledger.read_bytes()
        self.cli("record", "jcode#2274", "deferred", "--head", "d" * 40, "--reason", "busy", expected=1)
        unpublished = self.commit(self.fork, "not in origin yet")
        args = ("record", "jcode#2274", "already-in-fork", "--head", sha, "--reason", "same behavior")
        self.cli(*args, "--covering", unpublished, expected=1)
        self.assertEqual(self.ledger.read_bytes(), before)
        self.git(self.fork, "update-ref", "refs/remotes/origin/master", "HEAD")
        self.cli(*args, "--covering", unpublished[:9])
        self.assertNotIn("[jcode#2274]", self.cli("status"))
        self.assertEqual(json.loads(self.ledger.read_bytes())["decisions"]["jcode#2274"]["covering"], [unpublished])

    def test_import_preserves_selected_and_changed_heads(self):
        self.ledger.write_text(json.dumps({"version": 1,
                                          "audited_through": {"jcode": 0, "jcode-desktop": 0},
                                          "decisions": {}}))
        audit = self.temp / "audit"
        audit.mkdir()
        snapshot_names = {"jcode": "jcode-prs.json", "jcode-desktop": "desktop-prs.json"}
        for repo, name in snapshot_names.items():
            data = json.loads((self.fixtures / f"{repo}-open.json").read_bytes())
            (audit / name).write_text(json.dumps([
                {"number": row["number"], "updatedAt": (row["updated_at"] if row["number"] != 2271 else "old")}
                for page in data for row in page]))
        (audit / "audit-a.md").write_text(
            "| PR | Title | What it does | Kind | Status | Value | Reason | Size / areas |\n"
            "| 2274 | selected | x | feature | stale/conflicting | high | worth porting | 1 |\n"
            "| 2273 | low | x | feature | open-and-relevant | low | out of scope | 1 |\n"
            "| 2271 | changed | x | feature | stale/conflicting | low | looked stale | 1 |\n")
        (audit / "audit-c.md").write_text(
            "| Repo | PR | What it does / title | Kind | Status | Value | Reason | Size and touched areas |\n"
            "| jcode-desktop | 29 | backlog | ui | open-and-relevant | med | worth considering | 1 |\n")
        args = ("import-audit", str(audit), "--keep-undecided", "jcode#2274")
        summary = self.cli(*args)
        self.assertIn("Imported 1 decisions", summary)
        self.assertIn("jcode#2271", summary)
        result = json.loads(self.ledger.read_bytes())
        self.assertEqual(set(result["decisions"]), {"jcode#2273"})
        self.assertEqual(result["audited_through"], {"jcode": 2274, "jcode-desktop": 29})
        self.assertIn("Imported 0 decisions", self.cli(*args))

    def setup_merge(self, fork_superset):
        # Local remotes use the real sync script's outbound origin allowlist.
        origin = self.temp / "github.com/xenking/jcode.git"
        upstream = self.temp / "upstream.git"
        origin.parent.mkdir(parents=True)
        for remote in (origin, upstream):
            self.git(self.fork, "clone", "--bare", ".", str(remote))
        self.git(self.fork, "remote", "add", "origin", str(origin))
        self.git(self.fork, "remote", "add", "upstream", str(upstream))
        self.git(self.fork, "switch", "-c", "landing")
        (self.fork / "feature").write_text("one implementation\n")
        landed = self.commit(self.fork, "upstream lands the feature")
        self.git(self.fork, "push", "upstream", "HEAD:master")
        detail = self.fixtures / "jcode-1789.json"
        metadata = json.loads(detail.read_bytes())
        # Git commits created by this test have different IDs from GitHub's.
        metadata["merge_commit_sha"] = landed
        detail.write_text(json.dumps(metadata))
        self.git(self.fork, "switch", "master")
        (self.fork / "feature").write_text("one implementation\n" + ("fork extras\n" if fork_superset else ""))
        port = self.port(1789)
        (self.fork / "scripts").mkdir()
        (self.fork / "docs").mkdir()
        shutil.copyfile(SCRIPTS / "xk-upstream-prs.py", self.fork / "scripts/xk-upstream-prs.py")
        shutil.copyfile(self.ledger, self.fork / LEDGER)
        (self.fork / "scripts/check_fork_removals.py").write_text("raise SystemExit(0)\n")
        self.commit(self.fork, "publish sync policy")
        self.git(self.fork, "push", "origin", "HEAD:master")
        return landed, port, self.temp / "worktrees/jcode"

    def sync(self, *args, expected=0, push=False):
        push_args = [] if push else ["--no-push"]
        return self.run_cli(["bash", str(SCRIPTS / "xk-upstream-sync.sh"),
                             "--repo", "jcode", "--skip-checks", *push_args, *args], expected)

    def test_sync_exact_port_reconciles_once_without_rewriting_history(self):
        landed, port, worktree = self.setup_merge(False)
        open_fixture = self.fixtures / "jcode-open.json"
        current_pages = open_fixture.read_bytes()
        pages = json.loads(current_pages)
        before_merge = json.loads((self.fixtures / "jcode-1789.json").read_bytes())
        pages[-1].append(dict(before_merge, state="open", merged_at=None))
        open_fixture.write_text(json.dumps(pages))
        self.assertNotIn("[jcode#1789]", self.cli("status"))
        open_fixture.write_bytes(current_pages)
        report = self.sync()
        self.assertIn("Ported, merged upstream (1)", report)
        self.assertIn(f"jcode#1789: kept upstream (upstream {landed})", report)
        self.assertEqual((worktree / "feature").read_text(), "one implementation\n")
        self.git(worktree, "merge-base", "--is-ancestor", port, "HEAD")
        self.git(worktree, "merge-base", "--is-ancestor", landed, "HEAD")
        head = self.git(worktree, "rev-parse", "HEAD")
        self.sync("--continue")
        self.assertEqual(self.git(worktree, "rev-parse", "HEAD"), head)
        self.git(worktree, "update-ref", "refs/remotes/origin/master", "HEAD")
        self.assertIn("· upstream", self.cli("status"))
        self.assertNotIn("pending in sync", self.cli("status"))

    def test_late_attribution_preserves_the_actual_port_sha(self):
        original = self.commit(self.fork, "Port https://github.com/1jehuang/jcode/pull/1789.")
        self.git(self.fork, "update-ref", "refs/remotes/origin/master", "HEAD")
        self.assertNotIn("[jcode#1789]", self.cli("status"))
        self.commit(self.fork, "record original port attribution\n\nUpstream-PR: 1jehuang/jcode#1789"
                    f"\nUpstream-PR-Commit: 1jehuang/jcode#1789 {original}")
        self.git(self.fork, "update-ref", "refs/remotes/origin/master", "HEAD")
        result = self.cli("status")
        self.assertIn("[jcode#1789]", result)
        self.assertIn(f"fork `{original[:12]}`", result)

    def test_late_origin_port_invalidates_earlier_reconciliation_before_push(self):
        _, _, worktree = self.setup_merge(False)
        self.assertIn("kept upstream", self.sync())
        (self.fork / "late-port").write_text("additional fork behavior\n")
        late = self.port(1789)
        self.git(self.fork, "push", "origin", "HEAD:master")
        result = self.sync("--continue", expected=3, push=True)
        self.assertIn("review before push", result)
        self.assertEqual(self.git(worktree, "rev-parse", "origin/master"), late)
        self.assertEqual(self.git(worktree, "ls-remote", "origin", "refs/heads/master").split()[0], late)
        self.assertEqual((worktree / "late-port").read_text(), "additional fork behavior\n")

    def test_sync_conflict_and_variant_cannot_ship_without_review(self):
        _, _, worktree = self.setup_merge(True)
        self.assertIn("CONFLICT (add/add)", self.sync(expected=3))
        (worktree / "feature").write_text("one implementation\nfork extras\n")
        self.git(worktree, "add", "feature")
        self.assertIn("review before push", self.sync("--continue", expected=3))
        self.cli("reconcile", "jcode", str(worktree), "--choose", "jcode#1789", "fork-superset",
                 "--reason", "one implementation plus fork-only extras")
        self.sync("--continue")
        self.assertEqual((worktree / "feature").read_text(), "one implementation\nfork extras\n")
        self.assertIn("fork-superset", self.git(worktree, "log", "-1", "--format=%B"))


if __name__ == "__main__":
    unittest.main()
