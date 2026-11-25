#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::{Tool, ToolContext, ToolOutput};
use crate::comm_format::{
    comm_cleanup_candidate_session_ids, default_comm_await_target_statuses,
    default_comm_cleanup_target_statuses, format_comm_awaited_members_with_reports,
    format_comm_channels, format_comm_context_entries, format_comm_context_history,
    format_comm_members, format_comm_plan_followup, format_comm_plan_status,
    format_comm_status_snapshot, format_comm_tool_summary, latest_assistant_comm_report,
    resolve_optional_comm_target_session,
};
use crate::protocol::{
    AgentInfo, AgentStatusSnapshot, AwaitedMemberStatus, CommDeliveryMode, CommToolCallSnapshot,
    ContextEntry, HistoryMessage, MAX_COMM_SUMMARY_TOOL_CALLS, PlanGraphStatus, Request,
    ServerEvent, SwarmChannelInfo, ToolCallSummary,
};
use anyhow::Result;
use async_trait::async_trait;
use jcode_swarm_core::derive_swarm_task_label;
use serde_json::{Value, json};
use std::collections::HashMap;

const REQUEST_ID: u64 = 1;

mod transport;
use transport::{send_request, send_request_with_timeout};

mod command;
use command::{AssignTask, Cleanup, Command, CommunicateInput, WorkerSpawn};

fn check_error(response: &ServerEvent) -> Option<&str> {
    if let ServerEvent::Error { message, .. } = response {
        Some(message)
    } else {
        None
    }
}

fn ensure_success(response: &ServerEvent) -> Result<()> {
    if let Some(message) = check_error(response) {
        Err(anyhow::anyhow!(message.to_string()))
    } else {
        Ok(())
    }
}

fn queued_cold_output(response: &ServerEvent) -> Option<ToolOutput> {
    match response {
        ServerEvent::RuntimeCommandResult {
            command,
            data,
            message,
            ..
        } if command == "swarm_message" && data["status"] == "queued_cold" => {
            Some(ToolOutput::new(message.clone()))
        }
        _ => None,
    }
}

pub(super) async fn deliver_agent_draft(
    sender: String,
    recipient: String,
    message: String,
) -> Result<Value> {
    let response = send_request(Request::CommMessage {
        id: REQUEST_ID,
        from_session: sender,
        to_session: Some(recipient),
        message,
        delivery: Some(CommDeliveryMode::Wake),
        channel: None,
        wake: None,
        tldr: None,
        to_swarm: None,
        require_target: true,
    })
    .await?;
    ensure_success(&response)?;
    Ok(match response {
        ServerEvent::RuntimeCommandResult { command, data, .. } if command == "swarm_message" => {
            data
        }
        _ => json!({"status":"sent"}),
    })
}

async fn fetch_plan_status(session_id: &str) -> Result<PlanGraphStatus> {
    let request = Request::CommPlanStatus {
        id: REQUEST_ID,
        session_id: session_id.to_string(),
    };
    match send_request(request).await {
        Ok(ServerEvent::CommPlanStatusResponse { summary, .. }) => Ok(summary),
        Ok(response) => {
            ensure_success(&response)?;
            Err(anyhow::anyhow!("No plan status returned."))
        }
        Err(e) => Err(anyhow::anyhow!("Failed to get plan status: {}", e)),
    }
}

fn format_plan_followup(summary: &PlanGraphStatus) -> String {
    format_comm_plan_followup(summary)
}

fn default_cleanup_target_statuses() -> Vec<String> {
    default_comm_cleanup_target_statuses()
}

fn cleanup_candidate_session_ids(
    owner_session_id: &str,
    members: &[AgentInfo],
    target_status: &[String],
    requested_session_ids: &[String],
    force: bool,
) -> Vec<String> {
    comm_cleanup_candidate_session_ids(
        owner_session_id,
        members,
        target_status,
        requested_session_ids,
        force,
    )
}

fn auto_assignment_needs_spawn(response: &ServerEvent) -> bool {
    check_error(response).is_some_and(|message| {
        message.contains(
            "No ready or completed swarm agents are available for automatic task assignment",
        )
    })
}

async fn fetch_swarm_members(session_id: &str) -> Result<Vec<AgentInfo>> {
    let request = Request::CommList {
        id: REQUEST_ID,
        session_id: session_id.to_string(),
    };
    match send_request(request).await {
        Ok(ServerEvent::CommMembers { members, .. }) => Ok(members),
        Ok(response) => {
            ensure_success(&response)?;
            Ok(Vec::new())
        }
        Err(e) => Err(anyhow::anyhow!("Failed to list swarm members: {}", e)),
    }
}

/// Members this session spawned that are still working, per the daemon. Their
/// completion wakes the spawner, so a headless runner must not poke meanwhile.
/// Fails open: with no daemon or no swarm there is nothing to wait for.
pub async fn working_members_spawned_by(session_id: &str) -> Vec<String> {
    let members = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fetch_swarm_members(session_id),
    )
    .await
    {
        Ok(Ok(members)) => members,
        _ => return Vec::new(),
    };
    spawned_working_member_ids(&members, session_id)
}

fn spawned_working_member_ids(members: &[AgentInfo], session_id: &str) -> Vec<String> {
    let mut working: Vec<String> = members
        .iter()
        .filter(|member| {
            member.session_id != session_id
                && member.report_back_to_session_id.as_deref() == Some(session_id)
                && member
                    .status
                    .as_deref()
                    .is_some_and(crate::todo::swarm_member_is_working)
        })
        .map(|member| member.session_id.clone())
        .collect();
    working.sort();
    working
}

#[cfg(test)]
fn swarm_member_is_in_flight(member: &AgentInfo) -> bool {
    matches!(
        member.status.as_deref(),
        Some("queued" | "running" | "running_stale")
    )
}

#[cfg(test)]
fn coordination_in_flight_count(
    summary: &PlanGraphStatus,
    members: &[AgentInfo],
    current_session_id: &str,
) -> usize {
    summary.active_ids.len().max(
        members
            .iter()
            .filter(|member| member.session_id != current_session_id)
            .filter(|member| swarm_member_is_in_flight(member))
            .filter(|member| swarm_member_is_drivable_worker(member, current_session_id))
            .count(),
    )
}

