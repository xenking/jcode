import json, re, difflib, tiktoken

e = tiktoken.get_encoding("o200k_base")
T = lambda s: len(e.encode(s, disallowed_special=()))
d = json.load(open("codex/codex-rs/models-manager/models.json"))["models"]
by = {m["slug"]: m for m in d}


def txt(v):
    if v is None:
        return ""
    return v if isinstance(v, str) else json.dumps(v)


print("slug | template tok | persistent | multi_agent | token_budget | collab | confirm | auto_review")
for m in d:
    mm = m["model_messages"]
    row = [T(txt(mm.get(k))) for k in ["instructions_template", "persistent_instructions", "multi_agent", "token_budget", "collaboration_modes", "confirmation_policies", "auto_review"]]
    print(m["slug"], row, "reasoning_default=", m.get("default_reasoning_level"), "verbosity=", m.get("default_verbosity"))

for s in ["gpt-6-luna", "gpt-6.1-terra", "gpt-6.2-sol"]:
    open(f"codex_{s}.md", "w").write(by[s]["model_messages"]["instructions_template"])
    heads = re.findall(r"^#+ .*$", by[s]["model_messages"]["instructions_template"], re.M)
    print(f"\n== {s} headings ({len(heads)}):", " | ".join(heads))


def diff(a, b):
    A = by[a]["model_messages"]["instructions_template"].splitlines()
    B = by[b]["model_messages"]["instructions_template"].splitlines()
    out = [l for l in difflib.unified_diff(A, B, lineterm="", n=0) if l[:1] in "+-" and not l.startswith(("+++", "---"))]
    print(f"\n== diff {a} -> {b}: {len(out)} changed lines")
    for l in out[:40]:
        print("  ", l[:220])


diff("gpt-6-luna", "gpt-6.1-terra")
diff("gpt-6.1-terra", "gpt-6.2-sol")
print("\nTOKEN_BUDGET luna:", txt(by["gpt-6-luna"]["model_messages"]["token_budget"])[:1500])
print("\nPERSISTENT luna:", txt(by["gpt-6-luna"]["model_messages"]["persistent_instructions"])[:1200])
