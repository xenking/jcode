pub(crate) const BACKGROUND_WAIT_GUIDANCE: &str = "While this task runs, try to make progress on other useful work that does not depend on its result. If nothing independent remains, wait for completion rather than repeatedly polling. Do not duplicate the running task or change inputs it is still using.";

#[path = "devtools/advisor.rs"]
mod advisor;
#[path = "search/agentgrep.rs"]
mod agentgrep;
#[path = "assist/ambient.rs"]
pub mod ambient;
#[path = "fs/apply_patch.rs"]
mod apply_patch;
#[path = "ui/ask.rs"]
mod ask;
#[path = "fs/line_anchor.rs"]
mod line_anchor;
pub(crate) use ask::{ask_client, ask_for_real_answer};
#[path = "bash/bash.rs"]
mod bash;
pub(crate) use bash::{BASH_HANDOFF, BashHandoff};
#[cfg(unix)]
pub(crate) use bash::{BASH_SUPERVISED, follow_detached_task, restore_detached_task_monitoring};
// The bash tool is the only place a turn cancel has to kill a process group,
// so its behaviour is exercised from the agent-side cancel tests too.
#[cfg(test)]
pub(crate) use bash::BashTool as TestBashTool;
#[path = "compose/batch.rs"]
mod batch;
#[path = "jobs/bg.rs"]
mod bg;
#[path = "jobs/rollout_wait.rs"]
pub(crate) mod rollout_wait;
pub(crate) use bg::replay_wait_input as replay_bg_wait_input;
#[cfg(unix)]
#[path = "jobs/bridge_reload.rs"]
pub(crate) mod bridge_reload;
#[path = "browser/browser.rs"]
mod browser;
#[path = "web/calendar.rs"]
mod calendar;
#[path = "compose/codemode.rs"]
mod codemode;
#[path = "compose/codemode_ledger.rs"]
pub(crate) mod codemode_ledger;
#[path = "swarm/communicate.rs"]
mod communicate;
pub(crate) use communicate::swarm_instruction_tokens;
#[cfg(target_os = "macos")]
#[path = "ui/computer/mod.rs"]
mod computer;
#[path = "devtools/config_edit_notice.rs"]
mod config_edit_notice;
#[path = "search/conversation_search.rs"]
mod conversation_search;
#[path = "devtools/debug_socket.rs"]
mod debug_socket;
#[path = "ui/desktop.rs"]
mod desktop;
#[path = "devtools/desktop_selfdev.rs"]
mod desktop_selfdev;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use desktop_selfdev::peer_process;
#[path = "fs/edit.rs"]
mod edit;
#[path = "fs/edit_stats.rs"]
mod edit_stats;
#[path = "devtools/feedback.rs"]
mod feedback;
#[path = "fs/file_diff.rs"]
mod file_diff;
#[path = "fs/file_lock.rs"]
pub(crate) mod file_lock;
#[path = "web/gmail.rs"]
mod gmail;
// The initiative tool is intentionally unregistered (4928a1c92) but kept for re-enable.
#[path = "ui/applet.rs"]
pub mod applet;
#[path = "ui/approval_request.rs"]
mod approval_request;
#[allow(dead_code)]
#[path = "todo/goal.rs"]
mod goal;
#[path = "jobs/inflight.rs"]
pub mod inflight;
#[path = "ui/interactive.rs"]
pub(crate) mod interactive;
#[path = "devtools/jcode_docs.rs"]
mod jcode_docs;
#[path = "registry/load_tools.rs"]
pub(crate) mod load_tools;
pub use jcode_docs::{BundledDocs, bundled_docs, register_bundled_docs};
#[cfg(unix)]
#[path = "jobs/background_worker.rs"]
pub mod background_worker;
#[cfg(unix)]
#[path = "jobs/detached_jobs.rs"]
pub mod detached_jobs;
#[path = "mcp/mcp.rs"]
pub mod mcp;
#[path = "ui/mcp_approval.rs"]
mod mcp_approval;
#[path = "assist/memory.rs"]
mod memory;
#[path = "assist/name_suggestions.rs"]
mod name_suggestions;
#[path = "fs/open.rs"]
mod open;
#[path = "ui/panel.rs"]
mod panel;
#[path = "fs/patch.rs"]
mod patch;
#[path = "fs/read.rs"]
mod read;
#[cfg(test)]
pub(crate) use read::set_test_read_gate;
#[path = "jobs/remote_exec.rs"]
pub mod remote_exec;
#[path = "mcp/sdk.rs"]
pub(crate) mod sdk;
#[path = "devtools/selfdev/mod.rs"]
pub mod selfdev;
#[path = "registry/serde_coerce.rs"]
pub(crate) mod serde_coerce;
pub(crate) use crate::session_http;
#[path = "search/session_search.rs"]
mod session_search;
#[path = "search/session_search_index.rs"]
pub(crate) mod session_search_index;
#[path = "ui/side_panel.rs"]
mod side_panel;
#[path = "assist/skill.rs"]
mod skill;
#[path = "todo/todo.rs"]
mod todo;
#[path = "ui/user_response.rs"]
mod user_response;
#[path = "web/webfetch.rs"]
mod webfetch;
#[path = "web/websearch.rs"]
mod websearch;
#[path = "registry/withheld_identity.rs"]
mod withheld_identity;
#[path = "fs/write.rs"]
mod write;

use withheld_identity::withheld_call_key;

use crate::compaction::CompactionManager;
use crate::provider::Provider;
use crate::skill::SkillRegistry;
use anyhow::Result;
use jcode_message_types::ToolDefinition;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "registry/name_policy.rs"]
mod name_policy;
use name_policy::is_fixed_mcp_tool;
pub(crate) use name_policy::{tool_name_is_allowed, tool_name_is_disabled};

#[path = "registry/builtin.rs"]
mod builtin;
#[path = "bash/read_hint.rs"]
mod read_hint;
#[path = "registry/session_policy.rs"]
mod session_policy;
pub(crate) use session_policy::{
    SessionToolPolicyRegistration, register_session_tool_policy, session_allows_tool,
    session_mcp_alias_is_allowed,
};
#[cfg(test)]
pub(crate) use session_policy::{
    clear_session_tool_policy, session_mcp_dispatch_is_allowed,
    session_tool_policy_allows_tool_for_test, set_session_tool_policy,
};
use session_policy::{ensure_mcp_action_allowed, session_tool_policy};

