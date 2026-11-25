//! How the agent surfaces MCP tools: eager, fixed deferred and late registration.

use super::*;

/// A trivial tool used to simulate an MCP tool registering on the registry
/// after the agent has already locked its tool snapshot.
pub(super) struct FakeMcpTool {
    pub(super) name: String,
}

#[async_trait]
impl crate::tool::Tool for FakeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake mcp tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: crate::tool::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::new("ok"))
    }
}

struct VerboseFakeMcpTool {
    pub(super) name: String,
    description: String,
}

#[async_trait]
impl crate::tool::Tool for VerboseFakeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}}
        })
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: crate::tool::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::new("ok"))
    }
}

pub(super) async fn register_fake_deferred_mcp_surface(registry: &Registry) {
    let manager = Arc::new(tokio::sync::RwLock::new(
        crate::mcp::McpManager::with_config(Default::default()),
    ));
    registry
        .register(
            "mcp".into(),
            Arc::new(
                crate::tool::mcp::McpManagementTool::new(manager.clone())
                    .with_registry(registry.clone()),
            ),
        )
        .await;
    registry
        .register(
            "mcp_search".into(),
            Arc::new(
                crate::tool::mcp::McpSearchTool::new(manager.clone())
                    .with_registry(registry.clone()),
            ),
        )
        .await;
    registry
        .register(
            "mcp_call".into(),
            Arc::new(crate::tool::mcp::McpCallTool::new(manager).with_registry(registry.clone())),
        )
        .await;
}

pub(super) async fn agent_with_fake_mcp_surface(
    mode: crate::config::McpToolsMode,
    _threshold: usize,
) -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    registry
        .register(
            "mcp__test__verbose".to_string(),
            Arc::new(VerboseFakeMcpTool {
                name: "verbose".to_string(),
                description: "large MCP definition ".repeat(32),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = mode;
    agent
}

#[tokio::test]
async fn mcp_exposure_modes_select_eager_or_fixed_definitions() {
    let _guard = crate::storage::lock_test_env();

    let mut eager = agent_with_fake_mcp_surface(crate::config::McpToolsMode::Eager, 0).await;
    let eager_names: Vec<String> = eager
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(eager_names.iter().any(|name| name == "mcp__test__verbose"));
    assert!(!eager_names.iter().any(|name| name == "mcp_search"));
    assert!(!eager_names.iter().any(|name| name == "mcp_call"));

    let mut deferred =
        agent_with_fake_mcp_surface(crate::config::McpToolsMode::Deferred, usize::MAX).await;
    let deferred_names: Vec<String> = deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(!deferred_names.iter().any(|name| name.starts_with("mcp__")));
    assert!(deferred_names.iter().any(|name| name == "mcp"));
    assert!(
        !deferred_names
            .iter()
            .any(|name| name == "mcp_search" || name == "mcp_call")
    );

    // `auto` never exposes per-server definitions on providers without
    // native deferred loading, even for a tiny catalog: any later change to
    // that list would bust the whole prompt cache.
    let mut auto_small =
        agent_with_fake_mcp_surface(crate::config::McpToolsMode::Auto, usize::MAX).await;
    let auto_small_names: Vec<String> = auto_small
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        !auto_small_names
            .iter()
            .any(|name| name.starts_with("mcp__"))
    );
    assert!(auto_small_names.iter().any(|name| name == "mcp"));

    let mut auto_deferred = agent_with_fake_mcp_surface(crate::config::McpToolsMode::Auto, 1).await;
    let auto_deferred_names: Vec<String> = auto_deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        !auto_deferred_names
            .iter()
            .any(|name| name.starts_with("mcp__"))
    );
    assert!(auto_deferred_names.iter().any(|name| name == "mcp"));
    let stable_auto_names: Vec<String> = auto_deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(auto_deferred_names, stable_auto_names);
    assert!(auto_deferred.mcp_late_register_resolved);
}

