#!/usr/bin/env python3
"""Analyze OpenAI persistent-websocket reuse and KV-cache effectiveness from jcode logs.

Motivation
----------
OpenAI prompt caching on the ChatGPT/Codex backend is driven by the persistent
websocket reuse path: it sends only a delta + ``previous_response_id`` on the
same connection, so the server already holds the KV tensors for that prefix.
When the socket is torn down, the chain is lost (``store=false``) and the next
turn re-sends the full conversation, relying on OpenAI prefix-hash routing which
frequently lands on a cold machine (zero cache read).

This script quantifies:
  * connection mix of completed provider calls (persistent-reuse vs persistent-fresh)
  * why fresh connections happen (state-reset reasons, idle reconnects)
  * realized cache hit rate per provider
  * OpenAI zero/low-read events

Use it before/after changing ``JCODE_OPENAI_WS_IDLE_RECONNECT_SECS`` (or the
default) to confirm idle-reconnect churn drops and reuse/cache rates rise.

Usage
-----
    python3 scripts/analyze_openai_ws_cache.py [LOGFILE ...]

With no arguments it scans ~/.jcode/logs/jcode-*.log.
"""

from __future__ import annotations

import argparse
import collections
import glob
import os
import re
import sys


def _log_files(argv: list[str]) -> list[str]:
    if argv:
        return argv
    home = os.environ.get("HOME", "")
    return sorted(glob.glob(os.path.join(home, ".jcode", "logs", "jcode-*.log")))


_FIELD_RE = re.compile(r"(\w+)=([^\s]+)")
_ACCOUNTING = {
    "openai": ("OpenAI", False),
    "openai-api": ("OpenAI API", False),
    "claude": ("Claude", True),
    "anthropic": ("Claude", True),
}


