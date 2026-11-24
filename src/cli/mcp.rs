//! Explicit MCP OAuth management. Listing never connects or refreshes tokens.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::args::McpCommand;
use crate::mcp::{McpConfig, McpServerConfig, oauth};

fn remote_server<'a>(config: &'a McpConfig, name: &str) -> Result<(&'a McpServerConfig, &'a str)> {
    let server = config.servers.get(name).with_context(|| {
        format!(
            "MCP server '{name}' is not configured. Run `jcode mcp list` to see configured names."
        )
    })?;
    if !server.is_remote() {
        bail!(
            "MCP server '{name}' uses stdio. OAuth login/logout is only supported for remote HTTP/SSE servers."
        );
    }
    let url = server
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .with_context(|| format!("Remote MCP server '{name}' has no URL configured."))?;
    Ok((server, url))
}

fn static_auth(server: &McpServerConfig) -> bool {
    server
        .headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"))
}

fn auth_status(server: &McpServerConfig, stored: Option<&oauth::OAuthStatus>) -> &'static str {
    if !server.is_remote() {
        return "not applicable";
    }
    if static_auth(server) {
        return "static Authorization header";
    }
    if stored.is_some_and(|entry| entry.logged_in) {
        "OAuth credentials stored"
    } else {
        "no OAuth credentials stored"
    }
}

fn server_rows(config: &McpConfig) -> Result<Vec<Value>> {
    let mut names: Vec<_> = config.servers.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let server = &config.servers[name];
            let stored = if server.is_remote() && !static_auth(server) {
                server.url.as_deref().map(oauth::status).transpose()?
            } else {
                None
            };
            Ok(json!({
                "name": name,
                "transport": server.remote_transport().map_or("stdio", |kind| kind.label()),
                "enabled": server.is_enabled(),
                "auth": auth_status(server, stored.as_ref()),
            }))
        })
        .collect()
}

fn render_list(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "No MCP servers configured. Add servers to ~/.jcode/mcp.json or .jcode/mcp.json.\n"
            .into();
    }
    let mut output = String::from("Configured MCP servers (not a connection check):\n");
    for row in rows {
        output.push_str(&format!(
            "  {} [{}; {}] {}\n",
            row["name"].as_str().unwrap_or("<unnamed>"),
            row["transport"].as_str().unwrap_or("unknown"),
            if row["enabled"].as_bool().unwrap_or(false) {
                "enabled"
            } else {
                "disabled"
            },
            row["auth"].as_str().unwrap_or("unknown")
        ));
    }
    output
}

/// One `mcp test` row: the tool name plus what jcode learned from the server.
/// `unannotated` and `writes` both mean a call asks first when the server opted
/// in to approval; only `read-only` runs free.
fn render_tool_line(server: &McpServerConfig, tool: &crate::mcp::McpToolDef) -> String {
    let mut notes = Vec::new();
    notes.push(if tool.is_read_only() {
        "read-only"
    } else if tool.annotations.is_some() {
        "writes"
    } else {
        "unannotated"
    });
    if tool.output_schema.is_some() {
        notes.push("typed result");
    }
    if server.requires_approval(tool) {
        notes.push("asks first");
    }
    format!("{}  ({})", tool.name, notes.join(", "))
}

