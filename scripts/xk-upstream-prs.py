#!/usr/bin/env python3
"""Track upstream pull requests without editing either fork's checkout."""

import argparse
from collections import defaultdict
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


BRANCHES = {"jcode": "master", "jcode-desktop": "main"}
KEY = re.compile(r"(jcode(?:-desktop)?)#([1-9][0-9]*)\Z")
SHA = re.compile(r"[0-9a-f]{40}\Z")
LEDGER = "docs/upstream-pr-decisions.json"
DECISIONS = ("skipped", "already-in-fork", "deferred")
RESOLUTIONS = ("upstream", "fork-superset")


class Error(Exception):
    pass


def run(*args, check=True):
    return subprocess.run(args, check=check, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          stdin=subprocess.DEVNULL, timeout=120)


def git(repo, *args, check=True):
    return run("git", "-C", repo, *args, check=check)


def text(result):
    return result.stdout.decode("utf-8", errors="replace").strip()


def upstream(key):
    return f"1jehuang/{key}"


def parse_key(key):
    match = KEY.fullmatch(key)
    if not match:
        raise Error(f"Expected jcode#NUMBER or jcode-desktop#NUMBER, got {key!r}")
    return match[1], int(match[2])


def key_order(key):
    return parse_key(key)


def fork_ref(repo):
    return f"refs/remotes/origin/{BRANCHES[repo]}"


def upstream_ref(repo):
    return f"refs/remotes/upstream/{BRANCHES[repo]}"


@dataclass(frozen=True)
class PR:
    key: str
    title: str
    body: str
    state: str
    head: str
    updated_at: str
    landed: str | None

    @classmethod
    def from_api(cls, repo, raw):
        key = f"{repo}#{raw['number']}"
        parse_key(key)
        head = raw["head"]["sha"]
        state = raw["state"]
        landed = raw["merge_commit_sha"] if raw["merged_at"] else None
        if (not SHA.fullmatch(head) or state not in ("open", "closed")
                or (raw["merged_at"] and (state != "closed" or not SHA.fullmatch(landed or "")))):
            raise Error(f"Invalid GitHub pull request metadata for {key}")
        return cls(key, raw["title"], raw["body"] or "", state, head, raw["updated_at"], landed)


def gh(*args):
    # Explicit GET: adding a field to `gh api` otherwise changes the method to POST.
    return json.loads(run("gh", "api", "--method", "GET", *args).stdout)


def all_open(repo):
    pages = gh("--paginate", "--slurp", f"repos/1jehuang/{repo}/pulls?state=open&per_page=100")
    prs = [PR.from_api(repo, raw) for page in pages for raw in page]
    if any(pr.state != "open" for pr in prs):
        raise Error(f"GitHub returned a non-open PR in the open list for {repo}")
    return {pr.key: pr for pr in prs}


def one_pr(key):
    repo, number = parse_key(key)
    pr = PR.from_api(repo, gh(f"repos/1jehuang/{repo}/pulls/{number}"))
    if pr.key != key:
        raise Error(f"GitHub returned {pr.key} instead of {key}")
    return pr


def history(path, repo, ref):
    ports, resolved = defaultdict(set), {}
    fmt = ("%H%x00%(trailers:key=Upstream-PR,valueonly,unfold,separator=%x1f)"
           "%x00%(trailers:key=Upstream-PR-Commit,valueonly,unfold,separator=%x1f)"
           "%x00%(trailers:key=Upstream-PR-Reconciled,valueonly,unfold,separator=%x1f)")
    log = text(git(path, "log", ref, "--no-notes", "--grep=^Upstream-PR", "--regexp-ignore-case", f"--format={fmt}"))
    for line in log.split("\n") if log else ():
        commit, port_values, original_values, resolved_values = line.split("\0")
        originals = defaultdict(set)
        for value in original_values.split("\x1f") if original_values else ():
            parts = value.split()
            if (len(parts) != 2 or not SHA.fullmatch(parts[1]) or not ancestor(path, parts[1], f"{commit}^1")):
                raise Error(f"Invalid Upstream-PR-Commit trailer in {commit}: {value!r}")
            originals[parts[0]].add(parts[1])
        for value in port_values.split("\x1f"):
            value = value.strip()
            key = value.removeprefix("1jehuang/")
            if value == upstream(key) and key.startswith(f"{repo}#") and KEY.fullmatch(key):
                ports[key].update(originals.pop(value, {commit}))
        if originals:
            raise Error(f"Upstream-PR-Commit without a matching Upstream-PR trailer in {commit}")
        for value in resolved_values.split("\x1f"):
            parts = value.split()
            if len(parts) == 4 and parts[0].startswith("1jehuang/") and parts[3] in RESOLUTIONS:
                key = parts[0].removeprefix("1jehuang/")
                resolved.setdefault((key, parts[1], parts[2]), parts[3])
    return ports, resolved


