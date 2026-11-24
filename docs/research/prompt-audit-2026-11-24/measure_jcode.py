import json, re, tiktoken

e = tiktoken.get_encoding("o200k_base")
T = lambda s: len(e.encode(s, disallowed_special=()))
for name in ["ctx_plain.json", "ctx_dev.json"]:
    c = json.load(open(name))
    st = c["system_prompt"]["static"]
    dy = c["system_prompt"]["dynamic"]
    tools = c["effective_tools"]
    print(f"== {name} mode={c['mode']}")
    print(f"system static: {len(st)} chars {T(st)} tok; dynamic {T(dy)} tok")
    for s in re.split(r"\n(?=# )", st):
        h = s.strip().split("\n", 1)[0][:60]
        print(f"   {T(s):6d}  {h}")
    tt = [(t["name"], T(json.dumps(t))) for t in tools]
    tot = sum(x for _, x in tt)
    print(f"tools: {len(tt)} defs, {tot} tok")
    for n, x in sorted(tt, key=lambda z: -z[1]):
        print(f"   {x:6d} {n}")
    print(f"TOTAL prefix ~ {T(st) + T(dy) + tot} tok")
