---
name: verify-jcode
description: "Verify Jcode's real Rust CLI/TUI after behavior or rendering changes. Launch an isolated fake-provider instance, drive terminal input, inspect frames and persisted side effects, and retain evidence after cleanup."
---

# Verify Jcode

Use the real built CLI, daemon, terminal input and renderer. Only the external model is fake. Read [features/README.md](features/README.md), then the feature affected by the change. A passing build is not runtime proof.

## Launch

Run from the repository root. Requires Python 3.9+, tmux, a Rust toolchain and justrust. macOS and Linux use Unix sockets. No API key, account, browser or paid provider is needed.

```bash
BUILD_DIR="$PWD/target/verify-jcode"
justrust build --target-dir "$BUILD_DIR" --profile selfdev -p jcode --bin jcode
BINARY="$BUILD_DIR/selfdev/jcode"
V="$PWD/.jcode/skills/verify-jcode/scripts/verify-jcode"
E="${JCODE_SCRATCH_DIR:-$HOME/.jcode/scratch}/verify-jcode-$(date -u +%Y%m%dT%H%M%SZ)-$$"
"$V" launch --binary "$BINARY" --evidence "$E"
```

If justrust is unavailable, use `scripts/dev_cargo.sh build --target-dir "$BUILD_DIR" --profile selfdev -p jcode --bin jcode`. Keep this worktree's explicit target directory: an implicit shared build target can reuse another checkout's artifacts. Never take `jcode` from PATH or use `selfdev reload` for this workflow.

Launch copies that binary into a short-lived owner-only runtime directory, records its SHA-256/version and checkout revision, starts the existing `scripts/fake_openai_server.py` on a kernel-selected loopback port, starts `serve --socket <private>/app.sock`, completes a real CLI inference and opens a 160x48 TUI in a private `tmux -S` server. Use `launch --cols 60 --rows 30` for a separate narrow-terminal run. It drives the optional first-run welcome screen with Right, Enter. Ready means a completed inference, usable terminal prompt and daemon attachment, not just a socket file.

Both server and clients receive a clean environment with private HOME, JCODE_HOME, JCODE_RUNTIME_DIR, debug-file paths, `JCODE_TEST_SESSION=1`, `JCODE_DEBUG_CONTROL=1`, and `JCODE_NO_MENUBAR=1`. The TUI uses `--fresh-spawn` to skip host setup hints. The disposable workspace contains only the seeded `verify-probe` and `verify-review` skills. Launch neither loads the developer's credentials/skills/config nor repoints or restarts their shared daemon. It refuses an existing evidence directory. Failed or interrupted commands automatically clean their processes and retain failure logs.

## Doctor

```bash
"$V" doctor --evidence "$E"
```

Require `status: ready`. This is read-only against the app: verify the runtime ownership marker, original process start/group identities, binary digest, helper/fixture hashes, fake-provider model catalog, daemon build hash and attached TUI session. It writes `doctor.json` into evidence. Run it whenever a drive looks wrong. An exited or cleaned run must be relaunched, not repaired by attaching to another socket.

## Drive

```bash
"$V" drive --evidence "$E" cli --mode json
"$V" drive --evidence "$E" tui
"$V" drive --evidence "$E" slash
"$V" drive --evidence "$E" resume
"$V" drive --evidence "$E" cancel
"$V" drive --evidence "$E" restart
"$V" drive --evidence "$E" reload-bg
"$V" drive --evidence "$E" side-panel
"$V" drive --evidence "$E" mode
"$V" drive --evidence "$E" mode-set
"$V" frame --evidence "$E" --label reviewed
```

Run only the recipes relevant to the change, plus their listed entry points. `cli` also accepts `--mode plain` and `--mode ndjson`. Each drive runs doctor first, records actual input and before/after frames, uses a fresh token to reject stale responses, and exits nonzero when an observable assertion fails. `cancel` presses Escape during a file-gated provider request. `mode` pins `verify-probe`, grows context, runs a real `/compact`, then requires the skill body in the first post-compaction provider request and in a spawned worker's first request (about 4 minutes). `restart` stops only the owned private daemon, starts the same copied binary, and resumes the saved session. Neither proves binary reload or real vendor authentication. A failed journey remains failed. Inspect its artifacts, clean up, then launch a fresh run after fixing the cause.

