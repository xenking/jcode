use super::*;
use crate::mcp::{McpApproval, McpConfig, McpToolDef};
use serde_json::json;

/// A private `JCODE_HOME` plus a project directory, restored on drop.
struct Env {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let lock = crate::storage::lock_test_env();
        let previous = std::env::var_os("JCODE_HOME");
        let home = tempfile::tempdir().unwrap();
        crate::env::set_var("JCODE_HOME", home.path());
        Self {
            _lock: lock,
            previous,
            home,
            project: tempfile::tempdir().unwrap(),
        }
    }

    fn global(&self) -> std::path::PathBuf {
        self.home.path().join("mcp.json")
    }

    fn write(&self, path: &std::path::Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    fn read(&self, path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn dir(&self) -> Option<&Path> {
        Some(self.project.path())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn tool(name: &str) -> McpToolDef {
    McpToolDef {
        name: name.into(),
        ..Default::default()
    }
}

fn two_servers() -> Value {
    json!({
        "theme": "keep me",
        "servers": {
            "gated": {"command": "bun", "approval": "annotations", "env": {"K": "V"}},
            "other": {"command": "x", "approve_free": ["keep"]}
        }
    })
}

#[test]
fn tool_grant_lands_in_the_file_the_server_comes_from_and_touches_nothing_else() {
    let env = Env::new();
    env.write(&env.global(), two_servers());

    let path = grant(env.dir(), "gated", Grant::Tool("plan_item")).unwrap();

    assert_eq!(path, env.global());
    let mut expected = two_servers();
    expected["servers"]["gated"]["approve_free"] = json!(["plan_item"]);
    assert_eq!(env.read(&env.global()), expected);

    let mut live = McpConfig::load_for_dir(env.dir()).servers["gated"].clone();
    // The in-memory copy of a long-running session predates the grant.
    live.approve_free.clear();
    assert!(live.requires_approval(&tool("plan_item")));
    refresh_policy(&mut live, env.dir(), "gated");
    assert_eq!(live.approval, McpApproval::Annotations);
    assert!(!live.requires_approval(&tool("plan_item")));
    assert!(live.requires_approval(&tool("gitlab_mr_post_review")));
}

#[test]
fn server_grant_exempts_every_tool_but_keeps_the_gate_configured() {
    let env = Env::new();
    env.write(&env.global(), two_servers());

    grant(env.dir(), "gated", Grant::Server).unwrap();
    // Granting twice does not duplicate.
    grant(env.dir(), "gated", Grant::Server).unwrap();

    let doc = env.read(&env.global());
    assert_eq!(doc["servers"]["gated"]["approve_free"], json!(["*"]));
    assert_eq!(doc["servers"]["gated"]["approval"], "annotations");
    assert_eq!(doc["servers"]["other"], two_servers()["servers"]["other"]);
    let live = &McpConfig::load_for_dir(env.dir()).servers["gated"];
    assert!(!live.requires_approval(&tool("anything_destructive")));
}

#[test]
fn project_local_definition_is_written_in_the_project_file() {
    let env = Env::new();
    env.write(&env.global(), two_servers());
    let local = env.project.path().join(".mcp.json");
    env.write(
        &local,
        json!({"mcpServers": {"gated": {"command": "bun-local", "approval": "annotations"}}}),
    );

    let found = locate(env.dir(), "gated").unwrap();
    assert_eq!(found.path, local);
    assert_eq!(
        found.config.command,
        McpConfig::load_for_dir(env.dir()).servers["gated"].command
    );

    grant(env.dir(), "gated", Grant::Tool("t")).unwrap();
    assert_eq!(
        env.read(&local)["mcpServers"]["gated"]["approve_free"],
        json!(["t"])
    );
    assert_eq!(env.read(&env.global()), two_servers());
}

#[test]
fn malformed_or_unexpected_files_are_never_rewritten() {
    let env = Env::new();
    let broken = "{\"servers\": {\"gated\": {\"command\": \"bun\"},, }";
    std::fs::write(env.global(), broken).unwrap();
    let error = grant(env.dir(), "gated", Grant::Server).unwrap_err();
    assert!(error.to_string().contains("not configured"), "{error:#}");
    assert_eq!(std::fs::read_to_string(env.global()).unwrap(), broken);

    let odd = json!({"servers": {"gated": {"command": "bun", "approve_free": "plan_item"}}});
    env.write(&env.global(), odd.clone());
    let before = std::fs::read_to_string(env.global()).unwrap();
    // `approve_free` is a string: serde refuses it, so the server is not even located.
    assert!(grant(env.dir(), "gated", Grant::Server).is_err());
    assert_eq!(std::fs::read_to_string(env.global()).unwrap(), before);
}

#[test]
fn claude_owned_json_is_refused_not_rewritten() {
    let env = Env::new();
    let claude = env.home.path().join("external/.claude.json");
    env.write(
        &claude,
        json!({"mcpServers": {"theirs": {"command": "x", "approval": "annotations"}}}),
    );
    let before = std::fs::read_to_string(&claude).unwrap();
    let error = grant(env.dir(), "theirs", Grant::Server).unwrap_err();
    assert!(error.to_string().contains("Claude Code owns"), "{error:#}");
    assert_eq!(std::fs::read_to_string(&claude).unwrap(), before);
}

#[test]
fn revoke_removes_one_tool_or_every_grant_and_reports_nothing_to_do() {
    let env = Env::new();
    env.write(&env.global(), two_servers());
    for name in ["a", "b"] {
        grant(env.dir(), "gated", Grant::Tool(name)).unwrap();
    }
    grant(env.dir(), "gated", Grant::Server).unwrap();

    let (_, removed) = revoke(env.dir(), "gated", Some("a")).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        env.read(&env.global())["servers"]["gated"]["approve_free"],
        json!(["b", "*"])
    );
    assert!(revoke(env.dir(), "gated", Some("a")).is_err());

    let (_, removed) = revoke(env.dir(), "gated", None).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(env.read(&env.global()), {
        let mut expected = two_servers();
        expected["servers"]["gated"]
            .as_object_mut()
            .unwrap()
            .remove("approve_free");
        expected
    });
    assert!(revoke(env.dir(), "gated", None).is_err());
    assert!(revoke(env.dir(), "missing", None).is_err());
}

#[test]
fn locate_picks_the_same_definition_the_loader_runs() {
    let env = Env::new();
    env.write(&env.global(), two_servers());
    env.write(
        &env.project.path().join(".jcode/mcp.json"),
        json!({"servers": {"gated": {"command": "bun-project"}}}),
    );
    // A remote twin in a later file never displaces the stdio definition.
    env.write(
        &env.project.path().join(".mcp.json"),
        json!({"mcpServers": {"gated": {"type": "http", "url": "https://example.test/mcp"}}}),
    );

    let loaded = McpConfig::load_for_dir(env.dir());
    let found = locate(env.dir(), "gated").unwrap();
    assert_eq!(found.config.command, "bun-project");
    assert_eq!(found.config.command, loaded.servers["gated"].command);
    assert_eq!(found.path, env.project.path().join(".jcode/mcp.json"));
}

#[test]
fn config_toml_mcp_servers_are_reported_as_inert() {
    let env = Env::new();
    assert!(inert_config_toml_warning().is_none());

    std::fs::write(
        env.home.path().join("config.toml"),
        "[tools]\nmcp_tools = \"auto\"\n",
    )
    .unwrap();
    assert!(inert_config_toml_warning().is_none());

    std::fs::write(
        env.home.path().join("config.toml"),
        "[mcp.servers.gated]\napproval = \"annotations\"\n[mcp.servers.zed]\ncommand = \"z\"\n",
    )
    .unwrap();
    let warning = inert_config_toml_warning().unwrap();
    assert!(warning.contains("config.toml"), "{warning}");
    assert!(warning.contains("[mcp.servers.gated]"), "{warning}");
    assert!(warning.contains("[mcp.servers.zed]"), "{warning}");
    assert!(warning.contains("mcp.json"), "{warning}");
}
