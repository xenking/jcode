# Cold-cache request barrier, 2026-11-17

## Observed failure

Read-only evidence: `~/.jcode/logs/jcode-2026-11-17.log`, installed revision `7e6f49012`. Times below are log-local AEDT (UTC+11).

| Time | Evidence |
| --- | --- |
| 23:20:18.703 | `shake_cache_expiry: elided 46 regions, ~48168 tokens saved` |
| 23:20:18.705 | `strategy=shake_cache_expiry tokens_before=442714 tokens_after=256155 changed=true` |
| 23:20:18.717 | `compaction_start: trigger=snapcompact_cache_expiry, active_messages=805, cutoff=795, estimated_tokens=256155` |
| 23:20:18.717 | `snapcompact_cache_expiry: cache ttl=3600s lapsed, tokens_before=256155, started=true` |
| 23:20:18.718 | `messages_for_provider (compaction): returning 806 messages` |
| 23:20:35.782 | Foreground API complete: `input=41 cache_read=28268 cache_write=328243` |
| 23:20:35.789 | `reason=expired missed=405545 expected=433813 read=28268`, idle 4h48m |
| 23:21:21.423 | Background cold-cache compaction finished in 62.70s |
| 23:21:30.481 | Applied on a later request: `pre_tokens=361346 post_tokens=129227` |

The trigger WAS wired and DID run. Shake alone did not get below 200K. Starting asynchronous snapcompact was not a barrier: the foreground request was constructed immediately, while the compaction artifact was still being generated. Seven foreground requests used the large context before the artifact was applied. The user-visible 433K was the previous provider-observed baseline, not an estimate of the already-shaken outgoing request. Actual first cold request input was 356,552 tokens including cached input (41 + 28,268 + 328,243).

Do not subtract the logged shake savings from the provider baseline: local character estimates, observed provider tokens, and archive/image overhead are different measurements. In particular, `record_shake` clears the stale observed-input baseline.

## Wiring and TTL

- Both daemon streaming and non-streaming turns construct messages in `jcode-app-core/src/agent.rs::messages_for_provider`. Streaming enters through `agent/stream_request_preparation.rs`; non-streaming through `agent/turn_loops.rs`.
- Remote TUI and Desktop clients use the daemon's agent streaming path. Desktop calls `JcodeClient::send_message_with_reminder` in `jcode-desktop-harness/src/harness.rs`; the agent reserves and executes streaming turns through `agent/turn_execution.rs::run_reserved_streaming_mpsc`. The local TUI also calls the agent, so the request boundary, not a client idle timer, owns enforcement.
- `cache_ttl_secs = 0` means provider-derived TTL, NOT disabled. `cache_expiry_ttl_secs` falls back to `provider::shake_cache_ttl_for_provider_model`.
- Anthropic/Claude selects 3600 seconds when `anthropic_cache_ttl_1h` is true, otherwise 300. The log explicitly records 3600.
- The expiry clock is the successful response completion recorded by `note_provider_response`, not request start. This avoids expiring during a long streaming response.
- Previously `plan_shake_with` considered the ordinary compaction threshold before cache expiry. A vision provider with snapcompact preferred could therefore skip the local shake on a cold, already-large request. The cold reason must take priority over method ordering intended for normal warm threshold compaction.

## omp comparison

Read-only `~/.omp/agent/config.yml`: `idleEnabled: true`, `thresholdTokens: 550000`, `idleThresholdTokens: 550000`, method order `remote, snapcompact, handoff, shake, soft`. Thus a 433K context is below this installation's ordinary idle compaction threshold.

Installed implementation is in `~/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/session/idle-maintenance.ts`:

- `#armCompaction` requires idle enabled, positive threshold, nonpersistent cache, and current context at/above threshold.
- `#compactionDelayMs` schedules a long (1h) entry shortly before expiry so summarization can still read the warm prefix. Short/unknown/expired entries use the configured idle delay.
- `#onCompactionDue` rechecks settled/blocked state and tokens before calling `runIdleCompaction`.
- `agent-session.ts::runIdleCompaction` defers while an asynchronous wake is pending and delegates to session maintenance.

The checkout `~/projects/github.com/xenking/pi-o-my/docs/context-and-compaction.md` documents profile-specific thresholds and its separate cache-expiry shake integration. Jcode's pre-request expiry hook is not the same as omp's idle scheduler: a background task alone cannot guarantee a small next request.

## Acceptance

Regression tests run on daemon request preparation with a 433,512-token local estimate, expired response timestamp, enabled shake, and a residual prose region too large for shake alone. Both explicit TTL=300 and `cache_ttl_secs=0` with Anthropic 1h produce a first outgoing message estimate of 21,042 tokens, below 200K, with a shake status event recorded. A tool-heavy 419,512-token fixture reaches 21,868 using shake alone, without launching compaction. Warm-cache preservation and a fail-closed protected-tail scenario are covered separately. These are deterministic local estimates, not billed Anthropic token counts.

The shared async daemon boundary now waits for the cold-cache artifact to be applied before building a request. It awaits the compaction lock instead of falling back to raw history. A cold-cache artifact uses a tighter generation budget derived from the expiry threshold. Timeout, cancellation, failed compaction, or an unreducible tail cannot silently send the oversized prompt. Ordinary warm-cache background compaction remains asynchronous.

Idle TUI notices announce pending reduction instead of quoting a stale cached baseline as the outgoing size. Daemon status events carry the measured pre/post estimates, and the final result remains in the TUI transcript. Runtime verification uses a private fake-provider daemon and private HOME/session storage, never the user's daemon or configuration.