def analyze(files: list[str], *, since=None, until=None, session=None) -> dict:
    conn = collections.Counter()           # persistent-reuse / persistent-fresh
    reset_reason = collections.Counter()   # persistent_state_reset reason=...
    reuse_detail = collections.Counter()   # persistent_reuse_unavailable_detail reason=...
    idle_reconnect_secs: list[int] = []     # observed idle durations that triggered reconnect
    calls = collections.defaultdict(collections.Counter)
    cache = collections.defaultdict(lambda: [0, 0, 0, 0, 0])  # prompt, uncached, read, write, n
    oa_readpct = collections.Counter()
    oa_readpct_n = 0
    oa_zero = 0
    oa_zero_tokens = 0

    idle_re = re.compile(r"Persistent WS idle for (\d+)s; reconnecting")

    for path in files:
        try:
            fh = open(path, errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                timestamp = line[1:24] if line.startswith("[") else ""
                if since and (not timestamp or timestamp < since):
                    continue
                if until and (not timestamp or timestamp >= until):
                    continue
                if session and f"session_id={session} " not in line.rstrip() + " ":
                    continue
                if "persistent_state_reset" in line:
                    m = re.search(r"reason=([a-z_]+)", line)
                    if m:
                        reset_reason[m.group(1)] += 1

                if "persistent_reuse_unavailable_detail" in line:
                    m = re.search(r"reason=([a-z_]+)", line)
                    if m:
                        reuse_detail[m.group(1)] += 1

                m = idle_re.search(line)
                if m:
                    idle_reconnect_secs.append(int(m.group(1)))

                if "event=AGENT_PROVIDER_STREAM_LIFECYCLE " not in line:
                    continue
                d = dict(_FIELD_RE.findall(line))
                if d.get("phase") != "stream_complete" or d.get("saw_message_end") != "true":
                    continue
                provider = d.get("provider", "?")
                connection = d.get("connection_type", "unknown")
                calls[provider][connection] += 1
                if provider.lower() == "openai":
                    if connection == "websocket/persistent-reuse":
                        conn["reuse"] += 1
                    elif connection == "websocket/persistent-fresh":
                        conn["fresh"] += 1
                accounting = _ACCOUNTING.get(provider.lower())
                if accounting is None:
                    continue
                label, split = accounting
                try:
                    reported, read, write = (int(d[k]) for k in ("input", "cache_read", "cache_write"))
                except (KeyError, ValueError):
                    continue
                prompt = reported + read + write if split else reported
                uncached = reported if split else reported - read
                if min(reported, read, write, uncached) < 0 or not prompt:
                    continue
                bucket = cache[label]
                for index, value in enumerate((prompt, uncached, read, write, 1)):
                    bucket[index] += value
                if label == "OpenAI":
                    if prompt > 1024 and read == 0:
                        oa_zero += 1
                        oa_zero_tokens += uncached
                    pct = 100 * read / prompt
                    oa_readpct_n += 1
                    if pct >= 90:
                        oa_readpct[">=90%"] += 1
                    elif pct >= 70:
                        oa_readpct["70-90%"] += 1
                    elif pct >= 50:
                        oa_readpct["50-70%"] += 1
                    elif pct > 0:
                        oa_readpct["1-50%"] += 1
                    else:
                        oa_readpct["0%"] += 1

    return {
        "conn": conn,
        "calls": calls,
        "reset_reason": reset_reason,
        "reuse_detail": reuse_detail,
        "idle_reconnect_secs": idle_reconnect_secs,
        "cache": cache,
        "oa_readpct": oa_readpct,
        "oa_readpct_n": oa_readpct_n,
        "oa_zero": oa_zero,
        "oa_zero_tokens": oa_zero_tokens,
    }


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Report completed provider calls from Jcode logs")
    parser.add_argument("logs", nargs="*")
    parser.add_argument("--since", help="inclusive local log time (YYYY-MM-DD HH:MM:SS)")
    parser.add_argument("--until", help="exclusive local log time (YYYY-MM-DD HH:MM:SS)")
    parser.add_argument("--session", help="exact session ID; excludes diagnostic lines without that ID")
    args = parser.parse_args(argv)
    files = _log_files(args.logs)
    if not files:
        print("no log files found", file=sys.stderr)
        return 1
    print(f"Scanned {len(files)} log file(s)")
    r = analyze(files, since=args.since, until=args.until, session=args.session)

    conn = r["conn"]
    total_conn = conn["reuse"] + conn["fresh"]
    print("\n== Connection mix ==")
    if total_conn:
        print(f"  reuse : {conn['reuse']:>6} ({100*conn['reuse']/total_conn:.1f}%)")
        print(f"  fresh : {conn['fresh']:>6} ({100*conn['fresh']/total_conn:.1f}%)")
    else:
        print("  (no completed OpenAI WebSocket calls)")
    unused = {p: sum(counts.values()) for p, counts in r["calls"].items() if p.lower() not in _ACCOUNTING}
    if unused:
        print(f"  provider input semantics not classified: {dict(sorted(unused.items()))}")

    print("\n== Fresh-connection causes ==")
    print("  persistent_reuse_unavailable_detail:")
    for reason, n in r["reuse_detail"].most_common():
        print(f"    {reason:24s} {n}")
    print("  persistent_state_reset:")
    for reason, n in r["reset_reason"].most_common():
        print(f"    {reason:24s} {n}")

    idle = r["idle_reconnect_secs"]
    print("\n== Idle-reconnect events (the avoidable churn) ==")
    if idle:
        idle_sorted = sorted(idle)
        print(f"  count={len(idle)}  min={idle_sorted[0]}s  "
              f"median={idle_sorted[len(idle_sorted)//2]}s  max={idle_sorted[-1]}s")
        # how many would be saved by a higher threshold
        for thr in (90, 300, 600, 900):
            saved = sum(1 for s in idle if s < thr)
            print(f"  threshold {thr:>4}s would have avoided {saved}/{len(idle)} reconnects")
    else:
        print("  count=0 (no idle reconnects logged)")

    print("\n== Cache read share (provider-reported completed calls) ==")
    for provider, (prompt, uncached, read, write, n) in sorted(r["cache"].items()):
        print(f"  {provider:10s} hit={100*read/prompt:5.1f}%  "
              f"read={read:>13,} written={write:>10,} uncached={uncached:>13,} n={n}")
    if not r["cache"]:
        print("  (no supported provider usage events; TUI estimates are not used)")

    print("\n== OpenAI cold-prefill cost ==")
    print(f"  zero-read prompts (>1024 tok): {r['oa_zero']}  "
          f"(~{r['oa_zero_tokens']:,} full-price input tokens)")
    n = r["oa_readpct_n"]
    if n:
        print(f"  read share distribution (n={n}):")
        for k in (">=90%", "70-90%", "50-70%", "1-50%", "0%"):
            c = r["oa_readpct"][k]
            print(f"    {k:8s}: {c:>5} ({100*c/n:.1f}%)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
