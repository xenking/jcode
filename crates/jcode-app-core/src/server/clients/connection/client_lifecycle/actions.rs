#![cfg_attr(test, allow(clippy::items_after_test_module))]

use super::super::{
    SwarmEvent, SwarmMember, SwarmState, VersionedPlan, broadcast_swarm_status,
    fanout_session_event, persist_swarm_state_for, remove_session_channel_subscriptions,
    remove_session_from_swarm, swarm_id_for_session, truncate_detail, update_member_status,
};
use super::*;
use crate::agent::Agent;
use crate::protocol::{FeatureToggle, ServerEvent};
use crate::server::turn_execution::process_reserved_message_streaming_mpsc;
use crate::session::Session;
use crate::util::truncate_str;
use jcode_agent_runtime::StreamError;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use super::super::SessionAgents;
type ChannelSubscriptions = Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;

const INPUT_SHELL_MAX_OUTPUT_LEN: usize = 30_000;

fn derive_subagent_description(prompt: &str) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(4).collect();
    if words.is_empty() {
        "Manual subagent".to_string()
    } else {
        words.join(" ")
    }
}

#[path = "input_shell_output.rs"]
mod input_shell_output;
use crate::server::session_notifications::{
    NotifySessionContext, handle_applet_action, handle_close_applet, handle_notify_session,
};
use input_shell_output::{build_input_shell_command, combine_input_shell_output};

fn handle_input_shell(
    id: u64,
    command: String,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let agent = Arc::clone(agent);
    let tx = client_event_tx.clone();

    crate::runtime_activity::spawn(async move {
        let cwd = {
            let agent_guard = agent.lock().await;
            agent_guard.working_dir().map(|dir| dir.to_string())
        };

        let started = Instant::now();
        let mut cmd = build_input_shell_command(&command);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd.as_ref() {
            cmd.current_dir(dir);
        }

        let result = match cmd.output().await {
            Ok(output) => {
                let (combined_output, truncated) =
                    combine_input_shell_output(&output.stdout, &output.stderr);
                crate::message::InputShellResult {
                    command,
                    cwd,
                    output: combined_output,
                    exit_code: output.status.code(),
                    duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    truncated,
                    failed_to_start: false,
                }
            }
            Err(error) => crate::message::InputShellResult {
                command,
                cwd,
                output: format!("Failed to run command: {}", error),
                exit_code: None,
                duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                truncated: false,
                failed_to_start: true,
            },
        };

        let _ = tx.send(ServerEvent::InputShellResult { result });
        let _ = tx.send(ServerEvent::Done { id });
    });
}

/// Routing for future workers is metadata, so it can be updated during a turn.
pub(in crate::server) async fn set_session_agent_model(
    agent: &Arc<Mutex<Agent>>,
    session_id: &str,
    target: &str,
    model: Option<String>,
) -> anyhow::Result<crate::session::AgentModelOverrides> {
    if let Ok(mut guard) = agent.try_lock() {
        guard.set_agent_model_override(target, model)?;
        Ok(Agent::read_session(&guard, |session| {
            session.agent_model_overrides()
        }))
    } else {
        let mut session = Session::load_startup_stub(session_id)?;
        session.set_agent_model_override(target, model)?;
        Ok(session.agent_model_overrides())
    }
}

/// Save a worker-model default in the server's own configuration, so remote
/// clients change the default that server-side workers actually use.
pub(in crate::server) async fn set_global_agent_model(
    agent: &Arc<Mutex<Agent>>,
    session_id: &str,
    target: &str,
    model: Option<String>,
) -> anyhow::Result<crate::session::AgentModelOverrides> {
    crate::config::Config::set_agent_model_default(target, model.as_deref())?;
    // Report the session's current overrides so the client view stays exact.
    if let Ok(guard) = agent.try_lock() {
        Ok(Agent::read_session(&guard, |session| {
            session.agent_model_overrides()
        }))
    } else {
        Ok(Session::load_startup_stub(session_id)?.agent_model_overrides())
    }
}

async fn handle_set_subagent_model(
    id: u64,
    model: Option<String>,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let mut agent_guard = agent.lock().await;
    match agent_guard.set_subagent_model(model) {
        Ok(()) => {
            let _ = client_event_tx.send(ServerEvent::Done { id });
        }
        Err(error) => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: crate::util::format_error_chain(&error),
                retry_after_secs: None,
            });
        }
    }
}

