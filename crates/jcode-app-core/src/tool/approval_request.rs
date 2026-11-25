//! Classification of ask request ids that only a person may answer.
//!
//! A leaf module: the interactive routes and the MCP approval gate both need
//! the same rule, and neither should depend on the other for it.

/// MCP approval request ids carry this prefix after the `ask-` the transport adds.
pub(crate) const MCP_REQUEST_PREFIX: &str = "mcp-approval-";

pub(crate) fn is_owner_approval(request_id: &str) -> bool {
    request_id.starts_with("ask-premium-spawn-")
}

/// Approvals only a person may answer. Owner approvals are the strictest and
/// are routed to the TUI alone. An MCP call approval may be answered from any
/// attached UI, but never by the model: automation tools and key injection
/// refuse to answer these, and the plain-text stdin path cannot.
pub(crate) fn is_human_approval(request_id: &str) -> bool {
    is_owner_approval(request_id)
        || request_id
            .strip_prefix("ask-")
            .is_some_and(|rest| rest.starts_with(MCP_REQUEST_PREFIX))
}
