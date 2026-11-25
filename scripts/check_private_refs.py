#!/usr/bin/env python3
"""Tracked files must not name private harnesses, repos or tooling."""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]

# The words are assembled from fragments so this gate does not trip itself.
WORDS = {
    "private harness": ("o" + "mp", "oh" + "-my-" + "pi", "oh" + "my" + "pi", "pi-o-" + "my"),
    "private tool": ("x" + "dev", "pi-" + "natives", "can" + "1357"),
}
BOUNDARY = r"(?<![A-Za-z0-9])(?:{})(?![A-Za-z0-9])"
PATTERN = re.compile(
    BOUNDARY.format("|".join(re.escape(w) for words in WORDS.values() for w in words)),
    re.IGNORECASE,
)

# path -> reason. Add an entry only for a compatibility alias that must keep an
# old serialized name loadable, and say why next to it.
ALLOWLIST: dict[str, str] = {}


def tracked_files(root: Path) -> list[Path]:
    listing = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"], capture_output=True, check=False
    )
    if listing.returncode == 0:
        names = [n for n in listing.stdout.decode("utf-8", "replace").split("\0") if n]
    else:
        names = [str(p.relative_to(root)) for p in root.rglob("*") if p.is_file() and ".git" not in p.parts]
    return [root / n for n in sorted(names)]


def violations(root: Path) -> list[str]:
    errors = []
    for path in tracked_files(root):
        relative = path.relative_to(root).as_posix()
        if relative in ALLOWLIST:
            continue
        if PATTERN.search(relative):
            errors.append(f"{relative}: file name names a private reference; rename it")
        if not path.is_file() or path.is_symlink():
            continue
        try:
            text = path.read_bytes().decode("utf-8")
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            match = PATTERN.search(line)
            if match:
                errors.append(
                    f"{relative}:{number}: `{match.group(0)}` names a private reference; "
                    "describe the behaviour in general terms instead"
                )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="repository to check")
    errors = violations(parser.parse_args().root.resolve())
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if errors:
        return 1
    print("no private references in tracked files: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