fn handle_run_subagent(
    id: u64,
    prompt: String,
    subagent_type: String,
    model: Option<String>,
    session_id: Option<String>,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let agent = Arc::clone(agent);
    let tx = client_event_tx.clone();

    crate::runtime_activity::spawn(async move {
        let description = derive_subagent_description(&prompt);
        let tool_call_id = crate::id::new_id("call");
        let tool_name = "subagent".to_string();
        let tool_input = serde_json::json!({
            "description": description,
            "prompt": prompt,
            "subagent_type": subagent_type,
            "model": model,
            "session_id": session_id,
            "command": "/subagent",
        });

        let message_id = {
            let mut agent_guard = agent.lock().await;
            match agent_guard.add_manual_tool_use(
                tool_call_id.clone(),
                tool_name.clone(),
                tool_input.clone(),
            ) {
                Ok(message_id) => message_id,
                Err(error) => {
                    let _ = tx.send(ServerEvent::Error {
                        id,
                        message: crate::util::format_error_chain(&error),
                        retry_after_secs: None,
                    });
                    return;
                }
            }
        };

        let _ = tx.send(ServerEvent::ToolStart {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
        });
        let _ = tx.send(ServerEvent::ToolInput {
            id: Some(tool_call_id.clone()),
            delta: tool_input.to_string(),
        });
        let _ = tx.send(ServerEvent::ToolExec {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
        });

        let (registry, session_id, working_dir) = {
            let agent_guard = agent.lock().await;
            (
                agent_guard.registry(),
                agent_guard.session_id().to_string(),
                agent_guard.working_dir().map(std::path::PathBuf::from),
            )
        };

        let ctx = crate::tool::ToolContext {
            session_id,
            message_id,
            tool_call_id: tool_call_id.clone(),
            working_dir,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::Direct,
        };

        let started = Instant::now();
        let tool_name_for_exec = tool_name.clone();
        let result = match crate::runtime_activity::spawn(async move {
            registry.execute(&tool_name_for_exec, tool_input, ctx).await
        }) {
            Some(handle) => match handle.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("Tool task panicked: {}", error)),
            },
            None => Err(anyhow::anyhow!(
                "Tool task was not admitted because the runtime is shutting down"
            )),
        };
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        match result {
            Ok(output) => {
                let output_text = output.output.clone();
                let _ = tx.send(ServerEvent::ToolDone {
                    codemode: None,
                    ask: None,
                    id: tool_call_id.clone(),
                    name: tool_name,
                    output: output_text,
                    error: None,
                });
                let persist = {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.add_manual_tool_result(tool_call_id, output, duration_ms)
                };
                if let Err(error) = persist {
                    let _ = tx.send(ServerEvent::Error {
                        id,
                        message: crate::util::format_error_chain(&error),
                        retry_after_secs: None,
                    });
                    return;
                }
                let _ = tx.send(ServerEvent::Done { id });
            }
            Err(error) => {
                let error_msg = format!("Error: {}", error);
                let _ = tx.send(ServerEvent::ToolDone {
                    codemode: None,
                    ask: None,
                    id: tool_call_id.clone(),
                    name: tool_name,
                    output: error_msg.clone(),
                    error: Some(error_msg.clone()),
                });
                let persist = {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.add_manual_tool_error(tool_call_id, error_msg, duration_ms)
                };
                if let Err(persist_error) = persist {
                    let _ = tx.send(ServerEvent::Error {
                        id,
                        message: crate::util::format_error_chain(&persist_error),
                        retry_after_secs: None,
                    });
                    return;
                }
                let _ = tx.send(ServerEvent::Done { id });
            }
        }
    });
}

#[expect(
    clippy::too_many_arguments,
    reason = "set feature mutates agent state, persistence, swarm/session metadata, and client notifications together"
)]
pub(super) async fn handle_set_feature(
    id: u64,
    feature: FeatureToggle,
    enabled: bool,
    agent: &Arc<Mutex<Agent>>,
    client_session_id: &str,
    _friendly_name: &Option<String>,
    swarm_enabled: &mut bool,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    match feature {
        FeatureToggle::Memory => {
            let mut agent_guard = agent.lock().await;
            agent_guard.set_memory_enabled(enabled);
            drop(agent_guard);
            if !enabled {
                crate::memory::clear_pending_memory(client_session_id);
            }
            crate::runtime_memory_log::emit_event(
                crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                    "memory_feature_toggled",
                    if enabled {
                        "memory_feature_enabled"
                    } else {
                        "memory_feature_disabled"
                    },
                )
                .with_session_id(client_session_id.to_string())
                .force_attribution(),
            );
            let _ = client_event_tx.send(ServerEvent::Done { id });
        }
        FeatureToggle::Autoreview => {
            let mut agent_guard = agent.lock().await;
            match agent_guard.set_autoreview_enabled(enabled) {
                Ok(()) => {
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                }
                Err(error) => {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: crate::util::format_error_chain(&error),
                        retry_after_secs: None,
                    });
                }
            }
        }
        FeatureToggle::Autojudge => {
            let mut agent_guard = agent.lock().await;
            match agent_guard.set_autojudge_enabled(enabled) {
                Ok(()) => {
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                }
                Err(error) => {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: crate::util::format_error_chain(&error),
                        retry_after_secs: None,
                    });
                }
            }
        }
        FeatureToggle::Swarm => {
            if *swarm_enabled == enabled {
                let _ = client_event_tx.send(ServerEvent::Done { id });
                return;
            }
            *swarm_enabled = enabled;

            let (old_swarm_id, working_dir) = {
                let mut members = swarm_members.write().await;
                if let Some(member) = members.get_mut(client_session_id) {
                    let old = member.swarm_id.clone();
                    let wd = member.working_dir.clone();
                    member.swarm_enabled = enabled;
                    if !enabled {
                        member.swarm_id = None;
                        member.role = "agent".to_string();
                    }
                    (old, wd)
                } else {
                    (None, None)
                }
            };

            if let Some(ref old_id) = old_swarm_id {
                remove_session_from_swarm(
                    client_session_id,
                    old_id,
                    swarm_members,
                    swarms_by_id,
                    swarm_coordinators,
                    swarm_plans,
                )
                .await;
                remove_session_channel_subscriptions(
                    client_session_id,
                    channel_subscriptions,
                    channel_subscriptions_by_session,
                )
                .await;
            }

            if enabled {
                let _ = working_dir;
                let new_swarm_id = swarm_id_for_session(client_session_id);
                if let Some(ref id) = new_swarm_id {
                    {
                        let mut swarms = swarms_by_id.write().await;
                        swarms
                            .entry(id.clone())
                            .or_insert_with(HashSet::new)
                            .insert(client_session_id.to_string());
                    }

                    {
                        let mut members = swarm_members.write().await;
                        if let Some(member) = members.get_mut(client_session_id) {
                            member.swarm_id = Some(id.clone());
                            member.role = "agent".to_string();
                        }
                    }

                    broadcast_swarm_status(id, swarm_members, swarms_by_id).await;
                    let swarm_state = SwarmState {
                        members: Arc::clone(swarm_members),
                        swarms_by_id: Arc::clone(swarms_by_id),
                        plans: Arc::clone(swarm_plans),
                        coordinators: Arc::clone(swarm_coordinators),
                    };
                    persist_swarm_state_for(id, &swarm_state).await;
                } else {
                    let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                        members: Vec::new(),
                    });
                }
            } else {
                let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                    members: Vec::new(),
                });
            }

            let _ = client_event_tx.send(ServerEvent::Done { id });
        }
    }
}

