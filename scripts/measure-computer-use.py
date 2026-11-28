#!/usr/bin/env python3
"""Count direct macOS computer-use calls without exporting session content.

Run with tiktoken==0.13.0 (o200k_base). Pass explicit workspace roots.
"""

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
import json
import math
from pathlib import Path
import re


TOOL = "macos_computer_use"
ACTIONS = set("""
    discover setup check_permissions screenshot window_screenshot ocr ui cursor move click
    double_click right_click drag scroll type key key_down key_up find_element element_at
    press get_value set_value perform_action select_menu list_apps list_windows activate_app
    hide_app quit_app focus_window move_window resize_window minimize_window close_window
    get_clipboard set_clipboard run_applescript run_jxa wait_for notify system_state set_brightness
""".split())
OBSERVATIONS = {"ui", "find_element", "element_at", "screenshot", "window_screenshot", "ocr"}
CAPTURES = {"screenshot", "window_screenshot"}
DIMENSIONS = re.compile(r"Captured (?:main display|window \d+): ([1-9]\d*)x([1-9]\d*) pixels")
REASONS = tuple((bucket, re.compile(pattern, re.I)) for bucket, pattern in (
    ("automation", r"apple events?|not authorized to send|-1743"),
    ("screen_recording", r"screen recording|could not create image|cannot capture"),
    ("accessibility", r"accessibility permission required|not allowed assistive access|-25211"),
    ("timeout", r"timed? out|timeout|deadline"),
    ("target_not_found_or_stale", r"not found|no (such|matching)|invalid index|invalid element|can.t get|doesn.t exist|-1719|-1728"),
    ("invalid_key_chord", r"unknown key|unrecognized key|non-modifier key|no main key|requires `keys`"),
    ("invalid_element_handle", r"requires an `element` handle|invalid `element` handle"),
    ("invalid_input", r"unknown .* action|requires |invalid |missing |unsupported"),
    ("capture_or_dependency", r"screencapture|swift|vision|failed to capture"),
    ("input_delivery", r"cgevent|keyboard event|mouse event|event source"),
    ("osascript_unspecified", r"applescript failed|osascript failed|jxa failed|execution error"),
))


def stamp(value):
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return dt if dt.tzinfo is not None else None
    except (AttributeError, ValueError):
        return None


def distribution(values):
    if not values:
        return {"n": 0}
    ordered = sorted(values)
    return {"n": len(values), "min": ordered[0],
            "p50": ordered[math.ceil(.5 * len(ordered)) - 1],
            "p90": ordered[math.ceil(.9 * len(ordered)) - 1], "max": ordered[-1]}


def json_read(stream, coverage, journal=False):
    try:
        value = json.loads(stream) if journal else json.load(stream)
        if isinstance(value, dict):
            return value
    except (ValueError, UnicodeError, RecursionError):
        pass
    coverage["invalid_journal_lines" if journal else "invalid_snapshots"] += 1
    return {}


def session(snapshot, coverage, roots, cutoff):
    try:
        with snapshot.open("rb") as stream:
            header = json_read(stream, coverage)
        if not header:
            return
        messages = header.pop("messages", [])
        if not isinstance(messages, list):
            coverage["invalid_snapshots"] += 1
            return
        journal = snapshot.with_suffix(".journal.jsonl")
        if journal.is_file() and not journal.is_symlink():
            with journal.open("rb") as stream:
                for line in stream:
                    if line.strip():
                        entry = json_read(line, coverage, journal=True)
                        meta = entry.get("meta", {})
                        tail = entry.get("append_messages", [])
                        if not isinstance(meta, dict) or not isinstance(tail, list):
                            coverage["invalid_journal_lines"] += 1
                            continue
                        header.update(meta)
                        messages.extend(tail)
    except (OSError, AttributeError, TypeError):
        coverage["unreadable_sessions"] += 1
        return
    if not any(Path(value).resolve() in roots for field in ("working_dir", "original_working_dir")
               if isinstance(value := header.get(field), str)):
        return
    updated = stamp(header.get("updated_at"))
    if updated and updated < cutoff:
        return
    coverage["workspace_sessions_in_scope"] += 1
    return messages