#[tokio::test]
async fn deferred_mcp_surface_ignores_late_per_tool_registration() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = crate::config::McpToolsMode::Deferred;

    let before: Vec<String> = agent
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    agent
        .registry
        .register(
            "mcp__late__tool".to_string(),
            Arc::new(FakeMcpTool {
                name: "late".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after: Vec<String> = agent
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert_eq!(
        before, after,
        "fixed deferred surface must stay cache-stable"
    );
    assert!(agent.mcp_late_register_resolved);
    assert!(!after.iter().any(|name| name.starts_with("mcp__")));
}

#[tokio::test]
async fn auto_mode_never_changes_the_tool_list_for_late_mcp_tools() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = crate::config::McpToolsMode::Auto;

    let before = agent.tool_definitions().await;
    assert!(before.iter().any(|tool| tool.name == "mcp"));
    agent
        .registry
        .register(
            "mcp__late__large".to_string(),
            Arc::new(VerboseFakeMcpTool {
                name: "large".to_string(),
                description: "late large definition ".repeat(32),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    agent.unlock_tools_if_needed("mcp");

    let after = agent.tool_definitions().await;
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&after).unwrap(),
        "late MCP registration and the mcp tool must not change the cached tool list"
    );
    assert!(agent.mcp_late_register_resolved);
}

/// Reproduction for #206: MCP tools that register on the registry *after* the
/// first turn locks the tool snapshot never reach the provider, because
/// `tool_definitions()` returns the frozen `locked_tools` snapshot and the only
/// unlock path (`unlock_tools_if_needed`) fires solely when the LLM invokes the
/// `"mcp"` management tool — which it never does, since it cannot see the
/// `mcp__*` tools it would need to trigger that unlock.
#[tokio::test]
async fn mcp_tools_registered_after_lock_are_visible_to_agent() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    // The late-registration rebuild (#206) only exists in explicit eager
    // mode; every other mode keeps the tool list fixed for cache stability.
    agent.mcp_tools_mode = crate::config::McpToolsMode::Eager;

    // First turn locks the snapshot (this is what happens before the async MCP
    // registration spawn completes).
    let before = agent.tool_definitions().await;
    let before_len = before.len();
    assert!(
        !before.iter().any(|t| t.name.starts_with("mcp__")),
        "precondition: no mcp tools before async registration completes"
    );

    // Simulate the spawned MCP registration task finishing: a new mcp__* tool
    // lands on the shared registry.
    agent
        .registry
        .register(
            "mcp__test__write_memory".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__write_memory".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;

    // The next turn should now advertise the MCP tool to the provider.
    let after = agent.tool_definitions().await;
    assert!(
        after.iter().any(|t| t.name == "mcp__test__write_memory"),
        "regression #206: MCP tool registered after the first turn never reaches \
         the agent's tool surface (locked snapshot of {} tools is reused forever)",
        before_len
    );

    // Once MCP tools are present in the locked snapshot, subsequent turns must
    // return the *same* stable snapshot so provider prompt-cache hits stay warm
    // (the whole point of locked_tools). The #206 fix must not flap.
    let names =
        |defs: &[ToolDefinition]| -> Vec<String> { defs.iter().map(|t| t.name.clone()).collect() };
    let stable_a = agent.tool_definitions().await;
    let stable_b = agent.tool_definitions().await;
    assert_eq!(
        names(&stable_a),
        names(&stable_b),
        "tool snapshot must be stable across turns once MCP tools are present"
    );
    assert_eq!(
        names(&stable_a),
        names(&after),
        "snapshot must not change after MCP tools are already included"
    );
}

/// The intentional, MCP-driven prompt-cache miss must happen at most ONCE per
/// locked snapshot. After the first late-registered `mcp__*` tool is picked up
/// (the one accepted miss), a *second* MCP tool that registers even later must
/// NOT trigger another rebuild — otherwise a server that connects in waves would
/// thrash the provider prompt cache. Guards the `mcp_late_register_resolved`
/// one-shot flag (#206 follow-up).
#[tokio::test]
async fn mcp_late_registration_rebuild_happens_at_most_once() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    // The late-registration rebuild (#206) only exists in explicit eager
    // mode; every other mode keeps the tool list fixed for cache stability.
    agent.mcp_tools_mode = crate::config::McpToolsMode::Eager;

    // First turn locks the snapshot with no MCP tools yet.
    let _ = agent.tool_definitions().await;

    // First MCP tool arrives -> one accepted rebuild exposes it.
    agent
        .registry
        .register(
            "mcp__test__first".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__first".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after_first = agent.tool_definitions().await;
    assert!(
        after_first.iter().any(|t| t.name == "mcp__test__first"),
        "first late MCP tool must be picked up by the one accepted rebuild"
    );
    assert!(
        agent.mcp_late_register_resolved,
        "one-shot guard must latch after the accepted rebuild"
    );

    // A SECOND MCP tool registers even later (server connected in a second
    // wave). The one-shot guard means we do NOT rebuild again, so the snapshot
    // stays cache-stable and this tool is intentionally not surfaced until the
    // tool list is explicitly unlocked.
    agent
        .registry
        .register(
            "mcp__test__second".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__second".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after_second = agent.tool_definitions().await;
    let names: Vec<String> = after_second.iter().map(|t| t.name.clone()).collect();
    assert!(
        names.iter().any(|n| n == "mcp__test__first"),
        "previously surfaced MCP tool must remain"
    );
    assert!(
        !names.iter().any(|n| n == "mcp__test__second"),
        "second-wave MCP tool must NOT trigger a second cache-busting rebuild"
    );

    // An explicit unlock (e.g. the `mcp` reload tool) re-arms the one-shot guard
    // and lets the next snapshot pick up everything currently registered.
    agent.unlock_tools();
    assert!(
        !agent.mcp_late_register_resolved,
        "explicit unlock must re-arm the one-shot guard"
    );
    let after_unlock = agent.tool_definitions().await;
    let unlocked_names: Vec<String> = after_unlock.iter().map(|t| t.name.clone()).collect();
    assert!(
        unlocked_names.iter().any(|n| n == "mcp__test__second"),
        "after explicit unlock, the second-wave MCP tool must finally surface"
    );
}

/// Without any newly-registered MCP tools, the locked snapshot must be returned
/// verbatim on every turn (no rebuild, no cache invalidation). Guards the #206
/// fix against re-snapshotting on turns where nothing changed.
#[tokio::test]
async fn tool_snapshot_is_stable_without_new_mcp_tools() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    // The late-registration rebuild (#206) only exists in explicit eager
    // mode; every other mode keeps the tool list fixed for cache stability.
    agent.mcp_tools_mode = crate::config::McpToolsMode::Eager;

    let first = agent.tool_definitions().await;
    // Register a NON-mcp tool after locking — this should NOT trigger a rebuild,
    // because the cache-stability optimization only yields to MCP arrival.
    agent
        .registry
        .register(
            "not_an_mcp_tool".to_string(),
            Arc::new(FakeMcpTool {
                name: "not_an_mcp_tool".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let second = agent.tool_definitions().await;
    let first_names: Vec<String> = first.iter().map(|t| t.name.clone()).collect();
    let second_names: Vec<String> = second.iter().map(|t| t.name.clone()).collect();
    assert_eq!(
        first_names, second_names,
        "non-MCP registry changes must not invalidate the locked tool snapshot"
    );
    assert!(
        !second_names.iter().any(|n| n == "not_an_mcp_tool"),
        "non-MCP tool registered after lock must not leak into the snapshot"
    );
}