/// Bookmark or unbookmark the session. A save label is the name the user chose,
/// so it doubles as the session title and is announced like a rename.
async fn handle_set_session_saved(
    id: u64,
    saved: bool,
    label: Option<String>,
    agent: &Arc<Mutex<Agent>>,
    client_session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let result = agent.lock().await.set_session_saved(saved, label.clone());
    if let Err(error) = result {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: crate::util::format_error_chain(&error),
            retry_after_secs: None,
        });
        return;
    }
    crate::session_list_cache::invalidate();
    let label = label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty());
    if saved && label.is_some() {
        let (session_id, display_title) = {
            let agent = agent.lock().await;
            (
                agent.session_id().to_string(),
                agent.session_display_title_or_name(),
            )
        };
        broadcast_session_renamed(
            swarm_members,
            client_session_id,
            client_event_tx,
            ServerEvent::SessionRenamed {
                session_id,
                title: label.map(ToOwned::to_owned),
                display_title,
            },
        )
        .await;
    }
    let _ = client_event_tx.send(ServerEvent::Done { id });
}

async fn broadcast_session_renamed(
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_session_id: &str,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    event: ServerEvent,
) -> usize {
    let ServerEvent::SessionRenamed { session_id, .. } = &event else {
        return 0;
    };
    let renamed_session_id = session_id.clone();
    let mut delivered =
        fanout_session_event(swarm_members, &renamed_session_id, event.clone()).await;
    if renamed_session_id != client_session_id {
        delivered += fanout_session_event(swarm_members, client_session_id, event.clone()).await;
    }
    if delivered == 0 {
        let _ = client_event_tx.send(event);
    }
    delivered
}

async fn handle_rename_session(
    id: u64,
    title: Option<String>,
    agent: &Arc<Mutex<Agent>>,
    client_session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let started = Instant::now();
    let normalized_title = title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned);
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "rename_start".to_string()),
            ("request_id", id.to_string()),
            ("session_id", client_session_id.to_string()),
            (
                "title_chars",
                normalized_title
                    .as_ref()
                    .map(|title| title.chars().count().to_string())
                    .unwrap_or_else(|| "0".to_string()),
            ),
        ],
    );

    let (renamed_session_id, display_title) = {
        let mut agent_guard = agent.lock().await;
        match agent_guard.rename_session_title(normalized_title.clone()) {
            Ok(display_title) => (agent_guard.session_id().to_string(), display_title),
            Err(error) => {
                crate::logging::event_warn(
                    "SESSION_LIFECYCLE",
                    vec![
                        ("phase", "rename_error".to_string()),
                        ("request_id", id.to_string()),
                        ("session_id", client_session_id.to_string()),
                        ("error", crate::util::format_error_chain(&error)),
                        ("elapsed_ms", started.elapsed().as_millis().to_string()),
                    ],
                );
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: crate::util::format_error_chain(&error),
                    retry_after_secs: None,
                });
                return;
            }
        }
    };

    crate::session_list_cache::invalidate();
    let event = ServerEvent::SessionRenamed {
        session_id: renamed_session_id.clone(),
        title: normalized_title,
        display_title,
    };
    let delivered =
        broadcast_session_renamed(swarm_members, client_session_id, client_event_tx, event).await;
    let _ = client_event_tx.send(ServerEvent::Done { id });
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "rename_done".to_string()),
            ("request_id", id.to_string()),
            ("session_id", renamed_session_id),
            ("client_session_id", client_session_id.to_string()),
            ("delivered", delivered.to_string()),
            ("elapsed_ms", started.elapsed().as_millis().to_string()),
        ],
    );
}

async fn handle_trigger_memory_extraction(
    id: u64,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let extraction = {
        let agent_guard = agent.lock().await;
        if !agent_guard.memory_enabled() {
            None
        } else {
            let transcript = agent_guard.build_transcript_for_extraction();
            if transcript.len() < 200 {
                None
            } else {
                Some((
                    transcript,
                    agent_guard.session_id().to_string(),
                    agent_guard.working_dir().map(|dir| dir.to_string()),
                ))
            }
        }
    };

    if let Some((transcript, session_id, working_dir)) = extraction {
        crate::memory_agent::trigger_final_extraction_with_dir(transcript, session_id, working_dir);
    }

    let _ = client_event_tx.send(ServerEvent::Done { id });
}