/// Whether `member` is a worker the coordinator can rely on to autonomously execute an
/// assignment (and therefore one it is safe to await): a spawned headless worker,
/// or one owned by the coordinator that issued the run. Foreign client-attached
/// sessions are not drivable and must not count toward its in-flight workers.
#[cfg(test)]
fn swarm_member_is_drivable_worker(member: &AgentInfo, coordinator_session_id: &str) -> bool {
    member.is_headless.unwrap_or(false)
        || member.report_back_to_session_id.as_deref() == Some(coordinator_session_id)
}

async fn cleanup_swarm_workers(ctx: &ToolContext, cleanup: Cleanup) -> Result<String> {
    let members = fetch_swarm_members(&ctx.session_id).await?;
    let Cleanup {
        target_status,
        session_ids,
        force,
    } = cleanup;
    let candidates = cleanup_candidate_session_ids(
        &ctx.session_id,
        &members,
        &target_status,
        &session_ids,
        force,
    );

    if candidates.is_empty() {
        return Ok(format!(
            "No cleanup candidates found. Default cleanup only stops sessions spawned by this coordinator with status in [{}].",
            target_status.join(", ")
        ));
    }

    Ok(stop_swarm_sessions(ctx, candidates, force).await.describe())
}

/// Result of stopping a batch of swarm sessions: which stops succeeded and
/// which failed (with reasons). Split from the human-readable formatting so
/// callers like the mid-run capacity recovery can count freed slots.
struct WorkerCleanupOutcome {
    stopped: Vec<String>,
    failed: Vec<String>,
}

impl WorkerCleanupOutcome {
    fn describe(&self) -> String {
        let mut output = String::new();
        if self.stopped.is_empty() {
            output.push_str("Stopped no swarm workers.");
        } else {
            output.push_str(&format!(
                "Stopped {} swarm worker(s): {}",
                self.stopped.len(),
                self.stopped.join(", ")
            ));
        }
        if !self.failed.is_empty() {
            output.push_str(&format!(
                "\nFailed to stop {} worker(s): {}",
                self.failed.len(),
                self.failed.join(", ")
            ));
        }
        output
    }
}

async fn stop_swarm_sessions(
    ctx: &ToolContext,
    candidates: Vec<String>,
    force: bool,
) -> WorkerCleanupOutcome {
    let mut stopped = Vec::new();
    let mut failed = Vec::new();
    for target in candidates {
        let request = Request::CommStop {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            target_session: target.clone(),
            force: Some(force),
        };
        match send_request(request).await {
            Ok(response) => match ensure_success(&response) {
                Ok(()) => stopped.push(target),
                Err(error) => failed.push(format!("{} ({})", target, error)),
            },
            Err(error) => failed.push(format!("{} ({})", target, error)),
        }
    }
    WorkerCleanupOutcome { stopped, failed }
}

async fn spawn_assignment_session(ctx: &ToolContext, worker: &WorkerSpawn) -> Result<String> {
    let spawn_request = Request::CommSpawn {
        id: REQUEST_ID,
        session_id: ctx.session_id.clone(),
        working_dir: worker.working_dir.clone(),
        initial_message: None,
        request_nonce: Some(ctx.tool_call_id.clone()),
        spawn_mode: worker.spawn_mode.clone(),
        model: worker.model.clone(),
        // Worker effort is not agent-controlled: the server applies
        // `agents.swarm_effort` (default "low").
        effort: None,
        speed: None,
        label: None,
        lane_id: None,
        profile: worker.profile.clone(),
    };

    match send_request(spawn_request).await {
        Ok(ServerEvent::CommSpawnResponse { new_session_id, .. }) if !new_session_id.is_empty() => {
            Ok(new_session_id)
        }
        Ok(spawn_response) => {
            ensure_success(&spawn_response)?;
            Err(anyhow::anyhow!(
                "Spawn succeeded but new session ID was not returned."
            ))
        }
        Err(e) => Err(anyhow::anyhow!(
            "Failed to spawn agent for task assignment: {}",
            e
        )),
    }
}

async fn assign_task_to_session(
    ctx: &ToolContext,
    assignment: &AssignTask,
    target_session: String,
    spawned_suffix: &str,
) -> Result<ToolOutput> {
    let retry_request = Request::CommAssignTask {
        id: REQUEST_ID,
        session_id: ctx.session_id.clone(),
        target_session: Some(target_session.clone()),
        task_id: assignment.task_id.clone(),
        message: assignment.message.clone(),
    };

    match send_request(retry_request).await {
        Ok(ServerEvent::CommAssignTaskResponse { task_id, .. }) => Ok(ToolOutput::new(format!(
            "Task '{}' assigned to {}{}",
            task_id, target_session, spawned_suffix
        ))),
        Ok(retry_response) => {
            ensure_success(&retry_response)?;
            Ok(ToolOutput::new(format!(
                "Assigned next runnable task to {}{}",
                target_session, spawned_suffix
            )))
        }
        Err(e) => Err(anyhow::anyhow!(
            "Failed to assign task after selecting {}: {}",
            target_session,
            e
        )),
    }
}

fn format_context_entries(entries: &[ContextEntry]) -> ToolOutput {
    ToolOutput::new(format_comm_context_entries(entries))
}

fn format_members(ctx: &ToolContext, members: &[AgentInfo]) -> ToolOutput {
    ToolOutput::new(format_comm_members(&ctx.session_id, members))
}

