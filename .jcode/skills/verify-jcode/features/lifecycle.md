# Cancellation and daemon restart

A user cancels a running turn and continues in the same conversation. A stopped daemon restores saved history after a new daemon starts.

## Sub-features

- CANCEL-TURN: Escape stops a request that the fake provider is holding.
- CANCEL-SAVE: the cancelled prompt remains saved without a completed model reply.
- CANCEL-CONTINUE: a new prompt completes in the same session.
- RESTART-HISTORY: a new daemon restores the original transcript without changing message IDs.
- RESTART-ID: the daemon identity changes while the conversation identity stays the same.

## How to get to it (user POV)

Press Escape while the TUI is processing. For daemon recovery, stop the daemon process, start `jcode --socket <private>/app.sock serve`, then reopen the conversation with `--resume <session_id>`.

## Driving it with verify-jcode

Run `drive reload-bg` on Unix to execute an actual same-binary reload, not a stop/start simulation. The fake provider invokes the real Bash tool with a file-gated background command. The private daemon pins its staged binary, then uses the normal reload channel, cleanup and exec. The helper verifies the unchanged daemon PID and command/supervisor PIDs, the changed server identity, TUI reconnect to the original session, post-exec task progress, exit 23, visible completion and a provider request caused by wake. It retains the reload trace and the terminal task record. The exec command is limited to the verifier's staged binary, socket and home. It cannot run on the normal daemon.

Preconditions: Launch and doctor passed. The provider uses the existing file-gated fixture.

- **Cancel:** `"$V" drive --evidence "$E" cancel`. The helper sends a fresh `[e2e:hold:<token>]` prompt, waits for the provider gate, captures the processing frame, and presses Escape through tmux. It requires an idle TUI before releasing the provider gate. It then sends a fresh continuation.
- **Cancel proof:** retain `*-held.*`, `*-cancelled.*`, `*-cancelled-session.json`, and the continued `*-session.json`. The cancelled snapshot must contain the prompt without its ack. The continuation must reach the provider and complete in the same session. The helper releases its gate even after a failed drive.
- **Restart:** `"$V" drive --evidence "$E" restart`. The helper saves a fresh turn and closes its own TUI. It checks daemon PID ownership, sends SIGTERM to that process group, waits for exit, and starts the copied binary on the same private socket and home. It reopens the exact session and continues.
- **Restart proof:** require different `previous_server_id` and `server_id` in the receipt, the same session ID, the original prompt visible, and unchanged original message IDs in the continued transcript. Doctor runs after reattachment.

## Gotchas

- Cancellation holds a provider request before response headers. It does not test cancellation during tool execution, queued follow-ups, or partial token streaming.
- Restart uses the same copied binary. It does not verify `/reload`, binary replacement, in-flight reload recovery, ask replay, or operating-system reboot recovery. Those need separate fixtures and journeys.
- Never send a reload command that can select a developer-installed binary or change shared build symlinks. This helper owns only its copied executable and private daemon.
- A failed cancellation or restart drive remains failed. Run doctor for diagnosis, clean up, and launch a fresh run.

## Source entry points

- `crates/jcode-tui/src/tui/app/remote/key_handling/key_events.rs` sends `keyboard_cancel` on Escape.
- `crates/jcode-app-core/src/server/client_lifecycle.rs` handles turn cancellation and reload fencing.
- `crates/jcode-base/src/session/persistence.rs` restores the snapshot and journal.
- `crates/jcode-app-core/src/server/reload_recovery.rs` owns reload continuation records, outside these two journeys.