/// No tool call may hold a turn longer than this many seconds. Model-requested
/// waits are capped to it, and work still running at the ceiling moves to the
/// background so the turn continues. Only a human decision (`ask`) is exempt.
pub(crate) const MAX_BLOCKING_TOOL_WAIT_SECS: u64 = 15 * 60;

fn parse_tool_args<T: serde::de::DeserializeOwned>(input: Value, example: &str) -> Result<T> {
    serde_path_to_error::deserialize(input).map_err(|err| {
        if err.path().iter().next().is_some() {
            anyhow::anyhow!(
                "Invalid parameter `{}`. Example: {example}\n{}",
                err.path(),
                err.inner()
            )
        } else {
            anyhow::anyhow!("Invalid arguments: {}. Example: {example}", err.inner())
        }
    })
}

use std::sync::{LazyLock, RwLock as StdRwLock};
use tokio::sync::RwLock;

pub use communicate::working_members_spawned_by;
pub(crate) use jcode_tool_core::intent_schema_property;
pub use jcode_tool_core::{StdinInputRequest, Tool, ToolContext, ToolExecutionMode};
pub use jcode_tool_types::{ToolImage, ToolOutput};
pub(crate) use session_search::spawn_recent_index_warmup;

/// Whether a tool call opted in to receiving an oversized (truncated) result.
///
/// Read straight off the raw input rather than each tool's typed args, so every
/// tool honors the flag without having to declare it. Tools deserialize their
/// own args with `#[serde(default)]` fields and ignore unknown keys, so an extra
/// key here is inert for the tool itself.
///
/// Only a real JSON `true` counts. A string `"true"` is also accepted because
/// models routinely stringify booleans, but anything else (including `1`) is
/// treated as absent: accidentally spending the remaining context window should
/// take an unambiguous yes.
fn accepts_large_output(input: &Value) -> bool {
    // The key is not in any schema (that would cost tokens on every request);
    // the refusal message names it.
    match input.get(jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY) {
        Some(Value::Bool(accepted)) => *accepted,
        Some(Value::String(raw)) => raw.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Leading text of the message that replaces a withheld result.
const OUTPUT_WITHHELD_MARKER: &str = "⚠️ OUTPUT WITHHELD";

/// The call each session most recently had withheld, so repeating it accepts
/// the cost.
///
/// Strict-schema providers cannot send a key the schema does not declare, so
/// the flag alone would leave them with no way to opt in. An immediate,
/// identical repeat is an equally unambiguous yes and needs no schema.
type WithheldCalls = HashMap<String, Vec<(u64, String)>>;
static WITHHELD_CALLS: LazyLock<StdRwLock<WithheldCalls>> =
    LazyLock::new(|| StdRwLock::new(HashMap::new()));

/// Withheld calls remembered per session. A batch can have several sub-calls
/// withheld at once, so one slot is not enough.
const WITHHELD_CALLS_PER_SESSION: usize = 8;

/// True when this call repeats one that was withheld for the session. The
/// record is consumed, so each refusal buys exactly one accepted repeat.
///
/// A record only counts for a *later* invocation than the one that earned it.
/// Sibling sub-calls of one `batch` or `codemode` call share a root invocation
/// and an identical key (`intent` is not identifying), so without this check the
/// second of two identical sub-calls would spend the first one's refusal and be
/// accepted instead of withheld. Which of the two ran second is a race, which
/// made the outcome of a batch depend on sub-call completion order.
fn take_repeated_withheld_call(session_id: &str, key: u64, invocation: &str) -> bool {
    let mut calls = WITHHELD_CALLS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(records) = calls.get_mut(session_id) else {
        return false;
    };
    let Some(position) = records
        .iter()
        .position(|(seen, earned_by)| *seen == key && earned_by != invocation)
    else {
        return false;
    };
    records.remove(position);
    if records.is_empty() {
        calls.remove(session_id);
    }
    true
}

fn remember_withheld_call(session_id: &str, key: u64, invocation: &str) {
    let mut calls = WITHHELD_CALLS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Bound the map so abandoned sessions cannot grow it without limit.
    if calls.len() >= 4096 && !calls.contains_key(session_id) {
        calls.clear();
    }
    let records = calls.entry(session_id.to_string()).or_default();
    records.retain(|(seen, _)| *seen != key);
    if records.len() >= WITHHELD_CALLS_PER_SESSION {
        records.remove(0);
    }
    records.push((key, invocation.to_string()));
}

/// Aliases are surface-dependent, but policy follows original identities for
/// the lifetime of the registry, including aliases retired after a reconnect.
#[derive(Default)]
struct McpPolicyIndex {
    current: HashMap<String, (String, String)>,
    history: HashMap<(String, String), HashSet<String>>,
    /// Configured servers whose first connection has not finished. Scripts
    /// wait briefly for these when they have no tools yet (see
    /// [`Registry::wait_for_cold_mcp_servers`]).
    connecting: HashSet<String>,
    /// Woken whenever a server leaves `connecting`.
    settled: Arc<tokio::sync::Notify>,
}

impl McpPolicyIndex {
    /// A connecting server that has registered no tools yet.
    fn has_cold_server(&self) -> bool {
        self.connecting
            .iter()
            .any(|server| !self.current.values().any(|(owner, _)| owner == server))
    }
}

/// Registry of available tools (Arc-wrapped for sharing)
///
/// Clone creates a fresh CompactionManager so each subagent gets independent
/// message history tracking. Tools and skills are shared via Arc.
pub struct Registry {
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    mcp_policy: Arc<StdRwLock<McpPolicyIndex>>,
    skills: Arc<RwLock<SkillRegistry>>,
    compaction: Arc<RwLock<CompactionManager>>,
}

/// Non-owning handle used by tools stored inside a registry.
///
/// A tool cannot strongly own the registry containing it without creating an
/// Arc cycle. Upgrade this handle only for the duration of a tool call.
#[derive(Clone)]
pub(super) struct WeakRegistry {
    tools: std::sync::Weak<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    mcp_policy: Arc<StdRwLock<McpPolicyIndex>>,
    skills: Arc<RwLock<SkillRegistry>>,
    compaction: Arc<RwLock<CompactionManager>>,
}

impl WeakRegistry {
    pub(super) fn upgrade(&self) -> Option<Registry> {
        Some(Registry {
            tools: self.tools.upgrade()?,
            mcp_policy: Arc::clone(&self.mcp_policy),
            skills: Arc::clone(&self.skills),
            compaction: Arc::clone(&self.compaction),
        })
    }
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
            mcp_policy: Arc::clone(&self.mcp_policy),
            skills: self.skills.clone(),
            // Each clone gets a fresh CompactionManager to prevent parallel
            // subagents from corrupting each other's message history
            compaction: Arc::new(RwLock::new(CompactionManager::new())),
        }
    }
}

