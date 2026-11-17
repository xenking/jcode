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

The same key is supported by per-model compaction overrides. `auto` chooses an available luna, haiku, flash, mini, or nano route, ordered by available reference-cost metadata. With no cheap route, extraction is skipped and the verbatim index remains authoritative. Opus is never selected, including an explicit Opus setting. The selected route lives on a fork, never on the active session provider.

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

The incremental benchmark splits the 53 real new messages chronologically into 26 and 27 messages. It measures a legacy cold pass and then a genuinely appended warm tail. It does not duplicate already archived messages or inject synthetic session content. The pixel check decodes every copied-fixture PNG and compares it with the original serial renderer, retaining exact pixels and geometry rather than spending on a new OCR call.
