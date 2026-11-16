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
   turn because submission already returned. The canonical `drain_sessions`
   projection excludes checkpointable pending asks, which reload captures durably
   without waiting for an answer. They remain busy for idle-upgrade policy. No
   streaming-marker/roster union decides quiescence. `server:turn_activity` exposes
   the snapshot and both derived busy/drain session lists to CLI planning.
   `--now` is allowed for agents because durable recovery owns continuation
   admission.
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

## Bootstrap from a legacy daemon

<a id="legacy-bootstrap"></a>

The installed daemon on this Mac was `483d7848f`, before `rollout_submit`,
`rollout_status`, and canonical turn activity existed. The new CLI probes
read-only `server:rollout_status` before git fetch, checkout, build, package,
stage, or channel changes. An unsupported daemon is rejected with exact manual
commands. A disabled general debug socket is also diagnosed without trying to
enable it or mutate the daemon.

There is deliberately **no automatic legacy fallback**. The old Reload request
contains only `id` and `force`, not an immutable target. Its normal reload path
reselects the newest candidate from its channel/selfdev flavors during exec.
Pretending that request was a pinned daemon-journal rollout would be false.
This one-time operator migration is the explicit exception. Subsequent rollouts,
including the first Desktop swap, use the new daemon journal normally.

### Commands for this Mac

Run these **by hand in a separate Terminal**, after active turns finish and
concurrent builds stop. Do not run them from an agent bash tool: the legacy
reload aborts that tool. The implementing agent does not execute these commands
against the user's runtime or `/Applications/Jcode.app`.

The validated candidate remains outside the deleted worktree as
`~/.jcode/scratch/rid-rollout-binaries/bootstrap-jcode`. Its `version --json`
reports the checked commit. The helper defaults to the existing legacy CLI
`~/.jcode/builds/versions/483d7848f/jcode`.

```bash
repo="$HOME/projects/github.com/xenking/jcode"
candidate="$HOME/.jcode/scratch/rid-rollout-binaries/bootstrap-jcode"
helper="$HOME/.jcode/rollout/bootstrap-legacy-rollout.py"
git -C "$repo" fetch origin
mkdir -p "$HOME/.jcode/rollout"
git -C "$repo" show origin/master:scripts/bootstrap-legacy-rollout.py > "$helper"
python3 "$helper" --binary "$candidate"           # read-only review
python3 "$helper" --binary "$candidate" --apply   # explicit operator migration
```

The `--apply` run prints resolved paths and commands, then:

1. Validates the candidate's JSON label. Copies and syncs that exact binary to
   `~/.jcode/builds/versions/<label>/jcode` through a private sibling, probes the
   copy, and hard-links it into place without overwriting an existing immutable
   version. Existing versions must report the same label.
2. Saves previous current/shared-server targets and marker contents in
   `~/.jcode/rollout/manual-bootstrap-previous.json`. Atomically replaces each
   `~/.jcode/builds/{shared-server,current}/jcode` symlink with that immutable
   binary, then atomically writes and syncs each `*-version` marker. The freshly
   copied candidate must remain the newest legacy reload candidate. Stop other
   builds until handoff finishes.
3. Asks the **old** CLI to use its normal graceful legacy reload path:
   `~/.jcode/builds/versions/483d7848f/jcode --no-update server reload --force --json`.
   This is an explicit operator action, not `server stop --force`. It does not
   deliberately kill live sessions, but does not claim recovery of sessions the
   old generation never recorded.
4. Waits for the selected socket's Pong to report the new label, immutable
   executable path, and daemon PID matching the kernel socket peer PID. A
   different fresh build selected by the legacy resolver is a hard failure, not
   success based on marker files. The first new generation may be generation 1
   because the old daemon did not carry the new identity envelope.
5. Executes the new immutable CLI's
   `--no-update rollout --jcode-ref <label>`. It prepares matched Desktop from the
   fork's `origin/main` and submits to the now-compatible daemon. **The new
   daemon**, not this helper, copies the bundle beside `/Applications/Jcode.app`,
   retains `.Jcode.<rollout-id>.backup.app`, and uses atomic directory exchange.
   There is never a remove-old-before-copy window. The helper waits for
   `completed`, including actual bridge/Sparkle verification, and prints
   `backup_app` and durable recovery receipt counts.

`--daemon-only` deliberately stops after step 4, leaving Desktop unchanged. Run
the printed rollout command later. `--legacy-cli <path>` supports isolated
fixtures or another known legacy installation. The helper refuses `--apply`
when `JCODE_SESSION_ID` is set. It never acts without explicit `--apply`.

### Verification and failure handling

After the helper completes:

```bash
candidate="$HOME/.jcode/scratch/rid-rollout-binaries/bootstrap-jcode"
"$candidate" --no-update rollout --status
"$HOME/.jcode/builds/current/jcode" version --json
"/Applications/Jcode.app/Contents/MacOS/jcode" version --json
"/Applications/Jcode.app/Contents/MacOS/jcode-desktop" --version
```

Status must have `phase: completed`, `serving: true`, `desktop_swapped: true`,
the selected label, and an existing `backup_app`. Peer-verified
`bootstrap_serving` output proves the daemon, rather than marker files, changed
generation. A subsequent failed Desktop verification restores the pair through
normal daemon compensation.

If the **manual legacy handoff itself** fails, stop and inspect its error and
`manual-bootstrap-previous.json` before another reload. That boundary cannot
provide the new daemon's crash-loop guarantee before the new daemon starts.
Do not report success or remove either application in that state.
