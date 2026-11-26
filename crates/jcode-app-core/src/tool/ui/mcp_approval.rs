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
//! Every call asks, with the tool name and its arguments in front of the user,
//! unless the user answered "Always allow this tool" (or "... every tool on
//! this server"), which records an `approve_free` entry in the MCP config file
//! the server comes from (see [`crate::mcp::approvals`]). A session with no
//! attached client waits a bounded time for one to attach, then is refused
//! with an error that says how to allow the tool.

use super::approval_request::MCP_REQUEST_PREFIX;
use super::{ToolContext, ask_client};
use crate::mcp::approvals::{self, Grant};
use crate::protocol::{AskOption, AskQuestion, AskResponse};
use anyhow::Result;
use jcode_tool_core::McpApprovalRequest;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALLOW: &str = "Allow once";
const DENY: &str = "Deny";
const ALWAYS_TOOL: &str = "Always allow this tool";
const ALWAYS_SERVER: &str = "Always allow every tool on this server";

/// How long a session with no attached client waits for one before the call is
/// refused. `0` refuses at once (headless batch runs).
const ATTACH_WAIT_ENV: &str = "JCODE_MCP_APPROVAL_ATTACH_WAIT_SECS";
const DEFAULT_ATTACH_WAIT: Duration = Duration::from_secs(30);

fn attach_wait() -> Duration {
    std::env::var(ATTACH_WAIT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .map_or(DEFAULT_ATTACH_WAIT, Duration::from_secs)
}

/// What the user chose. Only the exact option strings the card offers count;
/// anything else, including a free-text answer, is a denial.
enum Decision {
    Deny,
    Once,
    AlwaysTool,
    AlwaysServer,
}

impl Decision {
    fn from_response(response: AskResponse) -> Self {
        let Some(answers) = response.answers else {
            return Self::Deny;
        };
        match answers.as_slice() {
            [answer] if answer.other.is_none() => match answer.selected.as_slice() {
                [choice] if choice == ALLOW => Self::Once,
                [choice] if choice == ALWAYS_TOOL => Self::AlwaysTool,
                [choice] if choice == ALWAYS_SERVER => Self::AlwaysServer,
                _ => Self::Deny,
            },
            _ => Self::Deny,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Deny => "denied",
            Self::Once => "allowed_once",
            Self::AlwaysTool => "allowed_always_tool",
            Self::AlwaysServer => "allowed_always_server",
        }
    }
}

fn log_line(outcome: &str, ctx: &ToolContext, request: &McpApprovalRequest) -> String {
    format!(
        "MCP_APPROVAL outcome={outcome} session={} server={} tool={}",
        ctx.session_id, request.server, request.tool
    )
}

fn log(outcome: &str, ctx: &ToolContext, request: &McpApprovalRequest) {
    crate::logging::info(&log_line(outcome, ctx, request));
}

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

/// Ask for a yes to one call, or fail. The arguments are shown as given, minus
/// the harness's own flags.
pub(crate) async fn require(ctx: &ToolContext, request: &McpApprovalRequest) -> Result<()> {
    require_within(ctx, request, attach_wait()).await
}

/// [`require`] with an explicit bound on the wait for a client to attach.
async fn require_within(
    ctx: &ToolContext,
    request: &McpApprovalRequest,
    wait: Duration,
) -> Result<()> {
    let McpApprovalRequest {
        server,
        tool,
        arguments,
        project_dir,
    } = request;
    let route = || {
        super::interactive::sender(&ctx.session_id)
            .or_else(|| ctx.stdin_request_tx.clone().filter(|tx| !tx.is_closed()))
    };
    let questions = [AskQuestion {
        question: format!(
            "Allow MCP tool \"{tool}\" on server \"{server}\" with arguments {}?",
            render_arguments(arguments)
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
            AskOption {
                label: ALWAYS_TOOL.into(),
                description: Some("Run it now and never ask for this tool again".into()),
            },
            AskOption {
                label: ALWAYS_SERVER.into(),
                description: Some("Run it now and never ask for this server again".into()),
            },
        ],
        multi_select: false,
        allow_other: false,
    }];
    let request_id = format!("{MCP_REQUEST_PREFIX}{}", uuid::Uuid::new_v4());
    let started = Instant::now();
    // The attach wait sits inside the parked ask, like the question itself, so a
    // reload does not wait for a person who is not there.
    let response = crate::turn_activity::global()
        .wait_for_ask(&ctx.session_id, &request_id, async {
            let tx = match route() {
                Some(tx) => tx,
                None => {
                    log("waiting_for_client", ctx, request);
                    let attached = tokio::time::timeout(
                        wait,
                        super::interactive::wait_for_sender(&ctx.session_id),
                    )
                    .await
                    .ok()
                    .or_else(route);
                    match attached {
                        Some(tx) => tx,
                        None => return None,
                    }
                }
            };
            log("asked", ctx, request);
            Some(ask_client(tx, &questions, &request_id).await)
        })
        .await;
    let _ = HUMAN_WAIT.try_with(|clock| clock.add(started.elapsed()));
    let response = match response {
        None => {
            log("refused_no_client", ctx, request);
            return Err(Refusal::error(
                "Needs a human yes",
                format!(
                    "MCP tool '{tool}' on server '{server}' needs a human yes (approval = \"annotations\") \
                     but session {} has no attached client (headless run or swarm worker; waited {}s), so the call was refused. \
                     To run it unattended: `jcode mcp approvals allow {server} {tool}` \
                     (or `jcode mcp approvals allow {server}` for every tool on the server), \
                     or answer \"{ALWAYS_TOOL}\" when it asks in an attached session.",
                    ctx.session_id,
                    wait.as_secs()
                ),
            ));
        }
        Some(Err(error)) => {
            log("no_answer", ctx, request);
            return Err(Refusal::error(
                "No answer to the prompt",
                format!(
                    "MCP tool '{tool}' on server '{server}' was not approved: no answer ({error:#}). \
                     The call did not run."
                ),
            ));
        }
        Some(Ok(response)) => response,
    };
    let decision = Decision::from_response(response);
    let grant = match decision {
        Decision::Deny => {
            log(decision.label(), ctx, request);
            return Err(Refusal::error(
                "Denied by the user",
                format!(
                    "MCP tool '{tool}' on server '{server}' was denied by the user. The call did not run; \
                     do not retry it unless the user asks."
                ),
            ));
        }
        Decision::Once => None,
        Decision::AlwaysTool => Some(Grant::Tool(tool.as_str())),
        Decision::AlwaysServer => Some(Grant::Server),
    };
    if let Some(grant) = grant {
        match approvals::grant(project_dir.as_deref(), server, grant) {
            Ok(path) => crate::logging::info(&format!(
                "MCP_APPROVAL saved {} in {}",
                decision.label(),
                path.display()
            )),
            Err(error) => {
                log("grant_not_saved", ctx, request);
                return Err(Refusal::error(
                    "Approval not saved",
                    format!(
                        "MCP tool '{tool}' on server '{server}' was not run: the \"always allow\" answer could not be saved ({error:#}). \
                         Answer \"{ALLOW}\" to run it once."
                    ),
                ));
            }
        }
    }
    log(decision.label(), ctx, request);
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
