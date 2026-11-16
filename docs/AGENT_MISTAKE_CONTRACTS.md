# Agent mistake contracts

Evidence window: the 600 commits ending at `6cfb648df`, down to `a9f3569ef`.
Only corrective commits count, not feature additions, merge commits, or a test
that merely expands coverage. Older originating fixes are marked separately.
Local jcode-repository sessions were also read, excluding scratch projects and
Desktop/cloud repositories. Session contents and credentials are not copied here.

## Test targets inheriting the operator's home and hooks

Eight incidents (nine corrective commits) in the window:
`96ef184f6`, `4ac87e9e2`, `06a64d9d8`, `aa912e04c`, `829c6a55f`,
`b620b8055`, `c6a68b6da`, `40cf25ba3`, `d0a390663`.
The last two overlap in subject, so they are one incident rather than two.
These cover both ambient environment leakage and process-global test state.
This gate specifically closes the recurring home/hook opt-in hole, not every
possible environment race.

The architectural fix makes `jcode-base/test-support` select
`test-isolated-hooks`, so downstream tests cannot get the isolated home while
silently inheriting machine hooks. `check_test_isolation.py` checks Cargo's
parsed graph for each package separately, including renamed/forwarded features,
and rejects production defaults or dependencies enabling test-only isolation.
Runtime consumers need their own unconditional dev dependency, rather than
accidentally borrowing feature unification from a workspace test run. Optional
runtime dependencies remain optional for lightweight test configurations.

| Gate | Real bad tree | Expected failure | Current tree |
| --- | --- | --- | --- |
| `python3 scripts/check_test_isolation.py` | `066c926b8673dbc4d84bc2995d18110cd87e1c73` (parent of `829c6a55f`) | exit 1, `test-support must enable test-isolated-hooks` | exit 0 |

Reproduce without changing the checkout or touching the real home:

```bash
past="$(mktemp -d)"
git archive 066c926b8673dbc4d84bc2995d18110cd87e1c73 | tar -x -C "$past"
python3 scripts/check_test_isolation.py --root "$past" # exits 1
python3 scripts/check_test_isolation.py                # exits 0
python3 scripts/test_check_test_isolation.py
```

No allowances or baseline increases are provided. A failure names the Cargo
feature or dev dependency to fix. Existing behavioral home-isolation and
hook-marker tests remain the runtime backstop.
