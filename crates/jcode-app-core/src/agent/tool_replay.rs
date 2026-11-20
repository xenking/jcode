use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Scope {
    working_dir: Option<String>,
    mcp_mode: crate::config::McpToolsMode,
    native_deferred: bool,
    allowed: Option<BTreeSet<String>>,
    disabled: BTreeSet<String>,
    canary: bool,
    desktop_selfdev: bool,
}

/// The provider-visible surface, not executable tool implementations. A saved
/// definition never gives a tool permission to execute or reconnects a server.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct ToolSurface {
    scope: Scope,
    tools: Vec<ToolDefinition>,
    mcp_late_register_resolved: bool,
}

impl Agent {
    fn tool_surface_scope(&self) -> Option<Scope> {
        // SDK policies/closures belong to their connection. Do not restore
        // a former caller's definitions into a session with a different policy.
        if crate::tool::sdk::config(self.session_id()).is_some() {
            return None;
        }
        Some(Scope {
            working_dir: self.session.working_dir.clone(),
            mcp_mode: self.mcp_tools_mode,
            native_deferred: self.native_deferred_mcp(),
            allowed: self
                .allowed_tools
                .as_ref()
                .map(|names| names.iter().cloned().collect()),
            disabled: self.disabled_tools.iter().cloned().collect(),
            canary: self.session.is_canary,
            desktop_selfdev: self.is_desktop_selfdev(),
        })
    }

    pub(super) fn restore_tool_surface(&mut self) {
        let Some(scope) = self.tool_surface_scope() else {
            return;
        };
        let Some(saved) = tool_surface_path(self.session_id())
            .ok()
            .and_then(|path| crate::storage::read_json::<ToolSurface>(&path).ok())
            .filter(|saved| saved.scope == scope)
        else {
            return;
        };
        self.locked_tools = Some(ToolDefinition::eager(&saved.tools));
        self.locked_tools_native_deferred = scope.native_deferred;
        self.mcp_late_register_resolved = saved.mcp_late_register_resolved;
        self.announced_mcp_tools.extend(
            saved
                .tools
                .iter()
                .filter(|def| !def.defer_loading && def.name.starts_with("mcp__"))
                .map(|def| def.name.clone()),
        );
        self.sent_tool_surface = Some(saved);
    }

    pub(super) fn prepare_tools_for_request(
        &mut self,
        mut tools: Vec<ToolDefinition>,
        messages: &[Message],
    ) -> Result<Vec<ToolDefinition>> {
        let Some(scope) = self.tool_surface_scope() else {
            return Ok(tools);
        };
        if let Some(saved) = self
            .sent_tool_surface
            .as_ref()
            .filter(|saved| saved.scope == scope)
        {
            if scope.native_deferred && saved.tools != tools {
                retain_revealed_deferred(&mut tools, &saved.tools, messages);
            }
            if saved.tools == tools
                && saved.mcp_late_register_resolved == self.mcp_late_register_resolved
            {
                return Ok(tools);
            }
        }
        let surface = ToolSurface {
            scope,
            tools: tools.clone(),
            mcp_late_register_resolved: self.mcp_late_register_resolved,
        };
        let path = tool_surface_path(self.session_id())?;
        crate::storage::assert_not_real_home(&path);
        crate::storage::write_json_secret(&path, &surface)?;
        self.sent_tool_surface = Some(surface);
        Ok(tools)
    }
}

fn tool_surface_path(session_id: &str) -> Result<std::path::PathBuf> {
    anyhow::ensure!(
        !session_id.is_empty() && !session_id.contains(['/', '\\']) && !session_id.starts_with('.'),
        "invalid session ID for tool replay"
    );
    Ok(crate::session::session_path(session_id)?.with_extension("tool-surface"))
}

/// A reference already on the wire must still resolve even if its registry
/// entry is late/offline or has changed schema. Keep live, unrevealed tools
/// eligible for discovery rather than freezing the entire deferred registry.
fn retain_revealed_deferred(
    tools: &mut Vec<ToolDefinition>,
    previous: &[ToolDefinition],
    messages: &[Message],
) {
    let referenced: HashSet<&str> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| {
            if let ContentBlock::ToolReference { tool_name, .. } = block {
                Some(tool_name.as_str())
            } else {
                None
            }
        })
        .collect();
    if referenced.is_empty() {
        return;
    }
    let mut deferred: HashMap<String, ToolDefinition> = HashMap::new();
    let mut order = Vec::new();
    tools.retain(|def| {
        if !def.defer_loading {
            return true;
        }
        order.push(def.name.clone());
        deferred.insert(def.name.clone(), def.clone());
        false
    });
    for def in previous.iter().filter(|def| def.defer_loading) {
        if referenced.contains(def.name.as_str()) {
            deferred.remove(&def.name);
            tools.push(def.clone());
        } else if let Some(live) = deferred.remove(&def.name) {
            tools.push(live);
        }
    }
    tools.extend(order.iter().filter_map(|name| deferred.remove(name)));
}
