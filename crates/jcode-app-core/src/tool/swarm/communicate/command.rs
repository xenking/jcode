//! The swarm tool's wire input and the typed command it parses into.
//!
//! [`CommunicateInput`] is the flat, model-facing JSON shape: every action's
//! fields live side by side and any may be missing. Nothing outside this module
//! reads those fields. [`CommunicateInput::into_command`] applies the aliases,
//! synonyms and per-action requirements once and yields a [`Command`] whose
//! variants carry exactly the validated data their action needs.

use super::{
    canonical_swarm_action, default_await_target_statuses, default_cleanup_target_statuses,
    non_blank, status_target_sessions,
};
use crate::plan::PlanItem;
use crate::protocol::{CommDeliveryMode, SwarmWorkerControl};
use anyhow::Result;
use jcode_message_types::SpeedMode;
use jcode_swarm_core::validate_swarm_tldr;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub(super) struct CommunicateInput {
    pub(super) action: String,
    #[serde(default)]
    pub(super) key: Option<String>,
    #[serde(default)]
    pub(super) value: Option<String>,
    #[serde(default)]
    pub(super) message: Option<String>,
    #[serde(default)]
    pub(super) to_session: Option<String>,
    /// Cross-swarm DM target: swarm label or id (see list_swarms).
    #[serde(default)]
    pub(super) to_swarm: Option<String>,
    #[serde(default)]
    pub(super) channel: Option<String>,
    #[serde(default)]
    pub(super) proposer_session: Option<String>,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) target_session: Option<String>,
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) working_dir: Option<String>,
    #[serde(default)]
    pub(super) initial_message: Option<String>,
    #[serde(default)]
    pub(super) prompt: Option<String>,
    #[serde(default)]
    pub(super) limit: Option<usize>,
    #[serde(default)]
    pub(super) task_id: Option<String>,
    #[serde(default)]
    pub(super) spawn_if_needed: Option<bool>,
    #[serde(default)]
    pub(super) prefer_spawn: Option<bool>,
    #[serde(default)]
    pub(super) plan_items: Option<Vec<PlanItem>>,
    #[serde(default)]
    pub(super) target_status: Option<Vec<String>>,
    #[serde(default)]
    pub(super) session_ids: Option<Vec<String>>,
    #[serde(default)]
    pub(super) mode: Option<String>,
    #[serde(default)]
    pub(super) timeout_minutes: Option<u64>,
    #[serde(default)]
    pub(super) wake: Option<bool>,
    #[serde(default)]
    pub(super) background: Option<bool>,
    #[serde(default)]
    pub(super) notify: Option<bool>,
    #[serde(default)]
    pub(super) delivery: Option<CommDeliveryMode>,
    #[serde(default)]
    pub(super) force: Option<bool>,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) validation: Option<String>,
    #[serde(default)]
    pub(super) follow_up: Option<String>,
    #[serde(default)]
    pub(super) spawn_mode: Option<String>,
    #[serde(default)]
    pub(super) speed: Option<SpeedMode>,
    /// Spawn: agent profile for the worker (see `agents.profiles`).
    #[serde(default)]
    pub(super) profile: Option<String>,
    /// One-line summary shown collapsed in the recipient's UI for long
    /// message/report bodies. Required when the body exceeds the collapse
    /// threshold.
    #[serde(default)]
    pub(super) tldr: Option<String>,
    /// Optional model override, authorized by the user's opt-in and exact allowlist.
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) effort: Option<String>,
    /// Short human-readable label for a spawned agent shown in swarm UI.
    /// Required and nonblank for the explicit `spawn` action.
    #[serde(default)]
    pub(super) label: Option<String>,
    /// Phase (todo `stage`) a spawned worker's lane joins. Defaults to the
    /// active phase.
    #[serde(default, alias = "stage")]
    pub(super) phase: Option<u32>,
}

/// How a message is delivered, shared by every messaging action.
pub(super) struct Delivery {
    pub(super) wake: Option<bool>,
    pub(super) mode: Option<CommDeliveryMode>,
    pub(super) tldr: Option<String>,
}

/// Fields that pick the worker an assignment spawns when no agent is free.
pub(super) struct WorkerSpawn {
    pub(super) working_dir: Option<String>,
    pub(super) spawn_mode: Option<String>,
    pub(super) model: Option<String>,
    pub(super) profile: Option<String>,
}

