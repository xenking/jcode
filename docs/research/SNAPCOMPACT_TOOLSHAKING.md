# Snapcompact and tool shaking: port research for jcode

Status: research only, 2026-11-15. No code changes.

## Sources

- omp user config: `~/.omp/agent/config.yml:182-194` (`compaction.methodOrder: [remote, snapcompact, handoff, shake, soft]`, `idleEnabled`, `thresholdTokens: 550000`, `dropUseless: true`).
- pi-o-my notes: `pi-o-my/docs/context-and-compaction.md:26-30` (cache-expiry shake, upstream omp PR #11190, patch `omp/patches/cache-expiry-shake-18.16.0.patch`), `pi-o-my/omp/config.d/codex.yml:60-62`.
- oxicode (https://github.com/project-oxi/oxicode, MIT, branch `main`), a Rust port of omp (`can1357/oh-my-pi`, MIT):
  - `oxicode-agent/src/agent_loop/compaction/shake.rs` (717 lines). Ported from omp `packages/agent/src/compaction/shake.ts`.
  - `oxicode-snapcompact/src/lib.rs` (896 lines, shape table at :100-214), `src/renderer/snapcompact_render.rs` (2013 lines, ported from omp `pi-natives/src/snapcompact.rs`), `NOTICE.md` (fonts).
  - `oxicode-sdk/src/snapcompact_compactor.rs` (311 lines, `Compactor` impl, frame cost heuristic ~1500 tokens/PNG at :189-192).
  - `docs/rfcs/RFC-009-...md`: shake runs after `should_compact` and before LLM compaction.
- jcode: `crates/jcode-compaction-core/src/lib.rs` (924 lines), `crates/jcode-base/src/compaction.rs`, `crates/jcode-config-types/src/lib.rs:26-34` (`CompactionMode` Reactive/Proactive/Semantic), `:1263-1265` (`openai_native_compaction_*`), `crates/jcode-provider-openai-runtime/src/native_compaction.rs`, `crates/jcode-tui/src/tui/app/model_context.rs:1514` (emergency truncate call site), MCP `mcp_tools` auto/deferred + `mcp_tools_token_threshold` (`src/cli/args.rs:147`, `src/cli/startup.rs:713`), `--tool-profile` (`src/cli/args.rs:131`).

## What each technique does

### Shake (omp "shake", the "toolshaking" in the question)

Mechanical, LLM-free elision (`shake.rs:1-22`):
1. Walk messages backwards, protect the newest `protect_window_tokens` (default 16384) verbatim.
2. In older messages, collect regions >= `min_elidable_tokens` (400): tool result payloads and fenced code blocks inside any text.
3. If total savings >= `min_savings_tokens` (4096), replace all regions with short placeholders in one pass, else mutate nothing (`ShakeOutcome::NoChange`, idempotent).
Token estimate is `chars/4`. Note: it shakes tool *outputs*, not tool *schemas*. omp also runs it as a "cache-expiry shake": prune before the first turn after the provider cache TTL expired (prefix is cold anyway, so the rewrite is free), skipped for persistent-cache providers (Codex login).

### Snapcompact

Instead of an LLM summary, discarded history is serialized and rasterized into dense PNG "frames" with bitmap fonts (BDF/HEX + Silver TTF), eval-tuned shapes (`8x8r-bw`, `6x6u-sent`, `5x8-bw`, ... with stopword dimming, 1-2 columns, line repeat). Vision models read the frames back. Deterministic, local, no API call. Cost model: ~1500 tokens per frame vs `bytes/4` for text, so compression depends on how many characters fit per frame (dense 6x6 cells on a large frame fit tens of thousands of chars, i.e. roughly 5-10x vs text tokens at some OCR fidelity loss).

## What jcode already has

| Capability | jcode | Equivalent? |
|---|---|---|
| LLM summary compaction, reactive/proactive/semantic triggers | `jcode-compaction-core`, `CompactionMode` | Covers omp `soft`/`handoff` |
| Provider-native compaction | `openai_native_compaction_mode/threshold` | Covers omp `remote` |
| Tool output trimming | `emergency_truncate_tool_results`, `emergency_truncate_large_payloads`, `emergency_strip_large_images` (`lib.rs:616-758`) | Partial. Only on emergency (overflow / payload-too-large), truncation not placeholder elision, no protected tail window, no savings gate, no code-block elision |
| Tool schema trimming | `mcp_tools` auto/deferred with token threshold, tool profiles, deferred tools | Better than omp/oxicode here. Shake does not touch schemas, nothing to port |
| Bitmap/vision compaction | none | No |
| Cache-expiry-aware pruning | none found (no TTL-gated rewrite) | No |

## Cache (KV prefix) implications

- Shake rewrites old messages, so it invalidates the prefix from the first elided message onward. Run opportunistically it would destroy jcode's cache hit rate. Only safe in three moments: (a) right before/instead of an LLM compaction (prefix is being rewritten anyway), (b) after cache TTL expiry (Anthropic 5m/1h, OpenAI ~5-60m; skip for persistent-cache routes), (c) emergency. Placeholders must be deterministic (content hash + tool name + original token count) so repeated shakes produce byte-identical prefixes. Once shaken, a message must never be un-shaken.
- Snapcompact replaces history with images at a compaction boundary, same cache cost as any compaction. But images are cached less predictably on some providers, PNG bytes must be byte-stable (deterministic encoder, fixed fonts) to keep the post-compaction prefix stable. Image tokens are also billed differently per provider.

## Expected wins

- Shake: sessions dominated by `read`/`bash`/`agentgrep` outputs typically have 50-80% of context in old tool results. One shake pass can reclaim that with zero latency and zero API cost, postponing the LLM compaction (which costs a full-context call, 10-60s, and summary fidelity loss). Expected: 1.5-3x more turns between LLM compactions, compaction latency 0 when shake suffices. As a cache-expiry pre-pass: smaller cold prefix to re-prefill, directly cheaper first turn after idle.
- Snapcompact: wins only vs LLM summary latency/cost, at the price of fidelity (OCR errors on code/identifiers), vision-model requirement, and provider image limits. jcode's emergency path already strips images on rejection (`is_image_rejection_error`), which would fight this. Uncertain net win, unmeasured outside omp's own evals.

## License

oxicode MIT, omp MIT: compatible with jcode, keep attribution header. Snapcompact fonts: Silver is CC BY 4.0 (attribution required in NOTICE), X.org misc BDF public domain, unscii CC0. All compatible but add NOTICE obligations.

## Port design

### Shake (GO)

- Crate: new module `crates/jcode-compaction-core/src/shake.rs` (pure functions on `Message`, no IO). Rewrite from oxicode `shake.rs` (MIT, attribute), adapt to jcode `ContentBlock::ToolResult`.
- Placeholder: `[elided tool_result <tool> ~N tokens, sha256:<8>; re-run tool to view]`, deterministic. Never elide tool_use/tool_result pairing (reuse `safe_compaction_cutoff` logic), never touch the system prompt, pinned/memory blocks, or images (images handled by existing strip).
- Hook points (via `jcode-base/src/compaction.rs` manager):
  1. Pre-pass in the compaction decision: when threshold fires, try shake first, if the estimate drops below threshold skip the LLM call (omp `methodOrder` `shake` before `soft`, oxicode RFC-009 ordering).
  2. Cache-expiry pass: on next user turn, if `now - last_request_at > provider_cache_ttl` and route cache is not persistent, shake before building the request. Needs a `cache_ttl()` hint on the provider trait (provider-core).
  3. Replace `emergency_truncate_with` first step with shake, fall back to existing truncation.
- Config (`jcode-config-types`, `[compaction]`): `shake = "off"|"pre_compact"|"cache_expiry"|"both"` (default `pre_compact` after soak, `off` initially), `shake_protect_tokens = 16384`, `shake_min_region_tokens = 400`, `shake_min_savings_tokens = 4096`, `shake_code_blocks = true`.
- Telemetry: emit `CompactionEvent` with kind `shake`, regions, saved tokens, and track cache read tokens on the following turn (`reusable_cache_tokens_from_usage`).
- Tests: red-first unit tests: protected tail untouched; below savings gate no mutation; idempotency (shake twice byte-identical); tool_use/result pairing preserved; code fences elided only outside tail; deterministic placeholder. Integration: TUI/server test that a forced threshold triggers shake and no LLM compaction request; cache-expiry test with a fake clock that a shake runs only after TTL and never on persistent routes.
- Effort: 2-3 days incl. config, telemetry, tests. Risk: model loses data it later needs (mitigated by "re-run tool" hint and 16k tail), cache churn if hooked wrongly (mitigated by limiting to the three moments above).

### Snapcompact (NO-GO for now)

- Would be: new crate `jcode-snapcompact` vendoring oxicode renderer (~3.2k lines + fonts + NOTICE), new `CompactionMode::Snapcompact` or a method order list, gated on model vision capability and provider image limits, output as user-role image blocks after a summary stub.
- Effort: 4-6 days port + eval harness (OCR fidelity on code/paths per model family) before it can be default.
- Reasons for no-go: fidelity loss on identifiers/code is exactly what coding sessions need; jcode already has provider-native compaction for OpenAI and fast LLM summaries; image handling conflicts with emergency image stripping and per-provider image size caps; binary size/font licensing overhead; benefit unproven outside omp evals. Revisit as an opt-in experimental method if an eval on jcode transcripts shows >=90% identifier recall at >=5x compression.

### Tool schema shaking

Nothing to port: jcode's deferred MCP tools with token threshold and tool profiles already cover it, and omp's shake does not address schemas.

## Recommendation

- Shake: GO. Small, pure, MIT, directly complements jcode's emergency-only truncation. Ship off-by-default, measure with cache telemetry, then default `pre_compact`, and `cache_expiry` for TTL providers.
- Snapcompact: NO-GO now, revisit with an eval.
- Tool schema trimming: already done in jcode.
