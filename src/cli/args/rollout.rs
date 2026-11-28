/// `jcode rollout` flags. Kept in their own struct: inlining them into
/// `Command` grows clap's generated parser frame past the 2 MiB test-thread
/// stack in unoptimized builds.
#[derive(clap::Args, Debug)]
pub(crate) struct RolloutArgs {
    /// Read durable daemon-owned rollout progress without starting a rollout
    #[arg(long, conflicts_with = "dry_run")]
    pub status: bool,
    /// Print the plan for this machine and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Refresh fork heads and print a paired update preview as JSON (no installation)
    #[arg(long, conflicts_with_all = ["dry_run", "status", "now", "no_desktop", "apply_reviewed"])]
    pub check_update: bool,
    /// Apply only the explicitly reviewed full CLI and Desktop commits
    #[arg(long, conflicts_with_all = ["dry_run", "status", "no_desktop"])]
    pub apply_reviewed: bool,
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
    /// Bypass the paired acceptance smoke gate in an emergency (logged)
    #[arg(long, conflicts_with_all = ["no_desktop", "status", "check_update"])]
    pub skip_acceptance_smoke: bool,
}

#[cfg(test)]
mod tests {
    use super::super::{Args, Command};
    use clap::Parser;

    #[test]
    fn reviewed_updates_can_explicitly_force_but_cannot_omit_desktop() {
        for flag in ["--no-desktop", "--status", "--dry-run", "--check-update"] {
            assert!(
                Args::try_parse_from(["jcode", "rollout", "--apply-reviewed", flag]).is_err(),
                "{flag}"
            );
        }
        let args = Args::try_parse_from(["jcode", "rollout", "--apply-reviewed", "--now"]).unwrap();
        assert!(
            matches!(args.command, Some(Command::Rollout(rollout)) if rollout.apply_reviewed && rollout.now)
        );
        let args = Args::try_parse_from(["jcode", "rollout", "--check-update"]).unwrap();
        assert!(matches!(args.command, Some(Command::Rollout(rollout)) if rollout.check_update));
    }

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
