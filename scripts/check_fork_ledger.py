#!/usr/bin/env python3
"""Check the fork ledger against the local origin remote-tracking refs."""

import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
HEADER = "| Репозиторий | Изменение | Коммиты | Основные места | Проверка | Статус |"
REFS = {"jcode": "refs/remotes/origin/master", "jcode-desktop": "refs/remotes/origin/main"}
SHA_CELL = re.compile(r"`([0-9a-f]{7,40})`(?:, `([0-9a-f]{7,40})`)*")


def entries(text):
    active = False
    seen = set()
    for lineno, line in enumerate(text.splitlines(), 1):
        if line == HEADER:
            active = True
            continue
        if not line.startswith("|"):
            active = False
            continue
        if not active or line.startswith("| ---"):
            continue
        parts = [part.strip() for part in re.split(r"(?<!\\)\|", line)[1:-1]]
        if len(parts) != 6 or parts[0] not in REFS or not SHA_CELL.fullmatch(parts[2]):
            raise ValueError(f"ledger line {lineno}: expected a repository and comma-separated `SHA`s")
        repo, feature, shas = parts[:3]
        seen.add(repo)
        for sha in re.findall(r"`([0-9a-f]+)`", shas):
            yield repo, feature, sha
    if missing := REFS.keys() - seen:
        raise ValueError(f"ledger has no commit rows for: {', '.join(sorted(missing))}")


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode", type=Path, default=ROOT, help="CLI checkout (default: script's checkout)")
    parser.add_argument("--desktop", type=Path, help="Desktop checkout (default: sibling of --jcode)")
    parser.add_argument("--ledger", type=Path, default=ROOT / "docs/FORK_LEDGER.md")
    args = parser.parse_args()
    paths = {"jcode": args.jcode, "jcode-desktop": args.desktop or args.jcode.parent / "jcode-desktop"}

    try:
        commits = list(entries(args.ledger.read_text(encoding="utf-8")))
    except (OSError, ValueError) as exc:
        print(f"Fork ledger: {exc}", file=sys.stderr)
        return 2

    tips = {}
    problems = []
    for name, ref in REFS.items():
        result = git(paths[name], "rev-parse", "--verify", f"{ref}^{{commit}}")
        if result.returncode:
            problems.append(f"{name}: cannot read {ref} in {paths[name]} (check path and fetch origin)")
        else:
            tips[name] = result.stdout.strip()

    for name, feature, sha in commits:
        if name not in tips:
            continue
        repo = paths[name]
        ref = REFS[name].removeprefix("refs/remotes/")
        resolved = git(repo, "rev-parse", "--verify", f"{sha}^{{commit}}")
        if resolved.returncode:
            problems.append(f"{name}: {sha} ({feature}) missing or ambiguous under {ref}")
            continue
        result = git(repo, "merge-base", "--is-ancestor", resolved.stdout.strip(), tips[name])
        if result.returncode == 1:
            problems.append(f"{name}: {sha} ({feature}) is not an ancestor of {ref}")
        elif result.returncode:
            problems.append(f"{name}: {sha} ({feature}) could not be checked: {result.stderr.strip()}")

    if problems:
        print("Fork ledger check failed:\n  " + "\n  ".join(problems), file=sys.stderr)
        return 1
    print(f"Fork ledger: {len(commits)} commit references verified against origin/master and origin/main")
    return 0


if __name__ == "__main__":
    sys.exit(main())
