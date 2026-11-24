"""Aggregate real jcode session telemetry by model class and role.

Reads ~/.jcode/sessions/*.json (read-only). Prints aggregate numbers only:
no prompts, paths or transcript text are emitted.
"""
import json, glob, os, re, statistics as st, collections, sys, datetime

SINCE = sys.argv[1] if len(sys.argv) > 1 else "2026-10-25"


def klass(model):
    m = (model or "").lower()
    if re.search(r"haiku|luna|flash|mini|nano|spark|lightning", m):
        return "fast"
    if re.search(r"opus|fable|mythos|sol\b|-sol|astra|gpt-5\.5|pro\b|-pro", m):
        return "frontier"
    if re.search(r"sonnet|terra|deepseek|kimi|glm|qwen|gemini|grok|minimax", m):
        return "general"
    return "other"


def parse(v):
    if isinstance(v, (dict, list)):
        return v
    if isinstance(v, str):
        try:
            return json.loads(v)
        except Exception:
            try:
                import ast
                return ast.literal_eval(v)
            except Exception:
                return None
    return None


agg = collections.defaultdict(lambda: collections.defaultdict(list))
tools = collections.defaultdict(collections.Counter)
models = collections.Counter()
efforts = collections.defaultdict(collections.Counter)
files = glob.glob(os.path.expanduser("~/.jcode/sessions/*.json"))
for f in files:
    try:
        if os.path.getsize(f) > 300_000_000:
            continue
        d = json.load(open(f))
    except Exception:
        continue
    if (d.get("created_at") or "") < SINCE:
        continue
    if d.get("is_debug") and not d.get("is_swarm_worker"):
        continue  # headless debug/test sessions
    model = d.get("model") or ""
    k = (klass(model), "worker" if d.get("is_swarm_worker") else "root")
    efforts[k][d.get("reasoning_effort") or "-"] += 1
    models[(model, k[1])] += 1
    msgs = d.get("messages") or []
    a = agg[k]
    a["sessions"].append(1)
    first = None
    user_turns = 0
    tool_calls = 0
    tr_chars = 0
    asst_text = 0
    user_chars = 0
    reqs = 0
    for m in msgs:
        role = m.get("role")
        content = parse(m.get("content")) or []
        if role == "user":
            texts = [c for c in content if isinstance(c, dict) and c.get("type") == "text"]
            results = [c for c in content if isinstance(c, dict) and c.get("type") == "tool_result"]
            for c in results:
                tr_chars += len(str(c.get("content", "")))
            if texts and not results and m.get("display_role") != "system":
                user_turns += 1
                user_chars += sum(len(c.get("text", "")) for c in texts)
        elif role == "assistant":
            for c in content:
                if isinstance(c, dict) and c.get("type") == "tool_use":
                    tool_calls += 1
                    tools[k][c.get("name", "?")] += 1
                elif isinstance(c, dict) and c.get("type") == "text":
                    asst_text += len(c.get("text", ""))
            u = parse(m.get("token_usage"))
            if isinstance(u, dict) and u.get("input_tokens"):
                reqs += 1
                inp = u.get("prompt_tokens") or ((u.get("input_tokens") or 0) + (u.get("cache_read_input_tokens") or 0) + (u.get("cache_creation_input_tokens") or 0))
                a["input_tok"].append(inp)
                a["output_tok"].append(u.get("output_tokens") or 0)
                cr = u.get("cache_read_input_tokens") or 0
                a["cache_read_frac"].append(min(cr / inp, 1.0) if inp else 0)
                if first is None:
                    first = inp
    if first:
        a["first_req_input"].append(first)
    a["requests"].append(reqs)
    a["user_turns"].append(user_turns)
    a["tool_calls"].append(tool_calls)
    if user_turns:
        a["tools_per_user_turn"].append(tool_calls / user_turns)
        a["reqs_per_user_turn"].append(reqs / user_turns)
    tot = tr_chars + asst_text + user_chars
    if tot:
        a["tool_result_share"].append(tr_chars / tot)


def q(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(p * len(v)))] if v else 0


print(f"sessions scanned since {SINCE}: {sum(len(a['sessions']) for a in agg.values())}")
hdr = "class/role | sessions | reqs | first-req in p50 | in/req p50 | p90 | out/req p50 | cache-read p50 | tools/user-turn p50 | reqs/user-turn p50 | tool-result share p50"
print(hdr)
for k in sorted(agg):
    a = agg[k]
    print(" | ".join(map(str, [
        f"{k[0]}/{k[1]}", len(a["sessions"]), sum(a["requests"]),
        q(a["first_req_input"], .5), q(a["input_tok"], .5), q(a["input_tok"], .9), q(a["output_tok"], .5),
        round(q(a["cache_read_frac"], .5), 2), round(q(a["tools_per_user_turn"], .5), 1),
        round(q(a["reqs_per_user_turn"], .5), 1), round(q(a["tool_result_share"], .5), 2)])))
print("\nTop tools per class/role (share of calls):")
for k in sorted(tools):
    c = tools[k]
    n = sum(c.values())
    print(f"{k[0]}/{k[1]} n={n}: " + ", ".join(f"{t} {v / n:.0%}" for t, v in c.most_common(10)))
print("\nTotals per class/role: prompt tokens (M), output tokens (M), share of all prompt tokens")
grand = sum(sum(a["input_tok"]) for a in agg.values()) or 1
for k in sorted(agg):
    a = agg[k]
    s = sum(a["input_tok"])
    print(f"  {k[0]}/{k[1]}: {s / 1e6:.1f}M in, {sum(a['output_tok']) / 1e6:.2f}M out, {s / grand:.0%}")
print("\nReasoning effort per class/role:")
for k in sorted(efforts):
    print(f"  {k[0]}/{k[1]}: {dict(efforts[k].most_common())}")
print("\nModels (sessions):")
for (m, r), n in models.most_common(30):
    print(f"  {n:5d} {r:6s} {klass(m):8s} {m}")
