# Repository Guidelines

## Repository Scope

- Jcode Desktop is in a separate repository.

## TUI style

- UI chrome must not wrap: startup headers, account rows, status labels, and
  other compact UI rows should fit their available terminal-cell width or
  truncate with an ellipsis.
  Preserve one logical row per visual row, including at narrow widths.
  Chat prose and other intentionally multiline content may wrap normally.
- Usage colors and used/remaining labels follow the info widget's conventions.

## Development Workflow

| Contract | Enforcement |
| --- | --- |
| Swarm registry locks follow the canonical order and never span unrelated awaits | `python3 scripts/check_lock_order.py` (zero-violation budget) |
| Test helpers isolate persistent homes and machine hooks together, never in production builds | `python3 scripts/check_test_isolation.py` (Cargo feature graph) |
| Header, status and info-widget measurements use display cells and shared truncation | `python3 scripts/check_tui_chrome.py` (zero scalar-width budget) |
| Quota bar glyphs have one owner, distinct from task-progress glyphs | `python3 scripts/check_usage_bar_owner.py` (no copied builders) |
| Every runtime slot and compatible endpoint participates in cached and forced catalog refresh | `python3 scripts/check_catalog_fanout.py` (derived slot census and shared consumer) |
| An idle Desktop-like client stays within daemon connection, state rewrite, log, and CPU budgets | `python3 scripts/check_idle_load.py <built-jcode>` (isolated 60-second runtime check, also run in Linux CI) |

- **Commit and push as you go** - Make small, scoped commits and push them to
  the current branch's remote as meaningful work is completed, rather than
  waiting until the end of the task. Preserve unrelated changes and never
  force-push unless the user explicitly authorizes it.
- **Use the user's Git identity** - Create commits with the configured
  `user.name` and `user.email`. Do not override them with `Jcode`, `Jcode agent`,
  or a fabricated agent email. Preserve existing contributor attribution when
  integrating work. If no identity is configured, ask rather than inventing one.
- **Welcome pull requests from everyone** - Review contributions on their merits,
  regardless of whether the author is a maintainer, an existing contributor, a
  first-time contributor, or an agent. Good PRs can be merged directly after review
  and validation. Do not require a maintainer-authored rewrite merely because of
  who submitted the change. See `CONTRIBUTING.md` for the contribution policy.
- **Prefer master/main** - Work directly on the repository's primary branch
  (`master` or `main`) by default. Do not create a branch or worktree merely to
  start a task. Only do so when the user explicitly requests it or isolation is
  genuinely necessary, and explain the concrete reason first.
- **Keep work scoped** - Preserve unrelated work. If already on a branch with
  ongoing changes, finish and integrate safely rather than switching blindly.
  When the user asks you to review or integrate a PR or branch, you may inspect,
  test, and integrate that contribution regardless of author status. Do not pull
  in unrelated branches or merge a PR without user authorization.

## Provider model discovery

- Model arrays are offline fallbacks, not release allowlists. Providers with a
  catalog must discover new model IDs without a Jcode release and expose them
  through switching lists and structured model routes.
- Implement both cached prefetch and explicit refresh. Explicit refresh must
  bypass fresh caches. Preserve the last good catalog on network, authentication,
  or malformed-response failures, and scope persistent caches to their endpoint.
- Publish model updates to long-lived sessions, not only startup or login.
- Regression tests must discover an arbitrary future model absent from all
  fallback arrays, using the provider's actual response schema. Check picker
  routes, forced refresh with a fresh cache, and last-good behavior on failure.
- If a backend has no discovery API, document that limitation instead of
  presenting a hardcoded list or a no-op refresh as live discovery.

## Install Notes
- `~/.local/bin/jcode` is the launcher symlink used from `PATH`.
- `~/.jcode/builds/current/jcode` is the active local/source-build channel; self-dev builds and `scripts/install_release.sh` point the launcher here.
- `~/.jcode/builds/stable/jcode` is the stable release channel; `scripts/install.sh` installs this and points the launcher here.
- `~/.jcode/builds/versions/<version>/jcode` stores immutable binaries.
- `~/.jcode/builds/canary/jcode` still exists for canary/testing flows, but it is not the primary self-dev install path.
- On Windows, the equivalents are `%LOCALAPPDATA%\\jcode\\bin\\jcode.exe` for the launcher, `%LOCALAPPDATA%\\jcode\\builds\\stable\\jcode.exe` for stable, and `%LOCALAPPDATA%\\jcode\\builds\\versions\\<version>\\jcode.exe` for immutable installs; `scripts/install.ps1` currently installs the stable channel.
- Ensure `~/.local/bin` is **before** `~/.cargo/bin` in `PATH`.

