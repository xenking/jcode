use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use crate::message::{ContentBlock, Role};
use crate::session::StoredMessage;

use super::{
    run_command_mcp_enabled, run_single_message_with_agent, wait_for_cold_cache_mcp_tools,
};

/// Run a standalone, in-process agent, including its local continuation loop.
///
/// Even when a daemon is available (or started on demand for swarm tools), it
/// does not own this agent or receive its turns. Daemon-owned auto-poke applies
/// only to daemon sessions, so these local loops must remain enabled here.
pub(in crate::cli) async fn run_single_message_command(
    choice: &crate::cli::provider_init::ProviderChoice,
    model: Option<&str>,
    persistence: RunPersistence<'_>,
    message: &str,
    output: RunOutput<'_>,
) -> Result<()> {
    let provider = crate::cli::provider_init::init_provider_for_run(
        choice,
        model,
        output.format != RunFormat::Plain,
    )
    .await?;
    // Older model caches contain names but no account-scoped speed capabilities.
    // Hydrate those before the first headless request so an unsupported saved
    // tier cannot slip through the backward-compatible unknown-catalog path.
    if provider.model_capabilities_need_refresh()
        && let Err(error) = provider.prefetch_models().await
    {
        crate::logging::warn(&format!(
            "Could not refresh model capabilities before headless run: {error}"
        ));
    }
    let registry = crate::tool::Registry::new(provider.clone()).await;
    // Load MCP servers from ~/.jcode/mcp.json so headless `jcode run` has the
    // same `mcp__*` tools as interactive/server sessions. This is non-blocking:
    // `register_mcp_tools` advertises cached tool schemas synchronously (so the
    // first locked tool snapshot already contains MCP tools, for zero
    // prompt-cache miss) and connects in the background (connect-on-first-call).
    // For a short single-message run, startup latency is unchanged.
    // (#390, #206 Phase 2)
    if run_command_mcp_enabled() {
        registry.register_mcp_tools(None, None, None).await;
        // Cold-cache gap: when a configured MCP server has no cached schema yet
        // (first ever use, or reconfigured), advertise-early registers nothing
        // for it, and a single-turn `jcode run` locks its tool snapshot before
        // the background connection finishes, so the model would never see those
        // tools. Long-lived sessions recover on a later turn, but `jcode run`
        // has no later turn. So, only when the cache is cold for some configured
        // server, briefly wait for the first connection to register tools before
        // the agent runs. Warm runs skip this entirely and stay instant. (#390)
        wait_for_cold_cache_mcp_tools(&registry).await;
    }
    let mut agent = match persistence {
        RunPersistence::Ephemeral => crate::agent::Agent::new_ephemeral(provider.clone(), registry),
        _ => crate::agent::Agent::new(provider.clone(), registry),
    };
    if let RunPersistence::Resume(session_id) = persistence
        && let Err(error) = agent.restore_session(session_id)
    {
        agent.mark_closed();
        return Err(error);
    }

    run_single_message_with_agent(&mut agent, provider, message, output).await
}

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
    pub(in crate::cli) fn from_args(resume: Option<&'a str>, ephemeral: bool) -> Self {
        if ephemeral {
            Self::Ephemeral
        } else {
            resume.map_or(Self::Save, Self::Resume)
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
        .unwrap_or_else(String::new);
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
