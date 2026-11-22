use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use crate::message::{ContentBlock, Role};
use crate::session::StoredMessage;

pub(in crate::cli) enum RunInput {
    Argument(String),
    Stdin,
}

impl From<Option<String>> for RunInput {
    fn from(message: Option<String>) -> Self {
        match message {
            Some(message) if message != "-" => Self::Argument(message),
            _ => Self::Stdin,
        }
    }
}

impl RunInput {
    pub(in crate::cli) fn read(self) -> Result<String> {
        let stdin = std::io::stdin();
        self.read_from(stdin.is_terminal(), stdin.lock())
    }

    fn read_from(self, stdin_is_terminal: bool, mut stdin: impl Read) -> Result<String> {
        let message = match self {
            Self::Argument(message) => message,
            Self::Stdin => {
                if stdin_is_terminal {
                    bail!("Pass a prompt or pipe it into jcode run; stdin is a terminal");
                }
                let mut message = String::new();
                stdin
                    .read_to_string(&mut message)
                    .context("Reading prompt from stdin")?;
                message
            }
        };
        if message.trim().is_empty() {
            bail!("The prompt is empty");
        }
        Ok(message)
    }
}

pub(in crate::cli) enum RunPersistence<'a> {
    Save,
    Resume(&'a str),
    Ephemeral,
}

impl<'a> RunPersistence<'a> {
    pub(in crate::cli) fn from_args(resume: Option<&'a str>, ephemeral: bool) -> Result<Self> {
        match (resume, ephemeral) {
            (Some(_), true) => bail!("--resume cannot be used with --ephemeral"),
            (None, true) => Ok(Self::Ephemeral),
            (Some(id), false) => Ok(Self::Resume(id)),
            (None, false) => Ok(Self::Save),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) enum RunFormat {
    Plain,
    Json,
    Ndjson,
}

pub(in crate::cli) struct RunOutput<'a> {
    pub(in crate::cli) format: RunFormat,
    pub(in crate::cli) last_message: Option<&'a Path>,
}

pub(super) fn write_last_message<'a>(
    path: &Path,
    messages: impl DoubleEndedIterator<Item = &'a StoredMessage>,
) -> Result<()> {
    let text = messages
        .rev()
        .filter(|message| message.role == Role::Assistant)
        .find_map(|message| {
            let text: String = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then_some(text)
        })
        .unwrap_or_default();
    let result = (|| -> Result<()> {
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        temp.write_all(text.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    })();
    result.with_context(|| format!("Writing last message to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_input_preserves_bytes_and_validates_prompts() {
        let prompt = "  first\nsecond\n\n";
        for message in [None, Some("-".to_owned())] {
            assert_eq!(
                RunInput::from(message.clone())
                    .read_from(false, prompt.as_bytes())
                    .unwrap(),
                prompt
            );
            assert!(RunInput::from(message).read_from(true, &[][..]).is_err());
        }
        assert_eq!(
            RunInput::Argument(prompt.to_owned())
                .read_from(true, &b"ignored"[..])
                .unwrap(),
            prompt
        );
        assert!(RunInput::Stdin.read_from(false, &b" \n"[..]).is_err());
        assert!(RunInput::Stdin.read_from(false, &[][..]).is_err());
        assert!(RunInput::Stdin.read_from(false, &[0xff][..]).is_err());
        assert!(
            RunInput::Argument(" ".into())
                .read_from(true, &[][..])
                .is_err()
        );
    }
}
