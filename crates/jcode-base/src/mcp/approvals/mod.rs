//! Persistent "always allow" answers for MCP approval (`approval = "annotations"`).
//!
//! A grant lives in the config file the server definition is read from, as
//! `approve_free` entries: the raw tool name for one tool, [`ALL_TOOLS`] for
//! every tool of the server. Policy is read back from disk on every call
//! ([`refresh_policy`]), so a grant or a revoke made by any process (the
//! approval card, `jcode mcp approvals`) applies to every running session
//! without a reload.

use super::protocol::McpServerConfig;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// `approve_free` entry that exempts every tool of the server.
pub use super::protocol::ALL_TOOLS;

/// What an "always allow" answer covers.
#[derive(Debug, Clone, Copy)]
pub enum Grant<'a> {
    Tool(&'a str),
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// jcode-style file: `servers` / `mcpServers` at the top level.
    Plain,
    /// `~/.claude.json`: owned and rewritten by Claude Code.
    ClaudeJson,
}

/// The file entry a server's effective definition comes from.
#[derive(Debug, Clone)]
pub struct Located {
    pub path: PathBuf,
    pub config: McpServerConfig,
    kind: Kind,
    pointer: String,
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// Config files in the order `McpConfig::load_for_dir` merges them (later wins).
/// Keep in step with that function; a test pins the agreement.
fn sources(project_dir: Option<&Path>) -> Vec<(PathBuf, Kind)> {
    let mut sources = Vec::new();
    if let Ok(dir) = crate::storage::jcode_dir() {
        sources.push((dir.join("mcp.json"), Kind::Plain));
    }
    if std::env::var_os("JCODE_DISABLE_CLAUDE_MCP").is_none() {
        if let Ok(path) = crate::storage::user_home_path(".claude.json") {
            sources.push((path, Kind::ClaudeJson));
        }
        if let Ok(path) = crate::storage::user_home_path(".claude/mcp.json") {
            sources.push((path, Kind::Plain));
        }
    }
    if let Some(root) = project_dir {
        for relative in [".jcode/mcp.json", ".mcp.json", ".claude/mcp.json"] {
            sources.push((root.join(relative), Kind::Plain));
        }
    }
    sources
}

fn entry_pointers(kind: Kind, server: &str, project_dir: Option<&Path>) -> Vec<String> {
    let name = escape(server);
    match kind {
        Kind::Plain => vec![format!("/servers/{name}"), format!("/mcpServers/{name}")],
        Kind::ClaudeJson => {
            let mut pointers = vec![format!("/mcpServers/{name}")];
            if let Some(dir) = project_dir {
                let dir = escape(&dir.to_string_lossy());
                pointers.push(format!("/projects/{dir}/mcpServers/{name}"));
            }
            pointers
        }
    }
}

/// The definition of `server` that jcode actually runs, and where it is stored.
pub fn locate(project_dir: Option<&Path>, server: &str) -> Option<Located> {
    let mut winner: Option<Located> = None;
    for (path, kind) in sources(project_dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cheap reject before parsing a large `~/.claude.json`.
        if !text.contains(server) {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for pointer in entry_pointers(kind, server, project_dir) {
            let Some(entry) = doc.pointer(&pointer) else {
                continue;
            };
            let Ok(config) = serde_json::from_value::<McpServerConfig>(entry.clone()) else {
                continue;
            };
            // A remote twin never displaces a stdio definition (see merge).
            if winner
                .as_ref()
                .is_some_and(|won| won.config.is_stdio() && !config.is_stdio())
            {
                continue;
            }
            winner = Some(Located {
                path: path.clone(),
                config,
                kind,
                pointer,
            });
        }
    }
    winner
}

/// Overwrite the approval fields of an in-memory definition with what is on
/// disk now. Left unchanged when the definition is not found in any file.
pub fn refresh_policy(config: &mut McpServerConfig, project_dir: Option<&Path>, server: &str) {
    if let Some(live) = locate(project_dir, server) {
        config.approval = live.config.approval;
        config.approve_free = live.config.approve_free;
    }
}

/// Apply `change` to the server's entry in the file it comes from. The file is
/// replaced atomically, and only when `change` reports a modification. A file
/// that does not parse is never rewritten.
static EDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn edit(
    project_dir: Option<&Path>,
    server: &str,
    change: impl FnOnce(&mut Map<String, Value>) -> Result<bool>,
) -> Result<PathBuf> {
    // Serializes this process's read-modify-write; another process can still interleave.
    let _guard = EDIT.lock().unwrap_or_else(|e| e.into_inner());
    let located = locate(project_dir, server).with_context(|| {
        format!(
            "MCP server '{server}' is not configured. Run `jcode mcp list` to see configured names."
        )
    })?;
    if located.kind == Kind::ClaudeJson {
        bail!(
            "MCP server '{server}' is defined in {}, which Claude Code owns and rewrites. \
             Define it in ~/.jcode/mcp.json to manage its approvals from jcode.",
            located.path.display()
        );
    }
    let path = located.path;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON; it was left untouched",
            path.display()
        )
    })?;
    let entry = doc
        .pointer_mut(&located.pointer)
        .and_then(Value::as_object_mut)
        .with_context(|| format!("server '{server}' vanished from {}", path.display()))?;
    if change(entry)? {
        let mut out = serde_json::to_string_pretty(&doc)?;
        if text.ends_with('\n') {
            out.push('\n');
        }
        let permissions = match std::fs::metadata(&path) {
            Ok(meta) => Some(meta.permissions()),
            Err(_) => None,
        };
        crate::storage::write_bytes(&path, out.as_bytes())?;
        if let Some(permissions) = permissions
            && let Err(error) = std::fs::set_permissions(&path, permissions)
        {
            crate::logging::warn(&format!(
                "MCP approvals: could not restore permissions of {}: {error}",
                path.display()
            ));
        }
    }
    Ok(path)
}

