"""Source support for zero-budget architecture gates, reusing the safety lexer."""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

from rust_safety_scan import production_lines, test_module_files

ROOT = Path(__file__).resolve().parents[1]


def rust_sources(directory: Path) -> list[Path]:
    if not directory.is_dir():
        raise FileNotFoundError(f"{directory}: restore the source directory or update its architecture gate")
    files = [
        p for p in sorted(directory.rglob("*.rs"))
        if not any(re.search(r"(?:^tests?$|_tests?$|^tests_)", part) for part in p.relative_to(directory).with_suffix("").parts)
    ]
    test_only = test_module_files(files, production_lines)
    return [p for p in files if p.resolve() not in test_only]


def production_code(path: Path) -> str:
    # Same length/line offsets as the source, except an irrelevant final newline.
    return "\n".join(production_lines(path))


def function_bodies(code: str):
    """Yield Rust function name, body start/end in already lexically masked code."""
    for match in re.finditer(r"\bfn\s+([A-Za-z_]\w*)\s*(?:<[^{};]*>)?\s*\(", code):
        opening = re.search(r"[;{]", code[match.end():])
        if opening is None or opening.group() == ";":
            continue
        start = match.end() + opening.start()
        end, depth = start + 1, 1
        while end < len(code) and depth:
            depth += (code[end] == "{") - (code[end] == "}")
            end += 1
        if depth:
            raise ValueError(f"unbalanced Rust function {match[1]}; fix its syntax before running the gate")
        yield match[1], start + 1, end - 1


def diagnostic(root: Path, path: Path, code: str, offset: int, fix: str) -> str:
    return f"{path.relative_to(root)}:{code.count(chr(10), 0, offset) + 1}: {fix}"


def run(description, check):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--root", type=Path, default=ROOT, help="source tree to check, including a git archive")
    args = parser.parse_args()
    try:
        errors = check(args.root.resolve())
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if errors:
        return 1
    print(f"{description}: OK")
    return 0
