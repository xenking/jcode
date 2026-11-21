# Rex integration contract

Jcode routes headed session launches into Rex when the requesting client has a nonempty `REX_BLOCK`. The shared launcher starts a right split in that block's working directory with literal command arguments:

```text
rex -S <REX_SERVER> split -b <REX_BLOCK> --split right -c <cwd> -- <jcode command...>
```

`REX_SERVER` selects the requesting client's server. If it is absent, the launcher uses Rex's default server. The launcher resolves `rex` through `PATH`, or uses `REX_BIN_PATH` when set. Rex's login shell preserves the command arguments and sets the new block's terminal identity.

This covers visible swarm agents, resume-in-new-terminal, self-development launches, and restart restores. A configured `[terminal].spawn_hook` takes precedence. Without a hook, `JCODE_TERMINAL` is the first candidate, followed by automatic terminal detection. Herdr stays ahead of Rex when both identify the client. Rex stays ahead of tmux, Zellij, screen, and emulator detection. Normal platform fallback candidates remain available if the Rex executable cannot start.

## Client environment

Jcode snapshots `REX_SESSION`, `REX_BLOCK`, `REX_SERVER`, and the optional `REX_BIN_PATH` from the requesting client. Spawn and focus paths forward native names and `JCODE_CLIENT_` aliases, just as they do for Herdr. A client snapshot takes precedence over the long-lived server's inherited terminal identity.

## Program status

When `REX_BLOCK` is nonempty, the TUI writes OSC 7501 through its stdout writer, the same output used for terminal titles. Reports use `app=jcode`:

```text
ESC ] 7501 ; state=idle:app=jcode ESC \
ESC ] 7501 ; state=working:app=jcode ESC \
ESC ] 7501 ; state=blocked:app=jcode:kind=question ESC \
```

The inline `ask` picker reports `blocked` until the user answers. Other processing reports `working`; an inactive turn reports `idle`. This integration does not claim a separate permission or authentication lifecycle. Unchanged states produce no output. Failed writes can retry on the next update.

On normal TUI quit, Jcode writes `ESC ] 7501 ; state=clear ESC \` once. Reloads keep the status because Jcode continues in the same block. Outside Rex, status reporting does nothing. No Rex subprocess is needed for status updates.