fn approve_free(entry: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
    entry
        .entry("approve_free")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("`approve_free` is not an array; fix it by hand")
}

/// Persist an "always allow" answer. Returns the file written (or already
/// holding the grant).
pub fn grant(project_dir: Option<&Path>, server: &str, grant: Grant<'_>) -> Result<PathBuf> {
    let name = match grant {
        Grant::Tool(tool) => tool,
        Grant::Server => ALL_TOOLS,
    };
    edit(project_dir, server, |entry| {
        let list = approve_free(entry)?;
        if list.iter().any(|item| item.as_str() == Some(name)) {
            return Ok(false);
        }
        list.push(Value::String(name.to_string()));
        Ok(true)
    })
}

/// Drop one tool's grant, or with `tool = None` every grant of the server
/// (the tool entries and the server-wide entry). Returns the file and the number
/// of entries removed; errors when there was nothing to remove.
pub fn revoke(
    project_dir: Option<&Path>,
    server: &str,
    tool: Option<&str>,
) -> Result<(PathBuf, usize)> {
    let mut removed = 0;
    let path = edit(project_dir, server, |entry| {
        let Some(list) = entry.get_mut("approve_free").and_then(Value::as_array_mut) else {
            return Ok(false);
        };
        let before = list.len();
        list.retain(|item| tool.is_some_and(|tool| item.as_str() != Some(tool)));
        removed = before - list.len();
        if list.is_empty() {
            entry.remove("approve_free");
        }
        Ok(removed > 0)
    })?;
    if removed == 0 {
        bail!(
            "nothing to revoke: {} has no approve_free entry for {}",
            server,
            tool.map_or("this server".to_string(), |tool| format!("tool '{tool}'"))
        );
    }
    Ok((path, removed))
}

/// Servers listed in `config.toml` under `[mcp.servers.*]`. jcode reads no such
/// table, so each entry is inert; returns the file and the server names.
pub fn inert_config_toml_servers() -> Option<(PathBuf, Vec<String>)> {
    let path = crate::config::Config::path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return None;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return None;
    };
    let mut names: Vec<String> = doc
        .get("mcp")?
        .get("servers")?
        .as_table()?
        .keys()
        .cloned()
        .collect();
    names.sort();
    (!names.is_empty()).then_some((path, names))
}

/// The warning for [`inert_config_toml_servers`], or `None` when there is nothing to warn about.
pub fn inert_config_toml_warning() -> Option<String> {
    let (path, names) = inert_config_toml_servers()?;
    let Ok(dir) = crate::storage::jcode_dir() else {
        return None;
    };
    let global = dir.join("mcp.json");
    Some(format!(
        "MCP: [mcp.servers.{}] in {} is ignored: jcode reads MCP servers (and their `approval` / `approve_free` policy) only from {} (or project .jcode/mcp.json), under the key \"servers\". Move the entries there.",
        names.join("], [mcp.servers."),
        path.display(),
        global.display()
    ))
}

/// Log [`inert_config_toml_warning`] once per process.
pub fn warn_inert_config_toml_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(warning) = inert_config_toml_warning() {
            crate::logging::warn(&warning);
        }
    });
}

#[cfg(test)]
mod tests;
