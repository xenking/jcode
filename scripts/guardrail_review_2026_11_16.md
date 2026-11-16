# Guardrail review, 2026-11-16 UTC

Scope: repair the new panic/error-handling regressions, retain intentional Option defaults and best-effort observations, then refresh syntactic baselines. No scanner, threshold, exclusion, or gate has been relaxed. No production or test file was split to satisfy a size threshold.

Initial census: panic 176 (baseline 147), swallowed-error-like 4150 (baseline 3877), wildcard reexports 19 (baseline 18), production size regressions 97, test size regressions 45.

After repairs: panic 147, swallowed-error-like 3990, wildcard reexports 18. The panic ceiling stays 147. The swallowed syntactic census drops by 160 from the observed 4150; its reviewed baseline grows from 3877 to 3990 because the remaining additions have the boundary-specific meanings below.

Reviewed size census: production tracks 111 -> 133 files and 225771 -> 268081 LOC, with 76 previously tracked files growing and 22 newly above 1200 LOC. Tests track 49 -> 58 files and 93457 -> 112081 LOC, with 37 growing and 9 newly above 1200 LOC. Final size regressions are 98 and 46, including regression tests and error-handling fixes from this work.

The swallowed-error scanner counts `let _ =`, `.ok()`, and `.unwrap_or_default()` syntactically. These also match discarded bools/counts, normal absent Options, classifier rejection, and notifications to disconnected observers. They are not all ignored Results. Retaining them requires the specific reasoning below, not blanket acceptance of I/O failures.

## Behavioral fixes

- SDK bundle identity hashing has a typed byte representation, not fallible JSON serialization. Concrete bridge launches propagate identity pin failures. Desktop-control exports explicitly name their public items. The TypeScript SDK exposes the same skill-message capability as its server.
- Provider diagnostic objects use typed object construction. Malformed vectors and missing canonical prompts return errors. Anthropic diagnostic fixture setup propagates failures. Standalone test helpers are test-only.
- Compaction font parsing retains a typed parse error. PNG dimensions use checked byte ranges and typed headers.
- Durable ask answers, queued prompts, reload stashes, goals, and todo state preserve corrupt/unreadable input rather than replacing it with empty data. Serialization occurs before destructive pending-state removal. A wrapped activity token cannot alias a live admission.
- Search-index update/prune operations are transactional. A failed source census cannot prune unseen sessions. SQLite/cache failures retain diagnostics and transient opening failures are retryable.
- Tool-core length framing rejects overflow and UTF-8 split boundaries. Codemode diagnostics retain correct coordinates for offsets inside a codepoint. Corrupt persistent codemode state is not overwritten. Ask rejects malformed responses rather than inventing a user decline.
- A broken execution-worker output pipe stops both transport directions. It cannot leave the worker waiting indefinitely for input EOF. Malformed wire input and non-Unicode command identities are errors.
- Worker metadata rejects partial/corrupt snapshots and journals. Present but malformed harness response lists produce correlated errors instead of a false empty history/image list.
- Base archive/journal operations distinguish missing from unreadable data. Startup metadata errors propagate. Todo/goals readers do not invent empty authoritative state. Rollout process censuses reject malformed or incomplete data before planning destructive actions.
- Runtime generation identities reject malformed inherited values. ACP/cloud/rollout paths preserve connection, identity, command-probe, journal, and response-body failure diagnostics.
- Cancellation registry poisoning preserves live signals, first-interruption reasons, session migration and RAII cleanup. It never reports a live turn as idle. Reload admission reads the durable session receipt once and propagates unreadable state before publishing a claim. Recheck errors stop execution with diagnostics.
- Queued skill validation distinguishes a genuinely untouched session with no snapshot from an unreadable existing session. Only the former uses the validated connection cwd. Ask response encoding errors never become a fabricated empty answer.

## Retained matches, by boundary

### CLI and runtime selection

- `src/cli/rollout.rs`, `rollout_host.rs`: optional version/build/legacy-daemon identity and executable probes return unknown on absent, unreadable or unrecognizable data. Unknown identity is not evidence of ownership and cannot authorize retirement. Strict process census and journal mutations propagate errors separately. Optional commit-list/history metadata cannot change the deployed revision. Test child cleanup matches are test-only.
- `src/cli/cloud_mux.rs`: an empty failed-command stderr has no last display line. That Option defaults to an empty error-detail label, not a successful transport result. Connection, child-wait, response-body and dispatch failures now retain diagnostics.
- `src/cli/commands/titles.rs`: a missing/non-Unicode file stem is only a failed-candidate report label; the actual Session is loaded from its Path and supplies its real ID. Missing report detail emits no extra note. Modified-time probe failures now log a warning.
- `jcode-build-support/runtime_generation_selection.rs`: failed canonicalization does not select an unproven managed reload target. Unknown file/modified times do not establish that a candidate is newer. Authoritative inherited identities and channel-marker failures are handled explicitly.
- `runtime_generation_socket.rs`: `let _ = stream` discards an unused stream reference on unsupported platform branches, not a Result. Peer PID observations remain unknown where unavailable.