## Verifying a change at runtime

### Keep benchmark sessions out of user history

For synthetic prompts and streaming/CPU/power benchmarks, set a dedicated
`JCODE_HOME` and `JCODE_TEST_SESSION=1` for both the server and its clients,
in addition to using a unique `--socket`. A private socket alone isolates the
daemon, not persisted sessions. Never run synthetic benchmark prompts against
the user's normal session storage.

`cargo build` alone proves nothing about behavior. `jcode run` and interactive
sessions are served by the long-lived daemon at
`~/.jcode/builds/shared-server/jcode`, which is a symlink into
`~/.jcode/builds/versions/<version>/`. Until that symlink is repointed and the
daemon restarted (`jcode self-dev --build`), a freshly built binary is inert and
every runtime check silently measures the old code.

To test a change without disturbing the shared daemon or the caller's session,
run your build against its own socket:

```bash
cargo build --profile selfdev
./target/selfdev/jcode run --no-update --socket /run/user/1000/jcode-mytest.sock '<prompt>'
```

Two things that waste time otherwise:

- `crate::logging::info` writes to a log file, not stderr, so instrumenting a
  code path with it produces no visible output under `--trace`. Use `eprintln!`
  for throwaway diagnostics and delete it before committing.
- Confirm which binary you are actually inspecting. `strings` on
  `builds/shared-server/jcode` reads a 70-byte symlink, not a program; resolve it
  with `readlink -f` first.

## Fork notes (xenking/jcode, keep at end for clean upstream merges)
- [Fork ledger](docs/FORK_LEDGER.md): CLI and Desktop features, confirmed removals, current owners and sync checks.
- Fork-only features: codemode (`crates/jcode-codemode`, docs `CODEMODE.md`, `[features] code_mode`), MCP `MCP-Protocol-Version` header on initialize, Soniox realtime `stt-rt-v6` in `crates/jcode-voice` (`[dictation] backend`), native `ask` tool (`crates/jcode-app-core/src/tool/ask.rs`, exposed on harness API), self-update source defaults to `xenking/jcode` branch `master`.
- For fork development and rollout, use `/selfdev`. Keep this section last on upstream merges.
- Use `justrust check`, `justrust test`, and `justrust clippy` for affected Rust crates. Verify a CLI slice with `justrust check -p jcode-codemode -p jcode-voice`. For install and runtime proof, follow the dedicated workflow above.
- Store local jcode secret files at mode `0600`; never print or commit their values.
- Upstream sync: `scripts/xk-upstream-agent.sh` starts a separate jcode session that runs `scripts/xk-upstream-sync.sh` (jcode + Desktop) and OpenRig's catch-up; `--dry-run` shows the gap.
- Before pushing any upstream sync, run `scripts/check_fork_removals.py jcode . HEAD` and `scripts/check_fork_removals.py desktop ../jcode-desktop HEAD` on the candidate commits. The sync script enforces this even with `--skip-checks`, including after a clean merge or a late merge from origin. Keep deliberate removals and owner changes from the ledger. Absence alone does not establish intent.
- Parallel agents (swarm workers, other sessions) never share a checkout: each works in its own `git worktree` under `~/.jcode/wt/<name>-<repo>` off origin, ships by rebase + fast-forward push to the primary branch, then removes the worktree and branch. This overrides "Prefer master/main" for parallel work.
- Run [`deslop-please`](https://github.com/deterministic-machines/deslop-please) `--allow .deslop-please.allow` before publishing Rust changes. New `module-cycle`, `tangle`, `field-leak`, `shallow-module`, `wide-directory` or `long-file` errors fail it. Fix them from first principles: decide which layer each module belongs to, why the fields or functions exist and what narrow interface hides them. Fix leaks of one type together, never by mechanical moves that only hide the error. After removing debt, run `--update-allow .deslop-please.allow` so the list only shrinks. Entries under `ratatui_core` are vendored upstream code.
