//! Human approval for MCP calls, opt in per server (`approval = "annotations"`).
//!
//! The decision whether a call needs a yes lives with the tool
//! ([`jcode_tool_core::Tool::mcp_approval`]). This module asks the person and
//! enforces the answer. It sits in the registry's dispatch path, so a call made
//! directly, through `batch`, or from a `codemode` program meets the same gate.
//! The model cannot answer for the user: the request id is classed as a human
//! approval, which the Desktop automation tool and TUI key injection refuse to
//! answer (see [`super::approval_request::is_human_approval`]).
//!
//! There is no "remember" option. Every call asks, with the tool name and its
//! arguments in front of the user. A session with nobody to ask (a headless
//! `jcode run` or a swarm worker) is refused with an error that says how to
//! allow the tool.

use super::approval_request::MCP_REQUEST_PREFIX;
use super::{ToolContext, ask_client};
use crate::protocol::{AskOption, AskQuestion};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

const ALLOW: &str = "Allow once";
const DENY: &str = "Deny";

/// Why the gate refused a call. It rides in the error chain so the codemode
/// receipt can show a safe category without echoing the message.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) category: &'static str,
    message: String,
}

impl Refusal {
    fn error(category: &'static str, message: String) -> anyhow::Error {
        anyhow::Error::new(Self { category, message })
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// Longest rendering of the arguments shown to the user.
const MAX_ARGUMENT_CHARS: usize = 800;

tokio::task_local! {
    /// Clock of the `codemode` run this call belongs to. The wait for a person
    /// is added to it so the script's wall-clock limit does not run while a
    /// human decides.
    pub(crate) static HUMAN_WAIT: Arc<jcode_codemode::PauseClock>;
}

/// Ask for a yes to one call, or fail. `input` is shown as given, minus the
/// harness's own flags.
pub(crate) async fn require(
    ctx: &ToolContext,
    server: &str,
    tool: &str,
    input: &Value,
) -> Result<()> {
    let route = super::interactive::sender(&ctx.session_id)
        .or_else(|| ctx.stdin_request_tx.clone().filter(|tx| !tx.is_closed()));
    let Some(route) = route else {
        return Err(Refusal::error(
            "Needs a human yes",
            format!(
                "MCP tool '{tool}' on server '{server}' needs a human yes (approval = \"annotations\") \
                 but this session has no interactive user (headless run or swarm worker), so the call \
                 was refused. List the tool in `approve_free` for that server, or set \
                 `approval = \"off\"`, to run it unattended."
            ),
        ));
    };
    let questions = [AskQuestion {
        question: format!(
            "Allow MCP tool \"{tool}\" on server \"{server}\" with arguments {}?",
            render_arguments(input)
        ),
        header: Some("MCP approval".into()),
        options: vec![
            AskOption {
                label: DENY.into(),
                description: Some("Do not run this call".into()),
            },
            AskOption {
                label: ALLOW.into(),
                description: Some("Run this one call only".into()),
            },
        ],
        multi_select: false,
        allow_other: false,
    }];
    let request_id = format!("{MCP_REQUEST_PREFIX}{}", uuid::Uuid::new_v4());
    let started = Instant::now();
    let response = crate::turn_activity::global()
        .wait_for_ask(
            &ctx.session_id,
            &request_id,
            ask_client(route, &questions, &request_id),
        )
        .await;
    let waited = started.elapsed();
    let _ = HUMAN_WAIT.try_with(|clock| clock.add(waited));
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Err(Refusal::error(
                "No answer to the prompt",
                format!(
                    "MCP tool '{tool}' on server '{server}' was not approved: no answer ({error:#}). \
                     The call did not run."
                ),
            ));
        }
    };
    let allowed = response.answers.is_some_and(|answers| {
        matches!(answers.as_slice(), [answer] if answer.selected == [ALLOW] && answer.other.is_none())
    });
    if !allowed {
        return Err(Refusal::error(
            "Denied by the user",
            format!(
                "MCP tool '{tool}' on server '{server}' was denied by the user. The call did not run; \
                 do not retry it unless the user asks."
            ),
        ));
    }
    Ok(())
}

/// The arguments on one line: the ask pickers flatten line breaks, so pretty
/// JSON would run together.
fn render_arguments(input: &Value) -> String {
    let mut shown = input.clone();
    if let Some(object) = shown.as_object_mut() {
        object.remove(jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY);
        object.remove("intent");
    }
    let text = shown.to_string();
    if text.chars().count() <= MAX_ARGUMENT_CHARS {
        return text;
    }
    let cut: String = text.chars().take(MAX_ARGUMENT_CHARS).collect();
    format!("{cut}... (truncated, {} chars total)", text.chars().count())
}

#[cfg(test)]
#[path = "mcp_approval_tests.rs"]
mod tests;