#[path = "session_transfer_snapshot.rs"]
mod session_transfer_snapshot;
use session_transfer_snapshot::*;

async fn handle_split(
    id: u64,
    client_session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let started = Instant::now();
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "split_start".to_string()),
            ("request_id", id.to_string()),
            ("session_id", client_session_id.to_string()),
        ],
    );
    // Splitting must remain available during a streaming turn. Never await the
    // Agent lock: busy sessions can still fork their last persisted snapshot.
    let result = {
        let idle_agent = agent.try_lock().ok();
        match idle_agent.as_ref() {
            Some(idle) => Agent::read_session(idle, |session| {
                clone_split_session(client_session_id, Some(session))
            }),
            None => clone_split_session(client_session_id, None),
        }
    };
    let (new_session_id, new_session_name) = match result {
        Ok(result) => result,
        Err(e) => {
            crate::logging::event_warn(
                "SESSION_LIFECYCLE",
                vec![
                    ("phase", "split_error".to_string()),
                    ("request_id", id.to_string()),
                    ("session_id", client_session_id.to_string()),
                    ("error", crate::util::format_error_chain(&e)),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Failed to save split session: {e}"),
                retry_after_secs: None,
            });
            return;
        }
    };
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "split_done".to_string()),
            ("request_id", id.to_string()),
            ("session_id", client_session_id.to_string()),
            ("new_session_id", new_session_id.clone()),
            ("elapsed_ms", started.elapsed().as_millis().to_string()),
        ],
    );

    let _ = client_event_tx.send(ServerEvent::SplitResponse {
        id,
        new_session_id,
        new_session_name,
    });
}

async fn handle_transfer(
    id: u64,
    client_session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let started = Instant::now();
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "transfer_start".to_string()),
            ("request_id", id.to_string()),
            ("session_id", client_session_id.to_string()),
        ],
    );
    let parent = match Session::load(client_session_id) {
        Ok(session) => session,
        Err(error) => {
            crate::logging::event_warn(
                "SESSION_LIFECYCLE",
                vec![
                    ("phase", "transfer_load_error".to_string()),
                    ("request_id", id.to_string()),
                    ("session_id", client_session_id.to_string()),
                    ("error", crate::util::format_error_chain(&error)),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Failed to load session for transfer: {error}"),
                retry_after_secs: None,
            });
            return;
        }
    };

    let provider = {
        let agent_guard = agent.lock().await;
        agent_guard.provider_fork()
    };

    let transfer_compaction = match crate::compaction::build_transfer_compaction_state(
        provider,
        transfer_active_messages(&parent),
        parent.compaction().map(|state| (*state).clone()),
    )
    .await
    {
        Ok(compaction) => compaction,
        Err(error) => {
            crate::logging::event_warn(
                "SESSION_LIFECYCLE",
                vec![
                    ("phase", "transfer_compaction_error".to_string()),
                    ("request_id", id.to_string()),
                    ("session_id", client_session_id.to_string()),
                    ("error", crate::util::format_error_chain(&error)),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Failed to compact session for transfer: {error}"),
                retry_after_secs: None,
            });
            return;
        }
    };

    let (new_session_id, new_session_name) = match create_transfer_child_session(
        client_session_id,
        &parent,
        transfer_compaction.map(Arc::new),
    ) {
        Ok(result) => result,
        Err(error) => {
            crate::logging::event_warn(
                "SESSION_LIFECYCLE",
                vec![
                    ("phase", "transfer_create_error".to_string()),
                    ("request_id", id.to_string()),
                    ("session_id", client_session_id.to_string()),
                    ("error", crate::util::format_error_chain(&error)),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Failed to create transfer session: {error}"),
                retry_after_secs: None,
            });
            return;
        }
    };
    crate::logging::event_info(
        "SESSION_LIFECYCLE",
        vec![
            ("phase", "transfer_done".to_string()),
            ("request_id", id.to_string()),
            ("session_id", client_session_id.to_string()),
            ("new_session_id", new_session_id.clone()),
            ("elapsed_ms", started.elapsed().as_millis().to_string()),
        ],
    );

    let _ = client_event_tx.send(ServerEvent::SplitResponse {
        id,
        new_session_id,
        new_session_name,
    });
}

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;

/// Decide whether an idle live session still owes the model a continuation.
///
/// This is the live-session analog of `restored_session_was_interrupted`: a
/// session "would continue if resumed" when it has a pending reload-recovery
/// directive, when it carries reload-interruption markers, or when its last
/// model-visible message is a user/tool turn the assistant never answered
/// (e.g. the turn errored or the process was interrupted mid-generation).
fn live_session_owes_continuation(agent: &Agent) -> bool {
    // Never continue an empty/fresh session.
    if agent.visible_conversation_message_count() == 0 {
        return false;
    }

    if super::super::reload_recovery::peek_for_session(agent.session_id())
        .ok()
        .flatten()
        .map(|record| record.status == super::super::reload_recovery::ReloadRecoveryStatus::Pending)
        .unwrap_or(false)
    {
        return true;
    }

    if super::super::client_session::session_was_interrupted_by_reload(agent) {
        return true;
    }

    matches!(
        agent.last_visible_conversation_role(),
        Some(crate::message::Role::User)
    )
}