fn format_tool_summary(
    target: &str,
    calls: &[ToolCallSummary],
    snapshot: Option<CommToolCallSnapshot>,
) -> ToolOutput {
    let freshness = match snapshot {
        Some(CommToolCallSnapshot::Available { captured_at_secs }) => {
            let captured_at = i64::try_from(captured_at_secs)
                .ok()
                .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0))
                .map_or_else(
                    || "unknown time".to_string(),
                    |time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                );
            format!(
                "Live runtime snapshot updated at {captured_at}. Includes recorded tool calls, not completion status."
            )
        }
        Some(CommToolCallSnapshot::Unavailable) => {
            return ToolOutput::new(format!(
                "Tool call snapshot unavailable for {target}. No live snapshot was published. Summary does not wake or restore sessions. Use `list` for agent status."
            ));
        }
        None if calls.is_empty() => {
            return ToolOutput::new(format!(
                "No tool call data returned for {target}. This daemon does not distinguish empty from unavailable."
            ));
        }
        None => "Snapshot freshness not reported by this daemon.".to_string(),
    };
    ToolOutput::new(format!(
        "{}\n\n{freshness}",
        format_comm_tool_summary(target, calls).trim_end()
    ))
}

fn format_status_snapshot(snapshot: &AgentStatusSnapshot) -> ToolOutput {
    ToolOutput::new(format_comm_status_snapshot(snapshot))
}

fn format_plan_status(summary: &PlanGraphStatus) -> ToolOutput {
    ToolOutput::new(format_comm_plan_status(summary))
}

fn format_context_history(target: &str, messages: &[HistoryMessage]) -> ToolOutput {
    ToolOutput::new(format_comm_context_history(target, messages))
}

#[cfg(test)]
fn format_awaited_members(
    completed: bool,
    summary: &str,
    members: &[AwaitedMemberStatus],
) -> ToolOutput {
    format_awaited_members_with_reports(completed, summary, members, &HashMap::new())
}

fn latest_assistant_report(messages: &[HistoryMessage]) -> Option<String> {
    latest_assistant_comm_report(messages)
}

fn resolve_optional_target_session(target: Option<String>, current_session: &str) -> String {
    resolve_optional_comm_target_session(target, current_session)
}

/// Sessions a `status` call reports on. `target_session` (or its `to_session`
/// alias) wins, then each of `session_ids`, and only with neither given does it
/// fall back to the caller's own session.
fn status_target_sessions(
    target: Option<String>,
    session_ids: Option<&[String]>,
    current_session: &str,
) -> Vec<String> {
    if target.is_some() {
        return vec![resolve_optional_target_session(target, current_session)];
    }
    let mut targets: Vec<String> = Vec::new();
    for id in session_ids.unwrap_or_default() {
        let id = resolve_optional_target_session(Some(id.clone()), current_session);
        if !targets.contains(&id) {
            targets.push(id);
        }
    }
    if targets.is_empty() {
        targets.push(current_session.to_string());
    }
    targets
}

mod awaited_reports;
use awaited_reports::fetch_awaited_member_reports;

fn format_awaited_members_with_reports(
    completed: bool,
    summary: &str,
    members: &[AwaitedMemberStatus],
    reports: &HashMap<String, String>,
) -> ToolOutput {
    ToolOutput::new(format_comm_awaited_members_with_reports(
        completed, summary, members, reports,
    ))
}

fn default_await_target_statuses() -> Vec<String> {
    default_comm_await_target_statuses()
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn external_dm_output(response: &ServerEvent, body: &str) -> Option<ToolOutput> {
    let ServerEvent::RuntimeCommandResult { command, data, .. } = response else {
        return None;
    };
    if command != "swarm_message" {
        return None;
    }
    let target = data["external_target"].as_str()?;
    let text = match data["status"].as_str()? {
        "queued_cold" => format!("External DM queued for {target}: {body}"),
        "sent" => format!("External DM sent to {target}: {body}"),
        _ => return None,
    };
    Some(ToolOutput::new(text).with_metadata(json!({"external_dm_target": target})))
}

/// Render the `list_swarms` directory: one line per live swarm with label,
/// id, coordinator, and member count. The caller's own swarm is marked.
pub(super) fn format_swarm_directory(swarms: &[crate::protocol::SwarmInfo]) -> String {
    if swarms.is_empty() {
        return "No live swarms.".to_string();
    }
    let mut out = String::from("Live swarms:\n\n");
    for swarm in swarms {
        let name = swarm.label.as_deref().unwrap_or("(unlabeled)");
        let own = if swarm.is_own { " [yours]" } else { "" };
        let coordinator = match (&swarm.coordinator_name, &swarm.coordinator_session_id) {
            (Some(name), Some(id)) => format!("{name} [{id}]"),
            (None, Some(id)) => id.clone(),
            _ => "none".to_string(),
        };
        out.push_str(&format!(
            "- {name}{own}\n  id: {}\n  coordinator: {coordinator}\n  members: {}\n",
            swarm.swarm_id, swarm.member_count
        ));
    }
    out.push_str(
        "\nCross-swarm DM: action=dm with to_swarm=<label or id> and optional to_session (defaults to that swarm's coordinator).",
    );
    out
}

/// Render the `sessions` directory: live root sessions on the daemon (any
/// swarm) with the id/name to pass to `message` or `focus`.
pub(super) fn format_session_directory(sessions: &[AgentInfo]) -> ToolOutput {
    if sessions.is_empty() {
        return ToolOutput::new("No other live sessions.");
    }
    let mut out = String::from("Live sessions (any swarm):\n\n");
    for session in sessions {
        let name = session.friendly_name.as_deref().unwrap_or("(unnamed)");
        let status = session.status.as_deref().unwrap_or("unknown");
        let attached = match session.live_attachments {
            Some(0) => " [no client attached]",
            _ => "",
        };
        out.push_str(&format!(
            "- {name} [{}] {status}{attached}\n",
            session.session_id
        ));
        if let Some(detail) = session.detail.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("  {detail}\n"));
        }
    }
    out.push_str(
        "\nMessage one: action=message with to_session=<id or unique name>. Raise it in its TUI/Desktop: action=focus with to_session.",
    );
    ToolOutput::new(out)
}

fn format_channels(channels: &[SwarmChannelInfo]) -> ToolOutput {
    ToolOutput::new(format_comm_channels(channels))
}

