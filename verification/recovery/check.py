#!/usr/bin/env python3
"""Check startup recovery ownership using policy exported from the current Rust."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SOURCE = ROOT / "crates/jcode-app-core/src/server.rs"
TLC_VERSION = "1.7.4"
TLC_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
RULES = (
    "TypeOK",
    "HistoricalEvidenceCannotAuthorize",
    "StopAfterReadPreventsLaunch",
    "ContinuationHasOwner",
)


def between(text, start, end):
    if text.count(start) != 1:
        raise ValueError(f"source mapping changed: expected one {start!r}")
    tail = text.split(start, 1)[1]
    if end not in tail:
        raise ValueError(f"source mapping changed: missing {end!r}")
    return tail.split(end, 1)[0]


def statement(text, name):
    start = f"let {name} ="
    body = between(text, start, ";")
    return start + body + ";"


def export_policy(source, out):
    text = source.read_text()
    predicate = "fn coordinator_was_live_at_restart(" + between(
        text, "fn coordinator_was_live_at_restart(", "\nfn worker_has_live_coordinator("
    )
    body = between(text, "async fn recover_headless_sessions_on_startup(&self) {", "\n    async fn finish_startup_after_bind(")
    decision = statement(body, "has_stored_recovery_intent")
    lane = statement(body, "open_lane")
    resume = statement(body, "should_resume")
    interrupted = between(body, "let was_interrupted =", "&& {").strip()
    mapped = {
        "OWNER_PREDICATE": predicate,
        "STORED_INTENT": decision,
        "OPEN_LANE": lane,
        "INTERRUPTION_GUARD": interrupted,
        "SHOULD_RESUME": resume,
    }
    template = (HERE / "policy.rs.in").read_text()
    for name, value in mapped.items():
        template = template.replace(f"@@{name}@@", value)
    crate = out / "policy"
    (crate / "src").mkdir(parents=True)
    (crate / "src/main.rs").write_text(template)
    (crate / "Cargo.toml").write_text(
        '[package]\nname = "jcode-formal-recovery-policy"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n'
    )
    target = out / "target"
    subprocess.run(
        ["justrust", "build", "--local=the dependency-free policy export reads this worktree",
         "--offline", "--manifest-path", str(crate / "Cargo.toml"),
         "--target-dir", str(target)],
        check=True,
        env={**os.environ, "JUSTRUST_SLOTS": "0", "JUSTRUST_LOCALCACHE": "0"},
        timeout=120,
    )
    binary = target / "debug/jcode-formal-recovery-policy"
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    result = subprocess.run([str(binary)], check=True, capture_output=True, text=True)
    owners, cases = [], []
    for line in result.stdout.splitlines():
        fields = line.split()
        if fields[0] == "owner":
            owners.append(json.dumps(fields[1]))
        elif fields[0] == "resume":
            cases.append("<<" + ", ".join(field.upper() for field in fields[1:]) + ">>")
        else:
            raise ValueError(f"unexpected policy output: {line}")
    (out / "RecoveryPolicy.tla").write_text(
        "---- MODULE RecoveryPolicy ----\n"
        + "LiveOwners == {" + ", ".join(owners) + "}\n"
        + "ResumeCases == {" + ", ".join(cases) + "}\n====\n"
    )
    (out / "source-map.json").write_text(json.dumps({
        "source": str(source),
        "sha256": hashlib.sha256(text.encode()).hexdigest(),
        "extracted": mapped,
    }, indent=2) + "\n")
    for name in ["RecoveryOwnership.tla", "RecoveryOwnership.cfg", "view.mjs"]:
        shutil.copyfile(HERE / name, out / name)


def tool_jar():
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "jcode-formal"
    cache.mkdir(parents=True, exist_ok=True)
    jar = Path(os.environ.get("TLA2TOOLS", cache / f"tla2tools-{TLC_VERSION}.jar")).resolve()
    if not jar.exists():
        url = f"https://github.com/tlaplus/tlaplus/releases/download/v{TLC_VERSION}/tla2tools.jar"
        data = urllib.request.urlopen(url, timeout=60).read()
        if hashlib.sha256(data).hexdigest() != TLC_SHA256:
            raise ValueError("TLC download checksum mismatch")
        temporary = jar.with_suffix(".download")
        temporary.write_bytes(data)
        temporary.replace(jar)
    if hashlib.sha256(jar.read_bytes()).hexdigest() != TLC_SHA256:
        raise ValueError(f"TLC must be the pinned {TLC_VERSION} artifact: {jar}")
    return jar


def check(out, jar):
    failed = set()
    for rule in RULES:
        config = out / f"{rule}.cfg"
        config.write_text(f"INIT Init\nNEXT Next\nINVARIANT {rule}\n")
        result = subprocess.run(
            [os.environ.get("JAVA", "java"), "-XX:+UseParallelGC", "-cp", str(jar),
             "tlc2.TLC", "-workers", "1", "-fp", "0", "-config", config.name,
             "RecoveryOwnership"],
            cwd=out, capture_output=True, text=True, timeout=60,
        )
        log = result.stdout + result.stderr
        (out / f"{rule}.out").write_text(log)
        violation = f"Invariant {rule} is violated."
        if result.returncode == 12 and violation in log:
            failed.add(rule)
            print(f"FAIL {rule}")
        elif result.returncode == 0 and "Model checking completed. No error has been found." in log:
            print(f"PASS {rule}")
        else:
            raise RuntimeError(f"TLC did not finish {rule}:\n{log}")
    (out / "results.json").write_text(json.dumps({
        "tlc": TLC_VERSION, "failed": sorted(failed),
        "passed": sorted(set(RULES) - failed),
    }, indent=2) + "\n")
    return failed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--export-only", action="store_true")
    parser.add_argument("--mutation-test", action="store_true")
    args = parser.parse_args()
    scratch = Path(os.environ.get("JCODE_SCRATCH_DIR", tempfile.gettempdir()))
    scratch.mkdir(parents=True, exist_ok=True)
    out = args.out or Path(tempfile.mkdtemp(prefix="jcode-recovery-", dir=scratch))
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if out.is_relative_to(ROOT):
        raise ValueError("keep generated files outside the checkout")
    print(f"Artifacts: {out}", flush=True)
    export_policy(SOURCE, out)
    if args.export_only:
        return 0
    jar = tool_jar()
    baseline = check(out, jar)
    if not args.mutation_test:
        return int(bool(baseline))
    mutant = out / "mutation"
    mutant.mkdir()
    source = mutant / "server.rs"
    text = SOURCE.read_text()
    guard = "live_before_restart.contains(&member.session_id)"
    if text.count(guard) != 1:
        raise ValueError("mutation location changed, inspect the owner predicate")
    source.write_text(text.replace(guard, "true", 1))
    export_policy(source, mutant)
    changed = check(mutant, jar)
    if "HistoricalEvidenceCannotAuthorize" in baseline:
        raise RuntimeError("baseline already violates the mutation property, choose a different scratch break")
    if changed - baseline != {"HistoricalEvidenceCannotAuthorize"}:
        raise RuntimeError(f"scratch break was not isolated: {sorted(changed - baseline)}")
    restored = out / "restored"
    restored.mkdir()
    export_policy(SOURCE, restored)
    if check(restored, jar) != baseline:
        raise RuntimeError("restored source does not reproduce the baseline")
    print("Mutation caught. Unmodified source returns to the same findings, not a clean pass.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"ERROR {error}", file=sys.stderr)
        sys.exit(2)
