#!/usr/bin/env python3
"""Keep test-home and hook isolation inseparable, including downstream test targets.

06a64d9d8 fixed real-home writes and 829c6a55f fixed machine hooks executing
inside tests. Both were opt-in mistakes. A test-support dependency now selects
both protections. This checks Cargo's parsed feature graph, not TOML spelling.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
BASE = "jcode-base"
STORAGE = "jcode-storage"
ISOLATION = {(BASE, "test-support"), (BASE, "test-isolated-hooks"), (STORAGE, "test-support")}


def feature_closure(packages: dict, name: str, features: list[str]) -> set[tuple[str, str]]:
    pending = [(name, feature) for feature in features]
    reached = set()
    while pending:
        package, feature = pending.pop()
        if (package, feature) in reached or package not in packages:
            continue
        reached.add((package, feature))
        data = packages[package]
        dependencies = {d.get("rename") or d["name"]: d["name"] for d in data["dependencies"]}
        for child in data.get("features", {}).get(feature, []):
            if child.startswith("dep:"):
                continue
            if "/" in child:
                dependency, child = child.split("/", 1)
                dependency = dependencies.get(dependency.rstrip("?"))
                if dependency:
                    pending.append((dependency, child))
            else:
                pending.append((package, child))
    return reached


def violations(metadata: dict) -> list[str]:
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    errors = []
    base = packages.get(BASE)
    if base is None:
        return ["Cargo.toml: jcode-base is missing; restore the test isolation owner"]
    isolated = feature_closure(packages, BASE, ["test-support"])
    if (BASE, "test-isolated-hooks") not in isolated:
        errors.append(
            "crates/jcode-base/Cargo.toml: test-support must enable test-isolated-hooks; "
            'set test-support = ["test-isolated-hooks"] so test homes and hooks cannot diverge'
        )
    if (STORAGE, "test-support") not in isolated:
        errors.append(
            "crates/jcode-base/Cargo.toml: test-support must enable jcode-storage/test-support; "
            "storage-level callers (active_pids, sessions) resolve the real ~/.jcode otherwise "
            'and leak phantom sessions: set test-support = [..., "jcode-storage/test-support"]'
        )
    owners = {
        name for name in packages
        if (BASE, "test-support") in feature_closure(packages, name, ["test-support"])
    }
    for name, package in sorted(packages.items()):
        location = package["manifest_path"]
        if ISOLATION & feature_closure(packages, name, ["default"]):
            errors.append(f"{location}: default enables test isolation in production; move test-support to dev-dependencies")
        dependencies = package["dependencies"]
        for dependency in dependencies:
            target = dependency["name"]
            enabled = feature_closure(packages, target, dependency["features"])
            support = (BASE, "test-support") in enabled
            if dependency["kind"] != "dev" and ISOLATION & enabled:
                errors.append(f"{location}: {target} enables test isolation in a production dependency; move that feature to dev-dependencies")
            if target not in owners:
                continue
            if dependency["kind"] == "dev":
                if not support:
                    errors.append(f'{location}: dev-dependency {target} is not isolated; add features = ["test-support"]')
            elif dependency["kind"] is None and not dependency["optional"]:
                # Cargo unifies dev features only for the package under test.
                # A workspace-wide test can hide a missing package-local opt-in.
                guarded = any(
                    d["name"] == target and d["kind"] == "dev" and d.get("target") is None
                    and (BASE, "test-support") in feature_closure(packages, target, d["features"])
                    for d in dependencies
                )
                if not guarded:
                    errors.append(
                        f'{location}: tests can inherit the real {target} home/hooks; '
                        f'add an unconditional [dev-dependencies] {target} with features = ["test-support"] '
                        'and default-features = false'
                    )
    return errors


STORAGE_LIB = "crates/jcode-storage/src/lib.rs"


def runtime_dir_violations(source: str) -> list[str]:
    """`runtime_dir()` is the real per-user dir that the daemon's sockets share.

    Tests that forgot JCODE_RUNTIME_DIR left hundreds of ledger/state files there,
    so test and test-support builds must resolve it to an isolated dir first,
    before $XDG_RUNTIME_DIR or the Darwin temp dir are consulted.
    """
    body = re.search(r"pub fn user_runtime_dir\(\) -> PathBuf \{(.*?)\n\}", source, re.S)
    if body is None:
        return [f"{STORAGE_LIB}: user_runtime_dir is missing; restore the runtime-dir isolation owner"]
    body = body.group(1)
    guard = body.find("test_runtime_dir()")
    real = [i for i in (body.find("XDG_RUNTIME_DIR"), body.find("darwin_user_temp_dir")) if i >= 0]
    errors = []
    if guard < 0 or (real and min(real) < guard):
        errors.append(
            f"{STORAGE_LIB}: user_runtime_dir must return test_runtime_dir() before resolving the real "
            "per-user runtime dir, or tests write into the daemon's runtime directory"
        )
    if not re.search(r'cfg\(any\(test, feature = "test-support"\)\)\]\s*fn test_runtime_dir\(\) -> Option<PathBuf> \{\s*Some\(',
                     source):
        errors.append(f"{STORAGE_LIB}: test and test-support builds must resolve test_runtime_dir() to Some(isolated dir)")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="source tree to check (also accepts a git archive)")
    args = parser.parse_args()
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=args.root, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if result.returncode:
        print(result.stderr, file=sys.stderr)
        return result.returncode
    errors = violations(json.loads(result.stdout))
    errors += runtime_dir_violations((args.root / STORAGE_LIB).read_text())
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if errors:
        return 1
    print("Test isolation contracts OK: test-support always isolates homes and machine hooks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