#[path = "registry/registry_touched_paths.rs"]
mod registry_touched_paths;

#[path = "registry/registry_lifecycle.rs"]
mod registry_lifecycle;

impl Registry {
    pub(crate) async fn parallel_read_allowed(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext,
    ) -> bool {
        let resolved = Self::resolve_tool_name_for_session(&ctx.session_id, name);
        let hooks = &crate::config::config().hooks;
        if sdk::custom(&ctx.session_id, resolved)
            || remote_exec::routes(resolved)
            || hooks
                .pre_tool_transform
                .as_ref()
                .is_some_and(|v| v.iter().any(|s| !s.trim().is_empty()))
            || ["pre_tool", "post_tool", "post_tool_feedback"]
                .iter()
                .any(|event| crate::hooks::hook_configured(event))
        {
            return false;
        }
        let tools = self.tools.read().await;
        tools.get(resolved).is_some_and(|tool| {
            tool.execution_class(input, ctx) == jcode_tool_core::ExecutionClass::ParallelRead
        })
    }

    /// A view for a *tool* sub-call (`batch`, `codemode`): same tool map, same
    /// session compaction state.
    ///
    /// [`Clone`] deliberately starts a fresh `CompactionManager`, because a
    /// parallel *agent* must not share a parent's message history. A tool
    /// sub-call is not an agent: it reports into the caller's transcript, so the
    /// context guard has to judge it against the caller's budget and usage.
    /// Cloning there made every sub-call see a default budget and a synthetic
    /// usage count, so a batch could refuse or admit outputs on numbers that had
    /// nothing to do with the session.
    fn for_tool_subcall(&self) -> Self {
        Self {
            tools: self.tools.clone(),
            mcp_policy: Arc::clone(&self.mcp_policy),
            skills: Arc::clone(&self.skills),
            compaction: Arc::clone(&self.compaction),
        }
    }

    fn downgrade(&self) -> WeakRegistry {
        WeakRegistry {
            tools: Arc::downgrade(&self.tools),
            mcp_policy: Arc::clone(&self.mcp_policy),
            skills: Arc::clone(&self.skills),
            compaction: Arc::clone(&self.compaction),
        }
    }