/// Render the swarm model catalog for the `list_models` action: the current
/// (spawn-default) model, any config pin, and one line per route with
/// availability, auth method, and a relative cost estimate.
fn format_swarm_model_list(
    current_model: Option<&str>,
    configured_swarm_model: Option<&str>,
    model_routes: &[jcode_provider_core::ModelRoute],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Current coordinator model: {}\n",
        current_model.unwrap_or("unknown")
    ));
    match configured_swarm_model {
        Some(pin) if !pin.trim().is_empty() => {
            out.push_str(&format!("Configured agents.swarm_model default: {pin}\n"));
        }
        _ => out.push_str(
            "No agents.swarm_model default configured (workers inherit the coordinator's model).\n",
        ),
    }
    if model_routes.is_empty() {
        out.push_str("\nNo model routes reported. Omit model to use the configured default.");
        return out;
    }
    out.push_str("\nModel routes (informational only, overrides require agents.swarm_model_selection and an exact agents.swarm_model_allowlist entry):\n");
    for route in model_routes {
        let availability = if route.available {
            ""
        } else {
            " [unavailable]"
        };
        let cost = route
            .estimated_reference_cost_micros()
            .map(|micros| format!(" ~${:.2}/ref-task", micros as f64 / 1_000_000.0))
            .unwrap_or_default();
        let detail = if route.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", route.detail)
        };
        out.push_str(&format!(
            "- {} via {} [{}]{}{}{}\n",
            route.model, route.provider, route.api_method, availability, cost, detail
        ));
    }
    out
}

pub struct CommunicateTool {
    /// Full tool description including the user-tunable swarm prompt
    /// (model-routing guidance loaded from `swarm-prompt.md`). Computed once at
    /// registry construction so `description()` can hand out a borrowed str.
    description: String,
}

impl CommunicateTool {
    pub fn new() -> Self {
        Self::new_for_working_dir(None)
    }

    fn new_for_working_dir(working_dir: Option<&std::path::Path>) -> Self {
        let swarm_prompt = crate::prompt::load_swarm_prompt(working_dir);
        let description = if swarm_prompt.is_empty() {
            SWARM_DESCRIPTION.to_string()
        } else {
            format!("{SWARM_DESCRIPTION}{SWARM_PROMPT_SEPARATOR}{swarm_prompt}")
        };
        Self { description }
    }
}

const SWARM_DESCRIPTION: &str = "Coordinate agents: spawn todo-lane workers and exchange messages. Use set_model (to_session, model, optional effort) or compact (to_session) on an owned running or idle worker. Both keep its session, worktree, and conversation. If busy, the change is scheduled for the next turn boundary.";
const SWARM_PROMPT_SEPARATOR: &str = "\n\nSwarm prompt:\n";

pub(crate) fn swarm_instruction_tokens(tool: &crate::message::ToolDefinition) -> usize {
    if tool.name != "swarm"
        || !tool
            .description
            .strip_prefix(SWARM_DESCRIPTION)
            .is_some_and(|tail| tail.starts_with(SWARM_PROMPT_SEPARATOR))
    {
        return 0;
    }
    let mut schema_only = tool.clone();
    schema_only.description = SWARM_DESCRIPTION.to_string();
    tool.prompt_token_estimate()
        .saturating_sub(schema_only.prompt_token_estimate())
}

/// Swarm actions advertised to models. Real usage is dominated by messaging,
/// listing and spawning; the plan/task-graph actions (propose_plan,
/// assign_task, assign_next, resync_plan, ...) are subsumed by worker-owned
/// todo lanes, which the server dispatches itself, and lane-owning `spawn`. Unlisted actions still
/// execute for old transcripts and scripts, they are just not offered.
const ADVERTISED_SWARM_ACTIONS: &[&str] = &[
    "spawn",
    "message",
    "status",
    "plan_status",
    "list",
    "summary",
    "await_members",
    "stop",
    "set_model",
    "compact",
    "resume",
    "retry",
    "list_models",
    "list_swarms",
    "sessions",
    "focus",
    "answer",
];

/// Map common action synonyms/typos to the canonical swarm action name. Models
/// frequently invent near-miss verbs (e.g. `inbox` for reading messages, `send`
/// for `message`), which previously produced an "Unknown action" error. Unknown
/// inputs are returned unchanged so the normal validation path still reports them.
pub(super) fn canonical_swarm_action(action: &str) -> &str {
    match action.trim().to_ascii_lowercase().as_str() {
        "inbox" | "messages" | "check_messages" | "read_messages" | "read_inbox" => "read",
        "send" | "msg" | "send_message" => "message",
        // `dm` was its own action; `message` with `to_session` is identical,
        // so route it there (one way to talk to an agent).
        "dm" | "dm_session" | "direct_message" | "whisper" => "message",
        "broadcast_all" | "announce" => "broadcast",
        "agents" | "members" | "list_agents" | "list_members" | "roster" => "list",
        "swarms" | "list_swarm" | "swarm_list" => "list_swarms",
        "list_sessions" | "all_sessions" | "other_sessions" => "sessions",
        "raise" | "raise_session" | "focus_session" | "show_session" | "open_session" => "focus",
        "label_swarm" | "name_swarm" | "rename_swarm" | "set_label" => "set_swarm_label",
        "models" | "model_list" | "list_model" | "list_providers" | "list_routes" => "list_models",
        "plan" | "status_plan" => "plan_status",
        "assign" => "assign_task",
        "kill" | "terminate" => "stop",
        _ => action,
    }
}

pub(super) fn display_swarm_action(action: &str) -> Option<&str> {
    ADVERTISED_SWARM_ACTIONS
        .iter()
        .copied()
        .find(|known| *known == action)
        .or(match action {
            "share"
            | "share_append"
            | "read"
            | "broadcast"
            | "channel"
            | "set_swarm_label"
            | "list_channels"
            | "channel_members"
            | "propose_plan"
            | "approve_plan"
            | "reject_plan"
            | "cleanup"
            | "assign_role"
            | "report"
            | "read_context"
            | "resync_plan"
            | "assign_task"
            | "assign_next"
            | "start"
            | "start_task"
            | "wake"
            | "reassign"
            | "replace"
            | "subscribe_channel"
            | "unsubscribe_channel" => Some(action),
            _ => None,
        })
}

