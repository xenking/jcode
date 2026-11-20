# Jcode verification map

Maintained verification source for the Rust CLI/TUI agent harness. Read this index, then the affected feature. Jcode Desktop lives in a separate repository and is outside this map.

## Baseline preconditions

- Follow the skill's Launch with a freshly built binary and a new evidence directory.
- `V` names `.jcode/skills/verify-jcode/scripts/verify-jcode`, `E` names that run's evidence directory.
- Doctor must return `ready`. Only this run's private daemon, model server, tmux and data may be driven.
- Seed: empty disposable workspace plus `verify-probe` skill with marker `VERIFY_PROBE_SKILL_BODY`. No real credentials or project repository are seeded.
- Fake model acknowledges normal turns. The `[e2e:verify-write]` prompt asks it for a real write tool call, followed by a normal ack. `[e2e:pad]` returns a long ack to grow real context. `[e2e:spawn]` asks for a real `swarm spawn` whose child prompt carries `[e2e:child]`.

## Driving conventions

- Commands are literal and use terminal input, not internal state injection.
- Run recipes from baseline unless the feature explicitly requires a previous turn.
- Exploratory `drive input --text ... --expect ...` and `drive keys --keys ... --expect ...` retain observations without claiming mapped feature coverage.
- Each mapped drive creates a fresh token and numbered evidence. Do not mistake an earlier response for the requested turn.
- Treat separate CLI modes, launch options, aliases and picker entry points as distinct coverage. A single convenient drive does not verify all alternatives listed below.
- Failed drives retain evidence and return nonzero. Clean up failed runs too, then fix the cause and relaunch. Never weaken a precondition to turn red green.

## Proof and skip reporting

Record the feature ID, exact user entry point, action, resulting frame/response, exit status and independently observed persistent side effect. Terminal JSON cell frames plus tmux text are the visual record, not pixel screenshots. Report excluded or unreachable entry points separately, with the attempted command and unmet precondition. After cleanup, require the artifacts and `cleanup.json` still exist.

## Features

| Feature | Automated baseline | Other entry points, not covered by that baseline |
| --- | --- | --- |
| [One-shot CLI](cli-run.md) | `drive cli --mode plain`, `json`, `ndjson` | stdin/shell integration is not a separate supported argument here |
| [Interactive turn and tools](tui-tool.md) | `drive tui`: type, Enter, stream, real write | paste, queued/interrupt turns, tool expansion/collapse |
| [Slash commands and skills](slash-skill.md) | `drive slash`: /help, /verify-probe prompt | /?, /commands, /skills listing, bare activation, completion picker |
| [Session resume](resume.md) | `drive resume`: quit UI, --resume exact ID, continue | --resume picker, /resume picker, CLI --resume run, full daemon restart |
| [Session mode](session-mode.md) | `drive mode`: pin, compact, spawn, clear. `drive mode-set`: toggle, restart, continued provider request, unpin one | auto-compaction, resume after compaction |
| [Side panel](side-panel.md) | `drive side-panel`: /splitview on | /splitview off/status, /todos panel, swarm worker lifecycle |
| [Cancellation and daemon restart](lifecycle.md) | `drive cancel`, `drive restart`, `drive reload-bg` (private exec, PID, progress, exit, notify and wake), `drive reload-selfdev` (build/test queue across exec) | tool cancellation, queue races, other in-flight reload recovery |
| [Provider route and auth boundaries](provider-route.md) | `drive cli --mode json` for the synthetic route | multi-route picker, forced catalog refresh, real vendor auth and token refresh |
