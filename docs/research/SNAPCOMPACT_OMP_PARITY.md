# Snapcompact + shake parity with omp (implemented)

Status: implemented 2026-11-15. Supersedes the port proposal in
[`SNAPCOMPACT_TOOLSHAKING.md`](SNAPCOMPACT_TOOLSHAKING.md) for the parts below.

Sources compared:

- omp 18.16.0 / CLI 18.20.0 as installed in
  `~/.bun/install/global/node_modules/@oh-my-pi/{snapcompact,pi-agent-core,pi-coding-agent}`.
- User config: `~/.omp/agent/config.yml`, `~/projects/github.com/xenking/pi-o-my/omp/config.d/*.yml`.
- jcode: this tree.

## 1. Tool calls and results inside the snapshot

omp renders tool activity into the frames; jcode now does the same and no
longer aborts the whole pass over payloads it cannot print.

| Behaviour | omp | jcode before | jcode after |
|---|---|---|---|
| Tool call rendered | `¶call:name(args)//intent`, args capped per value at 500 and per call at 2000 (`snapcompact.ts:815,1085-1110`) | same format (`serialize.rs:180`) | same, plus data-URL elision inside args (`serialize.rs:196`) |
| Tool result rendered | merged into its call as `<out>…</out>`, body dimmed via U+000E/U+000F, capped at 2000 head-0.6/tail-0.4 (`snapcompact.ts:808,819,1046`) | same (`serialize.rs:235`) | same (`serialize.rs:235`) |
| Images / provider-native items | skipped: only text blocks are archived (`snapcompact.ts:1057-1130`) | **whole pass failed** (`Err("snapcompact cannot preserve a non-text history block")`) | skipped, text preserved (`serialize.rs:120-150`) |
| Inline base64 data URL | atomic elision to `[data URL omitted: <mime>, N base64 chars]`, Markdown wrapper swallowed (`snapcompact.ts:900-990`) | **whole pass failed** | elided (`serialize.rs:512`, marker-aware re-elision for archives) |
| Short prose `data:…;base64,abc` | left alone (non-canonical payload < 40 chars) | n/a | left alone |
| Local-only reasoning traces | n/a | excluded | excluded |
| `useless` tool results | skipped with their call (`snapcompact.ts:1029-1035`) | n/a (jcode has no `dropUseless` flag) | n/a |

Consequence: any session with a screenshot or a provider-native search item
used to fall back to the LLM summarizer every time; now it archives.

## 2. Shake

| Behaviour | omp | jcode before | jcode after |
|---|---|---|---|
| Protect window / region floor / savings gate | 16000 / 400 / 4000 (`shake.ts:48-53`) | 16384 / 400 / 4096 (`shake.rs:66`) | 16000 / 400 / 4000 (`shake.rs:63-72`, config defaults match) |
| Protected tools | `skill`, skill-file reads, artifact-recovery reads (`shake.ts:51`) | none | skill tool names + `read` of `/skills/` or `SKILL.md` (`shake.rs:317`) |
| Elidable text regions | fences and top-level XML elements, XML suppressed inside fences, outermost span wins (`shake.ts:151-260`) | fences only | fences + XML, same nesting rules (`shake.rs:251`) |
| Presets | aggressive 4000/0, rescue 0/0 (`shake.ts:61-76`) | none | `ShakeConfig::aggressive()` / `::rescue()` (`shake.rs:80-95`) |
| Savings accounting | `tokens - 16` per region (`shake.ts:355`) | `tokens - placeholder tokens` | `tokens - 16` |
| Order vs snapcompact | `methodOrder: [remote, snapcompact, handoff, shake, soft]` (`~/.omp/agent/config.yml:187-192`); shake is a fallback method after a failed snapcompact (`session-maintenance.ts:5255-5290`) | pre-compaction shake ran before snapcompact and pre-elided the history the archive would have imaged | `plan_shake_with(.., snapcompact_preferred)` suppresses pre-compaction shake whenever snapcompact can archive the session (`compaction.rs:1024`) |
| Cache-expiry shake | first user turn after the prompt-cache TTL lapsed, never for persistent caches (`pi-o-my` patch `cache-expiry-shake-18.16.0.patch`) | gated on `shake_cache_expiry` + TTL | unchanged, but the warm-cache guard now only gates this reason |
| Pre-compaction shake vs warm cache | n/a (omp has no such guard) | **unreachable**: the warm-cache guard returned before the threshold branch, and Anthropic's 1h TTL made "warm" the normal case | threshold branch first, guard only for `CacheExpiry` (`compaction.rs:1024-1060`) |

## 3. Snapshot fonts and rendering