### App-core

- `agent/turn_execution.rs`: missing queued client IDs, optional disabled-tool lists, missing optional MCP tool-name lists and optional extraction metadata are empty by domain definition. `/mode` is classified before stripping its exact prefix. `let _ = system_reminder` discards an unused value, not a Result. Stream/soft-interrupt events notify an observer after durable prompt admission.
- `agent/turn_streaming_mpsc.rs`: compaction, usage, tool and turn-state events are observer notifications. A disconnected event receiver does not reverse committed state or own the running turn. Missing optional display fields remain absent/empty.
- `mobile_push.rs`: platform/hardware probes and optional cwd/name lookups affect notification labels, not execution or permissions. Unavailable platform files are normal on supported hosts. Push persistence/queue failures are handled separately.
- `server/client_comm_message.rs`: missing swarm/channel membership yields no targets, not elevated permissions. Root/global routing checks still validate the sender and unique target. Fanout and soft-interrupt queue helpers return delivery counts/bools, not Result. They own their persistence diagnostics. Per-client Done/Error sends may fail when that observing client has disconnected.
- `client_comm_swarms.rs`, `client_lifecycle.rs`, `client_processing.rs`: observer Ack/Error/Done/status/history/queue notifications are best effort after the authoritative operation has completed or failed explicitly. The durable queue controls acknowledge only successful durable mutations. A closed receiver cannot authorize or persist work. Missing client IDs and snapshot labels are optional fields. Unused `title` is not a Result. Skill cwd and ask-response serialization errors are now explicit.
- `comm_session.rs`: no terminal snapshot means no extra terminal environment. Busy-agent `try_lock` is contention, not an I/O failure. Optional route observations remain unknown; model selection still passes the configured allowlist and provider selection. Explicit requested cwd, live-agent cwd and recorded member cwd retain priority. Persisted-parent cwd load now propagates corruption/read errors instead of silently selecting daemon cwd. Visible-session cwd resolution also propagates current-directory failures. Inherited preferences ignore only a genuinely missing untouched parent, not unreadable existing data.
- `history_paging_bounded.rs`: cursor decoding/parsing `.ok()` is immediately converted to a correlated invalid-cursor error. It does not swallow the failure or return empty authoritative history.
- `live_turn.rs`: a failed nonblocking agent lock means another owner is busy. It never creates a second owner. Completion/stop event receivers may disappear independently of lease release and cleanup.
- `reload_recovery.rs`: old message IDs are classified as queued receipt IDs only when their JSON is valid. Nonmatching ordinary message IDs are normal. Legacy ReloadContext consumers return Option, not Result, and are called only after a durable receipt is committed. Missing/failed observer event sends do not remove the recovery obligation. Durable session reads, journal writes and claim rechecks no longer silently default.
- `rollout_desktop.rs`: failed canonicalization, inaccessible process environment and invalid executable/UID probes cannot establish process ownership, so they refuse retirement. The Darwin argc conversion checks the byte range first. Non-platform `let _ =` bindings discard unused arguments, not Results.
- `swarm_watchdog.rs`: unavailable/poisoned advisory stall observations are unknown, not proof that a turn is idle. Turn ownership comes from the separate live-turn registry. Fanout and optional nudge helpers return counts/bools and own diagnostics; losing a watchdog observation cannot start duplicate work.
- `turn_failover.rs`: absent API-method metadata remains unknown while choosing only permitted provider routes. Model/connection/usage events notify observers after route application. They do not own provider execution or durable persistence.
- `tool/interactive.rs`: no attached ask routes is an ordinary reconnect state. An empty destination list selects the original response target, not a fabricated answer. Pending request removal occurs after successful serialization and durable answer handling.

### Base, protocols, parsing and caches