def port_fingerprint(commits):
    return hashlib.sha256("\n".join(sorted(commits)).encode()).hexdigest()


def resolution_key(pr, commits):
    return pr.key, pr.landed, port_fingerprint(commits)


def load_ledger(args):
    if args.ledger_ref:
        raw = git(args.forks / "jcode", "show", f"{args.ledger_ref}:{LEDGER}").stdout
    else:
        raw = args.ledger.read_bytes()
    ledger = json.loads(raw)
    if ledger["version"] != 1 or set(ledger["audited_through"]) != set(BRANCHES):
        raise Error("Unsupported upstream PR ledger format")
    for key, row in ledger["decisions"].items():
        parse_key(key)
        if row["decision"] not in DECISIONS or not SHA.fullmatch(row["head"]) or not row["reason"].strip():
            raise Error(f"Invalid ledger decision for {key}")
        if (row["decision"] == "already-in-fork") != bool(row.get("covering")):
            raise Error(f"Invalid covering commits for {key}")
        if any(not SHA.fullmatch(sha) for sha in row.get("covering", [])):
            raise Error(f"Invalid covering commit SHA for {key}")
    return ledger


def atomic_write(path, contents):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as output:
        temp = Path(output.name)
        try:
            output.write(contents)
            output.close()
            temp.replace(path)
        finally:
            temp.unlink(missing_ok=True)


def save_ledger(path, ledger):
    # One diff-friendly line per decision, with Unicode audit prose intact.
    rows = [f"    {json.dumps(key)}: {json.dumps(row, ensure_ascii=False)}"
            for key, row in sorted(ledger["decisions"].items(), key=lambda pair: key_order(pair[0]))]
    atomic_write(path, '{\n  "version": 1,\n  "audited_through": '
                 + json.dumps(ledger["audited_through"]) + ',\n  "decisions": {\n'
                 + ',\n'.join(rows) + '\n  }\n}\n')


def ancestor(path, commit, ref):
    if git(path, "cat-file", "-e", f"{commit}^{{commit}}", check=False).returncode:
        return False
    result = git(path, "merge-base", "--is-ancestor", commit, ref, check=False)
    if result.returncode not in (0, 1):
        result.check_returncode()
    return result.returncode == 0


def covering_shas(args, key, candidates):
    repo, _ = parse_key(key)
    path = args.forks / repo
    commits = []
    for sha in candidates:
        if not re.fullmatch(r"[0-9a-f]{7,40}", sha):
            raise Error(f"Invalid covering SHA {sha!r} for {key}")
        commit = text(git(path, "rev-parse", "--verify", f"{sha}^{{commit}}"))
        if not ancestor(path, commit, fork_ref(repo)):
            raise Error(f"{key}: covering commit {sha} is not in {fork_ref(repo)}")
        commits.append(commit)
    return commits


def record(args, ledger):
    repo, _ = parse_key(args.key)
    ports, _ = history(args.forks / repo, repo, fork_ref(repo))
    if args.key in ports:
        raise Error(f"{args.key} is already ported (from Git trailers)")
    if not args.reason.strip():
        raise Error("A nonempty --reason is required")
    if (args.decision == "already-in-fork") != bool(args.covering):
        raise Error("--covering is required for, and only accepted with, already-in-fork")
    pr = one_pr(args.key)
    if pr.state != "open" or pr.head != args.head:
        raise Error(f"{args.key} is not open at {args.head}; rerun status before recording")
    row = {"decision": args.decision, "head": pr.head, "reason": args.reason.strip()}
    if args.covering:
        row["covering"] = covering_shas(args, args.key, args.covering)
    ledger["decisions"][args.key] = row
    save_ledger(args.ledger, ledger)
    print(f"Recorded {args.key} as {args.decision} at {pr.head}")


