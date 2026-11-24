import os, difflib, tiktoken, glob

e = tiktoken.get_encoding("o200k_base")
T = lambda s: len(e.encode(s, disallowed_special=()))
R = lambda p: open(p, errors="ignore").read()
X = "x1/"
files = sorted(glob.glob(X + "Cursor Prompts/*")) + sorted(glob.glob(X + "VSCode Agent/*.txt")) + sorted(glob.glob(X + "Amp/*.yaml")) \
    + sorted(glob.glob(X + "Windsurf/*")) + sorted(glob.glob(X + "Open Source prompts/*/*")) + sorted(glob.glob(X + "Devin AI/*")) + sorted(glob.glob(X + "Augment Code/*")) + sorted(glob.glob(X + "Kiro/*"))
for f in files:
    if os.path.isfile(f):
        print(f"{T(R(f)):7d}  {f[len(X):]}")


def sim(a, b):
    A, B = R(a).splitlines(), R(b).splitlines()
    return difflib.SequenceMatcher(None, A, B).ratio()


print("\nVSCode per-model similarity vs claude-sonnet-4:")
base = X + "VSCode Agent/claude-sonnet-4.txt"
for f in sorted(glob.glob(X + "VSCode Agent/*.txt")):
    if "titles" in f or "nes" in f:
        continue
    print(f"  {sim(base, f):.2f}  {os.path.basename(f)}")
print("\nAmp claude vs gpt-5 similarity:", round(sim(X + "Amp/claude-4-sonnet.yaml", X + "Amp/gpt-5.yaml"), 2))
