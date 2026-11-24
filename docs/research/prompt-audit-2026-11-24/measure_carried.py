"""Estimate how many prompt tokens of each class/role are re-sent tool output.

For every provider request, count the tool_result chars already in context
(chars/4 ~ tokens) and attribute them to the producing tool. Compaction is
ignored, so this is an upper bound for long sessions. Aggregates only.
"""
import json, glob, os, re, collections, ast, sys

SINCE = sys.argv[1] if len(sys.argv) > 1 else "2026-11-01"
import importlib.util

_src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "measure_sessions.py")).read()
_ns = {}
exec(_src.split("agg = collections")[0], _ns)  # reuse klass/parse only
klass, parse = _ns["klass"], _ns["parse"]

carried = collections.defaultdict(collections.Counter)
prompt_total = collections.Counter()
for f in glob.glob(os.path.expanduser("~/.jcode/sessions/*.json")):
    try:
        d = json.load(open(f))
    except Exception:
        continue
    if (d.get("created_at") or "") < SINCE or (d.get("is_debug") and not d.get("is_swarm_worker")):
        continue
    k = (klass(d.get("model")), "worker" if d.get("is_swarm_worker") else "root")
    names = {}
    live = collections.Counter()
    prev = 0
    for m in d.get("messages") or []:
        content = parse(m.get("content")) or []
        if m.get("role") == "assistant":
            for c in content:
                if isinstance(c, dict) and c.get("type") == "tool_use":
                    names[c.get("id")] = c.get("name", "?")
            u = parse(m.get("token_usage"))
            if isinstance(u, dict) and u.get("input_tokens"):
                p = u.get("prompt_tokens") or u.get("input_tokens")
                if prev and p < 0.6 * prev:
                    live.clear()  # compaction or rewind dropped history
                prev = p
                prompt_total[k] += p
                tot_live = sum(live.values()) // 4
                scale = min(1.0, p / tot_live) if tot_live else 1.0
                for t, ch in live.items():
                    carried[k][t] += int(ch // 4 * scale)
        elif m.get("role") == "user":
            for c in content:
                if isinstance(c, dict) and c.get("type") == "tool_result":
                    live[names.get(c.get("tool_use_id"), "?")] += len(str(c.get("content", "")))
for k in sorted(carried):
    tot = sum(carried[k].values())
    pt = prompt_total[k] or 1
    top = ", ".join(f"{t} {v / tot:.0%}" for t, v in carried[k].most_common(6))
    print(f"{k[0]}/{k[1]}: carried tool output ~{tot / 1e6:.0f}M tok = {tot / pt:.0%} of {pt / 1e6:.0f}M prompt tok; by tool: {top}")