Snapcompact has no user-selectable font family or point size: the glyphs come
from bundled bitmap fonts (X.org 5x8/6x12/8x13, unscii 8x8, Silver TTF for
non-Latin) and the shape table decides cell geometry per reader
(`snapcompact.ts:130-215`; jcode `mod.rs:96-312`, same 17 variants).

| Setting | omp | jcode before | jcode after |
|---|---|---|---|
| Shape per model | model rules: Claude `11on16-bw`, Opus 4.7+/Fable/Mythos also 1932px; Gemini `8on22-bw` at 2048px; gpt/codex/kimi `8on22-bw`; glm `8on16-bw` (`snapcompact.ts:347-441`) | substring guesses, no high-res frames, no kimi/glm (`mod.rs:332-347`) | same rules incl. frame sizes, family fallback by provider (`mod.rs:343-437`) |
| Unknown model | family winner (OpenAI/Google shape for an unknown API) | Anthropic shape | family winner; `resolve_shape_for(model, provider)` uses the Anthropic shape only for Anthropic-family providers |
| Frame-count budget | `min(catalog image-count-budget, 80)` (`snapcompact.ts:577-585`) | fixed 80 for every provider | `frame_budget(provider)` mirrors omp's catalog values (xai/cursor 60, antigravity 20, umans 10, else 80) (`mod.rs:445`) |
| Too much history | foveated archive: middle dropped, both ends kept, `truncatedChars` reported (`snapcompact.ts:2460-2540`) | **error → LLM fallback forever** once the archive outgrew 80 frames | both ends kept, middle replaced by a marker in the source text, `truncated_chars` in the result (`mod.rs:782-820`) |
| Config | `snapcompact.shape` = `auto` or a variant name (`context-settings.ts:502`) | none | `[compaction] snapcompact_shape` (default `"auto"`), wired into `CompactOptions` |
| Summary text | reading guide from `prompts/snapcompact-summary.md` (frame count, cols/rows, ink, dropped chars), no LLM call | `"Resume prior conversation. <first 120 chars of archive>…"` **plus** a full LLM summarizer request | reading guide ported verbatim (`mod.rs:909`), LLM summarizer skipped (`compaction.rs:2272`) |

## 4. Per-provider / per-model thresholds and budgets

omp (`~/.omp/agent/config.yml:182-194`, `pi-o-my/omp/config.d/*.yml`):
`thresholdTokens 550000` (Claude), `280000` (Google), `-1` = native (Codex,
Arch), `idleThresholdTokens 550000`, `methodOrder` as above, `dropUseless`.

`~/.jcode/config.toml` `[compaction]` (backed up before the edit):

| omp setting | jcode key | state |
|---|---|---|
| `snapcompact` first in `methodOrder` | `snapcompact = true` + `plan_shake_with(snapcompact_preferred)` | migrated, order now enforced |
| `snapcompact.shape` | `snapcompact_shape = "auto"` | added |
| `DEFAULT_SHAKE_CONFIG` | `shake_protect_tokens = 16000`, `shake_min_region_tokens = 400`, `shake_min_savings_tokens = 4000`, `shake_code_blocks = true` | pinned explicitly |
| Claude `thresholdTokens` | `[compaction.models."claude-opus-5-5"] reactive_threshold_tokens = 500000` (owner choice; omp uses 550000) | kept as configured |
| Google `thresholdTokens 280000` | `[compaction.providers.gemini]` / `.google` `reactive_threshold_tokens = 280000` | added |
| Codex/Sol `thresholdTokens -1` (native) | `[compaction.models."gpt-6.1-sol"/"gpt-6.1-terra"] 400000`, `luna 200000` | kept as configured |
| `idleThresholdTokens` | `cache_expiry_threshold_tokens = 200000` | already present |
| `idleEnabled` | `shake_cache_expiry`, `snapcompact_cache_expiry` | already present |
| `dropUseless` | n/a | jcode has no contextually-useless flag |

## Test coverage added

`crates/jcode-compaction-core/src/snapcompact/serialize.rs`: images/native items
skipped, data-URL elision (bare, Markdown-wrapped, marker-aware), prose mentions
untouched, truncation head/tail.
`crates/jcode-compaction-core/src/snapcompact/mod.rs`: model shape rules and
frame sizes, unknown-model family fallback, provider frame budgets, overflow
keeps both ends, reading-guide summary, previous-summary labelling.
`crates/jcode-compaction-core/src/shake.rs`: omp defaults, skill protection,
XML elision, fence/XML nesting, unterminated fences, aggressive/rescue presets.
`crates/jcode-base/src/compaction_tests.rs`: pre-compaction shake under a warm
cache, snapcompact preference suppressing shake, cache-expiry gating narrowed,
snapcompact skips the LLM summarizer (call counter).
`crates/jcode-config-types/src/lib.rs`: migrated TOML keys parse and resolve.
