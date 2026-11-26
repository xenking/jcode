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
//! with an error that says how to allow the tool. A swarm worker has no client:
//! its card goes to the nearest client up its spawn chain, in practice the
//! root session's (see [`super::chain_route`]; never to the coordinator model
//! through the `ask` escalation), and that client's answer resolves the call.

use super::approval_request::{MCP_REQUEST_PREFIX, WORKER_REQUEST_PREFIX};
use super::{ToolContext, chain_route};
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
    match std::env::var(ATTACH_WAIT_ENV) {
        Ok(raw) => match raw.trim().parse() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => DEFAULT_ATTACH_WAIT,
        },
        Err(_) => DEFAULT_ATTACH_WAIT,
    }
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

fn log_line(
    outcome: &str,
    ctx: &ToolContext,
    request: &McpApprovalRequest,
    routed_to: Option<&str>,
) -> String {
    let routed = routed_to.map_or_else(String::new, |root| format!(" routed_to={root}"));
    format!(
        "MCP_APPROVAL outcome={outcome} session={}{routed} server={} tool={}",
        ctx.session_id, request.server, request.tool
    )
}

fn log(outcome: &str, ctx: &ToolContext, request: &McpApprovalRequest, routed_to: Option<&str>) {
    crate::logging::info(&log_line(outcome, ctx, request, routed_to));
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

/// The four answers, in the order the card shows them. Each fits one line of an
/// 80-column terminal next to the picker's `› 4. ` prefix, so none is cut.
fn card_options() -> Vec<AskOption> {
    let option = |label: &str, description: &str| AskOption {
        label: label.into(),
        description: Some(description.into()),
    };
    vec![
        option(DENY, "Do not run this call"),
        option(ALLOW, "Run this one call only"),
        option(ALWAYS_TOOL, "Run it now; never ask again"),
        option(ALWAYS_SERVER, "Run it now; never ask again"),
    ]
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
    // A swarm worker has no client; its approvals go to the nearest client up
    // its spawn chain (see `chain_route`).
    let chain = chain_route::chain(ctx).await;
    let who = if chain.is_empty() {
        String::new()
    } else {
        format!(
            "Worker \"{}\": ",
            crate::id::extract_session_name(&ctx.session_id).unwrap_or(&ctx.session_id)
        )
    };
    let questions = [AskQuestion {
        question: format!(
            "{who}Allow MCP tool \"{tool}\" on server \"{server}\" with arguments {}?",
            render_arguments(arguments)
        ),
        header: Some("MCP approval".into()),
        options: card_options(),
        multi_select: false,
        allow_other: false,
    }];
    let prefix = if !chain.is_empty() {
        WORKER_REQUEST_PREFIX
    } else {
        MCP_REQUEST_PREFIX
    };
    let request_id = format!("{prefix}{}", uuid::Uuid::new_v4());
    let started = Instant::now();
    // The attach wait sits inside the parked ask, like the question itself, so a
    // reload does not wait for a person who is not there.
    let response = crate::turn_activity::global()
        .wait_for_ask(&ctx.session_id, &request_id, async {
            let route = match chain_route::find(ctx, &chain) {
                Some(route) => route,
                None => {
                    log("waiting_for_client", ctx, request, None);
                    chain_route::wait(ctx, &chain, wait).await?
                }
            };
            let routed_to = route.via.clone();
            log("asked", ctx, request, routed_to.as_deref());
            Some((
                chain_route::deliver(route, &questions, &request_id).await,
                routed_to,
            ))
        })
        .await;
    // Outside a turn there is no wait clock to charge; that is not an error.
    if HUMAN_WAIT
        .try_with(|clock| clock.add(started.elapsed()))
        .is_err()
    {
        crate::logging::debug("MCP approval wait was not charged to a turn clock");
    }
    let (response, routed_to) = match response {
        None => {
            let root = chain.last();
            log("refused_no_client", ctx, request, root.map(String::as_str));
            let nobody = match root {
                Some(root) => format!(
                    "session {} and its root session {root} have no attached client",
                    ctx.session_id
                ),
                None => format!("session {} has no attached client", ctx.session_id),
            };
            return Err(Refusal::error(
                "Needs a human yes",
                format!(
                    "MCP tool '{tool}' on server '{server}' needs a human yes (approval = \"annotations\") \
                     but {nobody} (headless run or swarm worker; waited {}s), so the call was refused. \
                     To run it unattended: `jcode mcp approvals allow {server} {tool}` \
                     (or `jcode mcp approvals allow {server}` for every tool on the server), \
                     or answer \"{ALWAYS_TOOL}\" when it asks in an attached session.",
                    wait.as_secs()
                ),
            ));
        }
        Some((Err(error), routed_to)) => {
            log("no_answer", ctx, request, routed_to.as_deref());
            return Err(Refusal::error(
                "No answer to the prompt",
                format!(
                    "MCP tool '{tool}' on server '{server}' was not approved: no answer ({error:#}). \
                     The call did not run."
                ),
            ));
        }
        Some((Ok(response), routed_to)) => (response, routed_to),
    };
    let decision = Decision::from_response(response);
    let grant = match decision {
        Decision::Deny => {
            log(decision.label(), ctx, request, routed_to.as_deref());
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
                log("grant_not_saved", ctx, request, routed_to.as_deref());
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
    log(decision.label(), ctx, request, routed_to.as_deref());
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
#[path = "mcp_approval_tests/mod.rs"]
mod tests;
