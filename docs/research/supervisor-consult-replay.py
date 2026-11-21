#!/usr/bin/env python3
"""Capture/replay Jcode routing metadata locally. Never emit message bodies or IDs."""

import argparse
from collections import Counter
from datetime import datetime
import json
from pathlib import Path
import sys


EVENTS = frozenset({
    "user_candidate", "system_input", "background_input", "other_user",
    "assistant_text_only", "assistant_with_tools", "assistant_other",
    "tool_call", "tool_result", "swarm_spawn", "swarm_message", "swarm_other",
    "ask_call", "ask_return",
})


def stamp(value):
    result = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if result.tzinfo is None:
        raise ValueError("timestamps must include a UTC offset")
    return result


def message_events(message, tool_kinds):
    blocks = message["content"]
    role, display = message["role"], message.get("display_role")
    calls = [block for block in blocks if block["type"] == "tool_use"]
    results = [block for block in blocks if block["type"] == "tool_result"]
    has_text = any(block["type"] == "text" for block in blocks)
    if role == "user":
        if display == "system":
            yield "system_input"
        elif display == "background_task":
            yield "background_input"
        elif display is None and has_text and not results:
            yield "user_candidate"
        elif not results:
            yield "other_user"
    elif role == "assistant":
        if calls:
            yield "assistant_with_tools"
        else:
            yield "assistant_text_only" if has_text else "assistant_other"

    for block in calls:
        name = block.get("name")
        if isinstance(name, str) and name.startswith("functions."):
            name = name.removeprefix("functions.")
        tool_kinds[block["id"]] = name == "ask"
        yield "tool_call"
        if name == "ask":
            yield "ask_call"
        elif name == "swarm":
            action = block["input"].get("action")
            if action == "spawn":
                yield "swarm_spawn"
            elif action == "message":
                yield "swarm_message"
            else:
                yield "swarm_other"
    for block in results:
        yield "tool_result"
        if tool_kinds.get(block["tool_use_id"], False):
            yield "ask_return"


def capture(snapshot, journal, since, until):
    start, end = stamp(since), stamp(until)
    if start >= end:
        raise ValueError("expected since < until")
    with snapshot.open(encoding="utf-8") as stream:
        saved = json.load(stream)
    with journal.open(encoding="utf-8") as stream:
        appended = [message for line in stream if line.strip()
                    for message in json.loads(line).get("append_messages", [])]
    # Snapshot and journal may overlap. Keys and times are never exported.
    rows = {m["id"]: m for m in saved["messages"] + appended}
    tool_kinds, events = {}, []
    for time, message in sorted(((stamp(m["timestamp"]), m) for m in rows.values()),
                                key=lambda row: row[0]):
        if time >= end:
            break
        kinds = list(message_events(message, tool_kinds))
        if time >= start:
            events.extend(kinds)
    return {"schema": 1, "since": since, "until": until,
            "scope": "worker" if saved.get("is_swarm_worker") else "primary", "events": events}


def replay(trace, consultations):
    if trace.get("schema") != 1 or any(x not in EVENTS for x in trace["events"]):
        raise ValueError("not a routing metadata trace")
    if consultations < 0:
        raise ValueError("expected a non-negative consultation scenario")
    counts = Counter(trace["events"])
    return {
        "observed": dict(sorted(counts.items())),
        "counterfactual": {
            "all_candidates_wake_advisor": counts["user_candidate"],
            "direct_candidate_wakes_in_either_proposal": 0,
            "assumed_explicit_consultations": consultations,
            "stateless": {"dispatches": consultations, "logical_contexts": consultations},
            "persistent_one_scope": {"dispatches": consultations, "logical_contexts": int(consultations > 0)},
        },
        "not_measured": ["provider_requests", "quota", "tokens", "dollars", "cache_hits",
                         "quality", "latency", "approved_actions"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    extract = commands.add_parser("capture")
    for name in ("snapshot", "journal"):
        extract.add_argument(f"--{name}", type=Path, required=True)
    for name in ("since", "until"):
        extract.add_argument(f"--{name}", required=True)
    simulate = commands.add_parser("replay")
    simulate.add_argument("trace", type=Path)
    simulate.add_argument("--consultations", type=int, default=0)
    args = parser.parse_args()
    try:
        if args.command == "capture":
            output = capture(args.snapshot, args.journal, args.since, args.until)
        else:
            with args.trace.open(encoding="utf-8") as stream:
                output = replay(json.load(stream), args.consultations)
    except (KeyError, TypeError, ValueError, OSError):
        # Exceptions can carry private paths, IDs or arbitrary input fields.
        parser.exit(2, "Invalid input or incomplete snapshot/journal. No payload exported.\n")
    json.dump(output, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