pub(crate) async fn run(action: McpCommand) -> Result<()> {
    let config = McpConfig::load();
    match action {
        McpCommand::Test { server: name } => {
            let server = config.servers.get(&name).with_context(|| format!("MCP server '{name}' is not configured. Run `jcode mcp list` to see configured names."))?;
            let mut client = crate::mcp::McpClient::connect(name.clone(), server).await
                .with_context(|| format!("Failed to connect to MCP server '{name}'. For remote OAuth authentication, run `jcode mcp login {name}` first."))?;
            let info = client.server_info();
            let mut tools = client.tools();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            client.shutdown().await;
            println!("Connected to MCP server '{name}' and disconnected cleanly.");
            if let Some(info) = info {
                println!(
                    "Server: {} {}",
                    info.name,
                    info.version.as_deref().unwrap_or("(version not reported)")
                );
            }
            println!("Tools: {}", tools.len());
            if !server.approval.is_off() {
                let asking = tools
                    .iter()
                    .filter(|tool| server.requires_approval(tool))
                    .count();
                println!(
                    "Approval: annotations ({asking} of {} tools ask a human first)",
                    tools.len()
                );
            }
            for tool in tools {
                println!("  {}", render_tool_line(server, &tool));
            }
        }
        McpCommand::List { json } => {
            let rows = server_rows(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", render_list(&rows));
            }
        }
        McpCommand::Login {
            server: name,
            no_browser,
        } => {
            let (server, url) = remote_server(&config, &name)?;
            if static_auth(server) {
                bail!(
                    "MCP server '{name}' has a static Authorization header, which takes precedence over OAuth. Remove that header from MCP config before using `jcode mcp login {name}`."
                );
            }
            let mode = if no_browser {
                oauth::BrowserMode::Print
            } else {
                oauth::BrowserMode::Open
            };
            eprintln!("Discovering OAuth configuration for MCP server '{name}'...");
            oauth::login(url, mode, |url| {
                eprintln!("Open this URL to authorize MCP access:\n{url}\nWaiting for the browser callback (up to five minutes). Keep this command running.");
                if no_browser {
                    eprintln!("The callback must reach this machine's loopback listener. Over SSH, forward the callback port shown in redirect_uri or run login on the browser's machine.");
                }
            }).await.with_context(|| format!("OAuth login failed for MCP server '{name}'"))?;
            println!(
                "Logged in to MCP server '{name}'. Reconnect it or reload MCP servers to use the credentials."
            );
        }
        McpCommand::Logout { server: name } => {
            let (_, url) = remote_server(&config, &name)?;
            let removed = oauth::logout(url)?;
            println!(
                "{} for MCP server '{name}'.",
                if removed {
                    "Removed local OAuth credentials"
                } else {
                    "No local OAuth credentials stored"
                }
            );
            println!(
                "This does not revoke tokens, remove static headers, or close existing MCP connections. Disconnect/reconnect active servers to stop using the old token."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> McpConfig {
        serde_json::from_value(json!({"servers": {
            "local": {"command": "test", "args": ["secret-command-arg"]},
            "remote": {"url": "https://example.com/mcp?secret=token", "headers": {"aUtHoRiZaTiOn": "Bearer secret-header"}, "enabled": false},
            "broken": {"type": "http"}
        }})).unwrap()
    }

    #[test]
    fn mcp_test_rows_show_what_the_server_declared_and_what_will_ask() {
        use crate::mcp::{McpApproval, McpToolAnnotations, McpToolDef};
        let tool = |name: &str, read_only: Option<bool>, typed: bool| McpToolDef {
            name: name.into(),
            input_schema: json!({"type": "object"}),
            output_schema: typed.then(|| json!({"type": "object"})),
            annotations: read_only.map(|flag| McpToolAnnotations {
                read_only_hint: Some(flag),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut server: McpServerConfig =
            serde_json::from_value(json!({"command": "x", "approve_free": ["free"]})).unwrap();
        assert_eq!(
            render_tool_line(&server, &tool("get", Some(true), true)),
            "get  (read-only, typed result)"
        );
        assert_eq!(
            render_tool_line(&server, &tool("put", Some(false), false)),
            "put  (writes)"
        );
        server.approval = McpApproval::Annotations;
        assert_eq!(
            render_tool_line(&server, &tool("put", Some(false), false)),
            "put  (writes, asks first)"
        );
        assert_eq!(
            render_tool_line(&server, &tool("bare", None, false)),
            "bare  (unannotated, asks first)"
        );
        assert_eq!(
            render_tool_line(&server, &tool("free", None, false)),
            "free  (unannotated)"
        );
    }

    #[test]
    fn mcp_cli_resolves_configured_remote_server_and_actionable_errors() {
        let mut config = config();
        let (server, url) = remote_server(&config, "remote").unwrap();
        assert!(std::ptr::eq(server, &config.servers["remote"]));
        assert_eq!(url, "https://example.com/mcp?secret=token");
        assert!(
            remote_server(&config, "missing")
                .unwrap_err()
                .to_string()
                .contains("jcode mcp list")
        );
        assert!(
            remote_server(&config, "local")
                .unwrap_err()
                .to_string()
                .contains("stdio")
        );
        assert!(
            remote_server(&config, "broken")
                .unwrap_err()
                .to_string()
                .contains("no URL")
        );
        for url in ["", " \t\n"] {
            config.servers.get_mut("broken").unwrap().url = Some(url.into());
            assert!(
                remote_server(&config, "broken")
                    .unwrap_err()
                    .to_string()
                    .contains("no URL")
            );
        }
    }

    #[test]
    fn mcp_cli_list_is_sorted_redacted_and_shows_static_auth_precedence() {
        let rows = server_rows(&config()).unwrap();
        assert_eq!(rows[0]["name"], "broken");
        assert_eq!(rows[2]["auth"], "static Authorization header");
        let output = render_list(&rows);
        assert!(output.contains("remote [http; disabled]"));
        assert!(!serde_json::to_string(&rows).unwrap().contains("secret"));
        assert!(!output.contains("secret"));
        assert!(render_list(&[]).contains("No MCP servers configured"));
    }

    #[test]
    fn mcp_cli_status_does_not_claim_stored_tokens_are_valid() {
        let config = config();
        let server = &config.servers["broken"];
        let expired = oauth::OAuthStatus {
            logged_in: true,
            expires_at: Some(1),
            ..Default::default()
        };
        assert_eq!(
            auth_status(server, Some(&expired)),
            "OAuth credentials stored"
        );
        assert_eq!(
            auth_status(&config.servers["remote"], Some(&expired)),
            "static Authorization header"
        );
        assert_eq!(
            auth_status(&config.servers["local"], Some(&expired)),
            "not applicable"
        );
    }

    #[test]
    fn mcp_cli_list_reports_corrupt_oauth_store_without_exposing_contents() {
        let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        let path = crate::storage::jcode_dir().unwrap().join("mcp-oauth.json");
        std::fs::write(path, "corrupt-secret-access-token").unwrap();
        let config = serde_json::from_value(
            json!({"servers": {"remote": {"url": "https://example.com/mcp"}}}),
        )
        .unwrap();
        let error = server_rows(&config).unwrap_err().to_string();
        assert!(error.contains("OAuth credential store is invalid"));
        assert!(!error.contains("secret-access-token"));
    }
}