For additional user paths, reuse terminal controls instead of inventing another harness:

```bash
"$V" drive --evidence "$E" input --text "/skills" --expect "verify-probe"
"$V" drive --evidence "$E" keys --keys Escape --expect "verify-probe"
```

These exploratory drives return `observed`, not `passed`: `--expect` can match pre-existing text, so inspect changed frames and independent side effects before claiming feature coverage. Key names are tmux names, such as Enter, Escape, C-u and Up.

Inputs travel through tmux's real terminal keystrokes. Frame/state/history are read-only observations through the explicitly private server debug socket. `client:frame` uses the existing current-draw path, not a cached frame from the file protocol. Do not substitute `inject`, `set_input`, direct tool execution, seeded transcript edits or internal state setters for the user's action.

## Evidence

All artifacts live in `$E`, outside disposable runtime state:

- `run.json`: source binary, SHA-256/version, helper/fixture hashes, checkout revision, owned PIDs, private paths and synthetic environment.
- `actions.jsonl`: timestamped process launches, exact CLI argv and terminal inputs.
- `doctor.json`, `launch-warm.{stdout,stderr,command.json}`: readiness and CLI exit status.
- `drive-NNN-*.json`: feature, entry point, fresh token and pass/fail. CLI captures stdout, stderr and exit code separately.
- `*.txt`: actual tmux terminal capture including scrollback. `*.frame.json`: the real renderer's cell/layout frame. `*.state.json`: matching client state. These are terminal evidence, not a pixel screenshot.
- `provider.jsonl`: actual HTTP request messages. `*-session.json` and `sessions/`: independent persisted transcript view. Active sessions use a snapshot plus append journal, so the helper reads both and deduplicates message IDs before asserting a fresh completed turn. The tool drive also retains the written file.
- `server.log`, `provider.log`, `source.diff`, `cleanup.json`: diagnosis, source delta and teardown receipt.

Proof requires the action AND result, a matching provider request when inference is involved, a completed reply and persisted side effects. Inspect frames for the specific UI regression. An ack alone does not prove layout, tool success, resume or skill expansion. A fake model proves Jcode's provider boundary and local behavior, not a real vendor's auth/model/reasoning. Claim only the entry points actually exercised. This is not an OS network sandbox: telemetry/update opt-outs and credential isolation are configured, but unrelated external network behavior is not proven absent.

## Cleanup

Always run cleanup, including after a failed drive:

```bash
"$V" cleanup --evidence "$E"
test -s "$E/cleanup.json" && test -s "$E/actions.jsonl"
```

Cleanup copies persisted sessions into evidence, stops only processes whose saved start/group identity still matches, stops the private tmux server, and finds orphaned companions and hotkey listeners by the exact runtime executable path plus PID identity. Runtime-owned launch agents are booted out only when their registered plist path matches the private runtime, never when they match the pre-existing hotkey. The helper records the global hotkey agent path at launch and checks it before and after cleanup. A change fails loudly and preserves runtime evidence. Missing baseline in older manifests also fails closed after stopping owned processes. The owner-marked runtime is removed only after successful checks. Repeating cleanup is safe. Each concurrent agent launches a separate run. Commands against one run are serialized by an evidence-directory lock. After an interrupted helper, remove `$E/.lock` only after confirming that no helper command still uses this evidence directory.

## Helpers

The executable is `.jcode/skills/verify-jcode/scripts/verify-jcode`. Run `"$V" --help` or `"$V" drive --help` for its complete interface. It reuses the repository's fake-provider server, real `run --socket`, private `serve`, tmux acceptance approach, native debug observations and renderer frames. Run `python3 .jcode/skills/verify-jcode/scripts/test_verify_jcode.py -v` for helper safety-contract regression tests, separately from real-app proof. It intentionally avoids the global/default debug socket and `tester:spawn`: the current tester launcher does not pass an explicit private `--socket` and sets the selfdev client flag. The older `debug_socket_test.sh` defaults to a shared socket, `test_e2e.sh` mixes unit tests with PATH binaries, and `capture_screenshot.sh` needs a human plus Linux grim/slurp. None is an isolation-safe replacement for this helper.

## Maintenance

Use `/maintain-verification-skill` when commands, feature entry points or evidence contracts change. Add journeys to the map before claiming new coverage. Known unverified paths and missing fixture preconditions belong in each feature's Gotchas.