/// Continue every live, idle session that would auto-resume on a reload.
///
/// This is the on-demand equivalent of the post-reload recovery sweep: it walks
/// the currently-live sessions, and for each idle one that still owes the model
/// a continuation, injects the standard "continue where you left off" reminder
/// so the session picks back up without the user having to open each one.
#[expect(
    clippy::too_many_arguments,
    reason = "resuming live sessions needs session, swarm membership, and status event state"
)]
pub(super) async fn handle_resume_all_sessions(
    id: u64,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    _swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    _event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    _event_counter: &Arc<std::sync::atomic::AtomicU64>,
    _swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    // Snapshot live sessions (those with at least one live client attachment).
    let live_session_ids: Vec<String> = {
        let members = swarm_members.read().await;
        members
            .iter()
            .filter(|(_, member)| !member.event_txs.is_empty() || !member.event_tx.is_closed())
            .map(|(session_id, _)| session_id.clone())
            .collect()
    };

    let mut resumed_sessions: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for session_id in live_session_ids {
        let agent = {
            let guard = sessions.snapshot().await;
            guard.get(&session_id).cloned()
        };
        let Some(agent) = agent else {
            continue;
        };

        // Only act on idle sessions; a busy session is already making progress.
        // The owned guard doubles as the turn reservation (#1152).
        let Ok(agent_guard) = Arc::clone(&agent).try_lock_owned() else {
            skipped += 1;
            continue;
        };

        if !live_session_owes_continuation(&agent_guard) {
            drop(agent_guard);
            skipped += 1;
            continue;
        }

        let reminder =
            match super::super::reload_recovery::pending_directive_for_session(&session_id) {
                Ok(Some(directive)) => directive.continuation_message,
                _ => {
                    crate::tool::selfdev::ReloadContext::interrupted_session_continuation_message()
                }
            };
        let display_name = agent_guard
            .session_short_name()
            .map(str::to_string)
            .unwrap_or_else(|| session_id[..8.min(session_id.len())].to_string());

        // Manual resume submits intent, just like reload capture. It must not
        // create a competing client-owned continuation path.
        if super::super::reload_recovery::peek_for_session(&session_id)
            .ok()
            .flatten()
            .is_none()
        {
            let reload_id = format!("manual-resume-{}", uuid::Uuid::new_v4());
            if let Err(error) = super::super::reload_recovery::persist_intent(
                &reload_id,
                &session_id,
                super::super::reload_recovery::ReloadRecoveryRole::InterruptedPeer,
                crate::tool::selfdev::ReloadRecoveryDirective {
                    continuation_message: reminder,
                    reconnect_notice: None,
                },
                "explicit resume-all request",
            ) {
                crate::logging::warn(&format!(
                    "Cannot schedule manual recovery for {session_id}: {error}"
                ));
                skipped += 1;
                continue;
            }
        }
        drop(agent_guard);

        resumed_sessions.push(display_name);
    }

    let resumed = resumed_sessions.len();
    let message = if resumed == 0 {
        "No interrupted sessions to resume. All live sessions are idle or already complete."
            .to_string()
    } else if resumed == 1 {
        format!("Resuming 1 interrupted session: {}.", resumed_sessions[0])
    } else {
        format!(
            "Resuming {} interrupted sessions: {}.",
            resumed,
            resumed_sessions.join(", ")
        )
    };

    crate::logging::info(&format!(
        "resume_all_sessions: resumed={} skipped={} sessions={:?}",
        resumed, skipped, resumed_sessions
    ));

    let _ = client_event_tx.send(ServerEvent::ResumeAllResult {
        id,
        resumed,
        skipped,
        resumed_sessions,
        message,
    });
}

fn handle_compact(
    id: u64,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    // An active turn can hold the agent for longer than the client's RPC
    // deadline. Acknowledge queuing, not the eventual compaction outcome.
    let _ = client_event_tx.send(ServerEvent::CompactResult {
        id,
        message: "Context compaction queued.".into(),
        success: true,
    });
    let agent = Arc::clone(agent);
    let tx = client_event_tx.clone();
    crate::runtime_activity::spawn(async move {
        let mut agent_guard = agent.lock().await;
        let session_id = agent_guard.session_id().to_string();
        let (message, success) = agent_guard.request_manual_compaction();
        // Send start before releasing the agent, so completion cannot overtake it.
        if success {
            let _ = tx.send(ServerEvent::CompactionStarted {
                trigger: "manual".into(),
                pre_tokens: 0,
            });
        }
        drop(agent_guard);

        if success {
            crate::runtime_memory_log::emit_event(
                crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                    "manual_compaction_requested",
                    "manual_compaction_started",
                )
                .with_session_id(session_id)
                .force_attribution(),
            );
        } else {
            let _ = tx.send(ServerEvent::CompactResult {
                id: 0,
                message,
                success: false,
            });
        }
    });
}

async fn handle_stdin_response(
    id: u64,
    request_id: String,
    input: String,
    stdin_responses: &Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let accepted = match stdin_responses.lock().await.remove(&request_id) {
        Some(tx) => tx.send(input).is_ok(),
        None => false,
    };
    let event = if accepted {
        ServerEvent::Done { id }
    } else {
        ServerEvent::Error {
            id,
            message: "The input request is no longer pending on this connection; your answer was not accepted".into(),
            retry_after_secs: None,
        }
    };
    let _ = client_event_tx.send(event);
}

