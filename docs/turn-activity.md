# In-process turn activity

`jcode_app_core::turn_activity::global()` owns runtime-local admission and lifecycle facts. Roster status, connection senders and streaming are not turn ownership signals.

## Producers

- `Agent::claim_server_turn` registers admission before returning the accepted lease. Its reservation retains the activity guard until execution and all queued publication pins are dropped.
- The active execution registry publishes a guard for execution, including non-server paths.
- Tool execution publishes a guard for every resolved tool. `ask` is represented as `ask_blocked`, not as idle. Running-tool guards also cover detached waits.
- Reload requests a checkpoint explicitly. The execution owner acknowledges it only after a successful durable save. Completion, roster changes and missing senders are not acknowledgements.

A snapshot is serialized from one locked state and includes a revision, admission-fence state, and a phase, active flag, checkpoint-pending flag and human-readable reason for every known session. Phases are `admitted`, `in_tool`, `ask_blocked`, `checkpoint_requested`, `checkpointed`, and `terminal`.

## Consumer policies

| Consumer | Shared facts | Policy |
|---|---|---|
| Forced reload capture | Snapshot recovery candidates | Close admission first, capture busy sessions and the initiator even without legacy reload context. Roster supplies headless metadata only. |
| Forced reload checkpoint wait | Explicit request and durable acknowledgement | Signal connection and execution interrupt handles. Exclude the initiator only from waiting. Include sender-less sessions in timeout diagnostics. Forced reload may continue after reporting unfinished sessions. |
| Idle runtime upgrade | Admission and publication ownership | Atomically prove quiescence and close admission. Recheck under the same lifecycle mutex through synchronous exec. Preserve the existing bus delivery checkpoint fence. |
| Idle hibernation | Snapshot busy state | Busy turns cannot lose caches. Retain the cache reservation and idle-duration checks, but never use roster `running` as ownership. |
| Rollout drain | Snapshot `drain_sessions()` | Wait for admitted/computing/tool work, but not checkpointable asks or already acknowledged checkpoints. `busy_sessions()` still includes their ownership for idle-upgrade safety. The daemon and root CLI consume the same serialized snapshot. Recovery `started` receipts, not roster `ready`, define resumed work. |

An ask is checkpointable without an answer, but still busy for idle upgrades until its ownership is released. Checkpoint acknowledgement does not release an admission or queued-publication lease.

## Focused verification

The initial production tool regression failed before its lifecycle hook. The real server admission regression failed before the reservation owned an activity guard. Both passed after their producer hooks were added.

| Acceptance | Check |
|---|---|
| Tool active while roster lags | Production running-tool hook and recovery capture with absent/ready roster entries |
| Admission lease without streaming | Real `Agent::claim_server_turn` and publication-pin lifetime test |
| Initiator captured, excluded only from wait | Durable recovery capture without legacy context and controlled interrupt/checkpoint participants |
| Timeout identifies unfinished session | Sender-less and partially acknowledged participant tests, including trace diagnostics |
| Turn accepted between observation and exec fence | Admission race test and runtime-upgrade final proof rejects late execution |
| Ask checkpointable, not busy forever | Controlled ask guard and explicit checkpoint receipt, independent of roster/user answer |

Run focused gates with:

```sh
justrust test -p jcode-app-core turn_activity::tests --lib
justrust test -p jcode-app-core server::reload::reload_tests --lib
justrust test -p jcode-app-core idle_hibernation --lib
justrust test -p jcode-app-core runtime_upgrade --lib
justrust test -p jcode-app-core turn_ownership --lib
justrust clippy -p jcode-app-core --all-targets -- -D warnings
```

The isolated runtime acceptance fixture is `tests/e2e/turn_activity_daemon.py` (optional second argument: expected commit prefix, asserted against the copied executable version). Pass an explicit freshly built binary. It copies that executable, uses a temporary HOME and private Unix socket, serves a localhost-only fixture provider, starts real bash and ask tool participants, and performs a real exec replacement. It checks canonical capture and durable checkpoint receipts, and writes a JSON evidence receipt under `JCODE_SCRATCH_DIR`. It never uses the installed app or the user's daemon/socket.

### Recorded real-runtime outcome

On 2026-11-16 at 15:29 UTC, the fixture passed against reader commit `1c2a1bdc6`, including admission commit `1dc1be8f6` and recovery checkpoint fix `19dece0d7`. The copied executable reported `jcode v0.99.1576-dev (1c2a1bdc6, dirty)` and SHA-256 `046873e54457afbabff8c084c824f6ad3643814da91f8dd85c1a5bea389544e9`. The dirty state consisted of the untracked acceptance script and this document.

Both real participants were captured with `active: true`: bash as `in_tool` with reason `executing tool bash`, and ask as `ask_blocked` with a checkpointable reason. The checkpoint trace had `unfinished: []`, excluded only the control initiator, and was followed by real `exec_start` and replacement `socket_ready`. The fixture receipt also records the session IDs, request ID, timestamp and executable digest.

The preceding runtime run failed on the earlier recovery hook: both sessions durably saved and released execution, but retained checkpoint obligations because the hook checked an interrupt flag already reset by guard destruction. The recovery owner fixed the hook to consume the canonical checkpoint obligation. Repeating the same real workflow then passed.

A separate stale shared-target artifact was rejected during acceptance because it lacked canonical capture fields. Force-refreshing source mtimes, rebuilding through the configured target, copying the artifact to a unique scratch path and asserting its commit prefix prevented an unrelated worktree binary from substituting for the tested runtime. No target override, installed application change or user-daemon restart was involved.