def audit_rows(path):
    # The three Nov 22 audit tables use three headers, but share the decision columns.
    columns = None
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in re.split(r"(?<!\\)\|", line)[1:-1]]
        if {"PR", "Status", "Value", "Reason"} <= set(cells):
            columns = cells
        elif columns and len(cells) == len(columns):
            fields = dict(zip(columns, cells))
            number = fields["PR"].removeprefix("#")
            if number.isdigit():
                yield f"{fields.get('Repo', 'jcode')}#{number}", fields


def import_audit(args, ledger):
    if any(ledger["audited_through"].values()):
        print("Imported 0 decisions. Audit snapshot was already imported.")
        return
    holds = set(args.keep_undecided)
    for key in holds:
        parse_key(key)
    added, changed = 0, []
    rows = {}
    for path in sorted(args.audit_dir.glob("audit-[abc].md")):
        for key, fields in audit_rows(path):
            if key in rows:
                raise Error(f"Duplicate audit row for {key}")
            rows[key] = fields
    if not rows:
        raise Error("No audit rows found")
    for repo in BRANCHES:
        snapshot_name = "jcode-prs.json" if repo == "jcode" else "desktop-prs.json"
        snapshot = json.loads((args.audit_dir / snapshot_name).read_bytes())
        by_key = {f"{repo}#{row['number']}": row for row in snapshot}
        missing = set(by_key) - set(rows)
        extra = {key for key in rows if parse_key(key)[0] == repo} - set(by_key)
        if missing or extra:
            raise Error(f"{repo}: audit and snapshot disagree: missing={sorted(missing)} extra={sorted(extra)}")
        ledger["audited_through"][repo] = max(row["number"] for row in snapshot)
        prs = all_open(repo)
        ports, _ = history(args.forks / repo, repo, fork_ref(repo))
        for key, saved in by_key.items():
            if key in holds or key in ports or key in ledger["decisions"]:
                continue
            fields = rows[key]
            state, value = fields["Status"].lower(), fields["Value"].lower()
            candidates = re.findall(r"\b[0-9a-f]{7,40}\b", state)
            kind, covering = "skipped", []
            if state.startswith("already-in-fork") or (state.startswith("already-in-upstream-master")
                    and ("also fork" in state or "is in fork" in fields["Reason"].lower())):
                if not candidates:
                    raise Error(f"{key}: audit says already in fork without a covering commit")
                covering = covering_shas(args, key, candidates)
                kind = "already-in-fork"
            elif not (value == "low" or state.startswith(("stale", "already-in-upstream-master"))
                      or (state.startswith("superseded") and not state.startswith("superseded in part"))):
                continue
            pr = prs.get(key)
            # Today's original gh snapshot recorded updatedAt but not headRefOid.
            # Do not transfer the audit to an intervening update or to a closed PR.
            if not pr or pr.updated_at != saved["updatedAt"] or (
                    saved.get("headRefOid") and pr.head != saved["headRefOid"]):
                changed.append(key)
                continue
            row = {"decision": kind, "head": pr.head, "reason": f"{fields['Status']}: {fields['Reason']}"}
            if covering:
                row["covering"] = covering
            ledger["decisions"][key] = row
            added += 1
    save_ledger(args.ledger, ledger)
    print(f"Imported {added} decisions. Changed or closed since snapshot ({len(changed)}): "
          + (", ".join(sorted(changed, key=key_order)) or "none"))


def md(value, limit):
    flat = " ".join(value.split())
    short = flat[:limit - 1].rstrip() + "…" if len(flat) > limit else flat
    return re.sub(r"([\\`*_\[\]<>~|])", r"\\\1", short)


def tldr(pr):
    fence = None
    for line in pr.body.splitlines():
        prose = re.sub(r"^\s*[-*+]\s+", "", line.strip())
        marker = prose[:3]
        if marker in ("```", "~~~"):
            if fence in (None, marker):
                fence = None if fence else marker
            continue
        if not prose or fence or prose.startswith(("#", "<", "[", "---", "![")):
            continue
        if line.startswith(("    ", "\t")) or prose.strip("*_ :").lower() in ("summary", "motivation", "changes"):
            continue
        if re.match(r"^(?:closes?|fixes|resolves|refs?)\s+#\d+", prose, re.I):
            continue
        return md(prose, 124)
    return ""


def pr_line(pr, extra=""):
    repo, number = parse_key(pr.key)
    link = f"[{pr.key}](https://github.com/1jehuang/{repo}/pull/{number})"
    return f"- {link} `{pr.head}` {md(pr.title, 125)}{extra}"


