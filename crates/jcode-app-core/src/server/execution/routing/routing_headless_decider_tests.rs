//! `jcode run` has no daemon `Server`, so nothing installs the routing decider
//! at startup. The decider is a process-wide `OnceLock`, so this test runs in a
//! child process of the test binary and cannot leak it into other tests.

use super::*;
use crate::message::{Message, ToolDefinition};
use crate::provider::{EventStream, Provider, has_alternate_account_in_pool};
use crate::tool::Registry;
use std::sync::Arc;

struct OAuthLogin;

#[async_trait::async_trait]
impl Provider for OAuthLogin {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        panic!("a routing decision sends no model request")
    }

    fn name(&self) -> &str {
        "OpenAI"
    }

    fn model(&self) -> String {
        "gpt-5.4".into()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(OAuthLogin)
    }
}

fn set_routing(mode: &str) {
    let mut config = format!("[routing]\nmode = \"{mode}\"\n");
    for id in ["openai-otter", "openai-fox"] {
        config += &format!("[[routing.accounts]]\nid = \"openai:{id}\"\n");
    }
    let home = crate::storage::jcode_dir().unwrap();
    std::fs::write(home.join("config.toml"), config).unwrap();
    crate::config::Config::invalidate_cache();
}

/// Re-runs this test in a child process with its own home, so the decider
/// installed by `route_unattached_turn` stays out of every other test.
fn in_child_process(test: &str) -> bool {
    const CHILD: &str = "JCODE_HEADLESS_DECIDER_TEST";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1")
        .env("HOME", home.path())
        .env("JCODE_HOME", home.path())
        .env("JCODE_TEST_SESSION", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[tokio::test]
async fn a_headless_run_in_auto_routing_leaves_account_choice_to_routing() {
    let name = concat!(
        module_path!(),
        "::a_headless_run_in_auto_routing_leaves_account_choice_to_routing"
    );
    if in_child_process(name.split_once("::").unwrap().1) {
        return;
    }
    for label in ["openai-otter", "openai-fox"] {
        crate::auth::codex::upsert_account_from_tokens(
            label,
            "fake-access",
            "fake-refresh",
            None,
            None,
        )
        .unwrap();
    }
    crate::auth::codex::set_active_account_override(Some("openai-otter".into()));
    let rotates = || has_alternate_account_in_pool("openai", "gpt-5.4");
    let mut agent = Agent::new(Arc::new(OAuthLogin), Registry::empty());

    // Routing off: the account pool owns failover, before and after a run.
    set_routing("off");
    route_unattached_turn(&mut agent).await.unwrap();
    assert!(rotates(), "with routing off the pool still rotates");

    // Routing auto: the only decider in this process is the one the turn installed.
    set_routing("auto");
    let _ = route_unattached_turn(&mut agent).await;
    assert!(
        !rotates(),
        "a headless run has one decider: routing, not the pool"
    );

    // The decider follows the live mode, so turning routing off hands it back.
    set_routing("off");
    assert!(rotates(), "routing off gives failover back to the pool");
}
