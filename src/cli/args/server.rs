use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub(crate) enum ServerCommand {
    /// Internal native client protocol bridge over stdin/stdout (for SSH attach)
    #[cfg(unix)]
    #[command(hide = true)]
    Stdio,

    /// Internal: serve workspace tool calls over stdin/stdout for a runtime
    /// that runs elsewhere (`JCODE_EXEC_WORKER`). Holds no model credentials.
    #[command(hide = true)]
    ExecWorker,

    /// Start the background server if it is not already running.
    Start {
        /// Emit JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },

    /// Internal: hold a lightweight connection open until stdin closes.
    #[command(hide = true)]
    Keepalive,

    /// Pin the shared server channel to an installed version.
    ///
    /// Defaults to the active `current` version. This only selects the daemon's
    /// binary; run `jcode server reload` separately to apply it.
    Promote {
        /// Installed version to promote (defaults to the current channel)
        version: Option<String>,

        /// Emit JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },

    /// Gracefully reload the running background server onto the newest binary.
    ///
    /// This is the preferred way to pick up an upgrade: the daemon hands its
    /// live sessions off to a freshly exec'd server (the same path `/reload`
    /// uses), so headless/swarm work is preserved instead of being killed. If
    /// no server is running, this is a no-op. Use `server stop --force` only
    /// when you need to hard-retire a wedged daemon.
    Reload {
        /// Reload even if the running server is already on the newest binary.
        #[arg(long)]
        force: bool,

        /// Emit JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },

    /// Stop the running background server and clear its socket.
    ///
    /// Prefer `server reload` after an upgrade; it preserves live sessions.
    /// `stop` terminates the daemon (SIGTERM, escalating to SIGKILL), which
    /// drops any in-flight headless/swarm sessions, so it requires `--force`
    /// as a deliberate acknowledgement.
    Stop {
        /// Confirm that terminating the daemon (and dropping live sessions) is intended.
        #[arg(long)]
        force: bool,

        /// Emit JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
}
