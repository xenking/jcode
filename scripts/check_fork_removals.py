#!/usr/bin/env python3
"""Check a committed fork tree for deliberately retired paths and behavior."""

import argparse
from pathlib import Path
import re
import subprocess
import sys


WORKFLOWS = ".github/workflows/"
# Evidence commits supply the exact deleted paths, including renames out of an
# old owner. Exceptions are present at the audit baseline, not blanket waivers.
DELETIONS = {
    "jcode": (
        ("ci-prune", "c7614da09", {
            WORKFLOWS + "release.yml",
            "scripts/test_post_discord_release.py",
            "tests/test_release_recovery.py",
            "tests/test_verify_public_release.py",
        }),
        ("sponsored-discovery", "c4c8724b2", set()),
        ("telemetry-delivery", "ad3d483fd", set()),
        ("automatic-setup", "035792212", set()),
        ("swarm-format-owner", "402c31582", set()),
        ("slash-parser-owner", "b1bc37f54", set()),
    ),
    "desktop": (
        ("ci-prune", "c256d495", set()),
        ("session-search-owner", "912bc302", set()),
        ("project-dock", "b13cf36c", set()),
    ),
}


def git(checkout, *args):
    result = subprocess.run(["git", "-C", str(checkout), *args], capture_output=True)
    if result.returncode:
        raise ValueError(f"git {' '.join(args)}: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout.decode("utf-8", errors="surrogateescape")


def deleted_paths(checkout, repo):
    for rule, commit, exceptions in DELETIONS[repo]:
        paths = git(checkout, "diff-tree", "--no-commit-id", "--no-renames",
                    "--diff-filter=D", "--name-only", "-r", "-z", f"{commit}^", commit)
        deleted = set(paths.rstrip("\0").split("\0"))
        if deleted == {""} or not exceptions <= deleted:
            raise ValueError(f"invalid removal evidence: {rule} ({commit})")
        for path in sorted(deleted - exceptions):
            yield rule, path


def inspect(checkout, repo, tree, retired):
    paths = set(git(checkout, "ls-tree", "-rz", "--name-only", tree).split("\0"))
    problems = [f"{rule}: retired path returned: {path}" for rule, path in retired if path in paths]

    def read(path):
        return git(checkout, "show", f"{tree}:{path}")

    def reject(rule, path, pattern):
        if path in paths and re.search(pattern, read(path), re.MULTILINE):
            problems.append(f"{rule}: retired behavior returned in {path} ({pattern})")

    def require_no_defaults(rule, path, dependency):
        entries = re.findall(rf"(?m)^{dependency}\s*=([^\n]+)", read(path))
        if not entries or any(not re.search(r"\bdefault-features\s*=\s*false\b", ent) for ent in entries):
            problems.append(f"{rule}: keep {dependency} default features disabled in {path}")

    for path in sorted(paths):
        if path.startswith(WORKFLOWS) and path.endswith((".yml", ".yaml")):
            reject("ci-prune", path, r'''^  ["']?schedule["']?\s*:''')

    if repo == "jcode":
        reject("ci-prune", WORKFLOWS + "ci.yml", r"^  release-automation:")
        reject("ci-prune", WORKFLOWS + "release.yml", r"^  (?:recovery-preflight|build-freebsd):")
        reject("sponsored-discovery", "crates/jcode-app-core/src/tool/mod.rs", r'"integration_tools"')
        reject("sponsored-discovery", "crates/jcode-config-types/src/lib.rs", r"\bstruct SponsorsConfig\b")

        telemetry_path = "crates/jcode-telemetry-core/src/lib.rs"
        telemetry = read(telemetry_path)
        for name in ("send_payload", "send_transcript_payload"):
            if not re.search(rf"(?m)^#\[cfg\(not\(test\)\)\]\s*fn {name}\([^\n]*\) -> bool \{{\s*false\s*\}}", telemetry):
                problems.append(f"telemetry-delivery: production {name} must remain a no-op in {telemetry_path}")
        for path in ("scripts/install.sh", "scripts/install_release.sh", "scripts/install.ps1"):
            reject("telemetry-delivery", path, r"\b(?:report_install_funnel|persist_install_conversion_id|telemetry\.jcode\.sh)\b")
        for path in ("scripts/install.sh", "scripts/install_release.sh"):
            reject("automatic-setup", path, r'''^\s*(?:if\s+)?["$][^\n]*\bsetup-hotkey\b''')
        reject("automatic-setup", "crates/jcode-setup-hints/src/lib.rs",
               r"\b(?:install_cli_launch_hints_notice|auto_install_macos_hotkey_listener|reinstall_launch_hotkeys_after_config_change)\b")
        require_no_defaults("mermaid-svg-only", "crates/jcode-tui-mermaid/Cargo.toml", "mermaid-rs-renderer")
    else:
        for path in ("crates/jcode-desktop-harness/Cargo.toml", "crates/jcode-desktop-ui/Cargo.toml"):
            require_no_defaults("desktop-provider-owner", path, "jcode-base")
        reject("desktop-provider-owner", "Cargo.lock", r'^name = "aws-sdk-bedrockruntime"$')
        reject("project-dock", "crates/jcode-desktop-ui/src/workspace.rs", r"\brender_project_dock\b")

    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", choices=DELETIONS)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("ref", nargs="?", default="HEAD")
    parser.add_argument("--deleted-workflows", action="store_true", help="print the known deleted workflow paths")
    args = parser.parse_args()

    try:
        retired = list(deleted_paths(args.checkout, args.repo))
        if args.deleted_workflows:
            for _, path in retired:
                if path.startswith(WORKFLOWS):
                    print(path)
            return 0
        tip = git(args.checkout, "rev-parse", "--verify", "--end-of-options", f"{args.ref}^{{commit}}").strip()
        problems = inspect(args.checkout, args.repo, tip, retired)
    except (ValueError, OSError) as exc:
        print(f"Fork removals: {args.repo}: {exc}", file=sys.stderr)
        return 2

    if problems:
        print(f"Fork removals: {args.repo} {tip[:12]} rejected:\n  " + "\n  ".join(problems), file=sys.stderr)
        return 1
    print(f"Fork removals: {args.repo} {tip[:12]} OK ({len(retired)} retired paths and behavior checks)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