struct AgentTaskContext<'a> {
    pub(super) client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
    pub(super) swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub(super) swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub(super) event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub(super) event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    pub(super) swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
}

async fn handle_agent_task(
    id: u64,
    task: String,
    client_session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    ctx: &AgentTaskContext<'_>,
) {
    let mut agent = agent.lock().await;
    let lease = match agent.claim_server_turn() {
        Ok(lease) => lease,
        Err(error) => {
            let _ = ctx.client_event_tx.send(ServerEvent::Error {
                id,
                message: error.to_string(),
                retry_after_secs: None,
            });
            return;
        }
    };
    let events = super::super::state::TurnEventChannel::new(
        client_session_id.into(),
        Arc::clone(ctx.swarm_members),
        ctx.client_event_tx.clone(),
        &lease,
    );
    update_member_status(
        client_session_id,
        "running",
        Some(truncate_detail(&task, 120)),
        ctx.swarm_members,
        ctx.swarms_by_id,
        Some(ctx.event_history),
        Some(ctx.event_counter),
        Some(ctx.swarm_event_tx),
    )
    .await;

    let result = process_reserved_message_streaming_mpsc(
        &mut agent,
        &lease,
        &task,
        vec![],
        None,
        events.sender(),
        None,
        None,
    )
    .await;
    match result {
        Ok(()) => {
            update_member_status(
                client_session_id,
                "completed",
                None,
                ctx.swarm_members,
                ctx.swarms_by_id,
                Some(ctx.event_history),
                Some(ctx.event_counter),
                Some(ctx.swarm_event_tx),
            )
            .await;
            let _ = events.sender().send(ServerEvent::Done { id });
        }
        Err(e) => {
            update_member_status(
                client_session_id,
                "failed",
                Some(truncate_detail(&e.to_string(), 120)),
                ctx.swarm_members,
                ctx.swarms_by_id,
                Some(ctx.event_history),
                Some(ctx.event_counter),
                Some(ctx.swarm_event_tx),
            )
            .await;
            let retry_after_secs = e
                .downcast_ref::<StreamError>()
                .and_then(|stream_error| stream_error.retry_after_secs);
            let _ = events.sender().send(ServerEvent::Error {
                id,
                message: crate::util::format_error_chain(&e),
                retry_after_secs,
            });
        }
    }
    drop(agent);
    events.finish().await;
}

/// What the session actions need of their connection, borrowed for one
/// request. The handlers in this module are reached only through `dispatch`.
pub(super) struct ActionCx<'a> {
    env: &'a ClientEnv,
    client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
    agent: &'a Arc<Mutex<Agent>>,
    client_session_id: &'a str,
    client_is_processing: bool,
    stdin_responses: &'a Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>>,
    local_jcode_peer: bool,
    connection_client_name: Option<&'a str>,
    friendly_name: &'a Option<String>,
    swarm_enabled: &'a mut bool,
}

