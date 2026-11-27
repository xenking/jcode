# jcode Docs

Reference documentation for the jcode codebase.

## Layout

- `docs/*.md` — architecture, feature, and behavior docs (current state of the system).
- `docs/plans/` — forward-looking plans, roadmaps, and TODO trackers. May be partially implemented or stale.
- `docs/audits/` — point-in-time audits and reviews. Historical snapshots, not kept up to date.
- `docs/proposals/` — design proposals not yet committed to.
- `docs/dev/` — developer-facing process and testing notes.
- `docs/benchmarks/` — dated measurement records. `docs/research/` — dated investigations and the measurements behind them. Both are snapshots, not kept up to date.

## Key entry points

- Architecture: `SERVER_ARCHITECTURE.md`, `MODULAR_ARCHITECTURE_RFC.md`, `CRATE_OWNERSHIP_BOUNDARIES.md`
- Swarm: `SWARM_ARCHITECTURE.md`
- Memory: `MEMORY_ARCHITECTURE.md`, `MEMORY_BUDGET.md`, `MEMORY_INCIDENT_RUNBOOK.md`
- Refactoring and quality: `REFACTORING.md`, `plans/CODE_QUALITY_10_10_PLAN.md`
- Providers: `PROVIDER_DOCTOR.md`, `AWS_BEDROCK_PROVIDER.md`
- CLI and TUI: [Run in CI, review Git changes, and start in a worktree](CLI_GIT_WORKFLOWS.md)
- MCP servers: `MCP_REMOTE.md`
- Platform: `WINDOWS.md`, `TERMINAL_CAPABILITIES.md`
- Fork (xenking/jcode): `FORK_LEDGER.md`, `FORK_UPDATES.md`, `PAIRED_DESKTOP.md`, `DESKTOP_STATE.md`, `rollout-daemon-transaction.md`
- Routing: `ROUTING.md`, `ACCOUNT_ROUTING.md`, `MODEL_ROUTING.md`
- Code Mode and MCP: `CODEMODE.md`, `MCP_TOOLS.md`
- Context size and token estimates: `TOKEN_ESTIMATION.md`

## Conventions

- Docs describing current behavior live at the top level; anything speculative goes in `plans/` or `proposals/`.
- Prefer updating an existing doc over adding a near-duplicate.
- Root of the repo should only hold README, CONTRIBUTING, RELEASING, AGENTS, LICENSE, and similar meta files. Put everything else here.
