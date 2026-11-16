# Daemon-owned local rollout

`jcode rollout` prepares a matched CLI/Desktop pair and submits a durable intent
through the daemon's owner-only debug socket. It returns the transaction ID, not
a claim that deployment already finished. `jcode rollout --status` reads the
journal even when no daemon is reachable. The production protocol command
`server:rollout_status` exposes the same JSON to future Desktop consumers.

## Ownership boundary

Build and package remain CLI-side, intentionally. They do not change installed
channels or the application, can be expensive, and can be restarted by rerunning
the command. Existing artifact version probes prevent rebuilding a valid pair.
The daemon owns every installed mutation starting at stage. Killing the CLI,
its agent tool, or its process group after submission does not cancel execution.
Normal tool kill-on-drop and reload cleanup are unchanged.

The journal is `JCODE_HOME/rollout/journal.json`, normally
`~/.jcode/rollout/journal.json`. Every mutation has a durable intent before the
operation and a result after it. Each replacement uses a private temporary file,
`fsync`, rename, then parent-directory `fsync`. Progress also goes to
`rollout/progress.jsonl` and the daemon log. A nonblocking OS file lock admits one
executor. The lock descriptor closes during exec. Startup acquires it again and
resumes the same journal, rather than submitting another rollout.

## Generation transition

1. Probe the candidate using `version --json`, checking the reported `git_hash`.
2. Install the immutable version and probe the installed binary again.
3. Journal and update the shared-server and current channels.
4. Drain the canonical turn-activity snapshot by default, including the initiating
   turn because submission already returned. No streaming-marker/roster union
   decides quiescence. `server:turn_activity` exposes the snapshot and its derived
   busy session IDs to CLI planning. `--now` is allowed for agents because durable
   recovery owns continuation admission.
5. Probe again, persist `exec_pending`, and request a pinned reload.
6. Once the replacement accept loops are live, startup records `serving` and
   resumes Desktop swap and verification.

The previous daemon is identified from its compiled serving image, not from a
mutable channel marker. Before any shared-daemon spawn chooses a channel, it
checks for a dead `exec_pending` generation with no serving record. Such a spawn
restores the previous shared-server/current selection and bundle, records
`rolled_back`, and uses the previous immutable binary directly, bypassing
normal automatic channel advancement. A transaction records its socket so a
custom-socket daemon cannot reconcile another runtime's rollout.

## Desktop transaction

The incoming bundle is copied and synced to a temporary sibling of the installed
application. The previous bundle is copied to a separate sibling backup. The
installed application is never removed before copying its replacement.
macOS `renamex_np(RENAME_SWAP)` or Linux `renameat2(RENAME_EXCHANGE)` atomically
exchanges the installed and incoming directories. The backup is retained.
A persisted incoming directory inode identifies a completed exchange even if the
daemon crashed before writing its result. Rollback copies the retained backup to
a sibling and atomically exchanges it, making repeated rollback safe.

The production application path defaults to `/Applications/Jcode.app`.
`JCODE_ROLLOUT_APP` overrides it in both CLI planning and daemon execution.
Overridden applications are launched directly, never through global macOS launch
services. Submission rejects a path different from the daemon's configured path.
Verification checks the packaged CLI hash, Desktop commit, Sparkle key, and a
`search_sessions` request to the actual Desktop-owned bridge. Kernel executable,
parent, HOME, runtime directory, and socket provenance must match the bundle.
Worker continuation is reported only from durable recovery outcomes, not from
roster `ready` or successful bridge connectivity.

## Isolated acceptance

No acceptance step may restart the user's daemon or write the real application.
Use a private HOME, JCODE_HOME, JCODE_RUNTIME_DIR, JCODE_SOCKET, and
JCODE_ROLLOUT_APP. Do not set JCODE_TEST_SESSION for real exec tests because it
intentionally disables exec.

`scripts/accept-rollout-daemon.py --old <binary> --new <binary>` uses two actual
CLI generations with different compiled labels and a small fixture Desktop that
owns a real bundled API bridge. It checks caller SIGKILL after acknowledgement,
real daemon exec and startup replay, a SIGKILL after backup copy before swap,
and a candidate that passes the version probe but exits immediately on serve.
It additionally corrupts the incoming Sparkle metadata and proves verification
failure restores both the previous application and previous daemon generation.
A PATH-scoped fixture `ditto` creates the backup crash window without adding a
fault-injection environment variable to production Rust code.

Acceptance on November 17, 2026 passed all five outcomes using distinct actual
CLI labels `a11111111` and `b22222222`: caller death, same-PID exec/startup resume,
backup/swap crash replay, unserved-generation fallback, and pair rollback after
verification failure. Pong confirmed the new hash, incremented generation and
journal reload ID. The canonical activity endpoint and `rollout --status` were
read through their real interfaces. The fixture Desktop owns a real bundled
API bridge, not a simulated successful verification response.