impl<'a> ActionCx<'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one borrow per piece of connection state an action may touch"
    )]
    pub(super) fn new(
        env: &'a ClientEnv,
        client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
        agent: &'a Arc<Mutex<Agent>>,
        client_session_id: &'a str,
        client_is_processing: bool,
        stdin_responses: &'a Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>>,
        local_jcode_peer: bool,
        connection_client_name: Option<&'a str>,
        friendly_name: &'a Option<String>,
        swarm_enabled: &'a mut bool,
    ) -> Self {
        Self {
            env,
            client_event_tx,
            agent,
            client_session_id,
            client_is_processing,
            stdin_responses,
            local_jcode_peer,
            connection_client_name,
            friendly_name,
            swarm_enabled,
        }
    }

    pub(super) async fn dispatch(&mut self, request: Request) -> Result<Outcome> {
        match request {
            Request::Split { id } => {
                handle_split(
                    id,
                    &self.client_session_id,
                    &self.agent,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::SplitSideQuestion { id } => {
                let event = session_transfer_snapshot::handle_split_side_question(
                    id,
                    &self.client_session_id,
                    &self.agent,
                );
                send_client_event_best_effort(
                    &self.client_event_tx,
                    event,
                    "client_lifecycle: SplitSideQuestion",
                );
            }
            Request::RoutingView { id } => {
                let event = super::super::routing_view_request::reply(id).await;
                send_client_event_best_effort(
                    &self.client_event_tx,
                    event,
                    "client_lifecycle: routing view",
                );
            }
            request @ (Request::ListSideQuestions { .. }
            | Request::GetSideQuestionHistory { .. }
            | Request::PromoteSideQuestion { .. }) => {
                let event = super::super::client_lightweight_control::side_question_control::handle_side_question_control(request).await;
                send_client_event_best_effort(
                    &self.client_event_tx,
                    event,
                    "client_lifecycle: side question read",
                );
            }
            Request::Transfer { id } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "transfer",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_transfer(
                    id,
                    &self.client_session_id,
                    &self.agent,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::Compact { id } => {
                handle_compact(id, &self.agent, &self.client_event_tx);
            }
            Request::TriggerMemoryExtraction { id } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "trigger_memory_extraction",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_trigger_memory_extraction(id, &self.agent, &self.client_event_tx).await;
            }

            // Agent-to-agent communication
            Request::AgentRegister { id, .. } => {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Done { id },
                    "client_lifecycle: Done",
                );
            }
            Request::StdinResponse {
                id,
                request_id,
                input,
            } => {
                if crate::tool::interactive::kind_of(&request_id)
                    .is_some_and(crate::protocol::AskKind::is_human_approval)
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: "An approval requires an interactive structured response"
                                .into(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: owner approval",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_stdin_response(
                    id,
                    request_id,
                    input,
                    &self.stdin_responses,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::AskUserResponse {
                id,
                request_id,
                response,
            } => {
                if crate::tool::interactive::kind_of(&request_id)
                    .is_some_and(crate::protocol::AskKind::is_owner_approval)
                    && !(self.local_jcode_peer && self.connection_client_name == Some("jcode-tui"))
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: "This client cannot answer owner approvals".into(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: owner approval",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                let input = match serde_json::to_string(&response) {
                    Ok(input) => input,
                    Err(error) => {
                        if let Err(delivery_error) = self.client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Cannot encode ask response: {error}"),
                            retry_after_secs: None,
                        }) {
                            crate::logging::debug(&format!(
                                "Ask response error receiver closed: {delivery_error}"
                            ));
                        }
                        return Ok(Outcome::SkipEpilogue);
                    }
                };
                handle_stdin_response(
                    id,
                    request_id,
                    input,
                    &self.stdin_responses,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::AgentTask { id, task, .. } => {
                handle_agent_task(
                    id,
                    task,
                    &self.client_session_id,
                    &self.agent,
                    &AgentTaskContext {
                        client_event_tx: &self.client_event_tx,
                        swarm_members: &self.env.swarm_members,
                        swarms_by_id: &self.env.swarms_by_id,
                        event_history: &self.env.event_history,
                        event_counter: &self.env.event_counter,
                        swarm_event_tx: &self.env.swarm_event_tx,
                    },
                )
                .await;
            }
            Request::AgentCapabilities { id } => {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Done { id },
                    "client_lifecycle: Done",
                );
            }
            Request::AgentContext { id } => {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Done { id },
                    "client_lifecycle: Done",
                );
            }
            Request::NotifySession {
                id,
                session_id,
                message,
            } => {
                handle_notify_session(
                    id,
                    session_id,
                    message,
                    NotifySessionContext {
                        sessions: &self.env.sessions,
                        soft_interrupt_queues: &self.env.soft_interrupt_queues,
                        swarm_members: &self.env.swarm_members,
                        swarms_by_id: &self.env.swarms_by_id,
                        event_history: &self.env.event_history,
                        event_counter: &self.env.event_counter,
                        swarm_event_tx: &self.env.swarm_event_tx,
                        client_event_tx: &self.client_event_tx,
                    },
                )
                .await;
            }
            Request::AppletAction {
                id,
                session_id,
                instance,
                action,
                state,
                source_key,
            } => {
                handle_applet_action(
                    id,
                    session_id,
                    instance,
                    action,
                    state,
                    source_key,
                    NotifySessionContext {
                        sessions: &self.env.sessions,
                        soft_interrupt_queues: &self.env.soft_interrupt_queues,
                        swarm_members: &self.env.swarm_members,
                        swarms_by_id: &self.env.swarms_by_id,
                        event_history: &self.env.event_history,
                        event_counter: &self.env.event_counter,
                        swarm_event_tx: &self.env.swarm_event_tx,
                        client_event_tx: &self.client_event_tx,
                    },
                )
                .await;
            }
            Request::CloseApplet {
                id,
                session_id,
                instance,
            } => {
                handle_close_applet(id, session_id, instance, &self.client_event_tx);
            }
            Request::Transcript {
                id,
                text,
                mode,
                session_id,
            } => {
                match super::super::session_connections::inject_transcript(
                    id,
                    text,
                    mode,
                    session_id,
                    &self.env.client_connections,
                    &self.env.client_debug_state,
                    &self.env.swarm_members,
                )
                .await
                {
                    Ok(event) => {
                        if let Err(error) = self.client_event_tx.send(event) {
                            crate::logging::debug(&format!(
                                "Client disconnected before event delivery: {error}"
                            ));
                        }
                    }
                    Err(error) => {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message: error.to_string(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                    }
                }
            }
            Request::VoiceActivity {
                id,
                session_id,
                activity,
            } => {
                let event = super::super::session_connections::forward_voice_activity(
                    id,
                    &session_id,
                    activity,
                    &self.env.swarm_members,
                )
                .await;
                if let Err(error) = self.client_event_tx.send(event) {
                    crate::logging::debug(&format!(
                        "client disconnected before voice activity reply: {error}"
                    ));
                }
            }
            Request::InputShell { id, command } => {
                handle_input_shell(id, command, &self.agent, &self.client_event_tx);
            }

            // === Agent communication ===
            request @ (Request::CommShare { .. }
            | Request::CommRead { .. }
            | Request::CommMessage { .. }
            | Request::CommList { .. }
            | Request::CommListSessions { .. }
            | Request::CommFocusSession { .. }
            | Request::CommListSwarms { .. }
            | Request::CommSetSwarmLabel { .. }
            | Request::CommListChannels { .. }
            | Request::CommChannelMembers { .. }
            | Request::CommProposePlan { .. }
            | Request::CommApprovePlan { .. }
            | Request::CommRejectPlan { .. }
            | Request::CommSpawn { .. }
            | Request::CommListModels { .. }
            | Request::CommStop { .. }
            | Request::CommWorkerControl { .. }
            | Request::CommAssignRole { .. }
            | Request::CommSummary { .. }
            | Request::CommStatus { .. }
            | Request::CommReport { .. }
            | Request::CommPlanStatus { .. }
            | Request::CommReadContext { .. }
            | Request::CommResyncPlan { .. }
            | Request::CommAssignTask { .. }
            | Request::CommAssignNext { .. }
            | Request::CommTaskControl { .. }
            | Request::CommSubscribeChannel { .. }
            | Request::CommUnsubscribeChannel { .. }
            | Request::CommAwaitMembers { .. }) => {
                if comm_dispatch::dispatch_comm_request(
                    request,
                    &self.client_event_tx,
                    &self.env.sessions,
                    &self.env.global_session_id,
                    &self.env.provider_template,
                    &self.env.swarm_members,
                    &self.env.swarms_by_id,
                    &self.env.shared_context,
                    &self.env.swarm_plans,
                    &self.env.swarm_coordinators,
                    &self.env.channel_subscriptions,
                    &self.env.channel_subscriptions_by_session,
                    &self.env.event_history,
                    &self.env.event_counter,
                    &self.env.swarm_event_tx,
                    &self.env.mcp_pool,
                    &self.env.soft_interrupt_queues,
                    &self.env.swarm_mutation_runtime,
                    &self.env.client_connections,
                    &self.env.file_touch,
                    &self.env.await_members_runtime,
                )
                .await
                {
                    return Ok(Outcome::Close);
                }
            }
            Request::RoutingGateAnswer {
                id,
                gate_id,
                choice,
            } => {
                super::super::routing_requests::handle_gate_answer(
                    id,
                    &self.client_session_id,
                    gate_id,
                    choice,
                    &self.client_event_tx,
                );
            }
            Request::RoutingIntent { id, action } => {
                let current = self
                    .agent
                    .try_lock()
                    .ok()
                    .and_then(|guard| super::super::routing_turn::current_account(&guard));
                super::super::routing_requests::handle_intent(
                    id,
                    action,
                    &self.client_session_id,
                    current,
                    &self.client_event_tx,
                );
            }

            // These are handled via channels, not direct requests from TUI
            Request::ClientDebugCommand { id, .. } => {
                handle_client_debug_command(id, &self.client_event_tx).await;
            }
            Request::ClientDebugResponse { id, output } => {
                handle_client_debug_response(id, output, &self.env.client_debug_response_tx);
            }
            Request::ResumeAllSessions { id } => {
                handle_resume_all_sessions(
                    id,
                    &self.env.sessions,
                    &self.env.swarm_members,
                    &self.env.swarms_by_id,
                    &self.env.event_history,
                    &self.env.event_counter,
                    &self.env.swarm_event_tx,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::SetAgentModel {
                id,
                target,
                model,
                global,
            } => {
                let result = if global {
                    set_global_agent_model(&self.agent, &self.client_session_id, &target, model)
                        .await
                } else {
                    set_session_agent_model(&self.agent, &self.client_session_id, &target, model)
                        .await
                };
                let event = match result {
                    Ok(overrides) => ServerEvent::AgentModelsChanged {
                        id,
                        session_id: self.client_session_id.to_string(),
                        overrides,
                    },
                    Err(error) => ServerEvent::Error {
                        id,
                        message: error.to_string(),
                        retry_after_secs: None,
                    },
                };
                if let Err(error) = self.client_event_tx.send(event) {
                    crate::logging::debug(&format!(
                        "Client disconnected before event delivery: {error}"
                    ));
                }
            }
            Request::SetSubagentModel { id, model } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "set_subagent_model",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_set_subagent_model(id, model, &self.agent, &self.client_event_tx).await;
            }
            Request::RunSubagent {
                id,
                prompt,
                subagent_type,
                model,
                session_id,
            } => {
                handle_run_subagent(
                    id,
                    prompt,
                    subagent_type,
                    model,
                    session_id,
                    &self.agent,
                    &self.client_event_tx,
                );
            }
            Request::SetSessionSaved { id, saved, label } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "set_session_saved",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_set_session_saved(
                    id,
                    saved,
                    label,
                    &self.agent,
                    &self.client_session_id,
                    &self.env.swarm_members,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::RenameSession { id, title } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "rename_session",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_rename_session(
                    id,
                    title,
                    &self.agent,
                    &self.client_session_id,
                    &self.env.swarm_members,
                    &self.client_event_tx,
                )
                .await;
            }
            Request::SetFeature {
                id,
                feature,
                enabled,
            } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "set_feature",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                handle_set_feature(
                    id,
                    feature,
                    enabled,
                    &self.agent,
                    &self.client_session_id,
                    &self.friendly_name,
                    self.swarm_enabled,
                    &self.env.swarm_members,
                    &self.env.swarms_by_id,
                    &self.env.swarm_coordinators,
                    &self.env.channel_subscriptions,
                    &self.env.channel_subscriptions_by_session,
                    &self.env.swarm_plans,
                    &self.client_event_tx,
                )
                .await;
            }

            other => unreachable!("request {other:?} is not a actions request"),
        }
        Ok(Outcome::Next)
    }
}
