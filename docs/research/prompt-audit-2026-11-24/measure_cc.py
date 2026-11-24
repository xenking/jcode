import re, os, glob, json, tiktoken

e = tiktoken.get_encoding("o200k_base")
T = lambda s: len(e.encode(s, disallowed_special=()))
readme = open("cc/README.md").read()
rows = re.findall(r"\[([^\]]+)\]\(\./system-prompts/([^)]+)\) \(\*\*(\d+)\*\* tks\)", readme)
ratio = []
cat = {}
for title, f, tks in rows:
    p = f"cc/system-prompts/{f}"
    if not os.path.exists(p):
        continue
    body = re.sub(r"^<!--.*?-->\n", "", open(p).read(), flags=re.S)
    o = T(body)
    ratio.append((int(tks), o))
    k = f.split("-")[0] + ("-" + f.split("-")[1] if f.startswith(("system-prompt", "tool-description", "agent-prompt")) else "")
    cat.setdefault(k, [0, 0])
    cat[k][0] += 1
    cat[k][1] += int(tks)
a = sum(x for x, _ in ratio); b = sum(y for _, y in ratio)
print(f"files {len(ratio)} claude-tks {a} o200k {b} ratio claude/o200k={a/b:.3f}")
for k, v in sorted(cat.items(), key=lambda z: -z[1][1]):
    print(k, v)

want = ["agent-prompt-explore.md", "agent-prompt-plan-mode-enhanced.md", "agent-prompt-general-purpose.md", "agent-prompt-coordinator-worker-instructions.md",
        "system-prompt-coordinator-mode-orchestration.md", "system-prompt-advisor-tool-instructions.md", "agent-prompt-worker-fork.md",
        "agent-prompt-read-only-search-agent.md", "agent-prompt-web-reading-specialist.md", "system-prompt-worker-instructions.md",
        "agent-prompt-conversation-summarization.md", "agent-prompt-coding-session-title-generator.md", "tool-description-agent.md", "tool-description-task.md"]
d = {f: int(t) for _, f, t in rows}
print()
for w in want:
    print(w, d.get(w))
print()
for _, f, t in rows:
    if re.search(r"tool-description-(agent|task|sendmessage|advisor|bash|todowrite|edit|read)", f):
        print(f, t)
