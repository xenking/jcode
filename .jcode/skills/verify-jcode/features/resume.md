# Session resume

A user closes the TUI, opens the same saved conversation again, reads prior messages and continues without losing its session identity.

## Sub-features

- RESUME-HISTORY: the original prompt/reply is visible after reattachment.
- RESUME-ID: the exact same session ID is retained.
- RESUME-CONTINUE: a new prompt runs and is saved in that conversation.

## How to get to it (user POV)

Launch `jcode --resume <session_id> --socket <private>/app.sock` to reopen one conversation. `jcode --resume` and the TUI's /resume are picker entry points. `--resume <id> run <prompt>` is a distinct noninteractive continuation path.

## Driving it with verify-jcode

Preconditions: Launch and doctor passed. The helper creates and persists a first real TUI turn itself, so no user session is needed.

- **Reopen and continue:** `"$V" drive --evidence "$E" resume`. The helper sends a fresh token, waits for persistence, closes only its own terminal session, then opens the real TUI with --resume and the saved ID on the same private daemon.
- **Proof:** compare `*-original.state.json` with `*-after.state.json`. Require identical remote_session_id, original token visible after reattachment, and a fresh continued prompt reaching the provider and appearing in the same saved transcript. Retain original and final frames and `*-session.json`.
- **Other entries:** picker selection, CLI continuation and a full daemon restart each require an additional explicit journey and must be reported separately.

## Gotchas

- Reattaching a TUI to a still-running daemon is not proof of recovery after restarting the daemon or the OS.
- The helper stops only its own tmux session. Never close the developer's terminal or use the developer's most recent session as fixture.
- A new session that happens to contain similar text fails RESUME-ID.
- If a picker has no saved sessions, record the missing seed as a failure. Do not silently switch to exact-ID resume and claim picker coverage.
