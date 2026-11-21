# /// script
# requires-python = ">=3.11"
# dependencies = ["tiktoken==0.14.0"]
# ///
"""Aggregate Code Mode traces for one Jcode workspace. Prints no trace content.

    uv run scripts/codemode_usage.py --workspace /path/to/jcode --end 2026-11-21T05:44:05Z
"""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import re
from statistics import median

import tiktoken


FOOTER = re.compile(r"^\[(\d+) tool calls?, (\d+(?:\.\d+)?)s(?:, stored: .*?)?\]$", re.M)
RECEIPT = re.compile(r"^  #(\d+) (\S+) (ok|failed|refused|in_flight) \d+ms \d+B", re.M)
NESTED_FAILURE = re.compile(r"\b(?:[1-9]\d* (?:failed|refused|in_flight))\b")
NATIVE_READS = {"agentgrep", "read", "webfetch", "websearch", "session_search", "conversation_search"}
MCP_READ = re.compile(r"^mcp__\w+__(?:get|read|search|list|find|query|fetch)(?:_|$)")
READ_LOOP = re.compile(r"\b(?:for\s+[\w$]+\s+in\b|while\s+read\b)[^\n]*|\bxargs(?:\s|$)")
FILE_READ = re.compile(r"(?:^|[;|&\n]|\bdo)\s*(?:rg|grep|cat|head|tail|awk|jq|find|ls|wc|stat)\b")
NOT_READ = re.compile(r"\b(?:cargo|justrust|git|ssh|curl|bun|npm|node|python\d*|mv|cp|rm|tee|chmod)\b|(?<![=<])>(?![=>])")
LOOKUP_WORD = re.compile(r"[\w./:-]{8,}")
ERRORS = {
    "syntax": r"\b(?:syntaxerror|typescript parse|typescript syntax|strip typescript|unexpected token|expected expression|unterminated)\b|\bfailed to parse codemode(?: source| code)?\b",
    "limits": r"\b(?:timed out|deadline|heap limit|call limit|call cap|out of memory|stack overflow|execution interrupted|execution timeout)\b",
    "arguments": r"\b(?:arguments? must be|bad args|invalid (?:tool )?arguments?|missing (?:required )?field|required (?:argument|parameter|field)|unknown tool|invalid type:|failed to parse (?:arguments|input)|agentgrep:.*(?:failed to parse|parse error))\b",
    "result_shape": r"cannot (?:read propert|convert .* to object|destructure)|not (?:a function|iterable|an? object)|JSON\.parse|could not parse[^\n]*(?:expected|json)|unexpected JSON|undefined is not an object|\b(?:missing|unexpected|unrecognized|unknown) (?:result |output )?(?:shape|format|wrapper)\b|\b(?:circular|cyclic)\b",
    "cancelled": r"\b(?:cancelled|canceled|aborted|interrupt)\b",
    "lookup_or_tool_failure": r"\b(?:file not found|no such file|permission denied|not found|command failed)\b",
}


def utc(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)


def blocks(message):
    content = message.get("content")
    return [block for block in content if isinstance(block, dict)] if isinstance(content, list) else []


def name(call):
    return (call.get("name") or "").removeprefix("functions.")


def args(call):
    value = call.get("input")
    return value if isinstance(value, dict) else {}


def result_text(result):
    text = result.get("content", "")
    if isinstance(text, list):
        return "\n".join(str(item.get("text", "")) for item in text if item.get("type") == "text")
    return text if isinstance(text, str) else ""


def error_kind(text):
    for kind, pattern in ERRORS.items():
        if re.search(pattern, text, re.I):
            return kind
    return "other"


