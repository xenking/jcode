# Snapcompact and shake behaviour (implemented)

Status: implemented 2026-11-15. Supersedes the port proposal in
[`SNAPCOMPACT_TOOLSHAKING.md`](SNAPCOMPACT_TOOLSHAKING.md) for the parts below.

The tables compare jcode before and after the change, against the behaviour
the bitmap-archive design calls for.

## 1. Tool calls and results inside the snapshot

Tool activity is rendered into the frames, and the pass no longer aborts over
payloads it cannot print.

| Behaviour | Target | jcode before | jcode after |
|---|---|---|---|
| Tool call rendered | `¶call:name(args)//intent`, args capped per value at 500 and per call at 2000 | same format (`serialize.rs:180`) | same, plus data-URL elision inside args (`serialize.rs:196`) |
| Tool result rendered | merged into its call as `<out>…</out>`, body dimmed via U+000E/U+000F, capped at 2000 head-0.6/tail-0.4 | same (`serialize.rs:235`) | same (`serialize.rs:235`) |
| Images / provider-native items | skipped: only text blocks are archived | **whole pass failed** (`Err("snapcompact cannot preserve a non-text history block")`) | skipped, text preserved (`serialize.rs:120-150`) |
| Inline base64 data URL | atomic elision to `[data URL omitted: <mime>, N base64 chars]`, Markdown wrapper swallowed | **whole pass failed** | elided (`serialize.rs:512`, marker-aware re-elision for archives) |
| Short prose `data:…;base64,abc` | left alone (non-canonical payload < 40 chars) | n/a | left alone |
| Local-only reasoning traces | n/a | excluded | excluded |
| `useless` tool results | skipped with their call | n/a (jcode has no `dropUseless` flag) | n/a |

Consequence: any session with a screenshot or a provider-native search item
used to fall back to the LLM summarizer every time; now it archives.

## 2. Shake

| Behaviour | Target | jcode before | jcode after |
|---|---|---|---|
| Protect window / region floor / savings gate | 16000 / 400 / 4000 | 16384 / 400 / 4096 (`shake.rs:66`) | 16000 / 400 / 4000 (`shake.rs:63-72`, config defaults match) |
| Protected tools | `skill`, skill-file reads, artifact-recovery reads | none | skill tool names + `read` of `/skills/` or `SKILL.md` (`shake.rs:317`) |
| Elidable text regions | fences and top-level XML elements, XML suppressed inside fences, outermost span wins | fences only | fences + XML, same nesting rules (`shake.rs:251`) |
| Presets | aggressive 4000/0, rescue 0/0 | none | `ShakeConfig::aggressive()` / `::rescue()` (`shake.rs:80-95`) |
| Savings accounting | `tokens - 16` per region | `tokens - placeholder tokens` | `tokens - 16` |
| Order vs snapcompact | method order `[remote, snapcompact, handoff, shake, soft]`; shake is a fallback method after a failed snapcompact | pre-compaction shake ran before snapcompact and pre-elided the history the archive would have imaged | `plan_shake_with(.., snapcompact_preferred)` suppresses pre-compaction shake whenever snapcompact can archive the session (`compaction.rs:1024`) |
| Cache-expiry shake | first user turn after the prompt-cache TTL lapsed, never for persistent caches | gated on `shake_cache_expiry` + TTL | unchanged, but the warm-cache guard now only gates this reason |
| Pre-compaction shake vs warm cache | no warm-cache guard | **unreachable**: the warm-cache guard returned before the threshold branch, and Anthropic's 1h TTL made "warm" the normal case | threshold branch first, guard only for `CacheExpiry` (`compaction.rs:1024-1060`) |

## 3. Snapshot fonts and rendering

Snapcompact has no user-selectable font family or point size: the glyphs come
from bundled bitmap fonts (X.org 5x8/6x12/8x13, unscii 8x8, Silver TTF for
non-Latin) and the shape table decides cell geometry per reader (`mod.rs:96-312`,
17 variants).