def transitions(messages, start, end, coverage):
    """Yield one call record and pair a result with its contiguous image blocks."""
    seen_messages = set()
    calls, results = {}, {}
    turn = 0
    for message in messages:
        if not isinstance(message, dict):
            coverage["invalid_messages"] += 1
            continue
        mid = message.get("id")
        if isinstance(mid, str):
            if mid in seen_messages:
                continue
            seen_messages.add(mid)
        blocks = message.get("content", [])
        if not isinstance(blocks, list):
            coverage["invalid_messages"] += 1
            continue
        blocks = [block for block in blocks if isinstance(block, dict)]
        if message.get("role") == "user" and not message.get("display_role") and any(
            block.get("type") == "text" for block in blocks
        ) and not any(block.get("type") == "tool_result" for block in blocks):
            turn += 1
        when = stamp(message.get("timestamp"))
        for i, block in enumerate(blocks):
            kind = block.get("type")
            if kind == "tool_use" and block.get("name") == TOOL:
                if when is None:
                    coverage["direct_calls_without_timestamp"] += 1
                elif start <= when < end and isinstance(block.get("id"), str):
                    args = block.get("input")
                    raw = args.get("action") if isinstance(args, dict) else None
                    action = raw if isinstance(raw, str) else ""
                    calls.setdefault(block["id"], (action if action in ACTIONS else "unknown", turn))
            result_id = block.get("tool_use_id")
            if kind == "tool_result" and isinstance(result_id, str) and result_id in calls and (when is None or when < end):
                images = 0
                for next_block in blocks[i + 1:]:
                    if next_block.get("type") != "image":
                        break
                    images += 1
                results.setdefault(result_id, (block, images))
    for call_id, (action, turn) in calls.items():
        yield action, turn, results.get(call_id)


def measure(paths, roots, start, end, encoding):
    coverage, reasons = Counter(), Counter()
    by_action, costs, patches, steps = defaultdict(Counter), defaultdict(list), defaultdict(list), []
    for path in paths:
        coverage["snapshot_candidates"] += 1
        try:
            newer = max(path.stat().st_mtime, path.with_suffix(".journal.jsonl").stat().st_mtime)
        except FileNotFoundError:
            try:
                newer = path.stat().st_mtime
            except OSError:
                coverage["unreadable_sessions"] += 1
                continue
        except OSError:
            coverage["unreadable_sessions"] += 1
            continue
        if newer < start.timestamp():
            continue
        coverage["recent_snapshot_candidates"] += 1
        messages = session(path, coverage, roots, start)
        if messages is None:
            continue
        turns = Counter()
        had_call = False
        for action, turn, result in transitions(messages, start, end, coverage):
            had_call = True
            if turn:
                turns[turn] += 1
            else:
                coverage["direct_calls_without_user_turn"] += 1
            counts = by_action[action]
            counts["calls"] += 1
            if result is None:
                counts["unmatched"] += 1
                continue
            block, images = result
            text = block.get("content")
            if not isinstance(text, str):
                text = ""
                coverage["invalid_tool_result_text"] += 1
            if block.get("is_error") is True:
                counts["errors"] += 1
                reasons[next((bucket for bucket, pattern in REASONS if pattern.search(text)), "other")] += 1
            else:
                counts["ok"] += 1
                if action in OBSERVATIONS:
                    costs[action].append(len(encoding.encode_ordinary(text)))
                if action in CAPTURES:
                    counts["with_retained_image"] += bool(images)
                    if images and (match := DIMENSIONS.search(text)):
                        width, height = map(int, match.groups())
                        patches[action].append(((width + 31) // 32) * ((height + 31) // 32))
        coverage["workspace_sessions_with_calls"] += had_call
        steps.extend(turns.values())
    totals = Counter()
    for counts in by_action.values():
        totals.update(counts)
    paired = totals["ok"] + totals["errors"]
    return {"utc_window": [start.isoformat(), end.isoformat()], "coverage": dict(coverage),
            "totals": {key: totals[key] for key in ("calls", "ok", "errors", "unmatched")},
            "error_rate_paired": totals["errors"] / paired if paired else None,
            "error_reasons": dict(reasons), "actions": {key: dict(value) for key, value in by_action.items()},
            "direct_calls_per_user_turn": distribution(steps),
            "retained_result_text_o200k_tokens": {key: distribution(value) for key, value in costs.items()},
            "retained_image_raw_32px_patches": {key: distribution(value) for key, value in patches.items()}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sessions", type=Path, default=Path.home() / ".jcode" / "sessions")
    parser.add_argument("--workspace", required=True, action="append", type=Path)
    parser.add_argument("--end", type=stamp, default=datetime.now(timezone.utc))
    args = parser.parse_args()
    if args.end is None:
        parser.error("--end requires ISO 8601 with timezone")
    if not args.sessions.is_dir():
        parser.error("session directory unavailable")
    import tiktoken

    encoding = tiktoken.get_encoding("o200k_base")
    report = measure((p for p in sorted(args.sessions.glob("session_*.json")) if not p.is_symlink()),
                     {p.resolve() for p in args.workspace}, args.end - timedelta(days=7), args.end, encoding)
    print(json.dumps(report, indent=2, sort_keys=True, ensure_ascii=True))


if __name__ == "__main__":
    main()