def inspect_report(result):
    text = result_text(result)
    footer = list(FOOTER.finditer(text))
    report_part = text[:footer[-1].start()] if footer else text
    meta_start = report_part.rfind("\ncalls:\n  #")
    receipt_part = report_part[meta_start:] if meta_start >= 0 else ""
    receipts = list(RECEIPT.finditer(receipt_part))
    count = int(footer[-1][1]) if footer else None
    header = report_part[:meta_start] if meta_start >= 0 else report_part
    if header.startswith("console:\n"):
        header = re.split(r"\n\n(?=(?:result|error):)", header, maxsplit=1)[-1]
    error_text = header if header.startswith("error:") or result.get("is_error") else None
    failed_receipt = any(match[3] != "ok" for match in receipts) or any(
        NESTED_FAILURE.search(line) for line in receipt_part.splitlines() if line.startswith("  ...")
    )
    return {
        "nested": count,
        "elapsed_seconds": float(footer[-1][2]) if footer else None,
        "receipt_text": receipt_part,
        "failed_receipt": failed_receipt,
        "error": error_kind(error_text) if error_text is not None else None,
        "framed": header.startswith(("error:", "result:")),
        "undefined": header.startswith("result: undefined"),
        "truncated": bool(re.search(r"\.\.\.\(truncated|\[.{0,70}truncated|truncated to \d|output truncated|exceeded the .* per-run limit", text[-600:], re.I)),
    }


def read_session(snapshot, journal, workspace, start, end, diagnostics):
    seen = set()
    messages = []
    metadata = {}

    def ingest(new):
        for message in new:
            mid = message.get("id")
            if not mid or mid in seen:
                continue
            seen.add(mid)
            try:
                timestamp = utc(message["timestamp"])
            except (KeyError, TypeError, ValueError):
                diagnostics["unusable_timestamp"] += 1
                continue
            if start <= timestamp < end:
                messages.append(message)

    if snapshot is not None:
        try:
            data = json.loads(snapshot.read_bytes())
            metadata = {key: data.get(key) for key in ("working_dir", "is_debug", "is_canary")}
            if metadata["working_dir"] and Path(metadata["working_dir"]).resolve() != workspace:
                return None
            ingest(data.get("messages", []))
        except (OSError, ValueError):
            diagnostics["unreadable_snapshot"] += 1
    if journal is not None:
        try:
            with journal.open("rb") as stream:
                for line in stream:
                    try:
                        event = json.loads(line)
                    except ValueError:
                        diagnostics["unreadable_journal_record"] += 1
                        continue
                    metadata.update(event.get("meta", {}))
                    if metadata.get("working_dir") and Path(metadata["working_dir"]).resolve() != workspace:
                        return None
                    ingest(event.get("append_messages", []))
        except OSError:
            diagnostics["unreadable_journal"] += 1
    if not messages or not metadata.get("working_dir"):
        return None
    return messages, metadata