def status(args, ledger):
    groups = {section: [] for section in ("New since audit", "Updated since decision",
                                         "Undecided backlog", "Ported, merged upstream",
                                         "Ported, closed without merge")}
    repo_counts = []
    for repo in args.repo or BRANCHES:
        ports, resolved = history(args.forks / repo, repo, fork_ref(repo))
        opened = all_open(repo)
        hidden = 0
        for key, pr in sorted(opened.items(), key=lambda pair: key_order(pair[0]), reverse=True):
            decision = ledger["decisions"].get(key)
            if key in ports or (decision and decision["head"] == pr.head):
                hidden += 1
                continue
            detail = tldr(pr)
            extra = f". {detail}" if detail else ""
            if decision:
                section = "Updated since decision"
                extra += f" (was {decision['decision']} at `{decision['head'][:12]}`)"
            else:
                section = ("New since audit" if parse_key(key)[1] > ledger["audited_through"][repo]
                           else "Undecided backlog")
            groups[section].append(pr_line(pr, extra))
        for key in sorted(ports.keys() - opened.keys(), key=key_order):
            pr = one_pr(key)
            if pr.state == "open":  # PR reopened between the list and detail requests.
                continue
            commits = ports[key]
            detail = " · fork " + ", ".join(f"`{sha[:12]}`" for sha in sorted(commits))
            if pr.landed:
                section = "Ported, merged upstream"
                choice = resolved.get(resolution_key(pr, commits), "pending in sync")
                detail += f" · upstream `{pr.landed[:12]}` · {choice}"
            else:
                section = "Ported, closed without merge"
            groups[section].append(pr_line(pr, detail))
        repo_counts.append(f"{repo}: {len(opened)} open, {hidden} hidden (same decision head or ported)")
    output = "# Upstream PR status\n\n" + "\n".join(repo_counts) + "\n"
    for section, lines in groups.items():
        if args.brief and section == "Undecided backlog":
            output += f"\n## {section} ({len(lines)})\n\nOmitted from sync TLDR. Run full status to review.\n"
            continue
        output += f"\n## {section} ({len(lines)})\n\n" + ("\n".join(lines) if lines else "None.") + "\n"
    print(output, end="")
    if args.report and (not args.report.exists() or args.report.read_text(encoding="utf-8") != output):
        atomic_write(args.report, output)
        if args.notify:
            subprocess.run(["/usr/bin/osascript", "-e", 'on run argv', '-e',
                            'display notification (item 1 of argv) with title "Upstream PR status"',
                            '-e', 'end run',
                            f"{len(groups['New since audit'])} new, {len(groups['Updated since decision'])} updated. {args.report}"],
                           check=True, stdin=subprocess.DEVNULL, timeout=30)


def patch(path, commit):
    parents = text(git(path, "rev-list", "--parents", "-n", "1", commit)).split()
    if len(parents) < 2:
        return b""
    return git(path, "diff", "--binary", "--full-index", "--no-color", "--no-ext-diff", "--no-textconv",
               "--no-renames", "--src-prefix=a/", "--dst-prefix=b/", parents[1], commit, "--").stdout


def exact_upstream(path, pr, ports, upstream_tip):
    if len(ports) != 1:
        return False
    port = next(iter(ports))
    change = patch(path, port)
    if not change or change != patch(path, pr.landed):
        return False
    names = git(path, "diff", "--name-only", "-z", "--no-renames", f"{port}^1", port, "--").stdout
    paths = [os.fsdecode(name) for name in names.split(b"\0") if name]
    result = git(path, "--literal-pathspecs", "diff", "--quiet", "--no-ext-diff", "--no-textconv",
                 upstream_tip, "HEAD", "--", *paths, check=False)
    if result.returncode not in (0, 1):
        result.check_returncode()
    # Every touched path must already match current upstream byte for byte.
    # This checks the actual merge result, not just similarity of two patches.
    return result.returncode == 0