/// The lane-owning `spawn` action.
pub(super) struct SpawnLane {
    pub(super) label: String,
    pub(super) prompt: Option<String>,
    pub(super) phase: Option<u32>,
    pub(super) working_dir: Option<String>,
    pub(super) spawn_mode: Option<String>,
    pub(super) model: Option<String>,
    pub(super) speed: Option<SpeedMode>,
    pub(super) profile: Option<String>,
}

pub(super) struct AssignTask {
    pub(super) target_session: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) message: Option<String>,
    pub(super) spawn_if_needed: bool,
    pub(super) prefer_spawn: bool,
    pub(super) spawn: WorkerSpawn,
}

pub(super) struct AssignNext {
    pub(super) target_session: Option<String>,
    pub(super) working_dir: Option<String>,
    pub(super) prefer_spawn: Option<bool>,
    pub(super) spawn_if_needed: Option<bool>,
    pub(super) message: Option<String>,
    pub(super) model: Option<String>,
}

pub(super) struct TaskControl {
    /// The action as the caller named it, for user-facing text.
    pub(super) action: String,
    /// The action as the server knows it (`start_task` is `start`).
    pub(super) control_action: String,
    pub(super) task_id: String,
    pub(super) target_session: Option<String>,
    pub(super) message: Option<String>,
}

pub(super) struct AwaitMembers {
    pub(super) target_status: Vec<String>,
    pub(super) session_ids: Vec<String>,
    pub(super) mode: Option<String>,
    pub(super) timeout_secs: u64,
    pub(super) blocking_was_requested: bool,
    pub(super) notify: bool,
    pub(super) wake: bool,
}

pub(super) struct Report {
    pub(super) status: Option<String>,
    pub(super) message: String,
    pub(super) validation: Option<String>,
    pub(super) follow_up: Option<String>,
    pub(super) tldr: Option<String>,
}

pub(super) struct Cleanup {
    pub(super) target_status: Vec<String>,
    pub(super) session_ids: Vec<String>,
    pub(super) force: bool,
}

pub(super) enum Command {
    Share {
        key: String,
        value: String,
        append: bool,
    },
    Read {
        key: Option<String>,
    },
    Message {
        message: String,
        to_session: Option<String>,
        channel: Option<String>,
        to_swarm: Option<String>,
        delivery: Delivery,
    },
    Broadcast {
        message: String,
        delivery: Delivery,
    },
    ChannelMessage {
        message: String,
        channel: String,
        delivery: Delivery,
    },
    List,
    Sessions,
    Focus {
        target: String,
    },
    ListSwarms,
    SetSwarmLabel {
        label: String,
    },
    ListChannels,
    ChannelMembers {
        channel: String,
    },
    ProposePlan {
        items: Vec<PlanItem>,
    },
    ApprovePlan {
        proposer: String,
    },
    RejectPlan {
        proposer: String,
        reason: Option<String>,
    },
    Spawn(SpawnLane),
    ListModels,
    WorkerControl {
        target_session: String,
        control: SwarmWorkerControl,
    },
    StopOne {
        target: String,
        force: Option<bool>,
    },
    StopMany {
        targets: Vec<String>,
        force: bool,
    },
    Cleanup(Cleanup),
    AssignRole {
        target: String,
        role: String,
    },
    Status {
        targets: Vec<String>,
    },
    Report(Report),
    PlanStatus,
    Summary {
        target: String,
        limit: Option<usize>,
    },
    ReadContext {
        target: String,
    },
    ResyncPlan,
    AssignTask(AssignTask),
    AssignNext(AssignNext),
    TaskControl(TaskControl),
    SubscribeChannel {
        channel: String,
    },
    UnsubscribeChannel {
        channel: String,
    },
    AwaitMembers(AwaitMembers),
}

impl CommunicateInput {
    /// The model override this call asks for, for policy checks that run
    /// before the call is interpreted.
    pub(super) fn requested_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(super) fn spawn_initial_message(&self) -> Option<String> {
        self.initial_message
            .as_ref()
            .filter(|message| !message.trim().is_empty())
            .cloned()
            .or_else(|| {
                self.prompt
                    .as_ref()
                    .filter(|prompt| !prompt.trim().is_empty())
                    .cloned()
            })
    }