- `jcode-base/auth/oauth.rs`: missing optional interval/expiry fields use bounded protocol defaults. Failed error-body reads retain the actual read error, not an empty body.
- `config/display_summary.rs`: absent optional voice device configuration produces an empty display list. `mcp/protocol.rs`: absent serverInfo is display metadata, not a capability source.
- `memory.rs`: the newly added optional decision-client construction logs the failure with `inspect_err` before converting to None and using local deduplication. It does not erase a persistent memory write failure. Earlier unrelated backup-copy and prompt-memory omission matches are unchanged by this task.
- `mobile_sync.rs`: the new structured-tool size serialization calls `inspect_err` before converting failure to None. It omits only supplemental structured metadata, retains the normal prose wire path, and logs the reason. Existing file timestamp probes are unchanged by this task.
- `provider/auto_reset_trigger.rs`: malformed/overflowed optional timestamps fail classification and cannot authorize redemption.
- `session/images.rs`: optional image-marker classification rejects nonmarkers. Existing archive I/O is strict and cannot overwrite an unreadable archive.
- `session/render.rs`: formatting writes into a String use only built-in string/integer Display implementations. String's fmt writer cannot fail. No file/socket I/O is ignored.
- `usage/banked.rs`, `usage/cache.rs`, `usage/omp_banked.rs`, `usage/openai_response.rs`: absent/invalid optional provider utilization and unsupported platform observations remain unknown. No absent observation becomes free quota. Persistent cache and lock failures are advisory and either preserve last-good data or report unknown; I/O failures now retain diagnostics.
- `jcode-codemode/ts.rs`: absent JSON-schema `required` means optional fields by schema semantics.
- `jcode-command-risk/lib.rs`: absent find-exec payload produces an empty display label while retaining Confirm risk. It cannot grant execution permission.
- `jcode-compaction-core/snapcompact/{mod.rs,renderer/snapcompact_render.rs,serialize.rs}`: missing optional bundled-font metrics and labels have display defaults. Unrecognized revision/elision markers are not classified as known markers, so actual user content is retained. Font parse and PNG header errors are handled explicitly.
- `jcode-provider-core/picker_order.rs`: generation tokens are ASCII digits with 1 through 5 bytes before u64 parsing, so conversion cannot overflow. Long release stamps are deliberately excluded from generation sorting.
- `jcode-session-types/ask.rs`: invalid/overflowed human question numbering is not classified as a structured answer. This is not loading authoritative persisted state.
- `jcode-setup-hints/macos_hotkey_guard.rs`: failed canonicalization does not match the installed binary and refuses hook installation.
- `jcode-tool-core/lib.rs`: `digits.parse().ok()?` rejects malformed/overflowed parent-call framing IDs as unrecognized. It returns no decoded parent ID rather than accepting invalid framing. Length slicing now also rejects overflow and non-UTF-8 boundaries.
- `jcode-voice/soniox.rs`: a close failure after no audio or a complete transcript must not change a successful transcript into a false transcription error. Invalid provider rejection-code fields become generic rejection, never success.

### Harness and TUI

- TUI `btw.rs`: an unavailable cwd probe falls back to the current relative directory for terminal launch, not a different daemon identity. An invalid explicit socket is now rejected, and unreadable/unpersistable child state is reported instead of opening an unproven session.

- `jcode-harness-api/worker_meta.rs`: missing optional role/phase labels remain unknown. Missing snapshots may coexist with a valid journal. Unknown file timestamps are not accepted as fresh cache authority. Existing unreadable or malformed sources reject the metadata record.
- `jcode-harness-api-server/search_index.rs`: missing optional title/name/timestamp labels affect search display. Unknown fingerprints force a rescan; they never establish freshness. SQL read errors log diagnostics. Source census/pruning are strict and transactional.
- `translate/search_hooks.rs`: an explicit absent title resets the advisory indexed title to empty. This is a title-clearing operation, not an ignored failure. The transactional search-index writer owns SQL error diagnostics.
- `translate.rs`: absent optional compatibility response fields retain protocol defaults. Present malformed authoritative lists now return structured errors. Optional response-stat decoding failures log while preserving the valid transcript.
- TUI `commands_cloud.rs`: missing optional account/host metadata cannot authorize a cloud action. It directs the user to configuration instead.
- TUI `helpers.rs`: the local clipboard fallback returns bool, not Result. A successful OSC52 copy remains successful even if optional local clipboard duplication is unavailable. Missing host/cwd labels are display observations.
- TUI `input.rs`, `remote/key_handling.rs`, `remote/server_events.rs`: command prefixes are classified before stripping. Missing focus/extraction labels are optional presentation data.
- TUI `model_context.rs`: absent pending usage/expiry/cache labels are display-only. Failover routes use typed objects rather than fallible JSON object assumptions.
- TUI `replay_export.rs`: an arbitrary tool output is classified as a todo card only if the JSON stream and normalized payload are valid. Other text remains ordinary tool output; no authoritative todo file is read or overwritten here.
- TUI `focus_carryover.rs`: missing/invalid PID and environment observations are unconfirmed and cannot restore focus to an unproven process.
- TUI `info_widget.rs`, `ui_header/account_router.rs`, `ui_input.rs`: optional expiry, usage and status labels remain unknown/empty. Conversion overflow produces unknown display data, not a fabricated quota value. The positive threshold branch already proves the optional threshold exists.
- TUI `rex.rs`: missing optional REX_BLOCK disables the external integration.
- TUI `ui/copy_selection.rs`: a binary-search miss means there is no chrome region on that line, which is normal.
- TUI `window_visibility.rs`: the module explicitly treats failed platform probes as Unknown, and Unknown as visible. A failed observation cannot hide a terminal or suppress repainting. Missing ancestor/process data is normal across terminal hosts.

## Panic baseline

All newly introduced production panic/unwrap/expect sites were replaced with error propagation, nonpanicking alternatives or typed construction. The two retained storage guard panics are test/test-support-only isolation enforcement and are documented at their construction boundary. They cannot execute in production builds. The panic baseline may only stay level or decrease from 147.

## Size refresh policy

Both size baselines are refreshed together, separately from behavior/safety changes, after reviewing their complete growth census. Threshold remains 1200 LOC. This follows upstream practice c59700f46, 025d8793e and 4c757af18. These are observations of reviewed repository growth, not claims that larger modules need no future design work.