def reconcile(args):
    path, repo = args.worktree, args.repo_name
    ports, resolved = history(path, repo, "HEAD" if not args.ref else args.ref)
    target = args.upstream_ref or upstream_ref(repo)
    picked = args.choose
    if picked and (picked[0] not in ports or picked[1] not in RESOLUTIONS or not args.reason.strip()):
        raise Error("--choose needs a ported PR, upstream or fork-superset, and a nonempty --reason")
    if not args.check:
        if text(git(path, "status", "--porcelain")) or git(path, "rev-parse", "-q", "--verify", "MERGE_HEAD",
                                                                  check=False).returncode == 0:
            raise Error(f"Finish the merge and commit edits before recording a reconciliation in {path}")
    pending, outcomes = [], []
    for key, commits in sorted(ports.items(), key=lambda pair: key_order(pair[0])):
        if picked and picked[0] != key:
            continue
        pr = one_pr(key)
        if not pr.landed or not ancestor(path, pr.landed, target):
            continue
        ref = args.ref or "HEAD"
        if not ancestor(path, pr.landed, ref):
            continue  # Not part of the candidate yet, e.g. origin before the upstream merge.
        identity = resolution_key(pr, commits)
        if identity in resolved:
            continue
        if picked:
            outcomes.append((identity, picked[1]))
        elif not args.check and exact_upstream(path, pr, commits, target):
            outcomes.append((identity, "upstream"))
        else:
            pending.append((pr, commits))
    if picked and not outcomes:
        raise Error(f"{picked[0]} is not awaiting reconciliation at this upstream and fork tip")
    if outcomes:
        message = ["chore(upstream): record port reconciliation", "",
                   args.reason if picked else "Exact landed patch and identical affected paths at upstream tip.", ""]
        for (key, landed, fingerprint), choice in outcomes:
            message.append(f"Upstream-PR-Reconciled: {upstream(key)} {landed} {fingerprint} {choice}")
        git(path, "commit", "--allow-empty", "-m", "\n".join(message))
        for (key, landed, _), choice in outcomes:
            print(f"{key}: kept {choice} (upstream {landed})")
    for pr, commits in pending:
        print(f"{pr.key}: review before push in {path}; upstream {pr.landed}; fork {', '.join(sorted(commits))}")
    return 3 if pending else 0


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--forks", type=Path, default=Path(os.environ.get(
        "XK_FORKS_DIR", Path.home() / "Projects/github.com/xenking")))
    p.add_argument("--ledger", type=Path, default=Path(__file__).resolve().parent.parent / LEDGER)
    p.add_argument("--ledger-ref", help="read the ledger from this Git ref instead of the checkout")
    commands = p.add_subparsers(dest="command", required=True)
    s = commands.add_parser("status")
    s.add_argument("--repo", choices=BRANCHES, action="append")
    s.add_argument("--brief", action="store_true", help="summarize the audited backlog without listing every PR")
    s.add_argument("--report", type=Path, help="atomically publish the Markdown report if changed")
    s.add_argument("--notify", action="store_true", help="notify via macOS when --report changes")
    r = commands.add_parser("record")
    r.add_argument("key")
    r.add_argument("decision", choices=DECISIONS)
    r.add_argument("--head", required=True, help="full SHA of the PR head you reviewed")
    r.add_argument("--reason", required=True)
    r.add_argument("--covering", action="append", default=[], help="fork commit covering the PR")
    i = commands.add_parser("import-audit")
    i.add_argument("audit_dir", type=Path)
    i.add_argument("--keep-undecided", action="append", default=[], help="exclude a selected PR from import")
    c = commands.add_parser("reconcile")
    c.add_argument("repo_name", choices=BRANCHES)
    c.add_argument("worktree", type=Path)
    c.add_argument("--upstream-ref", help="upstream candidate, defaults to its remote-tracking ref")
    c.add_argument("--ref", help="read an explicit candidate ref with --check")
    mode = c.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="read only, exit 3 if any PR needs reconciliation")
    mode.add_argument("--choose", nargs=2, metavar=("PR", "VERSION"), help="record the reviewed result")
    c.add_argument("--reason", default="", help="why the chosen tree keeps exactly one implementation")
    args = p.parse_args()
    if args.command == "reconcile":
        if args.ref and not args.check:
            p.error("--ref requires --check")
        return reconcile(args)
    if args.ledger_ref and args.command != "status":
        p.error("--ledger-ref is read-only")
    if args.command == "status" and args.notify and not args.report:
        p.error("--notify requires --report")
    handlers = {"status": status, "record": record, "import-audit": import_audit}
    handlers[args.command](args, load_ledger(args))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr.decode(errors="replace").strip() if exc.stderr else ""
        print(f"upstream-prs: {exc}. {detail}", file=sys.stderr)
        sys.exit(1)
    except (Error, OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as exc:
        print(f"upstream-prs: {exc}", file=sys.stderr)
        sys.exit(1)