    pub(super) fn required_spawn_label(&self) -> Result<String> {
        let label = self
            .label
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("'label' is required for spawn action"))?
            .trim();
        if label.is_empty() {
            return Err(anyhow::anyhow!(
                "'label' must not be blank for spawn action"
            ));
        }
        Ok(label.to_string())
    }

    fn required_message(&mut self, action: &str) -> Result<String> {
        self.message
            .take()
            .ok_or_else(|| anyhow::anyhow!("'message' is required for {action} action"))
    }

    fn delivery(&mut self, message: &str, context: &str) -> Result<Delivery> {
        let tldr = validate_swarm_tldr(self.tldr.as_deref(), message, context)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(Delivery {
            wake: self.wake,
            mode: self.delivery,
            tldr,
        })
    }

    fn required_channel(&mut self, action: &str) -> Result<String> {
        self.channel
            .take()
            .ok_or_else(|| anyhow::anyhow!("'channel' is required for {action} action"))
    }

    fn worker_spawn(&mut self) -> WorkerSpawn {
        WorkerSpawn {
            working_dir: self.working_dir.take(),
            spawn_mode: self.spawn_mode.take(),
            model: self.model.take(),
            profile: self.profile.take(),
        }
    }

    /// Interpret the call for `current_session`.
    ///
    /// `to_session` and `target_session` both name a single session id.
    /// Historically different actions required different field names, which
    /// models frequently confuse, so the two are interchangeable aliases for
    /// any action that targets a session. Near-miss verbs (`inbox`, `send`,
    /// `msg`) map to the real action instead of erroring out.
    pub(super) fn into_command(mut self, current_session: &str) -> Result<Command> {
        match (self.to_session.is_some(), self.target_session.is_some()) {
            (true, false) => self.target_session = self.to_session.clone(),
            (false, true) => self.to_session = self.target_session.clone(),
            _ => {}
        }
        let action = canonical_swarm_action(&self.action).to_string();

        Ok(match action.as_str() {
            "share" | "share_append" => {
                let key = self
                    .key
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("'key' is required for share action"))?;
                let value = self
                    .value
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("'value' is required for share action"))?;
                Command::Share {
                    key,
                    value,
                    append: action == "share_append",
                }
            }
            "read" => Command::Read {
                key: self.key.take(),
            },
            "message" => {
                let message = self.required_message("message")?;
                let delivery = self.delivery(&message, "this message")?;
                Command::Message {
                    message,
                    to_session: self.to_session.take(),
                    channel: self.channel.take(),
                    to_swarm: non_blank(self.to_swarm.take()),
                    delivery,
                }
            }
            "broadcast" => {
                // `broadcast` targets the sender's spawned subtree (the swarm
                // coordinator reaches the whole swarm). Any `to_session`/
                // `channel` is intentionally ignored so the action stays an
                // unambiguous group send; use `message`/`channel` to target.
                // Prefer DMs; group sends are for rare coordination moments,
                // not routine status updates.
                let message = self.required_message("broadcast")?;
                let delivery = self.delivery(&message, "this broadcast")?;
                Command::Broadcast { message, delivery }
            }
            "channel" => {
                let message = self.required_message("channel")?;
                let delivery = self.delivery(&message, "this channel message")?;
                let channel = self.required_channel("channel")?;
                Command::ChannelMessage {
                    message,
                    channel,
                    delivery,
                }
            }
            "list" => Command::List,
            "sessions" => Command::Sessions,
            "focus" => Command::Focus {
                target: self
                    .to_session
                    .take()
                    .or_else(|| self.target_session.take())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "'to_session' (or 'target_session') is required for focus action"
                        )
                    })?,
            },
            "list_swarms" => Command::ListSwarms,
            "set_swarm_label" => Command::SetSwarmLabel {
                label: self
                    .label
                    .take()
                    .or_else(|| self.value.take())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "'label' is required for set_swarm_label (empty string clears it)"
                        )
                    })?,
            },
            "list_channels" => Command::ListChannels,
            "channel_members" => Command::ChannelMembers {
                channel: self.required_channel("channel_members")?,
            },
            "propose_plan" => {
                let items = self.plan_items.take().ok_or_else(|| {
                    anyhow::anyhow!("'plan_items' is required for propose_plan action")
                })?;
                if items.is_empty() {
                    return Err(anyhow::anyhow!(
                        "'plan_items' must include at least one item"
                    ));
                }
                Command::ProposePlan { items }
            }
            "approve_plan" => Command::ApprovePlan {
                proposer: self.proposer_session.take().ok_or_else(|| {
                    anyhow::anyhow!("'proposer_session' is required for approve_plan action")
                })?,
            },
            "reject_plan" => Command::RejectPlan {
                proposer: self.proposer_session.take().ok_or_else(|| {
                    anyhow::anyhow!("'proposer_session' is required for reject_plan action")
                })?,
                reason: self.reason.take(),
            },
            "spawn" => {
                let label = self.required_spawn_label()?;
                Command::Spawn(SpawnLane {
                    prompt: self.spawn_initial_message(),
                    label,
                    phase: self.phase,
                    working_dir: self.working_dir.take(),
                    spawn_mode: self.spawn_mode.take(),
                    model: self.model.take(),
                    speed: self.speed,
                    profile: self.profile.take(),
                })
            }
            "list_models" => Command::ListModels,
            control @ ("set_model" | "compact") => {
                let target_session = self
                    .to_session
                    .take()
                    .filter(|target| !target.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("'to_session' is required for {control} action")
                    })?;
                let control = if control == "set_model" {
                    let model = self
                        .model
                        .take()
                        .filter(|model| !model.trim().is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!("'model' is required for set_model action")
                        })?;
                    SwarmWorkerControl::SetModel {
                        model,
                        effort: self.effort.take(),
                    }
                } else {
                    SwarmWorkerControl::Compact
                };
                Command::WorkerControl {
                    target_session,
                    control,
                }
            }
            "stop" => match self.target_session.take() {
                Some(target) => Command::StopOne {
                    target,
                    force: self.force,
                },
                None => {
                    let targets = self.session_ids.take().unwrap_or_default();
                    if targets.is_empty() {
                        anyhow::bail!("stop requires to_session or a nonempty session_ids list");
                    }
                    Command::StopMany {
                        targets,
                        force: self.force.unwrap_or(false),
                    }
                }
            },
            "cleanup" => Command::Cleanup(Cleanup {
                target_status: self
                    .target_status
                    .take()
                    .unwrap_or_else(default_cleanup_target_statuses),
                session_ids: self.session_ids.take().unwrap_or_default(),
                force: self.force.unwrap_or(false),
            }),
            "assign_role" => {
                let target = self.target_session.take().ok_or_else(|| {
                    anyhow::anyhow!("'target_session' is required for assign_role action")
                })?;
                let role = self
                    .role
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("'role' is required for assign_role action"))?;
                Command::AssignRole {
                    // "current" names the caller's own session.
                    target: if target == "current" {
                        current_session.to_string()
                    } else {
                        target
                    },
                    role,
                }
            }
            "status" => Command::Status {
                targets: status_target_sessions(
                    self.target_session.take(),
                    self.session_ids.as_deref(),
                    current_session,
                ),
            },
            // Compatibility for old transcripts and scripts only. Worker output is
            // collected automatically, so this action is not advertised to models.
            "report" => {
                let message = self.required_message("report")?;
                let tldr = validate_swarm_tldr(self.tldr.as_deref(), &message, "this report")
                    .map_err(|e| anyhow::anyhow!(e))?;
                Command::Report(Report {
                    status: self.status.take(),
                    message,
                    validation: self.validation.take(),
                    follow_up: self.follow_up.take(),
                    tldr,
                })
            }
            "plan_status" => Command::PlanStatus,
            "summary" => {
                if self.mode.is_some() || self.session_ids.is_some() || self.to_swarm.is_some() {
                    anyhow::bail!(
                        "`summary` does not aggregate. Use `list` for swarm members or `plan_status` for plan progress. For one agent, pass `to_session` with its exact ID from `list`, without `mode`, `session_ids`, or `to_swarm`."
                    );
                }
                let target = self.to_session.as_deref().map(str::trim).filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!(r#"`summary` requires one `to_session` ID from `list`, e.g. {{"action":"summary","to_session":"session_123"}}. Use `list` or `plan_status` for the whole swarm."#))?
                    .to_owned();
                Command::Summary {
                    target,
                    limit: self.limit,
                }
            }
            "read_context" => Command::ReadContext {
                target: self.target_session.take().ok_or_else(|| {
                    anyhow::anyhow!("'target_session' is required for read_context action")
                })?,
            },
            "resync_plan" => Command::ResyncPlan,
            "assign_task" => Command::AssignTask(AssignTask {
                target_session: self.target_session.take(),
                task_id: self.task_id.take(),
                message: self.message.take(),
                spawn_if_needed: self.spawn_if_needed.unwrap_or(false),
                prefer_spawn: self.prefer_spawn.unwrap_or(false),
                spawn: self.worker_spawn(),
            }),
            "assign_next" => Command::AssignNext(AssignNext {
                target_session: self.target_session.take(),
                working_dir: self.working_dir.take(),
                prefer_spawn: self.prefer_spawn,
                spawn_if_needed: self.spawn_if_needed,
                message: self.message.take(),
                model: self.model.take(),
            }),
            "launch" | "run_plan" | "fill_slots" => {
                return Err(anyhow::anyhow!(
                    "'{}' was removed. Mark a todo lane `owner: \"worker\"` (todo phases[].lanes[].owner, \
                     or update lane_owner) and the server spawns a worker for it as soon as its \
                     dependencies are met. Use `plan_status` to watch lanes.",
                    action
                ));
            }
            "start" | "start_task" | "wake" | "resume" | "retry" | "reassign" | "replace"
            | "salvage" => {
                let task_id = match self.task_id.take() {
                    Some(task_id) => task_id,
                    None if self.target_session.is_some() => String::new(),
                    None => {
                        return Err(anyhow::anyhow!(
                            "'task_id' is required for {} action unless 'target_session' uniquely identifies the assigned task. Use `swarm list`/`swarm plan_status` to inspect assignments, or pass task_id explicitly.",
                            action
                        ));
                    }
                };
                if matches!(action.as_str(), "reassign" | "replace" | "salvage")
                    && self.target_session.is_none()
                {
                    return Err(anyhow::anyhow!(
                        "'target_session' is required for {} action",
                        action
                    ));
                }
                Command::TaskControl(TaskControl {
                    control_action: if action == "start_task" {
                        "start".to_string()
                    } else {
                        action.clone()
                    },
                    action,
                    task_id,
                    target_session: self.target_session.take(),
                    message: self.message.take(),
                })
            }
            "subscribe_channel" => Command::SubscribeChannel {
                channel: self.required_channel("subscribe_channel")?,
            },
            "unsubscribe_channel" => Command::UnsubscribeChannel {
                channel: self.required_channel("unsubscribe_channel")?,
            },
            "await_members" => {
                let target_status = self
                    .target_status
                    .take()
                    .unwrap_or_else(default_await_target_statuses);
                let mut session_ids = self.session_ids.take().unwrap_or_default();
                if let Some(target_session) = self.target_session.take()
                    && !session_ids.iter().any(|id| id == &target_session)
                {
                    session_ids.push(target_session);
                }
                Command::AwaitMembers(AwaitMembers {
                    target_status,
                    session_ids,
                    mode: self.mode.take(),
                    timeout_secs: self.timeout_minutes.unwrap_or(60) * 60,
                    // Public member waits are always asynchronous. The blocking
                    // CommAwaitMembers protocol remains available internally, but
                    // agents must not park an entire turn waiting on a worker or
                    // a long-lived socket.
                    blocking_was_requested: self.background == Some(false),
                    notify: self.notify.unwrap_or(true),
                    wake: self.wake.unwrap_or(true),
                })
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unknown action '{}'. Valid actions: share, share_append, read, message, broadcast, dm, channel, list, list_channels, channel_members, \
                     propose_plan, approve_plan, reject_plan, spawn, stop, assign_role, status, plan_status, summary, read_context, \
                     resync_plan, assign_task, assign_next, cleanup, start, start_task, wake, resume, retry, reassign, replace, salvage, subscribe_channel, unsubscribe_channel, await_members, set_model, compact. \
                     To read messages addressed to you, use action='read'.",
                    action
                ));
            }
        })
    }
}
