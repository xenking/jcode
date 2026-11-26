# Daemon-owned local rollout

`jcode rollout` prepares a matched CLI/Desktop pair and submits a durable intent
through the daemon's owner-only debug socket. It returns the transaction ID, not
a claim that deployment already finished. `jcode rollout --status` reads local
ownership and journal state even when no daemon is reachable. The production
protocol command `server:rollout_status` exposes the daemon journal.

## Ownership boundary

Build and package remain CLI-side, intentionally. They do not change installed
channels or the application, can be expensive, and can be restarted by rerunning
the command. Existing artifact version probes prevent rebuilding a valid pair.
The CLI resolves both Git refs once and records their full SHAs, the session ID
if available, the PID, and the deployment destination. From admission through
submission, only that owner can change `JCODE_HOME/wt/rollout` or build and
package through `jcode rollout`. The mutating child processes inherit the lock.
SIGKILL of the CLI does not let a second owner change their checkout or kill
the remaining build. The next caller may reuse its verified artifacts once
the children exit.

Send pinned `--jcode-ref` and `--desktop-ref` values when coordinating rollout:

- Identical requests, and requests covered by descendants in both repositories,
  join the owning transaction at durable submission. A join returns its ID and
  status, not a new build or a claim that rollout completed. A CLI-only owner
  cannot cover a request that needs Desktop. A join does not change the owner's
  drain policy, even if the joining caller passed `--now`.
- A terminal caller with a newer compatible pair waits for the existing rollout
  to complete before starting preparation. A session caller receives the pending
  pair and owner instead of blocking the daemon's drain with its active turn.
- A divergent pair or different destination is refused. The output includes the
  owner, status and both full pairs. Jcode attempts a direct `notify` with
  `wake=false` when both session IDs are available. The operator or coordinators
  decide the next reviewed pair. Rollout never merges, rebases, force-checks out
  user changes or replaces the in-flight intent to reconcile a conflict.

After submission or join, a session can call `rollout_wait` with `rollout_id`,
`cli_sha`, and `desktop_sha` from the owning transaction. It must be the only
tool call in the model's response. It reads the terminal receipt for that exact
transaction. A bare shell sleep or a background wait does not register as a
rollout waiter. The wait is ignored by that transaction's drain only while its
foreground turn is waiting exclusively on the registered call. Another active
tool or a waiter for a different transaction still blocks. Idle upgrades still
see these turns as active.

Reload checkpoints the unfinished typed tool call and replays it in the new
daemon. A later transaction archives the previous terminal journal under
`rollout/receipts/`, so replay can return the old outcome without reading the
new transaction or running a build. `jcode rollout --status` shows the live
blockers and reasons when the owning daemon is reachable during drain. The
offline journal still exposes the phase, owner, and selected pair. It also
retains the `blockers` from a drain timeout after rollback. If the daemon is
reachable, `retry_blockers` describes current activity separately, so a client
can wait to retry without treating the historical blockers as still active.

Desktop submits the two full SHAs approved by the user with `--apply-reviewed`.
That mode prints a JSON `id` receipt to stdout and sends build progress to
stderr. Desktop uses the receipt to avoid following somebody else's rollout
even if it selected the same two commits.

The submitting CLI re-probes artifact versions against the pinned commits.
`handoff.lock` serializes a change of preparation record with submission. If
the caller dies during submission, the next caller either finds the daemon
journal or replaces the reservation so a late submit is rejected. A nonterminal
journal prevents another preparation even while the daemon restarts. CLI
admission never acquires `executor.lock`, so it cannot block startup recovery.

The lock covers cooperating `jcode rollout` clients that share one `JCODE_HOME`.
It does not serialize unrelated Cargo commands, old rollout clients, or separate
homes that point at the same target or Applications directory. The owner checks
available space on `JCODE_HOME` and the target filesystem before checkout or
build. It does not reserve that space against other processes. Upgrade the daemon
to one that supports `rollout_preparation_v1` before running the new CLI against
its live installation. The legacy bootstrap procedure is separate.

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
4. Drain the canonical turn-activity snapshot by default. Exclude the initiating
   session and eligible typed waiters for this transaction. The canonical
   projection excludes checkpointable pending asks, which reload captures durably
   without waiting for an answer. They remain busy for idle-upgrade policy. No
   streaming-marker/roster union decides quiescence. `server:turn_activity` exposes
   the snapshot and both derived busy/drain session lists to CLI planning.
   Reviewed rollouts atomically fence admissions once only checkpointable turns
   remain and require durable checkpoint acknowledgement before replacing the
   daemon. A pending `ask` is re-presented on reconnect before the provider
   continues. If only Desktop changes, the daemon retains the pending question
   in memory while Desktop reconnects. The explicit `--now` option skips the
   drain and uses interruption and normal reload recovery instead.
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

### Desktop tabs during macOS rollout

