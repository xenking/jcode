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
    if let RunPersistence::Resume(session_id) = persistence {
        let requested_route = requested_resume_route(choice, model, &agent);
        if let Err(error) =
            resume_session_for_run(&mut agent, session_id, requested_route.as_deref(), message)
        {
            agent.mark_closed();
            return Err(error);
        }
    }

    run_single_message_with_agent(&mut agent, provider, message, output).await
}

/// `-p`/`-m` are an explicit route choice that outranks the saved one. `-m` is
/// passed on as typed: the agent's own model string has already lost the
/// credential account of a spec like `claude-oauth@<account>:<model>`, so
/// re-applying it could never leave the saved (possibly exhausted) account.
pub(super) fn requested_resume_route(
    choice: &crate::cli::provider_init::ProviderChoice,
    model: Option<&str>,
    agent: &crate::agent::Agent,
) -> Option<String> {
    model.map(str::to_owned).or_else(|| {
        (*choice != crate::cli::provider_init::ProviderChoice::Auto).then(|| agent.provider_model())
    })
}

/// Restore `session_id` for a one-shot run. A requested route replaces the
/// session's saved one (and is persisted), and an unanswered earlier copy of
/// `message` is dropped so a retried run delivers it exactly once.
pub(super) fn resume_session_for_run(
    agent: &mut crate::agent::Agent,
    session_id: &str,
    requested_route: Option<&str>,
    message: &str,
) -> Result<()> {
    agent.restore_session(session_id)?;
    if let Some(route) = requested_route {
        agent
            .set_model(route)
            .with_context(|| format!("Selecting {route} for resumed session {session_id}"))?;
    }
    agent.discard_unanswered_retry(message);
    Ok(())
}

/// A failover offer is a machine-readable wire string; a one-shot run has no
/// one to ask, so say what is blocked and how to proceed instead.
pub(super) fn explain_run_error(error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    let Some(prompt) = crate::provider::parse_failover_prompt_message(&text) else {
        return error;
    };
    let reset = match crate::provider::usage_limit_details(&text).and_then(|details| details.reset)
    {
        Some(reset) => format!(", resets {reset}"),
        None => String::new(),
    };
    anyhow::anyhow!(
        "{} is unavailable ({}{reset}). The message was not delivered: jcode run never switches provider on its own. Pass -p/--provider or -m/--model (for example `jcode --resume <id> -p {} run ...`) to continue on another route.",
        prompt.from_label,
        prompt.reason,
        prompt.to_provider,
    )
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
