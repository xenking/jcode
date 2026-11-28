//! The registry as the tools that compose over it see it.
//!
//! `batch`, `codemode` and the MCP tools are stored inside a registry and call
//! back into it. They hold a [`WeakHost`] and reach the registry only through
//! [`ToolHost`], so the registry builds them without them knowing its type.
//!
//! Layer order, each knowing only those below it:
//! `ToolHost` < composing tools (`batch`, `codemode`, `mcp`) < `Registry`.

use super::codemode_ledger::CallLedger;
use super::{Registry, Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_message_types::ToolDefinition;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub(super) trait ToolHost: Send + Sync {
    async fn execute(&self, name: &str, input: Value, ctx: ToolContext) -> Result<ToolOutput>;

    async fn execute_codemode_subcall(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
        ledger: &CallLedger,
        seq: usize,
    ) -> Result<ToolOutput>;

    /// A view for a tool sub-call that reports into the caller's transcript.
    fn for_tool_subcall(&self) -> Arc<dyn ToolHost>;

    /// The input schema of the registered tool `route`, if there is one.
    async fn parameters_schema(&self, route: &str) -> Option<Value>;

    async fn output_schemas(&self) -> HashMap<String, Value>;

    async fn definitions_for_session(&self, session_id: &str) -> Vec<ToolDefinition>;

    async fn wait_for_cold_mcp_servers(&self, max: std::time::Duration);

    fn mcp_alias(&self, server: &str, raw: &str) -> Option<String>;

    fn mcp_dispatch_is_allowed(
        &self,
        session: &str,
        server: &str,
        raw: &str,
        alias: &str,
        surface: &str,
    ) -> bool;

    async fn refresh_mcp_tools(
        &self,
        fresh: Vec<(String, Arc<dyn Tool>)>,
        refreshed_servers: &[String],
    );

    async fn reconcile_mcp_tools(&self, fresh: Vec<(String, Arc<dyn Tool>)>);

    async fn unregister_mcp_server(&self, server: &str) -> Vec<String>;

    async fn unregister_prefix(&self, prefix: &str) -> Vec<String>;
}

/// Non-owning handle used by tools stored inside a registry.
///
/// A tool cannot strongly own the registry containing it without creating an
/// Arc cycle. Upgrade this handle only for the duration of a tool call.
#[derive(Clone)]
pub struct WeakHost {
    upgrade: Arc<dyn Fn() -> Option<Arc<dyn ToolHost>> + Send + Sync>,
}

impl WeakHost {
    pub(super) fn new(
        upgrade: impl Fn() -> Option<Arc<dyn ToolHost>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            upgrade: Arc::new(upgrade),
        }
    }

    pub(super) fn upgrade(&self) -> Option<Arc<dyn ToolHost>> {
        (self.upgrade)()
    }
}

#[async_trait]
impl ToolHost for Registry {
    async fn execute(&self, name: &str, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        Registry::execute(self, name, input, ctx).await
    }

    async fn execute_codemode_subcall(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
        ledger: &CallLedger,
        seq: usize,
    ) -> Result<ToolOutput> {
        Registry::execute_codemode_subcall(self, name, input, ctx, ledger, seq).await
    }

    fn for_tool_subcall(&self) -> Arc<dyn ToolHost> {
        Arc::new(Registry::for_tool_subcall(self))
    }

    async fn parameters_schema(&self, route: &str) -> Option<Value> {
        Registry::parameters_schema(self, route).await
    }

    async fn output_schemas(&self) -> HashMap<String, Value> {
        Registry::output_schemas(self).await
    }

    async fn definitions_for_session(&self, session_id: &str) -> Vec<ToolDefinition> {
        Registry::definitions_for_session(self, session_id).await
    }

    async fn wait_for_cold_mcp_servers(&self, max: std::time::Duration) {
        Registry::wait_for_cold_mcp_servers(self, max).await;
    }

    fn mcp_alias(&self, server: &str, raw: &str) -> Option<String> {
        Registry::mcp_alias(self, server, raw)
    }

    fn mcp_dispatch_is_allowed(
        &self,
        session: &str,
        server: &str,
        raw: &str,
        alias: &str,
        surface: &str,
    ) -> bool {
        Registry::mcp_dispatch_is_allowed(self, session, server, raw, alias, surface)
    }

    async fn refresh_mcp_tools(
        &self,
        fresh: Vec<(String, Arc<dyn Tool>)>,
        refreshed_servers: &[String],
    ) {
        Registry::refresh_mcp_tools(self, fresh, refreshed_servers).await;
    }

    async fn reconcile_mcp_tools(&self, fresh: Vec<(String, Arc<dyn Tool>)>) {
        Registry::reconcile_mcp_tools(self, fresh).await;
    }

    async fn unregister_mcp_server(&self, server: &str) -> Vec<String> {
        Registry::unregister_mcp_server(self, server).await
    }

    async fn unregister_prefix(&self, prefix: &str) -> Vec<String> {
        Registry::unregister_prefix(self, prefix).await
    }
}
