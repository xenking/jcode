use super::*;

pub(crate) fn tool_name_is_allowed(allowed: &HashSet<String>, name: &str) -> bool {
    allowed.contains(name)
        || (allowed.contains("mcp") && is_mcp_tool_name(name))
        || (name == "mcp" && allowed.iter().any(|tool| is_mcp_tool_name(tool)))
        || (is_fixed_mcp_tool(name) && allowed.iter().any(|tool| tool.starts_with("mcp__")))
}

pub(crate) fn tool_name_is_disabled(disabled: &HashSet<String>, name: &str) -> bool {
    disabled.contains(name) || (disabled.contains("mcp") && is_mcp_tool_name(name))
}

pub(super) fn is_fixed_mcp_tool(name: &str) -> bool {
    matches!(name, "mcp_search" | "mcp_call")
}

pub(super) fn is_mcp_tool_name(name: &str) -> bool {
    name == "mcp" || name.starts_with("mcp__") || is_fixed_mcp_tool(name)
}
