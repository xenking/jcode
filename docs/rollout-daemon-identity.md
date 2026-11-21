# Rollout daemon identity: hardlink alias regression

## Reproduction and root cause (2026-11-16)

The daemon did not change process or build between plan and reload. Its kernel-reported **pathname** changed.

A temporary instrumentation build of `MacHost::daemon()` printed the selected PID, executable, socket, cwd and lsof exit status. All mutating host operations were disabled in that build, including reload, desktop swap, state writes, stage and rollback. Running the real `rollout --now` orchestration produced:

```text
self=5036 socket=/var/folders/1c/7fb7bbdn43s5jxfxkwnpyc080000gn/T/jcode.sock
cwd=/Users/xenking/.jcode/wt/rrc/jcode lsof_status=0 stderr=""
plan:   selected pid=49601 exe=/Users/xenking/.jcode/builds/versions/483d7848f/jcode
reload: selected pid=49601 exe=/Users/xenking/.jcode/shared-build/target/aarch64-apple-darwin/release/jcode
Error: IDENTITY PROBE: real reload is disabled
```

`stat -f '%d %i %l %N'` proved that both executable paths were hardlinks:

```text
device=16777234 inode=519245725 links=2 builds/versions/483d7848f/jcode
device=16777234 inode=519245725 links=2 shared-build/target/aarch64-apple-darwin/release/jcode
```

`install_binary_at_version()` deliberately installs with `hard_link`, falling back to a copy. Between plan and reload, `built_cli()` executes the release-path binary to check `version --json`. On macOS `proc_pidpath` can then expose the other pathname of the running image's vnode. `canonicalize` resolves symlinks, not hardlinks. `version_label_of` accepts only paths below `builds/versions`, so it returned `None` for the release alias. The correct daemon PID was selected both times. No leaked rollout client or PID wrap is required.

A separate read-only probe performed five iterations of `deployed(); workers(); deployed()` and retained PID 49601 and label 483d7848f throughout. The change appeared on the real build-validation path, not during worker enumeration.

The isolated `legacy_daemon_hardlink_alias_still_identifies_installed_version` test reproduces the relevant state with a child executable launched through `target/release/jcode` and hardlinked into `builds/versions/483d7848f/jcode`. Before the fix it returned the correct child PID but no label.

## Identity contract

* Real daemon Ping responses include optional `daemon_identity` with PID, executable pathname and compiled git hash. Older Ping responses and streaming keepalives remain compatible.
* Rollout validates a reported PID against the connected socket's kernel peer credentials. It uses the compiled hash for the label, not the possibly aliased pathname.
* For older daemons, the kernel peer PID identifies the listener unambiguously. Rollout excludes its own PID from this fallback. On macOS, lsof is restricted to that proven peer's mapped executable (`-a -p PID -d txt -F fDin`), not a global socket pathname scan. Device/inode matching identifies the immutable installed version even if the release-path artifact was subsequently replaced. Linux reads the running image identity from `/proc/PID/exe`.
* Ambiguous inode-to-label mappings and mismatched reported/peer PIDs fail closed.
* Both orchestration and `MacHost::reload()` skip an already-running target. A failed reload acknowledgement is followed by bounded deployment checks. An error cannot block desktop-swap if a fresh deployment observation proves the target, but an old or unidentified daemon still blocks it.

## Regression gates

The pre-fix library test run had three red failures: reload RPC failure after target activation blocked desktop-swap, the hardlink alias lost its label, and MacHost never queried the real UnixListener while another same-process client connection was open. Existing skip and wrong-target protections remained green.

Run the focused gates:

```sh
justrust test -p jcode --lib rollout -- --nocapture
justrust test -p jcode-protocol --lib daemon_identity
justrust test -p jcode-app-core --lib daemon_ping_reports_own_process_and_build_without_creating_session
justrust clippy -p jcode --bin jcode -- -D warnings
```

The rollout tests are in the **library**, not the binary test target. `--bin jcode rollout` matches zero tests and is not acceptance evidence. Tests use isolated sockets and child processes. The user's daemon is never reloaded or restarted, and the installed Desktop bundle is not modified.

Acceptance on macOS passed: the rollout suite (including child-process fixtures), all 117 active protocol tests (one pre-existing ignored test), all seven server startup tests, and strict binary clippy. The newly built CLI's read-only `rollout --dry-run` reported `daemon pid 49601 runs 483d7848f now` while the same process's `proc_pidpath` had already exposed the release-path hardlink alias. The same-process listener test also calls `MacHost::reload("483d7848f")` and proves that it sends only Ping, never a reload RPC.