def distribution(values):
    if not values:
        return {"n": 0}
    values = sorted(values)
    return {"n": len(values), "median": median(values), "p90": values[(9 * len(values) - 1) // 10], "max": values[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sessions", type=Path, default=Path.home() / ".jcode/sessions")
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--end", type=utc, default=datetime.now(timezone.utc))
    options = parser.parse_args()
    workspace = options.workspace.resolve()
    start = options.end - timedelta(days=options.days)
    encoding = tiktoken.get_encoding("o200k_base")

    def tokens(text):
        return len(encoding.encode_ordinary(text))

    def input_text(call):
        return json.dumps(args(call), ensure_ascii=False, separators=(",", ":"))

    records = defaultdict(dict)
    diagnostics, totals, cases, errors, cadence = (Counter() for _ in range(5))
    session_sets = defaultdict(set)
    sizes = defaultdict(list)
    global_calls = set()
    examples, per_session_calls = Counter(), Counter()
    for suffix in (".json", ".journal.jsonl"):
        for path in options.sessions.glob("session_*" + suffix):
            try:
                if path.stat().st_mtime >= start.timestamp():
                    records[path.name.removesuffix(suffix)][suffix] = path
            except FileNotFoundError:
                diagnostics["file_replaced_during_scan"] += 1
    for key, paths in sorted(records.items()):
        session = read_session(paths.get(".json"), paths.get(".journal.jsonl"), workspace, start, options.end, diagnostics)
        if not session:
            continue
        messages, metadata = session
        mode = "canary" if metadata.get("is_canary") else "debug" if metadata.get("is_debug") else "non_debug"
        results = {
            block.get("tool_use_id"): (index, block)
            for index, message in enumerate(messages) for block in blocks(message)
            if block.get("type") == "tool_result"
        }
        series = []
        previous_errors = None
        previous_result_index = -1

        def flush():
            if len(series) >= 5:
                summary = [name(call) if name(call) in NATIVE_READS else "mcp_read" for call, _ in series]
                # A shared long token can be an ID or just a common path. This is not proof of a dependency.
                links = sum(
                    bool((set(LOOKUP_WORD.findall(input_text(cur))) - set(LOOKUP_WORD.findall(input_text(prev))))
                         & set(LOOKUP_WORD.findall(result_text(prev_result)[:100_000])))
                    for (prev, prev_result), (cur, _) in zip(series, series[1:])
                )
                examples[" → ".join(summary[:5])] += 1
                for group in ("serial_candidates", "lexically_linked_candidates") if links else ("serial_candidates",):
                    session_sets[group].add(key)
                    cases[group + ":chains"] += 1
                    cases[group + ":turns_replaced"] += len(series) - 1
                    cases[group + ":input_args_tokens"] += sum(tokens(input_text(c)) for c, _ in series)
                    cases[group + ":intermediate_output_tokens"] += sum(tokens(result_text(r)) for _, r in series[:-1])
                    cases[group + ":steps"] += len(series)
            series.clear()

        for index, message in enumerate(messages):
            content = blocks(message)
            if message.get("role") != "assistant":
                if any(b.get("type") == "text" for b in content):
                    flush()
                    previous_errors = None  # Don't count cross-user-message retries.
                continue
            calls = [b for b in content if b.get("type") == "tool_use"]
            if not calls and not any(b.get("type") in ("text", "provider_native") for b in content):
                continue  # Streaming/reasoning-only persist records aren't model responses.
            if calls and all(b.get("id") in global_calls for b in calls):
                diagnostics["inherited_or_duplicate_assistant_messages"] += 1
                continue
            session_sets["assistant_response"].add(key)
            session_sets[mode + "_assistant_response"].add(key)
            totals[mode + "_assistant_responses"] += 1
            totals["assistant_responses"] += 1
            day = message["timestamp"][:10]
            cadence[(day, "responses")] += 1
            if calls:
                totals["tool_responses"] += 1
                session_sets["tool_response"].add(key)
            if any(name(call) == "codemode" for call in calls):
                totals["codemode_responses"] += 1
                cadence[(day, "codemode")] += 1
                session_sets["codemode"].add(key)
                session_sets[mode + "_codemode"].add(key)
                totals[mode + "_codemode_responses"] += 1
            turn_errors = []
            turn_codemode = []
            for call in calls:
                cid = call.get("id")
                if not cid or cid in global_calls:
                    continue
                global_calls.add(cid)
                tool = name(call)
                totals["top_level_calls"] += 1
                _, result = results.get(cid, (-1, {}))
                if tool == "codemode":
                    totals["codemode_calls"] += 1
                    per_session_calls[key] += 1
                    source = next((args(call)[k] for k in ("code", "script", "source", "js") if k in args(call)), "")
                    source = source if isinstance(source, str) else ""
                    sizes["source_tokens"].append(tokens(source))
                    turn_codemode.append(source)
                    if re.search(r"\b(?:listTools|types|describeTool)\s*\(", source):
                        totals["calls_with_discovery_syntax"] += 1
                    if re.search(r"\b(?:store|load|storedKeys)\s*\(", source):
                        totals["calls_with_store_syntax"] += 1
                    if re.search(r"\bJSON\.parse\s*\(", source):
                        totals["calls_with_json_parse_syntax"] += 1
                    if not result:
                        totals["missing_codemode_result"] += 1
                        continue
                    report = inspect_report(result)
                    report_tokens = tokens(result_text(result))
                    sizes["report_tokens"].append(report_tokens)
                    for budget in (2000, 4000, 8000):
                        if report_tokens > budget:
                            session_sets[f"reports_over_{budget}_tokens"].add(key)
                    sizes["visible_receipt_tokens"].append(tokens(report["receipt_text"]))
                    if report["nested"] is None:
                        totals["unknown_nested_count"] += 1
                    else:
                        sizes["nested_calls"].append(report["nested"])
                        sizes["elapsed_seconds"].append(report["elapsed_seconds"])
                        totals["runs_with_0_to_1_nested"] += report["nested"] <= 1
                        totals["runs_with_5_plus_nested"] += report["nested"] >= 5
                    if report["failed_receipt"]:
                        totals["runs_with_failed_nested_receipt"] += 1
                        session_sets["failed_nested_receipt"].add(key)
                    totals["runs_undefined"] += report["undefined"]
                    totals["runs_truncation_marker"] += report["truncated"]
                    totals["runs_unframed"] += not report["framed"]
                    if report["error"]:
                        errors[report["error"]] += 1
                        turn_errors.append(source)
                        session_sets["error"].add(key)
                    if report["truncated"]:
                        session_sets["truncated"].add(key)
                elif tool == "bash":
                    command = args(call).get("command", "")
                    totals["bash_calls"] += 1
                    if isinstance(command, str) and READ_LOOP.search(command):
                        totals["bash_loop_signals"] += 1
                        session_sets["bash_loop_signals"].add(key)
                        if FILE_READ.search(command) and not NOT_READ.search(command):
                            totals["bash_file_read_loop_candidates"] += 1
                            session_sets["bash_file_read_loop_candidates"].add(key)
                            sizes["bash_file_loop_source_tokens"].append(tokens(command))
                            if result:
                                sizes["bash_file_loop_output_tokens"].append(tokens(result_text(result)))
            if previous_errors and turn_codemode:
                totals["error_turns_followed_by_codemode"] += 1
                if set(previous_errors) & set(turn_codemode):
                    totals["error_turns_followed_by_identical_source"] += 1
                if not turn_errors:
                    totals["retry_turns_without_observed_script_error"] += 1
                session_sets["error_followed_by_codemode"].add(key)
            previous_errors = turn_errors
            totals["codemode_error_turns"] += bool(turn_errors)

            if (
                len(calls) == 1
                and not any(b.get("type") == "provider_native" for b in content)
                and (name(calls[0]) in NATIVE_READS or MCP_READ.search(name(calls[0])))
            ):
                ri, result = results.get(calls[0].get("id"), (-1, {}))
                if ri > index and not result.get("is_error") and result_text(result):
                    if series and previous_result_index >= index:
                        flush()
                    series.append((calls[0], result))
                    previous_result_index = ri
                    continue
            flush()
        flush()

    for group in ("serial_candidates", "lexically_linked_candidates"):
        # Same tool args, plus 20 JS/receipt tokens per call and 40 wrapper tokens per chain.
        overhead = 20 * cases[group + ":steps"] + 40 * cases[group + ":chains"]
        for filter_pct in (50, 75, 90):
            cases[f"{group}:net_tokens_if_{filter_pct}pct_intermediate_filtered"] = round(
                cases[group + ":intermediate_output_tokens"] * filter_pct / 100 - overhead
            )
    output = {
        "window_utc": [start.isoformat(), options.end.isoformat()],
        "totals": totals,
        "sessions_by_metric": {k: len(v) for k, v in session_sets.items()},
        "per_day": {day: {k: cadence[day, k] for k in ("responses", "codemode")} for day in sorted({day for day, _ in cadence})},
        "distributions": {k: distribution(v) for k, v in sizes.items()},
        "script_errors_by_type": errors,
        "gross_report_tokens_above_budget": {
            budget: {
                "runs": sum(n > budget for n in sizes["report_tokens"]),
                "tokens": sum(max(0, n - budget) for n in sizes["report_tokens"]),
            }
            for budget in (2000, 4000, 8000)
        },
        "bash_file_read_loop_total_output_tokens": sum(sizes["bash_file_loop_output_tokens"]),
        "opportunity_screen": cases,
        "codemode_calls_per_using_session": distribution(list(per_session_calls.values())),
        "top_5_anonymous_session_call_counts": sorted(per_session_calls.values(), reverse=True)[:5],
        "top_anonymous_chain_shapes": dict(examples.most_common(5)),
        "diagnostics": diagnostics,
    }
    print(json.dumps(output, indent=2, ensure_ascii=False, sort_keys=True))


if __name__ == "__main__":
    main()