Rollout and rollback of the app bundle stage each running Desktop process's
window manifests before requesting quit. They reopen the saved windows with
their own workspace snapshots, including the tab order, selection and unsent
drafts. This path does not use the 30-second crash-recovery cutoff. An idle
window's manifest remains valid for its process lifetime.

If a running Desktop process has no valid manifest, the rollout stops before
quitting it. The installed bundle is not replaced in that case. The handoff
uses the latest checkpoint written by Desktop, which normally updates once per
second. It cannot recover edits that have not reached that checkpoint.

### Open an archived workspace separately

`Resume` opens an individual session's history without replacing your current
workspace. It does not load the workspace layout or unsent drafts.

To view an entire archived workspace, use `--restore-state` in a Desktop
instance with separate `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, and
`JCODE_DESKTOP_RESTART_DIR`. Extract `.snapshot` from the archive into a new
owner-only file. The app consumes this file, so never pass the original
archive. Keep the working Desktop open and do not replace its recovery file.
Use a private `JCODE_HOME` and `JCODE_RUNTIME_DIR` as well if you need the
view to be completely disconnected from the live runtime.

### Regression check

Run `scripts/accept-rollout-tabs.py --old OLD_CLI --new CLI_UNDER_TEST
--bundle DESKTOP_APP` on macOS. It copies the app into a temporary directory
and verifies two native windows, ordered tabs, drafts, expired timestamps,
verification-triggered rollback, and refusal to quit without a manifest. The
local provider fixture must receive no turns. The check keeps the CLI daemon
generation fixed while exercising the app-bundle transaction.

## Isolated acceptance

No acceptance step may restart the user's daemon or write the real application.
Use a private HOME, JCODE_HOME, JCODE_RUNTIME_DIR, JCODE_SOCKET, and
JCODE_ROLLOUT_APP. Do not set JCODE_TEST_SESSION for real exec tests because it
intentionally disables exec.

Run `scripts/accept-rollout-multi-owner.py /path/to/jcode` on macOS to check
concurrent preparation. It runs real rollout clients with private Git worktrees,
fixture build/package commands, and a private submission socket. It covers
identical and newer requests, divergent requests, owner death with a surviving
build, and an artifact whose version changes before submission. It does not
compile a release or install a bundle.

`scripts/accept-rollout-wait.py /path/to/jcode` checks stale preparation
rejection, two typed waiters, a working session that still blocks drain, and
recovery of both waits across a forced exec of the same binary. It uses a
loopback provider and a synthetic journal on the private daemon. It does not
install a build or package.

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

## Machines without Xcode

Precompiled Desktop shaders need Xcode's Metal toolchain. Rollout and the paired
smoke ask `xcrun --sdk macosx --find metal` under the developer directory they
will use: `DEVELOPER_DIR` when set, else Xcode when installed, else
`/Library/Developer/CommandLineTools`. When `metal` is missing, they build
Desktop with `--features gpui_platform/runtime_shaders`, so shaders compile when
the app starts, and the plan prints that choice on the `package-desktop` line.
Packaging passes the feature through `DESKTOP_CARGO_FEATURES`. A Desktop commit
whose `scripts/package-macos.sh` predates that variable is refused with a clear
message instead of failing later inside the shader build.

## Paired acceptance smoke

Before preparing a CLI and Desktop rollout, `jcode rollout` requires a local
`PASS` receipt for the full resolved Git SHA pair. `--dry-run` checks it too.
`--status`, `--check-update`, and `--no-desktop` do not require a paired receipt.
The refusal prints the smoke command with the exact two SHAs. For example:

```sh
python3 scripts/accept-smoke.py \
  --jcode-sha "$(git rev-parse origin/master)" \
  --desktop-sha "$(git -C ../jcode-desktop rev-parse origin/main)"
```

After acquiring the build lock, the runner checks for a valid `PASS` receipt for
the same SHA pair and, if found, prints its path without rebuilding. Use
`--force` to remove that receipt and run the smoke again.

The runner requires a macOS graphical login, Xcode or the Command Line Tools,
and `justrust`. Install
`tesseract` to verify painted text when Desktop does not expose visible rows.
The runner builds
the exact commits in disposable sibling worktrees and launches only private
processes: a CLI daemon and bridge with isolated sockets and `JCODE_HOME`, a
loopback fixture provider, and hidden Desktop windows. It checks send in Clean
Chat, queue while busy, `/btw` via Enter, answering an `ask`, image paste,
scrolling in a long transcript, and restoring a window after restarting both
Desktop and the daemon. The paste check uses private image bytes through the
native Paste action. It does not read or change the macOS system pasteboard.

The runner writes `$JCODE_HOME/rollout-acceptance/<cli-sha>-<desktop-sha>.json`
with the schema version, exact pair, per-flow outcomes, overall outcome, and UTC
finish time. A failure is `ISSUES`; a flow that could not run is `BLOCKED`.
Neither authorizes rollout. The receipt points to retained evidence for the run.
Use `--skip-acceptance-smoke` only in an emergency. A real invocation logs
the override and both target SHAs. A dry run does not write an override log.