| Setting | Target | jcode before | jcode after |
|---|---|---|---|
| Shape per model | model rules: Claude `11on16-bw`, Opus 4.7+/Fable/Mythos also 1932px; Gemini `8on22-bw` at 2048px; gpt/codex/kimi `8on22-bw`; glm `8on16-bw` | substring guesses, no high-res frames, no kimi/glm (`mod.rs:332-347`) | same rules incl. frame sizes, family fallback by provider (`mod.rs:343-437`) |
| Unknown model | family winner (OpenAI/Google shape for an unknown API) | Anthropic shape | family winner; `resolve_shape_for(model, provider)` uses the Anthropic shape only for Anthropic-family providers |
| Frame-count budget | `min(provider image-count budget, 80)` | fixed 80 for every provider | `frame_budget(provider)` (xai/cursor 60, antigravity 20, umans 10, else 80) (`mod.rs:445`) |
| Too much history | foveated archive: middle dropped, both ends kept, truncated chars reported | **error → LLM fallback forever** once the archive outgrew 80 frames | both ends kept, middle replaced by a marker in the source text, `truncated_chars` in the result (`mod.rs:782-820`) |
| Config | `snapcompact_shape` = `auto` or a variant name | none | `[compaction] snapcompact_shape` (default `"auto"`), wired into `CompactOptions` |
| Summary text | reading guide (frame count, cols/rows, ink, dropped chars), no LLM call | `"Resume prior conversation. <first 120 chars of archive>…"` **plus** a full LLM summarizer request | reading guide (`mod.rs:909`), LLM summarizer skipped (`compaction.rs:2272`) |

## 4. Per-provider / per-model thresholds and budgets

Reference thresholds: `thresholdTokens 550000` (Claude), `280000` (Google),
`-1` = native (Codex), idle threshold `550000`, method order as above.

`~/.jcode/config.toml` `[compaction]` (backed up before the edit):

| Setting | jcode key | state |
|---|---|---|
| `snapcompact` first in the method order | `snapcompact = true` + `plan_shake_with(snapcompact_preferred)` | migrated, order now enforced |
| Frame shape | `snapcompact_shape = "auto"` | added |
| Standard shake profile | `shake_protect_tokens = 16000`, `shake_min_region_tokens = 400`, `shake_min_savings_tokens = 4000`, `shake_code_blocks = true` | pinned explicitly |
| Claude threshold | `[compaction.models."claude-opus-5-5"] reactive_threshold_tokens = 500000` (owner choice; the reference is 550000) | kept as configured |
| Google threshold 280000 | `[compaction.providers.gemini]` / `.google` `reactive_threshold_tokens = 280000` | added |
| Codex/Sol threshold `-1` (native) | `[compaction.models."gpt-6.1-sol"/"gpt-6.1-terra"] 400000`, `luna 200000` | kept as configured |
| Idle threshold | `cache_expiry_threshold_tokens = 200000` | already present |
| Idle compaction enabled | `shake_cache_expiry`, `snapcompact_cache_expiry` | already present |
| Drop useless results | n/a | jcode has no contextually-useless flag |

## Test coverage added

`crates/jcode-compaction-core/src/snapcompact/serialize.rs`: images/native items
skipped, data-URL elision (bare, Markdown-wrapped, marker-aware), prose mentions
untouched, truncation head/tail.
`crates/jcode-compaction-core/src/snapcompact/mod.rs`: model shape rules and
frame sizes, unknown-model family fallback, provider frame budgets, overflow
keeps both ends, reading-guide summary, previous-summary labelling.
`crates/jcode-compaction-core/src/shake.rs`: standard defaults, skill protection,
XML elision, fence/XML nesting, unterminated fences, aggressive/rescue presets.
`crates/jcode-base/src/compaction_tests.rs`: pre-compaction shake under a warm
cache, snapcompact preference suppressing shake, cache-expiry gating narrowed,
snapcompact skips the LLM summarizer (call counter).
`crates/jcode-config-types/src/lib.rs`: TOML keys parse and resolve.
