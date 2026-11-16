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
The gate also rejects the lower-level `test-isolated-hooks` feature in
production dependencies and defaults, including renamed and forwarded features.
Its regression tests fail against the original gate, which only checked
`test-support`.

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

## Chrome measured in characters instead of terminal cells

One corrective incident in the window: `776b05d05`. It fixed scalar-count
truncation in compact status labels, but left the git branch label using the
same pattern. Earlier originating fixes `3c8245b6c` and `d0b2a5e7b` corrected
emoji and CJK overflow caused by counting characters instead of display cells.
Those two commits precede the evidence window. Repository-session corrections
also explicitly called out cell-bounded compact rows, not character bounds.

The shared `jcode-tui-render` truncators already own cell-bounded truncation.
Header, status and info-widget callers now use those helpers and
`UnicodeWidthStr::width`. The git branch label uses the existing status fitter,
which also replaces control characters so a branch cannot add rows.

The lint rejects `.chars().count()` and `.chars().take()` in chrome renderers,
including new info widgets and nested modules. A Rust string still needs scalar
operations for text editing, so a global prohibition or a different string type
would reject legitimate input and prose operations. The gate checks the
presentation boundary instead. It ignores comments, literals and test-only code,
and reports the file, line and replacement helper.

| Gate | Real bad tree | Expected failure | Current tree |
| --- | --- | --- | --- |
| `python3 scripts/check_tui_chrome.py` | `2ad342b23edd00104d3bccc10b6f67ec11bf1d92` (parent of `776b05d05`) | exit 1, `ui_input.rs:1939` and `:1942`: use display cells and shared truncation | exit 0 |

```bash
past="$(mktemp -d)"
git archive 2ad342b23edd00104d3bccc10b6f67ec11bf1d92 crates/jcode-tui/src/tui | tar -x -C "$past"
python3 scripts/check_tui_chrome.py --root "$past" # exits 1
python3 scripts/check_tui_chrome.py                # exits 0
python3 scripts/test_check_tui_chrome.py
justrust test -p jcode-tui --lib chrome_contract_tests
```

The renderer tests exercise CJK, emoji, combining characters and control
characters at cell budgets from zero through 79. Both gates and their Python
regression suites run in CI and `scripts/check_guardrails.sh`. Neither gate has
an allowance list or an adjustable baseline. The broad no-wrap rule remains:
scalar-width enforcement does not prove every future layout has only one row.
The fully replaced scalar-count prose was removed from AGENTS.md.

## Copied quota bars drifting from the info widget

Two corrective incidents in the window: `776b05d05` (legacy transcript bar)
and `0b430381d` (labels and color thresholds). The older originating fix is
`b331e4cc5` (account-row bar). Repository sessions explicitly repeated the
shared-renderer requirement. Glyph duplication is the enforced subset of this
broader style failure; color and wording are still backed by renderer tests.

The quota primitive is already `info_widget_usage.rs`. The source gate rejects
bar construction throughout production TUI modules, including newly added
files, rather than grandfathering individual violations. Task progress has a
different meaning and glyph vocabulary, so its existing three builders were
migrated to one private `progress_bar.rs` primitive. Quota surfaces cannot call
that primitive as a workaround. Plain-text wire reports outside the TUI remain
plain text, and parsing their old glyphs is allowed: display regenerates the
bar through LimitBar. Progress and usage-card runtime tests verify that this
ownership change preserves glyphs, widths, styles and quota rendering.

| Gate | Real bad tree | Expected failure | Current tree |
| --- | --- | --- | --- |
| `python3 scripts/check_usage_bar_owner.py` | `2ad342b23edd00104d3bccc10b6f67ec11bf1d92` (parent of `776b05d05`) | exit 1, `ui_messages/usage.rs:835-836`: use `info_widget::LimitBar` | exit 0 |

```bash
past="$(mktemp -d)"
git archive 2ad342b23edd00104d3bccc10b6f67ec11bf1d92 crates/jcode-tui/src/tui | tar -x -C "$past"
python3 scripts/check_usage_bar_owner.py --root "$past" # exits 1
python3 scripts/check_usage_bar_owner.py                # exits 0
python3 scripts/test_check_usage_bar_owner.py
justrust test -p jcode-tui --lib progress
justrust test -p jcode-tui --lib usage_card
```

The replaced copied-glyph prose is removed from AGENTS.md. Color and
used/remaining-label prose remains, because glyph ownership alone does not
prove every future label or color. The gate has no legacy violation allowances.

## Catalog discovery integration dropping runtime slots or forced refresh

The broader integration class has two corrective incidents in the window:
`9204b3d04` (busy-session publication) and `2ad342b23` (endpoint-scoped cache).
Five earlier corrective commits establish recurrence: `15ef6f755` (all runtime
slots and compatible profiles), `ce353038d` (non-Grok discovery gaps),
`0f757ede2` (forced Anthropic refresh), `812c174c3` (periodic attached refresh),
and `602b27590` (capability forwarding). Coverage-only commits `f89e22bf7` and
`9a370b07e` are deliberately not counted. The new enforcement covers the
shared fanout/force boundary, not every provider's network/cache implementation
or publication protocol, which retain their existing behavioral suites.

One typed CatalogTarget list now drives the concurrent refresh and error
handling for every runtime and compatible endpoint. There is no separate
per-provider join/result list that can silently omit a new target. The source
gate derives all Arc-held Provider fields from MultiProvider, including arbitrary
future slots and endpoint maps, without a frozen provider allowlist. It checks
the actual collection consumption and both cached/forced entry points. Mutation
tests remove a target, introduce a future provider, bypass the collection and
drop force at the consumer, all of which must be rejected. The existing runtime
probe exercises all dynamic slots and independent compatible profiles; existing
provider catalog tests retain real-response-schema/future-model coverage.

| Gate | Real bad tree | Expected failure | Current tree |
| --- | --- | --- | --- |
| `python3 scripts/check_catalog_fanout.py` | `3ed9b4b351509d8da85e3fd9dc59e9b5ea4b1b75` (parent of `15ef6f755`) | exit 1, `MultiProvider.openai_compatible_profiles: missing catalog fanout` | exit 0 |

```bash
past="$(mktemp -d)"
git archive 3ed9b4b351509d8da85e3fd9dc59e9b5ea4b1b75 crates/jcode-base/src/provider | tar -x -C "$past"
python3 scripts/check_catalog_fanout.py --root "$past" # exits 1, names the actual missing map
python3 scripts/check_catalog_fanout.py                # exits 0
python3 scripts/test_check_catalog_fanout.py
justrust test -p jcode-base --lib catalog_refresh_reaches_every_runtime_slot_and_compatible_profile
```

Only fully enforced fanout prose was removed. Offline-fallback, endpoint cache,
last-good and publication rules remain: this check does not pretend to prove
those network/session semantics. Existing ratchets were not reimplemented.


