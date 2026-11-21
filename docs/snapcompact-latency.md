# Snapcompact latency, 2026-11-17

## Measurement boundary

The opt-in `snapcompact_copied_session_*_benchmark` tests run the production artifact pipeline against an explicitly supplied **copy** of a session, with a fake external provider. They do not call a vendor, modify the live session, or include compilation in reported pipeline wall time. Use an isolated `JCODE_HOME` and `JCODE_TEST_SESSION=1`.

The copied fixture has 6,202 stored messages, a compacted prefix of 6,149 messages, 53 new messages, and an 80-page prior archive containing 1,661,416 source characters. This is the current saved snapshot, not a reconstruction of the earlier 131-message incident. The fake decisions response is immediate in the baseline.

| Baseline phase | Wall time |
| --- | ---: |
| Prepare | 38 ms |
| Render all 80 pages | 11,011 ms |
| Index | 49,368 ms |
| Decisions, fake Opus provider | 0 ms |
| Base64 | 3 ms |
| Artifact pipeline total | 60,424 ms |

After the integrated change, the same 53-message copied input completed in **1,756 ms** (34.4× less local wall time). This comparison uses the same unoptimized test profile and excludes compilation and external inference.

| Phase | Cold legacy archive, 53 new messages | Warm appended tail, 27 new messages |
| --- | ---: | ---: |
| Prepare | 36 ms | 26 ms |
| Render | 1,247 ms, 80 new pages | 307 ms, 3 new / 77 reused pages |
| Index | 468 ms | 451 ms |
| Base64 | 3 ms | 2 ms |
| Artifact pipeline total | **1,756 ms** | **792 ms** |
| Decisions | deferred until after apply | deferred until after apply |

The warm run follows a real first pass over the first 26 of those 53 messages (1,886 ms), then appends the remaining 27. It is not directly interchangeable with the 53-message cold comparison. Both final archives have 80 pages, 1,674,875 source characters, and 13,984,968 base64 bytes. Valid saved page coordinates replace some formerly `UNKNOWN` index references, so the corrected final index is 110,980 rather than 111,002 characters.

A separate original serial-render profile measured 10,068 ms: font/layout 30 ms, raster 1,004 ms, PNG encoding 8,176 ms. `Compression::Fast` reduced serial rendering to 5,146 ms, but increased the fixture PNGs from 10,408,068 to 24,778,718 bytes. That tradeoff was rejected. Production retains the original `Best` compression and raster geometry, with at most eight page workers. Render subphase counters sum worker CPU wall durations and can exceed total parallel-render wall time.

## Incremental archive

New archives persist a renderer/geometry key and character-coordinate page ranges. A later pass reuses PNGs only when both the key and complete renderer input, including incoming dim state, match. Invalid, overlapping, out-of-bounds, or legacy metadata causes a safe rebuild. Legacy archives migrate on their first pass.

Existing page boundaries are preserved, including archives with an omitted-middle marker between pages. Only the previous partial tail and newly appended text are repaginated. Otherwise inserting the old omission marker into a fresh global pagination would invalidate every old tail page. Foveation still keeps the chronological head and tail within the existing page budget. Saved page ranges always refer to the saved source, not to the pre-foveation source.

## Optional decisions

```toml
[compaction]
snapcompact_decisions_model = "auto"
# Or an explicit provider/model route, for example "openai:gpt-6-luna".
# "off" disables optional extraction.
```

The same key is supported by per-model compaction overrides. `auto` chooses an available luna, haiku, flash, mini, or nano route, ordered by available reference-cost metadata. With no cheap route, extraction is skipped and the verbatim index remains authoritative. Opus is never selected, including an explicit Opus setting. The selected route lives on an independent execution-runtime fork, never on the active session provider.

Background routing must not activate a foreground profile or switch process-global credential environment. Only initialized runtimes with a matching credential mode are eligible. An uninitialized subscription/named profile or a route requiring a different credential activation is skipped, not activated as a side effect. Explicit configuration is therefore a preference subject to this safety boundary, not a request to mutate the live provider configuration.

Artifact generation returns an optional enrichment job. The manager starts it only after assigning the archive. Its input contains only newly archived index lines, not the full accumulated index. Completion sends its result before waking the session owner. The owner grounds and bounds decisions against the current index, persists enrichment without another compaction divider, and rejects results for a stale compacted prefix. Errors, missing routes, timeout, replacement, and shutdown do not block apply.

## Finish-to-apply delay

Generation completion does not grant ownership of the session's `Agent`. The completion listener awaits the same agent mutex held by an active model/tool turn. Applying also clears the provider session/cache state and persists the transcript, so bypassing that owner while its turn is in flight is not safe. The existing turn-boundary behavior is retained rather than replacing it with a concurrent session mutation.

New `snapcompact_apply_wait` logs measure artifact-ready to manager apply. `compaction_agent_lock_wait` separately measures the completion listener's agent-lock wait. Together they distinguish local generation from waiting for an active turn. The supplied historical 100-second gap cannot be uniquely attributed by the old logs, which lack these timers.

## Reproduce

Run only from a dedicated worktree, with enough free build disk, and point the fixture variable at the copy:

```sh
export JCODE_HOME="$HOME/.jcode/scratch/snapcompact-benchmark"
export JCODE_TEST_SESSION=1
export JCODE_SNAPCOMPACT_FIXTURE="/absolute/path/to/copied-session.json"
justrust test -p jcode-base --lib snapcompact_copied_session_pipeline_benchmark -- --ignored --nocapture
justrust test -p jcode-base --lib snapcompact_copied_session_incremental_benchmark -- --ignored --nocapture
justrust test -p jcode-compaction-core --lib copied_archive -- --ignored --nocapture --test-threads=1
```

The incremental benchmark splits the 53 real new messages chronologically into 26 and 27 messages. It measures a legacy cold pass and then a genuinely appended warm tail. It does not duplicate already archived messages or inject synthetic session content. The pixel check decodes every copied-fixture PNG and compares it with the original serial renderer: all 80 pages have identical pixels and the same aggregate 10,408,068 encoded bytes. Fonts, density, geometry and production PNG compression are unchanged. This preserves the image input relevant to the 2026-11-16 recall study. It is not a new paid model-recall experiment and does not claim unchanged LLM-generated decisions quality.

Behavioral regressions assert exact new-render counts on a second compaction, foveated page-boundary reuse, apply before a 30-second fake decisions response within a 2-second budget, eventual grounded decisions persistence without another compaction event, config/override round trips, and runtime-fork selection without foreground credential activation. Index tests compare output byte-for-byte with the previous algorithm, including Unicode, ambiguous/overlapping matches, source-boundary matches, and corrected page provenance. The copied real index comparison measured 47,840 ms for the frozen legacy implementation versus 469 ms cold / 470 ms optional-prior, with identical output bytes. Its opt-in 500 ms performance gate uses the median of three independently verified new-index runs, rather than a single scheduler-sensitive sample. The production caller supplies only prior source/index/range metadata, without cloning prior PNG payloads for index reuse.
