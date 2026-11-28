use super::{Args, Command};

impl Args {
    pub(in crate::cli) fn validate_run_options(&self) -> anyhow::Result<()> {
        if self.resume.is_some()
            && matches!(
                self.command,
                Some(Command::Run {
                    ephemeral: true,
                    ..
                })
            )
        {
            anyhow::bail!("--resume cannot be used with --ephemeral");
        }
        Ok(())
    }
}