    /// Create a lightweight empty registry (no tools, no skill loading).
    /// Used by remote-mode clients that don't execute tools locally.
    pub fn empty() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
            mcp_policy: Arc::default(),
            skills: Arc::new(RwLock::new(SkillRegistry::default())),
            compaction: Arc::new(RwLock::new(CompactionManager::new())),
        }
    }

    /// Get all tool definitions for the API
    pub async fn definitions(
        &self,
        allowed_tools: Option<&HashSet<String>>,
    ) -> Vec<ToolDefinition> {
        let tools = self.tools.read().await;
        let mut defs: Vec<ToolDefinition> = tools
            .iter()
            .filter(|(_, tool)| tool.model_visible())
            .filter(|(name, _)| allowed_tools.is_none_or(|set| self.tool_is_allowed(set, name)))
            .map(|(name, tool)| {
                let mut def = tool.to_definition();
                // Use registry key as the tool name (important for MCP tools where
                // the registry key is "mcp__server__tool" but Tool::name() returns
                // just the raw tool name)
                if def.name != *name {
                    def.name = name.clone();
                }
                def
            })
            .collect();

        // Sort by name for deterministic ordering - critical for prompt cache hits
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    pub async fn tool_names(&self) -> Vec<String> {
        let tools = self.tools.read().await;
        tools.keys().cloned().collect()
    }

    /// Whether `name` is registered, without waiting. `None` while the map is
    /// being written.
    pub(crate) fn try_has_tool(&self, name: &str) -> Option<bool> {
        self.tools
            .try_read()
            .ok()
            .map(|tools| tools.contains_key(Self::resolve_tool_name(name)))
    }

    /// Tool definitions a session may call, honoring its registered allow and
    /// deny policy. Per-server MCP tools are always listed individually and
    /// legacy `mcp_search`/`mcp_call` aliases stay hidden. Scripts may use
    /// unified `mcp` discovery/calls or address server tools directly.
    /// Used by `codemode`.
    pub(crate) async fn definitions_for_session(&self, session_id: &str) -> Vec<ToolDefinition> {
        let policy = session_tool_policy(session_id);
        let allowed = policy.as_ref().and_then(|p| p.allowed_tools.clone());
        let mut defs = self.definitions(allowed.as_ref()).await;
        if let Some(policy) = policy.as_ref() {
            defs.retain(|def| !self.tool_is_disabled(&policy.disabled_tools, &def.name));
        }
        defs.retain(|def| !is_fixed_mcp_tool(&def.name));
        defs
    }

    /// Servers still making their first connection, so a script that runs
    /// during startup can wait for tools it would otherwise not see.
    pub(crate) fn mark_mcp_connecting(&self, servers: impl IntoIterator<Item = String>) {
        self.mcp_policy
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .connecting
            .extend(servers);
    }

    pub(crate) fn mark_mcp_settled(&self, server: &str) {
        let mut index = self.mcp_policy.write().unwrap_or_else(|e| e.into_inner());
        index.connecting.remove(server);
        index.settled.notify_waiters();
    }

    /// Wait up to `max` for servers that are still connecting and have no
    /// tools registered yet (a server that was never listed has nothing cached
    /// to advertise). Returns at once when there is nothing to wait for, so a
    /// warm session pays nothing.
    pub(crate) async fn wait_for_cold_mcp_servers(&self, max: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let (notify, cold) = {
                let index = self.mcp_policy.read().unwrap_or_else(|e| e.into_inner());
                (Arc::clone(&index.settled), index.has_cold_server())
            };
            if !cold {
                return;
            }
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // A server may have settled between the check and `enable`.
            let still_cold = self
                .mcp_policy
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .has_cold_server();
            if !still_cold || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Whether this call will stop and ask the user. Such a call must stay in
    /// the foreground: a person's decision is not work to move to the
    /// background after a few seconds.
    pub(crate) async fn awaits_human(&self, name: &str, input: &Value, session_id: &str) -> bool {
        let resolved = Self::resolve_tool_name_for_session(session_id, name);
        if sdk::custom(session_id, resolved) {
            return false;
        }
        let tool = self.tools.read().await.get(resolved).cloned();
        match tool {
            Some(tool) => tool.mcp_approval(input).await.is_some(),
            None => false,
        }
    }

    /// Whether a `codemode` or `batch` call may ask the user from inside,
    /// because some registered MCP tool is gated. Its approval prompt belongs
    /// to the live turn, which ends the moment the call is moved to the
    /// background, so the call keeps its foreground until the blocking ceiling.
    pub(crate) async fn may_await_human_inside(&self, name: &str, session_id: &str) -> bool {
        let resolved = Self::resolve_tool_name_for_session(session_id, name);
        if !matches!(resolved, "codemode" | "batch") {
            return false;
        }
        let gated: Vec<Arc<dyn Tool>> = self
            .tools
            .read()
            .await
            .values()
            .filter(|tool| tool.mcp_identity().is_some())
            .cloned()
            .collect();
        for tool in gated {
            if tool.mcp_approval(&Value::Null).await.is_some() {
                return true;
            }
        }
        false
    }

    /// Declared result schemas of the registered tools that have one, by
    /// registry name. Used by `codemode` to type results.
    pub(crate) async fn output_schemas(&self) -> HashMap<String, Value> {
        self.tools
            .read()
            .await
            .iter()
            .filter_map(|(name, tool)| Some((name.clone(), tool.output_schema()?)))
            .collect()
    }

    /// Resolve tool name aliases.
    ///
    /// When using OAuth, the API presents tools with Claude Code names
    /// (e.g. `file_grep`, `shell_exec`). The model uses those names in
    /// sub-tool calls (e.g. inside `batch`), but our registry uses internal
    /// names (`grep`, `bash`). This mapping ensures both forms resolve
    /// correctly.
    ///
    /// The canonical mapping lives in `jcode-tool-types::resolve_tool_name` so
    /// lower-level crates (e.g. config) can normalize tool names without
    /// depending on the tool subsystem; this method delegates to it.
    pub(crate) fn resolve_tool_name(name: &str) -> &str {
        jcode_tool_types::resolve_tool_name(name)
    }

    /// Estimate token count for a string (chars / 4, matching compaction heuristic)
    fn estimate_tokens(s: &str) -> usize {
        crate::util::estimate_tokens(s)
    }

    /// Maximum fraction of context budget a single tool output may occupy.
    /// Even if we have room, a single output shouldn't dominate the context.
    const SINGLE_OUTPUT_MAX_FRACTION: f32 = 0.30;

    /// Hard ceiling on a single tool output, independent of context budget.
    ///
    /// A fraction alone is not enough. On a model reporting a 1M-token window,
    /// 30% permits a 300k-token single result, so a repo-wide grep sailed
    /// through the guard and cost 233k tokens in one call. No individual tool
    /// result is worth that much of any window: past roughly 50k tokens the
    /// caller is reading a haystack, not an answer, and should narrow the query.
    /// The effective ceiling is the smaller of this and the budget fraction, so
    /// small windows still get proportional protection.
    const SINGLE_OUTPUT_MAX_TOKENS: usize = 50_000;

    /// Message returned instead of an oversized tool result.
    ///
    /// It has one job: make the price legible and the retry obvious. The caller
    /// gets the exact cost, what would survive truncation, the cheap fixes, and
    /// the exact flag to pass if they really want the whole thing.
    fn oversized_output_refusal(
        output_tokens: usize,
        affordable_tokens: usize,
        current_tokens: usize,
        budget: usize,
    ) -> String {
        let percent_of_budget = if budget > 0 {
            (output_tokens as f32 / budget as f32) * 100.0
        } else {
            0.0
        };
        format!(
            "{OUTPUT_WITHHELD_MARKER}: this result is ~{output}k tokens ({percent:.0}% of the \
             {budget}k context budget, of which {used}k is already used), so it was not \
             returned. Nothing was added to the context except this message.\n\n\
             Narrow the request first: add or tighten `path`, `glob`, or `type`, set \
             `max_files`/`max_regions`, use `paths_only` when you only need locations, or \
             read a specific line range. A targeted query is almost always the better \
             answer than a truncated dump.\n\n\
             If you genuinely need this output and accept the token cost, repeat the same \
             call unchanged as your next call (or add `\"accept_large_output\": true`). That \
             returns the first ~{affordable}k tokens and permanently spends them from this \
             session's context.",
            output = output_tokens as f32 / 1000.0,
            percent = percent_of_budget,
            budget = budget / 1000,
            used = current_tokens / 1000,
            affordable = affordable_tokens as f32 / 1000.0,
        )
    }

    /// Resolve a model-supplied tool name for a session: strips a
    /// `functions.` namespace and maps aliases, but leaves SDK custom tool
    /// names untouched so they are never rewritten to a built-in.
    pub(crate) fn resolve_tool_name_for_session<'a>(session_id: &str, name: &'a str) -> &'a str {
        let unqualified_name = name.strip_prefix("functions.").unwrap_or(name);
        if sdk::custom(session_id, unqualified_name) {
            unqualified_name
        } else {
            Self::resolve_tool_name(unqualified_name)
        }
    }

    /// Execute a tool by name
    pub async fn execute(&self, name: &str, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        self.execute_inner(name, input, ctx, false, None).await
    }

    pub(crate) async fn execute_parallel_read(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput> {
        self.execute_inner(name, input, ctx, true, None).await
    }

    async fn execute_codemode_subcall(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
        ledger: &codemode_ledger::CallLedger,
        seq: usize,
    ) -> Result<ToolOutput> {
        self.execute_inner(name, input, ctx, false, Some((ledger, seq)))
            .await
    }

    async fn execute_inner(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
        parallel_read: bool,
        codemode: Option<(&codemode_ledger::CallLedger, usize)>,
    ) -> Result<ToolOutput> {
        let _runtime_activity =
            crate::runtime_activity::global().try_enter_session(&ctx.session_id)?;
        // Mark this call in-flight for the whole execution so the missing
        // tool-output repair paths do not mistake a slow tool for an
        // interrupted one and inject a duplicate synthetic result. See
        // `tool::inflight`.
        let _in_flight = inflight::mark_tool_in_flight(&ctx.tool_call_id);
        let tools = self.tools.read().await;
        let resolved_name = Self::resolve_tool_name_for_session(&ctx.session_id, name);
        let _turn_activity = crate::turn_activity::global().in_tool_call(
            &ctx.session_id,
            resolved_name,
            &ctx.tool_call_id,
        );
        let is_custom = sdk::custom(&ctx.session_id, resolved_name);
        if is_custom && let Some(config) = sdk::config(&ctx.session_id) {
            let disabled = config.disabled.into_iter().collect();
            anyhow::ensure!(
                !self.tool_is_disabled(&disabled, resolved_name),
                "Tool '{}' is disabled",
                resolved_name
            );
        }
        // Enforce product separation here too: batch/subcalls dispatch through
        // the registry without going through Agent::validate_tool_allowed.
        if !is_custom
            && matches!(
                resolved_name,
                "selfdev" | "debug_socket" | "desktop_selfdev" | "jcode_docs"
            )
        {
            let desktop = ctx
                .working_dir
                .as_deref()
                .and_then(jcode_selfdev_types::desktop_repo_root)
                .is_some();
            if desktop && resolved_name == "jcode_docs" {
                anyhow::bail!(
                    "Tool 'jcode_docs' is disabled in Desktop self-development mode. Read the working tree documentation instead."
                );
            }
            if desktop && matches!(resolved_name, "selfdev" | "debug_socket") {
                anyhow::bail!(
                    "Tool '{}' targets Jcode CLI, not Desktop. Use 'desktop_selfdev'.",
                    resolved_name
                );
            }
            if !desktop && resolved_name == "desktop_selfdev" {
                anyhow::bail!("Tool 'desktop_selfdev' requires a Jcode Desktop source checkout.");
            }
        }
        if !is_custom && let Some(policy) = session_tool_policy(&ctx.session_id) {
            if let Some(allowed) = policy.allowed_tools.as_ref()
                && !self.tool_is_allowed(allowed, resolved_name)
            {
                return Err(anyhow::anyhow!("Tool '{}' is not allowed", resolved_name));
            }
            if self.tool_is_disabled(&policy.disabled_tools, resolved_name) {
                return Err(anyhow::anyhow!("Tool '{}' is disabled", resolved_name));
            }
        }
        let tool: Arc<dyn Tool> = if is_custom {
            Arc::new(sdk::CallbackTool(resolved_name.into()))
        } else {
            match tools.get(resolved_name) {
                Some(tool) => tool.clone(),
                None => {
                    // List available tools so the model can recover instead of
                    // spiraling through hallucinated names like "ToolSearch" (#104).
                    let mut available: Vec<&str> = tools.keys().map(|k| k.as_str()).collect();
                    available.sort_unstable();
                    let suggestions = Self::closest_tool_names(name, &available);
                    let mut msg = format!("Unknown tool: {name}.");
                    if !suggestions.is_empty() {
                        msg.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
                    }
                    msg.push_str(&format!(" Available tools: {}.", available.join(", ")));
                    return Err(anyhow::anyhow!(msg));
                }
            }
        };

        // Drop the lock before executing
        drop(tools);

        if parallel_read
            && (is_custom
                || tool.execution_class(&input, &ctx)
                    != jcode_tool_core::ExecutionClass::ParallelRead
                || !self.parallel_read_allowed(name, &input, &ctx).await)
        {
            anyhow::bail!("Parallel read eligibility changed before execution");
        }

        let working_dir = ctx
            .working_dir
            .as_ref()
            .map(|dir| dir.display().to_string());
        let input = crate::hooks::transform_tool_input(
            &ctx.session_id,
            working_dir.as_deref(),
            resolved_name,
            input,
        )
        .await;

        // User-configured pre_tool gate: external policy hook that can block
        // this call (exit 2 or JSON deny), rewrite its input, or attach
        // context for the model. Skipped entirely when not configured.
        let mut input = input;
        let mut gate_context: Vec<String> = Vec::new();
        if crate::hooks::hook_configured("pre_tool") {
            let input_json = input.to_string();
            let outcome = crate::hooks::run_pre_tool_gate(
                &ctx.session_id,
                working_dir.as_deref(),
                resolved_name,
                &input_json,
            )
            .await;
            gate_context = outcome.additional_context;
            if let Some(updated) = outcome.updated_input {
                input = updated;
            }
            if let crate::hooks::GateDecision::Block { mut reason } = outcome.decision {
                for context in &gate_context {
                    reason.push_str("\n\n[pre_tool context]\n");
                    reason.push_str(context);
                }
                let mut fields =
                    Self::tool_lifecycle_fields("blocked", name, resolved_name, &input, &ctx);
                fields.push(("block_reason".to_string(), reason.clone()));
                crate::logging::event_warn("TOOL_LIFECYCLE", fields);
                return Err(anyhow::anyhow!(
                    "Tool call blocked by pre_tool hook: {reason}"
                ));
            }
        }

        let tool_trace = crate::logging::LogLevel::Debug.is_enabled();
        if tool_trace {
            crate::logging::event_debug(
                "TOOL_LIFECYCLE",
                Self::tool_lifecycle_fields("start", name, resolved_name, &input, &ctx),
            );
        }

        // Human approval for tools whose server opted in. After every hook so
        // the user sees the final arguments, and before dispatch so a refusal
        // never half-runs. Nested `batch`/`codemode` calls land here too.
        if !is_custom
            && let Some(request) = tool.mcp_approval(&input).await
            && let Err(error) =
                mcp_approval::require(&ctx, &request.server, &request.tool, &request.arguments)
                    .await
        {
            let mut fields =
                Self::tool_lifecycle_fields("blocked", name, resolved_name, &input, &ctx);
            fields.push(("block_reason".to_string(), format!("{error:#}")));
            crate::logging::event_warn("TOOL_LIFECYCLE", fields);
            // A refusal at the human gate is the outcome of this call, so a
            // codemode run lists it (as failed) next to the calls that ran.
            if let Some((ledger, seq)) = codemode {
                let refused: Result<ToolOutput> = Err(error);
                let now = std::time::Instant::now();
                ledger.dispatched(seq, resolved_name, &input, &ctx.session_id, now, !is_custom);
                ledger.finished(seq, 0, &input, &refused);
                return refused;
            }
            return Err(error);
        }

        let started_at = std::time::Instant::now();
        if let Some((ledger, seq)) = codemode {
            ledger.dispatched(
                seq,
                resolved_name,
                &input,
                &ctx.session_id,
                started_at,
                !is_custom,
            );
        }
        // Hooks, policy, telemetry and the context guard stay in the runtime.
        // Only the workspace side effect moves to the execution environment.
        let result = if !is_custom && remote_exec::routes(resolved_name) {
            remote_exec::execute(resolved_name, input.clone(), &ctx).await
        } else {
            tool.execute(input.clone(), ctx.clone()).await
        };
        let latency_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        if let Some((ledger, seq)) = codemode {
            ledger.finished(seq, latency_ms, &input, &result);
        }

        crate::telemetry::record_tool_execution(resolved_name, &input, result.is_ok(), latency_ms);
        Self::fire_post_tool_hook(resolved_name, &input, &ctx, &result, latency_ms);

        let mut output = match result {
            Ok(output) => output,
            Err(error) => {
                let mut fields =
                    Self::tool_lifecycle_fields("error", name, resolved_name, &input, &ctx);
                fields.push(("elapsed_ms".to_string(), latency_ms.to_string()));
                fields.push(("error".to_string(), crate::util::format_error_chain(&error)));
                crate::logging::event_warn("TOOL_LIFECYCLE", fields);
                if gate_context.is_empty() {
                    return Err(error);
                }
                return Err(anyhow::anyhow!(
                    "{error:#}\n\n[pre_tool context]\n{}",
                    gate_context.join("\n")
                ));
            }
        };

        if !gate_context.is_empty() {
            codemode_ledger::clear_report_boundary(&mut output.metadata);
        }
        for context in &gate_context {
            if !output.output.is_empty() && !output.output.ends_with('\n') {
                output.output.push('\n');
            }
            output.output.push_str("\n[pre_tool context]\n");
            output.output.push_str(context);
        }
        Self::apply_post_tool_feedback(resolved_name, &input, &ctx, latency_ms, &mut output).await;

        // Context overflow guard: check if this output would push us over the limit
        // Opt in with the flag, or by repeating a call that was just withheld.
        let call_key = withheld_call_key(resolved_name, &input);
        let invocation = ctx.root_invocation_id();
        let accepted = accepts_large_output(&input)
            | take_repeated_withheld_call(&ctx.session_id, call_key, invocation);
        let already_marked = output.output.starts_with(OUTPUT_WITHHELD_MARKER);
        output = self.guard_context_overflow(name, output, accepted).await;
        if !already_marked && output.output.starts_with(OUTPUT_WITHHELD_MARKER) {
            remember_withheld_call(&ctx.session_id, call_key, invocation);
        }

        crate::logging::event_info(
            "TOOL_LIFECYCLE",
            [
                ("phase", "done".to_owned()),
                ("tool", resolved_name.to_owned()),
                ("session_id", ctx.session_id.clone()),
                ("elapsed_ms", latency_ms.to_string()),
                ("output_bytes", output.output.len().to_string()),
                ("image_count", output.images.len().to_string()),
            ],
        );
        if tool_trace {
            let mut fields =
                Self::tool_lifecycle_fields("done_detail", name, resolved_name, &input, &ctx);
            fields.push((
                "output_chars".to_string(),
                output.output.chars().count().to_string(),
            ));
            crate::logging::event_debug("TOOL_LIFECYCLE", fields);
        }

        Ok(output)
    }

    /// Maximum fraction of context budget a single tool output may consume.
    /// Outputs that would push total context beyond this are truncated.
    const CONTEXT_GUARD_THRESHOLD: f32 = 0.90;

    /// Check if a tool output would overflow the context window and truncate if needed.
    /// Returns the (possibly truncated) output.
    ///
    /// An oversized result is **refused** rather than truncated. Truncating by
    /// default was the worse failure: the caller still paid the full remaining
    /// context for a prefix that usually did not contain the answer, and the
    /// damage was already done by the time they read the warning. Refusing costs
    /// a few dozen tokens, states the price, and lets the caller either narrow
    /// the query or knowingly pay by passing `accept_large_output`.
    async fn guard_context_overflow(
        &self,
        tool_name: &str,
        mut output: ToolOutput,
        accept_large_output: bool,
    ) -> ToolOutput {
        let compaction = self.compaction.read().await;
        let budget = compaction.token_budget();
        if budget == 0 {
            return output;
        }

        let current_tokens = compaction.effective_token_count();
        let output_tokens = Self::estimate_tokens(&output.output);

        // Check 1: Would adding this output push us over the safety threshold?
        let projected = current_tokens + output_tokens;
        let threshold_tokens = (budget as f32 * Self::CONTEXT_GUARD_THRESHOLD) as usize;

        // Check 2: Is this single output unreasonably large? Proportional to the
        // budget, but also absolutely capped, because 30% of a 1M-token window is
        // 300k tokens and no single tool result is worth that.
        let single_max_tokens = ((budget as f32 * Self::SINGLE_OUTPUT_MAX_FRACTION) as usize)
            .min(Self::SINGLE_OUTPUT_MAX_TOKENS);

        let needs_truncation = projected > threshold_tokens || output_tokens > single_max_tokens;

        if !needs_truncation {
            return output;
        }

        // Past the safety threshold there is no room left to spend, so opting in
        // cannot buy anything: returning a slice would push the conversation over
        // the window instead of merely being expensive. Refuse outright, and say
        // how to make room. This is checked before computing an affordable size
        // so the outcome does not depend on budget arithmetic happening to land
        // under a character floor.
        if current_tokens >= threshold_tokens {
            codemode_ledger::clear_report_boundary(&mut output.metadata);
            crate::logging::info(&format!(
                "Context guard: refused {} output of ~{}k tokens, context exhausted \
                 ({}k/{}k)",
                tool_name,
                output_tokens / 1000,
                current_tokens / 1000,
                budget / 1000,
            ));
            return ToolOutput {
                output: format!(
                    "⚠️ CONTEXT LIMIT REACHED: cannot return this tool output (~{:.0}k tokens) \
                     because the context window is nearly full ({:.0}k/{}k tokens). \
                     accept_large_output does not apply here: there is no room left to spend. \
                     Use /compact to free space, then retry with a narrower query.",
                    output_tokens as f32 / 1000.0,
                    current_tokens as f32 / 1000.0,
                    budget / 1000,
                ),
                title: output.title,
                metadata: output.metadata,
                images: output.images,
                structured: None,
            };
        }

        // How much of this output the remaining context could absorb.
        let max_tokens = (threshold_tokens - current_tokens).min(single_max_tokens);

        // Convert token limit back to approximate character limit
        let max_chars = max_tokens * 4;

        if output.output.len() <= max_chars {
            return output;
        }

        codemode_ledger::clear_report_boundary(&mut output.metadata);

        if !accept_large_output {
            crate::logging::info(&format!(
                "Context guard: refused {} output of ~{}k tokens \
                 (context: {}k/{}k, {:.0}% used); caller may retry with accept_large_output",
                tool_name,
                output_tokens / 1000,
                current_tokens / 1000,
                budget / 1000,
                (current_tokens as f32 / budget as f32) * 100.0,
            ));
            return ToolOutput {
                output: Self::oversized_output_refusal(
                    output_tokens,
                    max_tokens,
                    current_tokens,
                    budget,
                ),
                title: output.title,
                metadata: output.metadata,
                images: output.images,
                structured: None,
            };
        }

        crate::logging::info(&format!(
            "Context guard: truncating {} output from ~{}k to ~{}k tokens \
             (context: {}k/{}k, {:.0}% used)",
            tool_name,
            output_tokens / 1000,
            max_tokens / 1000,
            current_tokens / 1000,
            budget / 1000,
            (current_tokens as f32 / budget as f32) * 100.0,
        ));

        // Truncate the output, keeping the beginning (usually most relevant)
        // Keep the beginning, which is usually the most relevant part, and leave
        // headroom for the notice itself. `max_chars` is at least 800 here: the
        // exhaustion check above already returned for anything tighter.
        let kept = &output.output[..output
            .output
            .floor_char_boundary(max_chars.saturating_sub(150))];
        let truncated = format!(
            "{}\n\n⚠️ OUTPUT TRUNCATED: you passed accept_large_output, so this ~{:.0}k \
             token result was returned truncated instead of withheld \
             ({:.0}k/{}k tokens were already used). Only the first ~{:.0}k tokens are \
             above, and they are now spent from this session's context. If the answer \
             is not in them, narrow the query rather than repeating this call.",
            kept,
            output_tokens as f32 / 1000.0,
            current_tokens as f32 / 1000.0,
            budget / 1000,
            max_tokens as f32 / 1000.0,
        );

        ToolOutput {
            output: truncated,
            title: output.title,
            metadata: output.metadata,
            images: output.images,
            structured: None,
        }
    }

    /// Register a tool dynamically (for MCP tools, etc.)
    pub async fn register(&self, name: String, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().await;
        let is_mcp = tool.mcp_identity().is_some();
        tools.insert(name, tool);
        if is_mcp {
            self.record_mcp_surface(&tools);
        }
    }

    fn record_mcp_surface(&self, tools: &HashMap<String, Arc<dyn Tool>>) {
        let mut index = self.mcp_policy.write().unwrap_or_else(|e| e.into_inner());
        index.current.clear();
        for (alias, tool) in tools {
            if let Some((server, raw)) = tool.mcp_identity() {
                let identity = (server.to_string(), raw.to_string());
                index.current.insert(alias.clone(), identity.clone());
                let names = index.history.entry(identity).or_default();
                names.insert(alias.clone());
                names.insert(crate::mcp::dispatch_name(server, raw));
            }
        }
    }

    fn mcp_alias(&self, server: &str, raw: &str) -> Option<String> {
        self.mcp_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .iter()
            .find(|(_, identity)| identity.0 == server && identity.1 == raw)
            .map(|(alias, _)| alias.clone())
    }

    fn mcp_policy_names(&self, server: &str, raw: &str) -> HashSet<String> {
        let index = self.mcp_policy.read().unwrap_or_else(|e| e.into_inner());
        let mut names = index
            .history
            .get(&(server.into(), raw.into()))
            .cloned()
            .unwrap_or_default();
        names.insert(crate::mcp::dispatch_name(server, raw));
        names
    }

    fn policy_names(&self, name: &str) -> HashSet<String> {
        let index = self.mcp_policy.read().unwrap_or_else(|e| e.into_inner());
        let mut names = index
            .current
            .get(name)
            .and_then(|identity| index.history.get(identity))
            .cloned()
            .unwrap_or_default();
        names.insert(name.to_string());
        names
    }

    pub(crate) fn tool_is_allowed(&self, allowed: &HashSet<String>, name: &str) -> bool {
        self.policy_names(name)
            .iter()
            .any(|name| tool_name_is_allowed(allowed, name))
    }

    pub(crate) fn tool_is_disabled(&self, disabled: &HashSet<String>, name: &str) -> bool {
        self.policy_names(name)
            .iter()
            .any(|name| tool_name_is_disabled(disabled, name))
    }

    /// Original `(server, tool)` for a registered MCP alias. Aliases are
    /// sanitized for providers, so they cannot be split back reliably.
    pub(crate) fn mcp_identity_for_alias(&self, alias: &str) -> Option<(String, String)> {
        self.mcp_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .get(alias)
            .cloned()
    }

    /// Whether this exact configured server has registered tools. Dispatch
    /// aliases are normalized and may be collision-suffixed, so callers must
    /// not infer server membership by matching a name prefix.
    pub fn has_mcp_tools_for_server(&self, server: &str) -> bool {
        self.mcp_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .values()
            .any(|(owner, _)| owner == server)
    }

    fn mcp_dispatch_is_allowed(
        &self,
        session: &str,
        server: &str,
        raw: &str,
        alias: &str,
        surface: &str,
    ) -> bool {
        let Some(policy) = session_tool_policy(session) else {
            return true;
        };
        let mut names = self.mcp_policy_names(server, raw);
        names.insert(alias.to_string());
        policy.allowed_tools.as_ref().is_none_or(|allowed| {
            allowed.contains(surface)
                || names.iter().any(|name| tool_name_is_allowed(allowed, name))
        }) && !names
            .iter()
            .any(|name| tool_name_is_disabled(&policy.disabled_tools, name))
    }

    /// True when every fresh MCP tool already exists with an identical
    /// description and input schema, so replacing it would change nothing.
    fn mcp_tools_match(
        current: &HashMap<String, Arc<dyn Tool>>,
        fresh: &[(String, Arc<dyn Tool>)],
    ) -> bool {
        let by_identity: HashMap<(&str, &str), &Arc<dyn Tool>> = current
            .values()
            .filter_map(|tool| tool.mcp_identity().map(|id| (id, tool)))
            .collect();
        fresh.iter().all(|(_, tool)| {
            tool.mcp_identity()
                .and_then(|id| by_identity.get(&id))
                .is_some_and(|existing| {
                    existing.description() == tool.description()
                        && existing.parameters_schema() == tool.parameters_schema()
                        && existing.output_schema() == tool.output_schema()
                        && existing.mcp_annotations() == tool.mcp_annotations()
                })
        })
    }

    /// Refresh connected servers, retaining cached proxies for offline servers.
    /// Recompute aliases over the combined surface, not only the live subset.
    pub async fn refresh_mcp_tools(
        &self,
        fresh: Vec<(String, Arc<dyn Tool>)>,
        refreshed_servers: &[String],
    ) {
        self.refresh_mcp_tools_inner(fresh, refreshed_servers, false)
            .await;
    }

    /// Like [`Self::refresh_mcp_tools`], for a single newly connected server.
    /// The cached proxies advertised at spawn usually match the live catalog
    /// exactly, and rebuilding every alias across ~400 tools for each of ~13
    /// connections per session was most of a reload storm's CPU. When the
    /// server's tools are already registered with identical identities and
    /// definitions, nothing about the combined surface (and so no alias) can
    /// change, so skip the rebuild. Removals still go through the full path.
    pub async fn refresh_connected_mcp_server(
        &self,
        fresh: Vec<(String, Arc<dyn Tool>)>,
        server: &str,
    ) {
        self.refresh_mcp_tools_inner(fresh, std::slice::from_ref(&server.to_string()), true)
            .await;
    }

    async fn refresh_mcp_tools_inner(
        &self,
        fresh: Vec<(String, Arc<dyn Tool>)>,
        refreshed_servers: &[String],
        skip_if_unchanged: bool,
    ) {
        let mut tools = self.tools.write().await;
        if skip_if_unchanged {
            let mut existing: Vec<(&str, &str)> = tools
                .values()
                .filter_map(|tool| tool.mcp_identity())
                .filter(|(server, _)| refreshed_servers.iter().any(|s| s == server))
                .collect();
            let mut incoming: Vec<(&str, &str)> = fresh
                .iter()
                .filter_map(|(_, tool)| tool.mcp_identity())
                .collect();
            existing.sort_unstable();
            incoming.sort_unstable();
            if existing == incoming && Self::mcp_tools_match(&tools, &fresh) {
                return;
            }
        }
        let mut proxies: Vec<Arc<dyn Tool>> = tools
            .values()
            .filter(|tool| {
                tool.mcp_identity()
                    .is_some_and(|(server, _)| !refreshed_servers.iter().any(|s| s == server))
            })
            .cloned()
            .collect();
        proxies.extend(fresh.into_iter().map(|(_, tool)| tool));
        let defs: Vec<_> = proxies
            .iter()
            .map(|tool| {
                let (server, raw) = tool.mcp_identity().expect("MCP proxy");
                (
                    server.to_string(),
                    crate::mcp::McpToolDef {
                        name: raw.to_string(),
                        description: None,
                        input_schema: Value::Null,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let names = crate::mcp::dispatch_names(&defs);
        tools.retain(|_, tool| tool.mcp_identity().is_none());
        tools.extend(names.into_iter().zip(proxies));
        self.record_mcp_surface(&tools);
    }

    /// Atomically replace the MCP surface. Aliases depend on the complete set,
    /// so inserting only new keys leaves obsolete cached aliases callable.
    pub async fn reconcile_mcp_tools(&self, fresh: Vec<(String, Arc<dyn Tool>)>) {
        let mut tools = self.tools.write().await;
        tools.retain(|_, tool| tool.mcp_identity().is_none());
        tools.extend(fresh);
        self.record_mcp_surface(&tools);
    }

    /// Remove exact server membership, never a lossy normalized prefix.
    pub async fn unregister_mcp_server(&self, server: &str) -> Vec<String> {
        let mut tools = self.tools.write().await;
        let mut removed = Vec::new();
        tools.retain(|name, tool| {
            let keep = tool.mcp_identity().is_none_or(|(owner, _)| owner != server);
            if !keep {
                removed.push(name.clone());
            }
            keep
        });
        self.record_mcp_surface(&tools);
        removed
    }

    /// Register MCP tools (MCP management and server tools)
    /// Connections happen in background to avoid blocking startup.
    /// If `event_tx` is provided, sends an McpStatus event when connections complete.
    /// If `shared_pool` is provided, shared servers reuse processes from the pool.
    pub async fn register_mcp_tools(
        &self,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::protocol::ServerEvent>>,
        shared_pool: Option<std::sync::Arc<crate::mcp::SharedMcpPool>>,
        session_id: Option<String>,
    ) {
        self.register_mcp_tools_for_dir(event_tx, shared_pool, session_id, None)
            .await
    }

    /// Unregister a tool
    pub async fn unregister(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let mut tools = self.tools.write().await;
        let removed = tools.remove(name);
        self.record_mcp_surface(&tools);
        removed
    }

    /// Unregister all tools matching a prefix
    pub async fn unregister_prefix(&self, prefix: &str) -> Vec<String> {
        let mut tools = self.tools.write().await;
        let to_remove: Vec<String> = tools
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for name in &to_remove {
            tools.remove(name);
        }
        self.record_mcp_surface(&tools);
        to_remove
    }

    /// Get shared access to the skill registry
    pub fn skills(&self) -> Arc<RwLock<SkillRegistry>> {
        self.skills.clone()
    }

    /// Get shared access to the compaction manager
    pub fn compaction(&self) -> Arc<RwLock<CompactionManager>> {
        self.compaction.clone()
    }
}

#[cfg(test)]
#[path = "registry/registry_mcp_allow_list_tests.rs"]
mod mcp_allow_list_tests;

#[cfg(test)]
#[path = "registry/tests.rs"]
mod tests;
