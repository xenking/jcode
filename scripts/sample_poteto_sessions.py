#!/usr/bin/env python3
"""Count observable poteto checkpoints in local sessions. Prints aggregates only."""

import argparse
from collections import Counter
from datetime import datetime
import json
from pathlib import Path
import re


KICKOFF = re.compile(r"^Read \S*/poteto-mode/worker\.md before anything else\.")
ACTIVE = re.compile(r"^Active modes?: ([^.\n]+)\. ", re.MULTILINE)


def instant(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")) if isinstance(value, str) else None


def load_journal(path, snapshot, data):
    journal = path.with_suffix(".journal.jsonl")
    entries = journal.read_bytes().splitlines(keepends=True) if journal.exists() else []
    ids = {message.get("id") for message in data["messages"] if message.get("id")}
    gap = False
    for line in entries:
        if not line.endswith(b"\n"):
            gap = True
            continue
        entry = json.loads(line)
        meta = entry.get("meta", {})
        current = meta.get("updated_at", "") >= data.get("updated_at", "")
        for message in entry.get("append_messages", []):
            mid = message.get("id")
            if mid and mid in ids:
                continue
            data["messages"].append(message)
            if mid:
                ids.add(mid)
        if current:
            data.update(meta)
    if path.read_bytes() != snapshot:
        raise ValueError("snapshot changed during read")
    return data, gap


def text(message):
    return "\n".join(b.get("text", "") for b in message["content"] if b.get("type") == "text")


def actual_user(message, body):
    return (message["role"].lower() == "user" and not message.get("display_role")
            and bool(body) and not body.startswith(("<system-reminder>", "[Queued prompts]")))


def read_kind(call, result):
    if result is None or result.get("is_error"):
        return None
    body = json.dumps(result.get("content"), ensure_ascii=False)
    args = call.get("input", {})
    if call.get("name") == "read":
        path = str(args.get("file_path", ""))
        if path.endswith("/poteto-mode/worker.md") and "You are working for the operator" in body:
            return "worker"
        if path.endswith("/poteto-mode/SKILL.md") and "# Poteto mode" in body:
            return "skill"
        if "/poteto-mode/playbooks/" in path and "### " in body:
            return "playbook"
        if path.endswith("/docs/harness/jcode.md") and "# pstack in jcode" in body:
            return "harness"
    if (call.get("name") in ("skill_manage", "skill") and args.get("action") in ("load", "read")
            and (args.get("name") or args.get("skill")) == "poteto-mode" and "# Poteto mode" in body):
        return "skill"
    return None


def checkpoint(data, gap, start, end):
    active = None
    prompt = None
    calls = []
    results = {}
    display = None
    for message in data["messages"]:
        at = instant(message.get("timestamp"))
        if at and at >= end:
            continue
        body = text(message)
        if prompt is None and message.get("display_role") == "system" and "# Active Skill\n" in body:
            if "Session mode is off." in body:
                active = False
            elif match := ACTIVE.search(body):
                active = "poteto-mode" in match[1].split(", ")
        if actual_user(message, body):
            if prompt is not None:
                break
            if at is None:
                return None
            if not start <= at < end:
                return None
            prompt = re.sub(r"^\[\d{4}-\d\d-\d\dT[^\]]+\]\s*", "", body)
            display = "worker" if data.get("is_swarm_worker") or KICKOFF.match(prompt) else "root_or_unmarked"
        if prompt is None:
            continue
        for block in message["content"]:
            if block.get("type") == "tool_use":
                calls.append(block)
            elif block.get("type") == "tool_result":
                results[block.get("tool_use_id")] = block
    if prompt is None:
        return None
    reads = [read_kind(call, results.get(call.get("id"))) for call in calls]
    direct_read = "observed" if set(reads) & {"worker", "skill"} else "unknown"
    declared = {True: "poteto", False: "other_or_off", None: "unknown"}[active]

    # Check only steps whose applicability and outcome are explicit in the log.
    applicable = "not_classified"
    if KICKOFF.match(prompt):
        applicable = "explicit_worker_kickoff"
        flow = ("worker_read_first" if reads and reads[0] == "worker" else
                "worker_first_other" if calls and not gap and calls[0].get("name") not in ("codemode", "read") else
                "unknown")
    elif active and (spawns := [c for c in calls if c.get("name") == "swarm"
                               and c.get("input", {}).get("action") in ("spawn", "spawn_visible")]):
        applicable = "direct_swarm_spawn"
        # Lane text and prompts inside codemode may not be persisted as direct arguments.
        prompts = [c["input"].get("prompt") for c in spawns]
        flow = ("spawn_kickoff_all" if all(isinstance(p, str) and KICKOFF.match(p) for p in prompts) else
                "spawn_kickoff_partial" if all(isinstance(p, str) and p for p in prompts) else "unknown")
    else:
        flow = "unknown"
    return display, declared, direct_read, applicable, flow, Counter(r for r in set(reads) if r)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sessions", type=Path, required=True)
    parser.add_argument("--repo", type=Path, action="append", required=True)
    parser.add_argument("--start", type=instant, required=True)
    parser.add_argument("--end", type=instant, required=True)
    args = parser.parse_args()
    if not args.start.tzinfo or not args.end.tzinfo or args.end <= args.start:
        parser.error("use an increasing, timezone-qualified window")
    roots = {repo.resolve(): repo.name for repo in args.repo}
    counters = {name: Counter() for name in roots.values()}
    unreadable = 0
    files = 0
    for path in sorted(args.sessions.glob("session_*.json")):
        files += 1
        try:
            snapshot = path.read_bytes()
            data = json.loads(snapshot)
            cwd = data.get("working_dir")
            created = instant(data.get("created_at"))
            repo = roots.get(Path(cwd).resolve()) if cwd else None
            if repo is None or created is None or not args.start <= created < args.end:
                continue
            data, gap = load_journal(path, snapshot, data)
            cwd = data.get("working_dir")
            repo = roots.get(Path(cwd).resolve()) if cwd else None
            if repo is None:
                continue
            counter = counters[repo]
            if data.get("is_test") or data.get("is_test_session"):
                counter["excluded_test_flag"] += 1
                continue
            counter["eligible_snapshots"] += 1
            if gap:
                counter["journal_partial"] += 1
            item = checkpoint(data, gap, args.start, args.end)
            if item is None:
                counter["no_observable_first_task"] += 1
                continue
            role, declared, read, applicable, flow, more = item
            counter["tasks"] += 1
            for label, value in (("role", role), ("declared", declared), ("read", read),
                                 ("applicability", applicable), ("flow", flow)):
                counter[f"{label}.{value}"] += 1
            for kind, value in more.items():
                counter[f"read_kind.{kind}"] += value
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            unreadable += 1
    print(json.dumps({"snapshot_files": files, "unreadable_or_unstable": unreadable,
                      "repos": {name: dict(counter) for name, counter in counters.items()}},
                     indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