#[path = "swarm_model_schema.rs"]
mod swarm_model_schema;
use swarm_model_schema::apply_swarm_model_policy;
#[async_trait]
impl Tool for CommunicateTool {
    fn name(&self) -> &str {
        "swarm"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ADVERTISED_SWARM_ACTIONS
                },
                "message": {
                    "type": "string",
                    "description": "to_session, then channel. Else DM parent/coordinator or broadcast as coordinator."
                },
                "tldr": {
                    "type": "string",
                    "description": "Summary under ~120 chars; auto-derived if omitted."
                },
                "to_session": {
                    "type": "string",
                    "description": "ID/unique name. Optional for message. summary needs exact same-swarm ID from list."
                },
                "effort": {
                    "type": "string",
                    "description": "Reasoning effort for set_model, within configured caps."
                },
                "to_swarm": {
                    "type": "string",
                    "description": "Swarm ID/label; targets to_session or its coordinator."
                },
                "channel": {
                    "type": "string",
                    "description": "Prefer DMs."
                },
                "label": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Required for spawn: todo lane (source of truth)."
                },
                "phase": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Spawn lane phase; defaults to active phase."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Spawn working directory override. Omit to inherit the requesting session's working directory."
                },
                "prompt": {
                    "type": "string",
                    "description": "Extra spawn context, not lane tasks."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_COMM_SUMMARY_TOOL_CALLS,
                    "description": "Maximum summary calls (default 10)."
                },
                "task_id": {
                    "type": "string",
                    "description": "Plan task; auto-selected if omitted."
                },
                "spawn_mode": {
                    "type": "string",
                    "enum": ["visible", "headless", "inline", "auto"],
                    "description": "Omit to use the user's configured mode. Set only when the user asks for one."
                },
                "speed": {
                    "type": "string", "enum": ["standard", "fast", "ultrafast"],
                    "description": "Worker speed. Set only on user request. Faster modes require owner approval."
                },
                "profile": {
                    "type": "string",
                    "description": "scout, task, sonic or reviewer."
                },
                "session_ids": {
                    "type": "array",
                    "items": {"type": "string"}
                },
                "mode": {
                    "type": "string",
                    "enum": ["all", "any"],
                    "description": "Only for await_members. Not a summary scope."
                },
                "target_status": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "await_members; defaults to ready/completed/stopped/failed."
                },
                "timeout_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "await_members; returned ID can be used in todo.waiting_on."
                },
                "notify": {
                    "type": "boolean",
                    "description": "await_members: default true."
                },
                "force": {
                    "type": "boolean",
                    "description": "stop: allow stopping non-owned/user-created sessions; default false."
                },
                "wake": {
                    "type": "boolean",
                    "description": "Wake on message/await completion; default true."
                },
                "delivery": {
                    "type": "string",
                    "enum": ["notify", "interrupt", "wake"]
                },
                "answers": {
                    "type": "array",
                    "description": "answer: one object per question, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "selected": {"type": "array", "items": {"type": "string"}},
                            "other": {"type": "string"}
                        }
                    }
                },
                "request_id": {
                    "type": "string",
                    "description": "answer: escalation id when the worker has several pending asks."
                },
            }
        });

        // Advertise one spelling per capability. target_session and
        // initial_message remain accepted by execute for older callers.
        // required_spawn_label checks spawn's conditional requirement locally,
        // before dispatch, without repeating the action enum in schema branches.

        apply_swarm_model_policy(&mut schema, &crate::config::config().agents);
        schema
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CommunicateInput = serde_json::from_value(input)?;
        crate::config::config()
            .agents
            .validate_swarm_model_request(params.requested_model())
            .map_err(anyhow::Error::msg)?;
        // Hard worker allowlist: any agent-chosen worker model must be on
        // `agents.spawn_allowed_models`. The error names the allowed set.
        crate::config::config()
            .agents
            .validate_spawn_allowed_model(params.requested_model())
            .map_err(anyhow::Error::msg)?;

        match params.into_command(&ctx.session_id)? {
            Command::Answer {
                worker,
                request_id,
                answers,
            } => {
                let request = Request::CommAnswerEscalatedAsk {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    worker_session_id: worker.clone(),
                    request_id,
                    answers,
                };
                let response = send_request(request).await?;
                ensure_success(&response)?;
                Ok(ToolOutput::new(format!(
                    "Escalated ask answered. Worker {worker} resumes with your answer."
                ))
                .with_title("Ask answered"))
            }
            Command::Share { key, value, append } => {
                let request = Request::CommShare {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    key: key.clone(),
                    value: value.clone(),
                    append,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        let verb = if append {
                            "Appended shared context"
                        } else {
                            "Shared with other agents"
                        };
                        Ok(ToolOutput::new(format!("{}: {} = {}", verb, key, value)))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to share: {}", e)),
                }
            }

            Command::Read { key } => {
                let request = Request::CommRead {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    key,
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommContext { entries, .. }) => {
                        Ok(format_context_entries(&entries))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No shared context found."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to read shared context: {}", e)),
                }
            }

            Command::Message {
                message,
                to_session,
                channel,
                to_swarm,
                delivery,
            } => {
                let request = Request::CommMessage {
                    id: REQUEST_ID,
                    from_session: ctx.session_id.clone(),
                    message: message.clone(),
                    to_session: to_session.clone(),
                    channel: channel.clone(),
                    wake: delivery.wake,
                    delivery: delivery.mode,
                    tldr: delivery.tldr,
                    to_swarm,
                    require_target: true,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        if let Some(output) = external_dm_output(&response, &message) {
                            return Ok(output);
                        }
                        if let Some(output) = queued_cold_output(&response) {
                            return Ok(output);
                        }
                        let default_target = match &response {
                            ServerEvent::RuntimeCommandResult { command, data, .. }
                                if command == "swarm_message" =>
                            {
                                data["default_target"].as_str()
                            }
                            _ => None,
                        };
                        let confirmation = match (to_session.as_deref().or(default_target), channel)
                        {
                            (Some(target), _) => {
                                format!("Direct message sent to {}: {}", target, message)
                            }
                            (None, Some(channel)) => {
                                format!("Channel message sent to #{}: {}", channel, message)
                            }
                            (None, None) => {
                                format!("Broadcast sent to your spawned subtree: {}", message)
                            }
                        };
                        Ok(ToolOutput::new(confirmation))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to send message: {}", e)),
                }
            }

            Command::Broadcast { message, delivery } => {
                let request = Request::CommMessage {
                    id: REQUEST_ID,
                    from_session: ctx.session_id.clone(),
                    message: message.clone(),
                    to_session: None,
                    channel: None,
                    wake: delivery.wake,
                    delivery: delivery.mode,
                    tldr: delivery.tldr,
                    to_swarm: None,
                    require_target: false,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        if let Some(output) = queued_cold_output(&response) {
                            return Ok(output);
                        }
                        Ok(ToolOutput::new(format!(
                            "Broadcast sent to your spawned subtree: {}",
                            message
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to broadcast message: {}", e)),
                }
            }

            Command::ChannelMessage {
                message,
                channel,
                delivery,
            } => {
                let request = Request::CommMessage {
                    id: REQUEST_ID,
                    from_session: ctx.session_id.clone(),
                    message: message.clone(),
                    to_session: None,
                    channel: Some(channel.clone()),
                    delivery: delivery.mode,
                    wake: delivery.wake,
                    tldr: delivery.tldr,
                    to_swarm: None,
                    require_target: false,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        if let Some(output) = queued_cold_output(&response) {
                            return Ok(output);
                        }
                        Ok(ToolOutput::new(format!(
                            "Channel message sent to #{}: {}",
                            channel, message
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to send channel message: {}", e)),
                }
            }

            Command::List => {
                let request = Request::CommList {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommMembers { members, .. }) => {
                        Ok(format_members(&ctx, &members))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No agents found."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list agents: {}", e)),
                }
            }

            Command::Sessions => {
                let request = Request::CommListSessions {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };
                match send_request(request).await {
                    Ok(ServerEvent::CommMembers { members, .. }) => {
                        Ok(format_session_directory(&members))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No other live sessions."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list sessions: {}", e)),
                }
            }

            Command::Focus { target } => {
                let request = Request::CommFocusSession {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: target.clone(),
                };
                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!(
                            "Asked the clients attached to {target} to raise it (Desktop selects the tab, terminal sessions use the configured focus hook)."
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to focus session: {}", e)),
                }
            }

            Command::ListSwarms => {
                let request = Request::CommListSwarms {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };
                match send_request(request).await {
                    Ok(ServerEvent::CommSwarms { swarms, .. }) => {
                        Ok(ToolOutput::new(format_swarm_directory(&swarms)))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No swarms found."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list swarms: {}", e)),
                }
            }

            Command::SetSwarmLabel { label } => {
                let request = Request::CommSetSwarmLabel {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    label: label.clone(),
                };
                match send_request(request).await {
                    Ok(ServerEvent::CommSwarms { swarms, .. }) => {
                        let applied = swarms
                            .iter()
                            .find(|swarm| swarm.is_own)
                            .and_then(|swarm| swarm.label.clone());
                        let head = match applied {
                            Some(label) => format!("Your swarm is now labeled '{label}'.\n\n"),
                            None => "Your swarm label was cleared.\n\n".to_string(),
                        };
                        Ok(ToolOutput::new(format!(
                            "{head}{}",
                            format_swarm_directory(&swarms)
                        )))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!("Swarm label set to '{label}'.")))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to set swarm label: {}", e)),
                }
            }

            Command::ListChannels => {
                let request = Request::CommListChannels {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommChannels { channels, .. }) => {
                        Ok(format_channels(&channels))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No channels found."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list channels: {}", e)),
                }
            }

            Command::ChannelMembers { channel } => {
                let request = Request::CommChannelMembers {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    channel: channel.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommMembers { members, .. }) => {
                        let mut output = format!("Members subscribed to #{}:\n\n", channel);
                        if members.is_empty() {
                            output.push_str("  (none)\n");
                        } else {
                            for member in members {
                                let name = member.friendly_name.unwrap_or(member.session_id);
                                let status = member.status.unwrap_or_else(|| "unknown".to_string());
                                output.push_str(&format!("  {} ({})\n", name, status));
                            }
                        }
                        Ok(ToolOutput::new(output))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No channel members found."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list channel members: {}", e)),
                }
            }

            Command::ProposePlan { items } => {
                let item_count = items.len() as u64;

                let request = Request::CommProposePlan {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    items,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!(
                            "Plan proposal submitted ({} items).",
                            item_count
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to propose plan: {}", e)),
                }
            }

            Command::ApprovePlan { proposer } => {
                let request = Request::CommApprovePlan {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    proposer_session: proposer.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!(
                            "Approved plan proposal from {}",
                            proposer
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to approve plan: {}", e)),
                }
            }

            Command::RejectPlan { proposer, reason } => {
                let request = Request::CommRejectPlan {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    proposer_session: proposer.clone(),
                    reason: reason.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        let reason_msg = reason
                            .as_ref()
                            .map(|r| format!(" (reason: {})", r))
                            .unwrap_or_default();
                        Ok(ToolOutput::new(format!(
                            "Rejected plan proposal from {}{}",
                            proposer, reason_msg
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to reject plan: {}", e)),
                }
            }

            Command::Spawn(spawn) => {
                // Every spawned worker owns one lane in the coordinator's todo
                // list: the lane named `label` in `phase` (default: the active
                // phase). When it does not exist it is created with one todo
                // named after the label, with the prompt as its details. The
                // worker's own todo list is that lane and syncs only it.
                let label = spawn.label;
                let (lane_id, seeded_count) = {
                    let todos = crate::todo::load_todos(&ctx.session_id).unwrap_or_default();
                    let title = derive_swarm_task_label(&label).unwrap_or_else(|| label.clone());
                    let (updated, lane_id) = crate::todo::stages::ensure_spawn_lane(
                        &todos,
                        &label,
                        spawn.phase,
                        &title,
                        spawn.prompt.as_deref(),
                        &{
                            let mut n = todos.len() + 1;
                            while todos.iter().any(|t| t.id == format!("lane-{n}")) {
                                n += 1;
                            }
                            format!("lane-{n}")
                        },
                    );
                    if updated != todos {
                        crate::todo::save_todos(&ctx.session_id, &updated)?;
                    }
                    let seeded_count =
                        crate::todo::stages::lane_todos_for_worker(&updated, &lane_id).len();
                    (lane_id, seeded_count)
                };
                let request = Request::CommSpawn {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    working_dir: spawn.working_dir,
                    initial_message: spawn.prompt,
                    request_nonce: Some(ctx.tool_call_id.clone()),
                    spawn_mode: spawn.spawn_mode,
                    model: spawn.model,
                    // Worker effort is not agent-controlled: the server applies
                    // `agents.swarm_effort` (default "low").
                    effort: None,
                    speed: spawn.speed,
                    label: Some(label),
                    lane_id: Some(lane_id.clone()),
                    profile: spawn.profile,
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommSpawnResponse {
                        new_session_id,
                        worker_speed,
                        ..
                    }) if !new_session_id.is_empty() => {
                        let speed = worker_speed
                            .as_ref()
                            .map(|speed| format!("\n{speed}"))
                            .unwrap_or_default();
                        Ok(ToolOutput::new(format!(
                            "Spawned new agent: {new_session_id}{speed}\nLane {lane_id}: {seeded_count} todo item(s) prepopulated before startup and synced to your lane. The worker received its lane tasks automatically."
                        ))
                        .with_metadata(
                            serde_json::json!({"session_id": new_session_id, "lane": lane_id, "seeded_todo_count": seeded_count, "lane_synced": true, "worker_speed": worker_speed}),
                        ))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Err(anyhow::anyhow!(
                            "Spawn succeeded but new session ID was not returned."
                        ))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to spawn agent: {}", e)),
                }
            }

            Command::ListModels => {
                let request = Request::CommListModels {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };
                match send_request(request).await {
                    Ok(ServerEvent::CommListModelsResponse {
                        current_model,
                        configured_swarm_model,
                        model_routes,
                        ..
                    }) => Ok(ToolOutput::new(format_swarm_model_list(
                        current_model.as_deref(),
                        configured_swarm_model.as_deref(),
                        &model_routes,
                    ))),
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No model catalog returned."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to list models: {}", e)),
                }
            }

            Command::WorkerControl {
                target_session,
                control,
            } => {
                match send_request(Request::CommWorkerControl {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session,
                    control,
                })
                .await?
                {
                    ServerEvent::CommWorkerControlResponse { message, .. } => {
                        Ok(ToolOutput::new(message))
                    }
                    response => {
                        ensure_success(&response)?;
                        anyhow::bail!("No worker control result returned")
                    }
                }
            }

            Command::StopMany { targets, force } => {
                let outcome = stop_swarm_sessions(&ctx, targets, force).await;
                if outcome.stopped.is_empty() {
                    return Err(anyhow::Error::msg(outcome.describe()));
                }
                Ok(ToolOutput::new(outcome.describe()))
            }

            Command::StopOne { target, force } => {
                let request = Request::CommStop {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: target.clone(),
                    force,
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!("Stopped agent: {}", target)))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to stop agent: {}", e)),
                }
            }

            Command::Cleanup(cleanup) => cleanup_swarm_workers(&ctx, cleanup)
                .await
                .map(ToolOutput::new),

            Command::AssignRole { target, role } => {
                let request = Request::CommAssignRole {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: target.clone(),
                    role: role.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!(
                            "Assigned role '{}' to {}",
                            role, target
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to assign role: {}", e)),
                }
            }

            Command::Status { targets } => {
                let mut sections = Vec::new();
                for target in targets {
                    let request = Request::CommStatus {
                        id: REQUEST_ID,
                        session_id: ctx.session_id.clone(),
                        target_session: target,
                    };
                    match send_request(request).await {
                        Ok(ServerEvent::CommStatusResponse { snapshot, .. }) => {
                            sections.push(format_status_snapshot(&snapshot).output);
                        }
                        Ok(response) => {
                            ensure_success(&response)?;
                            sections.push("No status snapshot returned.".to_string());
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!("Failed to get status snapshot: {}", e));
                        }
                    }
                }
                Ok(ToolOutput::new(sections.join("\n\n---\n\n")))
            }

            Command::Report(report) => {
                let request = Request::CommReport {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    status: report.status,
                    message: report.message,
                    validation: report.validation,
                    follow_up: report.follow_up,
                    tldr: report.tldr,
                };
                match send_request(request).await {
                    Ok(ServerEvent::CommReportResponse {
                        status, message, ..
                    }) => Ok(ToolOutput::new(format!(
                        "Report recorded with status `{status}`. {message}"
                    ))),
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("Report recorded."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to record report: {}", e)),
                }
            }

            Command::PlanStatus => {
                let summary = fetch_plan_status(&ctx.session_id).await?;
                Ok(format_plan_status(&summary))
            }

            Command::Summary { target, limit } => {
                let request = Request::CommSummary {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: target.clone(),
                    limit,
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommSummaryResponse {
                        tool_calls,
                        snapshot,
                        ..
                    }) => Ok(format_tool_summary(&target, &tool_calls, snapshot)),
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No tool call data returned."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to get summary: {}", e)),
                }
            }

            Command::ReadContext { target } => {
                let request = Request::CommReadContext {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: target.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommContextHistory { messages, .. }) => {
                        Ok(format_context_history(&target, &messages))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("No context data returned."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to read context: {}", e)),
                }
            }

            Command::ResyncPlan => {
                let request = Request::CommResyncPlan {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("Swarm plan re-synced to your session."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to resync plan: {}", e)),
                }
            }

            Command::AssignTask(assignment) => {
                let target = assignment
                    .target_session
                    .clone()
                    .unwrap_or_else(|| "next available agent".to_string());

                if assignment.prefer_spawn && assignment.target_session.is_none() {
                    let spawned_session = spawn_assignment_session(&ctx, &assignment.spawn).await?;
                    return assign_task_to_session(
                        &ctx,
                        &assignment,
                        spawned_session,
                        " (spawned by planner preference)",
                    )
                    .await;
                }

                let request = Request::CommAssignTask {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: assignment.target_session.clone(),
                    task_id: assignment.task_id.clone(),
                    message: assignment.message.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommAssignTaskResponse {
                        task_id,
                        target_session,
                        ..
                    }) => {
                        let mut output =
                            format!("Task '{}' assigned to {}", task_id, target_session);
                        if let Ok(summary) = fetch_plan_status(&ctx.session_id).await {
                            output.push_str(&format!("\n{}", format_plan_followup(&summary)));
                        }
                        Ok(ToolOutput::new(output))
                    }
                    Ok(response)
                        if assignment.spawn_if_needed
                            && assignment.target_session.is_none()
                            && auto_assignment_needs_spawn(&response) =>
                    {
                        let spawned_session =
                            spawn_assignment_session(&ctx, &assignment.spawn).await?;
                        assign_task_to_session(
                            &ctx,
                            &assignment,
                            spawned_session,
                            " (spawned automatically)",
                        )
                        .await
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        let msg = assignment.task_id.as_deref().map_or_else(
                            || format!("Assigned next runnable task to {}", target),
                            |task_id| format!("Task '{}' assigned to {}", task_id, target),
                        );
                        Ok(ToolOutput::new(msg))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to assign task: {}", e)),
                }
            }

            Command::AssignNext(next) => {
                let target = next
                    .target_session
                    .clone()
                    .unwrap_or_else(|| "next available agent".to_string());

                let request = Request::CommAssignNext {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: next.target_session,
                    working_dir: next.working_dir,
                    prefer_spawn: next.prefer_spawn,
                    spawn_if_needed: next.spawn_if_needed,
                    message: next.message,
                    model: next.model,
                    // Worker effort is not agent-controlled: the server applies
                    // `agents.swarm_effort` (default "low").
                    effort: None,
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommAssignTaskResponse {
                        task_id,
                        target_session,
                        ..
                    }) => Ok(ToolOutput::new(format!(
                        "Task '{}' assigned to {}",
                        task_id, target_session
                    ))),
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!(
                            "Assigned next runnable task to {}",
                            target
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to assign next task: {}", e)),
                }
            }

            Command::TaskControl(control) => {
                let request = Request::CommTaskControl {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    action: control.control_action.clone(),
                    task_id: control.task_id.clone(),
                    target_session: control.target_session.clone(),
                    message: control.message.clone(),
                };

                match send_request(request).await {
                    Ok(ServerEvent::CommTaskControlResponse {
                        task_id,
                        action,
                        target_session,
                        status,
                        summary,
                        ..
                    }) => {
                        let mut output = format!("Task '{}' {}", task_id, action);
                        if let Some(target_session) = target_session {
                            output.push_str(&format!(" -> {}", target_session));
                        }
                        output.push_str(&format!("\nStatus: {}", status));
                        if !summary.next_ready_ids.is_empty() {
                            output.push_str(&format!(
                                "\nNext ready: {}",
                                summary.next_ready_ids.join(", ")
                            ));
                        }
                        if !summary.newly_ready_ids.is_empty() {
                            output.push_str(&format!(
                                "\nNewly ready: {}",
                                summary.newly_ready_ids.join(", ")
                            ));
                        }
                        Ok(ToolOutput::new(output))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        let target_suffix = control
                            .target_session
                            .as_deref()
                            .map(|target| format!(" -> {}", target))
                            .unwrap_or_default();
                        Ok(ToolOutput::new(format!(
                            "Task '{}' {}{}",
                            control.task_id, control.action, target_suffix
                        )))
                    }
                    Err(e) => Err(anyhow::anyhow!(
                        "Failed to {} task: {}",
                        control.control_action,
                        e
                    )),
                }
            }

            Command::SubscribeChannel { channel } => {
                let request = Request::CommSubscribeChannel {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    channel: channel.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!("Subscribed to #{}", channel)))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to subscribe: {}", e)),
                }
            }

            Command::UnsubscribeChannel { channel } => {
                let request = Request::CommUnsubscribeChannel {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    channel: channel.clone(),
                };

                match send_request(request).await {
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new(format!("Unsubscribed from #{}", channel)))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to unsubscribe: {}", e)),
                }
            }

            Command::AwaitMembers(wait) => {
                let request = Request::CommAwaitMembers {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_status: wait.target_status,
                    session_ids: wait.session_ids,
                    mode: wait.mode,
                    timeout_secs: Some(wait.timeout_secs),
                    background: true,
                    notify: wait.notify,
                    wake: wait.wake,
                };

                // Member waits return promptly with a snapshot, so the short
                // socket timeout suffices.
                let socket_timeout = std::time::Duration::from_secs(30);

                match send_request_with_timeout(request, Some(socket_timeout)).await {
                    Ok(ServerEvent::CommAwaitMembersResponse {
                        completed,
                        members,
                        summary,
                        background_started,
                        ..
                    }) => {
                        if background_started {
                            let compatibility_note = if wait.blocking_was_requested {
                                "\n\n(Blocking member waits are no longer supported; this wait was started asynchronously.)"
                            } else {
                                "\n\n(You can keep working; this wait runs in the background.)"
                            };
                            return Ok(ToolOutput::new(format!(
                                "{}{}\n\n{}",
                                summary,
                                compatibility_note,
                                super::BACKGROUND_WAIT_GUIDANCE
                            )));
                        }
                        let reports = fetch_awaited_member_reports(&ctx, &members).await;
                        Ok(format_awaited_members_with_reports(
                            completed, &summary, &members, &reports,
                        ))
                    }
                    Ok(response) => {
                        ensure_success(&response)?;
                        Ok(ToolOutput::new("Await completed."))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to await members: {}", e)),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "communicate_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "communicate_status_target_tests.rs"]
mod status_target_tests;
