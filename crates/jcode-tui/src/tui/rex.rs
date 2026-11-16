//! Rex program status over OSC 7501, using the TUI's stdout writer.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentState {
    Idle,
    Working,
    BlockedQuestion,
    Clear,
}

impl AgentState {
    fn osc(self) -> &'static str {
        match self {
            Self::Idle => "\x1b]7501;state=idle:app=jcode\x1b\\",
            Self::Working => "\x1b]7501;state=working:app=jcode\x1b\\",
            Self::BlockedQuestion => "\x1b]7501;state=blocked:app=jcode:kind=question\x1b\\",
            Self::Clear => "\x1b]7501;state=clear\x1b\\",
        }
    }
}

#[derive(Default)]
struct Reporter {
    last_sent: Option<AgentState>,
}

impl Reporter {
    fn from_block(block: Option<&str>) -> Option<Self> {
        block.filter(|block| !block.trim().is_empty())?;
        Some(Self::default())
    }

    fn sync(&mut self, writer: &mut impl Write, state: AgentState) -> io::Result<()> {
        if self.last_sent == Some(state) || self.last_sent == Some(AgentState::Clear) {
            return Ok(());
        }
        writer.write_all(state.osc().as_bytes())?;
        writer.flush()?;
        self.last_sent = Some(state);
        Ok(())
    }
}

static REPORTER: OnceLock<Option<Mutex<Reporter>>> = OnceLock::new();

pub(crate) fn sync(state: AgentState) {
    let Some(reporter) = REPORTER.get_or_init(|| {
        Reporter::from_block(std::env::var("REX_BLOCK").ok().as_deref()).map(Mutex::new)
    }) else {
        return;
    };
    let mut reporter = reporter.lock().unwrap_or_else(|error| error.into_inner());
    // Match terminal_title.rs: stdout is the TUI's terminal output, not a raw fd.
    if let Err(error) = reporter.sync(&mut io::stdout().lock(), state) {
        crate::logging::warn(&format!("rex: failed to report agent state: {error}"));
    }
}

/// Clear the status on quit, but not before a reload that keeps this pane.
pub fn release() {
    sync(AgentState::Clear);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rex_reporter_activates_only_with_block() {
        assert!(Reporter::from_block(None).is_none());
        assert!(Reporter::from_block(Some(" ")).is_none());
        assert!(Reporter::from_block(Some("block-1")).is_some());
    }

    #[test]
    fn rex_osc_formats_state_app_and_question_kind() {
        for (state, expected) in [
            (AgentState::Idle, "\x1b]7501;state=idle:app=jcode\x1b\\"),
            (
                AgentState::Working,
                "\x1b]7501;state=working:app=jcode\x1b\\",
            ),
            (
                AgentState::BlockedQuestion,
                "\x1b]7501;state=blocked:app=jcode:kind=question\x1b\\",
            ),
        ] {
            let mut reporter = Reporter::default();
            let mut output = Vec::new();
            reporter.sync(&mut output, state).unwrap();
            assert_eq!(output, expected.as_bytes());
        }
    }

    #[test]
    fn rex_deduplicates_unchanged_state_and_clears_on_release() {
        let mut reporter = Reporter::default();
        let mut output = Vec::new();
        for state in [
            AgentState::Idle,
            AgentState::Idle,
            AgentState::Working,
            AgentState::Working,
            AgentState::BlockedQuestion,
            AgentState::Idle,
            AgentState::Clear,
            AgentState::Clear,
            AgentState::Working,
        ] {
            reporter.sync(&mut output, state).unwrap();
        }
        assert_eq!(
            output,
            concat!(
                "\x1b]7501;state=idle:app=jcode\x1b\\",
                "\x1b]7501;state=working:app=jcode\x1b\\",
                "\x1b]7501;state=blocked:app=jcode:kind=question\x1b\\",
                "\x1b]7501;state=idle:app=jcode\x1b\\",
                "\x1b]7501;state=clear\x1b\\",
            )
            .as_bytes()
        );
    }

    #[test]
    fn rex_retries_state_after_write_failure() {
        let mut reporter = Reporter::default();
        let mut full_buffer = &mut [][..];
        assert!(reporter.sync(&mut full_buffer, AgentState::Idle).is_err());
        assert_eq!(reporter.last_sent, None);
        let mut output = Vec::new();
        reporter.sync(&mut output, AgentState::Idle).unwrap();
        assert_eq!(output, AgentState::Idle.osc().as_bytes());
    }
}
