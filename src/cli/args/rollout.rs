/// `jcode rollout` flags. Kept in their own struct: inlining them into
/// `Command` grows clap's generated parser frame past the 2 MiB test-thread
/// stack in unoptimized builds.
#[derive(clap::Args, Debug)]
pub(crate) struct RolloutArgs {
    /// Print the plan for this machine and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Reload without waiting for mid-turn sessions; reload recovery resumes them
    #[arg(long)]
    pub now: bool,
    /// Seconds to wait for mid-turn sessions before refusing to reload
    #[arg(long, default_value_t = 900, value_name = "SECS")]
    pub drain_timeout: u64,
    /// jcode ref to ship
    #[arg(long, default_value = "origin/master")]
    pub jcode_ref: String,
    /// Jcode Desktop ref to ship
    #[arg(long, default_value = "origin/main")]
    pub desktop_ref: String,
    /// Roll out the CLI and daemon only
    #[arg(long)]
    pub no_desktop: bool,
}

#[cfg(test)]
mod tests {
    use super::super::{Args, Command};
    use clap::Parser;

    #[test]
    fn rollout_parses_flags_and_defaults() {
        let args = Args::try_parse_from(["jcode", "rollout", "--dry-run"]).unwrap();
        let Some(Command::Rollout(rollout)) = args.command else {
            panic!("expected rollout");
        };
        assert!(rollout.dry_run && !rollout.now && !rollout.no_desktop);
        assert_eq!(rollout.drain_timeout, 900);
        assert_eq!(rollout.jcode_ref, "origin/master");
        assert_eq!(rollout.desktop_ref, "origin/main");

        let args = Args::try_parse_from([
            "jcode",
            "rollout",
            "--now",
            "--no-desktop",
            "--drain-timeout",
            "60",
            "--jcode-ref",
            "abc",
        ])
        .unwrap();
        let Some(Command::Rollout(rollout)) = args.command else {
            panic!("expected rollout");
        };
        assert!(rollout.now && rollout.no_desktop);
        assert_eq!(rollout.drain_timeout, 60);
        assert_eq!(rollout.jcode_ref, "abc");
    }
}
