"""Break down where the todo tool's model-visible output size comes from.

Aggregates only. Reads ~/.jcode/sessions read-only.
"""
import json, glob, os, re, ast, collections, sys

SINCE = sys.argv[1] if len(sys.argv) > 1 else "2026-11-15"


def parse(v):
    if isinstance(v, (list, dict)):
        return v
    try:
        return json.loads(v)
    except Exception:
        try:
            return ast.literal_eval(v)
        except Exception:
            return None


sizes = []
parts = collections.Counter()
fields = collections.Counter()
compact_total = 0
n = 0
for f in glob.glob(os.path.expanduser("~/.jcode/sessions/*.json")):
    try:
        d = json.load(open(f))
    except Exception:
        continue
    if (d.get("created_at") or "") < SINCE:
        continue
    names = {}
    for m in d.get("messages") or []:
        for b in parse(m.get("content")) or []:
            if not isinstance(b, dict):
                continue
            if b.get("type") == "tool_use":
                names[b.get("id")] = b.get("name")
            elif b.get("type") == "tool_result" and names.get(b.get("tool_use_id")) == "todo":
                text = str(b.get("content", ""))
                if not text.lstrip().startswith("["):
                    continue
                n += 1
                sizes.append(len(text))
                dec = json.JSONDecoder()
                try:
                    todos, end = dec.raw_decode(text.lstrip())
                except Exception:
                    continue
                body = text.lstrip()
                parts["todos"] += end
                for t in todos:
                    for k, v in t.items():
                        fields["todo." + k] += len(json.dumps(v)) + len(k) + 4
                compact = json.dumps(todos, separators=(",", ":"))
                for label in ["Plan:", "Goals:", "Plan updates:", "Goal updates:"]:
                    i = body.find("\n\n" + label + "\n")
                    if i < 0:
                        continue
                    try:
                        val, e = dec.raw_decode(body[i + len(label) + 3:])
                    except Exception:
                        continue
                    parts[label] += e
                    compact += json.dumps(val, separators=(",", ":"))
                    if label == "Goals:":
                        for g in val:
                            for k, v in g.items():
                                fields["goal." + k] += len(json.dumps(v)) + len(k) + 4
                    if label == "Plan:":
                        for k, v in val.items():
                            fields["plan." + k] += len(json.dumps(v)) + len(k) + 4
                rest = len(body) - sum(parts.values())
                compact_total += len(compact)
tot = sum(sizes) or 1
sizes.sort()
print(f"todo results={n} total_chars={tot} p50={sizes[len(sizes)//2]} p90={sizes[int(len(sizes)*.9)]}")
print(f"compact JSON (no whitespace) would be {compact_total / tot:.0%} of current")
for k, v in parts.most_common():
    print(f"  section {k}: {v / tot:.0%}")
print("top fields (share of all chars):")
for k, v in fields.most_common(18):
    print(f"  {k}: {v / tot:.1%}")
