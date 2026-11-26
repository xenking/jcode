//! Legacy daemon event -> API event translation.

use super::*;

impl BridgeState {
    pub(super) fn translate_event(&mut self, event: &Value, legacy_text: bool) -> Vec<ServerFrame> {
        let history_activity = if self.turn_fence.scoped() {
            Some(self.turn_fence.active())
        } else {
            event["activity"]["is_processing"].as_bool()
        };
        let kind = event["type"].as_str().unwrap_or("");
        if kind == "runtime_command_result" {
            let Some(api_id) = self.take_simple(
                event["id"].as_u64().unwrap_or(0),
                SimpleKind::RuntimeCommand,
            ) else {
                return vec![];
            };
            let Ok(command) = serde_json::from_value(event["command"].clone()) else {
                return vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::Error {
                        code: ErrorCode::Internal,
                        message: "Invalid runtime command response".into(),
                    },
                )];
            };
            return vec![ServerFrame::reply(
                api_id,
                ApiEvent::RuntimeCommandResult {
                    session_id: self.session_id.clone().unwrap_or_default(),
                    command,
                    data: event["data"].clone(),
                    message: event["message"].as_str().unwrap_or("").into(),
                },
            )];
        }
        // Filter stale owner callbacks before updating any turn/text state.
        if kind == "tool_call"
            && (self.session_id.is_none()
                || event["session_id"].as_str() != self.session_id.as_deref())
        {
            return vec![];
        }
        let session = |state: &Self| state.session_id.clone().unwrap_or_default();
        if self.text_response_ended
            && matches!(
                kind,
                "text_delta" | "reasoning_delta" | "tool_start" | "tool_exec" | "tool_call"
            )
        {
            // A new response or execution commits the previous provider
            // attempt. Do not retract it if the next response retries before
            // producing any text (reasoning/tool output is replay-visible too).
            self.text_attempt.clear();
            self.text_response_ended = false;
        }
        if matches!(
            kind,
            "text_delta" | "reasoning_delta" | "tool_start" | "tool_exec" | "tool_call"
        ) || (kind == "connection_phase"
            && event["phase"].as_str().is_some_and(|phase| {
                matches!(
                    phase,
                    "authenticating"
                        | "connecting"
                        | "sending request"
                        | "waiting for response"
                        | "streaming"
                ) || phase.starts_with("retrying (")
            }))
        {
            self.observed_turn_active = true;
            self.activity_version += 1;
        }
        if !legacy_text
            && (matches!(
                kind,
                "text_done" | "text_replace" | "text_suffix_replace" | "retry_rollback"
            ) || kind == "text_delta"
                && event["text"].as_str().is_some_and(|text| !text.is_empty()))
        {
            return vec![];
        }
        let mut frames = match kind {
            "peer_user_message" => event["content"]
                .as_str()
                .filter(|content| !content.trim().is_empty())
                .map(|content| {
                    ServerFrame::event(ApiEvent::PeerUserMessage {
                        session_id: session(self),
                        content: content.to_owned(),
                    })
                })
                .into_iter()
                .collect(),
            "user_image_ids" => {
                let image_ids = match Self::daemon_list(event, "image_ids") {
                    Ok(ids) => ids,
                    Err(error) => return Self::invalid_daemon_event(None, error),
                };
                vec![ServerFrame::event(ApiEvent::UserImageIds {
                    session_id: self.session_id.clone().unwrap_or_default(),
                    original_content: event["original_content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    content: event["content"].as_str().unwrap_or_default().to_owned(),
                    image_ids,
                    image_ordinals: serde_json::from_value(event["image_ordinals"].clone())
                        .unwrap_or_default(),
                })]
            }
            "tools" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::Tools) else {
                    return vec![];
                };
                let reply = match serde_json::from_value(event["tools"].clone()) {
                    Ok(tools) => ApiEvent::Tools {
                        session_id: session(self),
                        tools,
                    },
                    Err(error) => ApiEvent::Error {
                        code: ErrorCode::Internal,
                        message: format!("invalid daemon tools response: {error}"),
                    },
                };
                vec![ServerFrame::reply(api_id, reply)]
            }
            "tool_call" => {
                // A custom call is an execution request, not a tool-start update.
                // Do not manufacture empty identifiers from malformed events.
                let (Some(call_id), Some(name), Some(input), Some(session_id)) = (
                    event["call_id"].as_str(),
                    event["name"].as_str(),
                    event.get("input"),
                    event["session_id"].as_str(),
                ) else {
                    return vec![];
                };
                // The callback owner may have switched attachments since this
                // event was queued. Never run another session's input using
                // the newly attached session's handler.
                if self.session_id.as_deref() != Some(session_id) {
                    return vec![];
                }
                vec![ServerFrame::event(ApiEvent::ToolCall {
                    session_id: session_id.to_string(),
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                })]
            }

            "session" => {
                let session_id = event["session_id"].as_str().unwrap_or("").to_string();
                // `session` is a broadcast lifecycle notification. The daemon
                // sends it for other live sessions too, so it cannot identify
                // which session this connection is attached to. Attachment
                // identity comes from the correlated `state` reply below.
                // Treating this broadcast as connection state made a panel for
                // session A start rejecting its own commands after session B
                // announced itself.
                vec![ServerFrame::event(ApiEvent::SessionStatus {
                    session_id,
                    status: "attached".into(),
                    pending_soft_interrupts: None,
                })]
            }
            "state" => {
                let session_id = event["session_id"].as_str().unwrap_or("").to_string();
                let id = event["id"].as_u64().unwrap_or(0);
                if let Some((state_id, api_id, requested_session)) = &self.pending_attach_id
                    && *state_id == id
                    && requested_session
                        .as_deref()
                        .is_none_or(|requested| requested == session_id)
                {
                    let api_id = *api_id;
                    self.pending_attach_id = None;
                    self.pending_attach_subscribe_id = None;
                    // `state` snapshots can also be broadcast for other live
                    // sessions. Only the reply correlated with this connection's
                    // attach request establishes its identity. Otherwise opening
                    // a second panel can retarget the first panel's bridge and
                    // make its next command fail with a wrong-session error.
                    if !session_id.is_empty() {
                        if self
                            .session_id
                            .as_ref()
                            .is_some_and(|old| old != &session_id)
                        {
                            self.available_models.clear();
                            self.available_routes.clear();
                            self.current_model = None;
                            self.current_provider = None;
                            self.current_effort = None;
                            self.current_credential = None;
                            self.current_account = None;
                            self.model_catalog_loaded = false;
                        }
                        if self.session_id.as_deref() != Some(&session_id) {
                            self.text_message_id = None;
                            self.text_attempt.clear();
                        }
                        self.session_id = Some(session_id.clone());
                    }
                    let fresh_activity =
                        self.pending_activity_snapshots.remove(&id) == Some(self.activity_version);
                    if fresh_activity {
                        self.observed_turn_active =
                            event["is_processing"].as_bool().unwrap_or(false);
                    }
                    let metadata = Self::resolve_session_metadata(&session_id);
                    // New sessions persist nothing until the first prompt.
                    let working_dir = metadata
                        .as_ref()
                        .and_then(|metadata| metadata.working_dir.clone())
                        .or_else(|| self.pending_create_dir.take())
                        .or_else(|| self.session_dirs.get(&session_id).cloned());
                    self.pending_create_dir = None;
                    if let Some(dir) = &working_dir
                        && !session_id.is_empty()
                    {
                        self.session_dirs
                            .entry(session_id.clone())
                            .or_insert_with(|| dir.clone());
                    }
                    let mut frames = vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Attached {
                            pending_ask_ids: event
                                .get("pending_ask_ids")
                                .and_then(|ids| serde_json::from_value(ids.clone()).ok()),
                            session: SessionInfo {
                                user_connected: self.user_connected_sessions.contains(&session_id),
                                edit_stats: None,
                                context: None,
                                parent_session_id: None,
                                agent_label: None,
                                swarm_status: None,
                                transcript_bytes: Self::transcript_bytes(&session_id),
                                saved: metadata.as_ref().is_some_and(|value| value.saved),
                                updated_at_ms: metadata
                                    .as_ref()
                                    .and_then(|value| value.updated_at_ms)
                                    .or_else(|| {
                                        Self::session_modified_ms(&session_id)
                                            .map(|value| value as i64)
                                    }),
                                last_active_at_ms: metadata
                                    .as_ref()
                                    .and_then(|value| value.last_active_at_ms),
                                session_id: session_id.clone(),
                                working_dir,
                                title: metadata
                                    .as_ref()
                                    .and_then(PersistedSessionMetadata::display_title),
                                status: if event["is_processing"].as_bool().unwrap_or(false) {
                                    "processing".into()
                                } else {
                                    "idle".into()
                                },
                                archived: false,
                                archived_at_ms: None,
                                save_label: metadata
                                    .as_ref()
                                    .and_then(PersistedSessionMetadata::save_label),
                            },
                        },
                    )];
                    if let Some(snapshot) = event.get("side_panel") {
                        frames.extend(Self::side_panel_frame(&session_id, snapshot));
                    }
                    if let Some(snapshot) = event.get("applets") {
                        frames.extend(Self::applet_frame(&session_id, snapshot));
                    }
                    if fresh_activity {
                        frames.push(ServerFrame::event(ApiEvent::SessionStatus {
                            session_id,
                            status: if self.observed_turn_active {
                                "running"
                            } else {
                                "idle"
                            }
                            .into(),
                            pending_soft_interrupts: event["pending_soft_interrupts"]
                                .as_u64()
                                .map(|count| count as u32),
                        }));
                    }
                    frames
                } else {
                    vec![]
                }
            }
            "text_delta" => {
                let text = event["text"].as_str().unwrap_or("").to_string();
                let message_id = if text.is_empty() {
                    self.text_message_id.clone()
                } else {
                    let id = self.text_message_id();
                    self.text_attempt.last_mut().unwrap().1.push_str(&text);
                    Some(id)
                };
                vec![ServerFrame::event(ApiEvent::TextDelta {
                    session_id: session(self),
                    text,
                    message_id,
                })]
            }
            "kv_cache_request" => {
                // Sent unconditionally immediately before complete_split, so
                // this is an actual provider-request boundary, not a status
                // heuristic. Late post-processing replacements precede it.
                let frames = self.finish_text();
                self.text_attempt.clear();
                self.text_response_ended = false;
                frames
            }
            "text_done" => self.finish_text(),
            "message_end" => {
                let frames = self.finish_text();
                // Post-processing can still replace wrapped-tool text after
                // MessageEnd. Retain it until the next response's first delta.
                self.text_response_ended = true;
                frames
            }
            "text_replace" => self.replace_text(event["text"].as_str().unwrap_or("")),
            "text_suffix_replace" => {
                match (event["old_text"].as_str(), event["new_text"].as_str()) {
                    (Some(old_text), Some(new_text)) => {
                        self.replace_text_suffix(old_text, new_text)
                    }
                    _ => vec![],
                }
            }
            "retry_rollback" => {
                let frames = self.replace_text("");
                self.text_message_id = None;
                self.text_attempt.clear();
                frames
            }
            "reasoning_delta" => vec![ServerFrame::event(ApiEvent::ReasoningDelta {
                session_id: session(self),
                text: event["text"].as_str().unwrap_or("").to_string(),
            })],
            "reasoning_done" => vec![ServerFrame::event(ApiEvent::ReasoningDone {
                session_id: session(self),
                duration_secs: event["duration_secs"].as_f64(),
            })],
            "connection_phase" => vec![ServerFrame::event(ApiEvent::ConnectionPhase {
                session_id: session(self),
                phase: event["phase"].as_str().unwrap_or("connecting").to_string(),
            })],
            "usage_limit" => vec![ServerFrame::event(ApiEvent::UsageLimit {
                session_id: session(self),
                provider: event["provider"].as_str().unwrap_or("unknown").to_string(),
                model: event["model"].as_str().unwrap_or("unknown").to_string(),
                account: event["account"].as_str().map(str::to_string),
                window: event["window"].as_str().unwrap_or("unknown").to_string(),
                reset: event["reset"].as_str().unwrap_or("unknown").to_string(),
                action: event["action"].as_str().unwrap_or("unknown").to_string(),
                target: event["target"].as_str().map(str::to_string),
                message: event["message"].as_str().unwrap_or("").to_string(),
            })],
            // Argument streaming is not an assistant-message boundary.
            "routing_decision" => vec![ServerFrame::event(ApiEvent::RoutingDecision {
                session_id: self.event_session_id(),
                account: event["account"].as_str().unwrap_or("").to_string(),
                reason: event["reason"].as_str().unwrap_or("").to_string(),
                warnings: serde_json::from_value(event["warnings"].clone()).unwrap_or_default(),
                pending: event["pending"].as_str().map(str::to_string),
            })],
            "routing_gate" => match serde_json::from_value(event["options"].clone()) {
                Ok(options) => vec![ServerFrame::event(ApiEvent::RoutingGate {
                    session_id: self.event_session_id(),
                    gate_id: event["gate_id"].as_str().unwrap_or("").to_string(),
                    reason: event["reason"].as_str().unwrap_or("").to_string(),
                    options,
                })],
                Err(error) => Self::invalid_daemon_event(
                    None,
                    format!("invalid daemon routing gate options: {error}"),
                ),
            },
            "routing_gate_closed" => vec![ServerFrame::event(ApiEvent::RoutingGateClosed {
                session_id: self.event_session_id(),
                gate_id: event["gate_id"].as_str().unwrap_or("").to_string(),
            })],
            "routing_status" => {
                let id = event["id"].as_u64().unwrap_or(0);
                match self.take_simple(id, SimpleKind::Routing) {
                    Some(api_id) => vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::RoutingStatus {
                            text: event["text"].as_str().unwrap_or("").to_string(),
                        },
                    )],
                    None => vec![],
                }
            }
            "ask_user_request" => match serde_json::from_value(event["questions"].clone()) {
                Ok(questions) => vec![ServerFrame::event(ApiEvent::AskRequest {
                    // The daemon can replay a pending ask before the correlated
                    // state reply establishes this new connection's identity.
                    session_id: self
                        .session_id
                        .clone()
                        .or_else(|| {
                            self.pending_attach_id
                                .as_ref()
                                .and_then(|(_, _, target)| target.clone())
                        })
                        .unwrap_or_default(),
                    request_id: event["request_id"].as_str().unwrap_or("").to_string(),
                    tool_call_id: event["tool_call_id"].as_str().unwrap_or("").to_string(),
                    questions,
                })],
                Err(error) => Self::invalid_daemon_event(
                    None,
                    format!("invalid daemon ask questions: {error}"),
                ),
            },
            "tool_start" => vec![ServerFrame::event(ApiEvent::ToolStart {
                session_id: session(self),
                call_id: event["id"].as_str().unwrap_or("").to_string(),
                name: event["name"].as_str().unwrap_or("").to_string(),
            })],
            "tool_input" => vec![ServerFrame::event(ApiEvent::ToolInputDelta {
                session_id: session(self),
                call_id: event["id"].as_str().unwrap_or("").to_string(),
                delta: event["delta"].as_str().unwrap_or("").to_string(),
            })],
            "tool_exec" => {
                let mut frames = self.finish_text();
                frames.push(ServerFrame::event(ApiEvent::ToolExec {
                    session_id: session(self),
                    call_id: event["id"].as_str().unwrap_or("").to_string(),
                    name: event["name"].as_str().unwrap_or("").to_string(),
                }));
                frames
            }
            "tool_done" => vec![ServerFrame::event(ApiEvent::ToolDone {
                codemode: Self::codemode_summary(event),
                session_id: session(self),
                call_id: event["id"].as_str().unwrap_or("").to_string(),
                name: event["name"].as_str().unwrap_or("").to_string(),
                output: event["output"].as_str().unwrap_or("").to_string(),
                error: event["error"].as_str().map(str::to_string),
            })],
            "side_panel_state" => {
                let session_id = event["session_id"].as_str().or(self.session_id.as_deref());
                match (session_id, event.get("snapshot")) {
                    (Some(session_id), Some(snapshot)) if !snapshot.is_null() => {
                        Self::side_panel_frame(session_id, snapshot)
                            .into_iter()
                            .collect()
                    }
                    _ => vec![],
                }
            }
            "applet_state" => {
                let session_id = event["session_id"].as_str().or(self.session_id.as_deref());
                match (session_id, event.get("snapshot")) {
                    (Some(session_id), Some(snapshot)) if !snapshot.is_null() => {
                        Self::applet_frame(session_id, snapshot)
                            .into_iter()
                            .collect()
                    }
                    _ => vec![],
                }
            }
            "side_pane_images" => {
                let images = match Self::daemon_list(event, "images") {
                    Ok(images) => images,
                    Err(error) => return Self::invalid_daemon_event(None, error),
                };
                vec![ServerFrame::event(ApiEvent::SidePaneImages {
                    session_id: event["session_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| session(self)),
                    images,
                })]
            }
            "tokens" => vec![ServerFrame::event(ApiEvent::TokenUsage {
                session_id: session(self),
                input: event["input"].as_u64().unwrap_or(0),
                output: event["output"].as_u64().unwrap_or(0),
                cache_read_input: event["cache_read_input"].as_u64(),
                cache_creation_input: event["cache_creation_input"].as_u64(),
                effective_budget: event["effective_budget"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok()),
            })],
            "request_usage" => {
                let Some((session_id, request_id, provider, model)) = event["session_id"]
                    .as_str()
                    .zip(event["request_id"].as_str())
                    .zip(event["provider"].as_str())
                    .zip(event["model"].as_str())
                    .map(|(((session_id, request_id), provider), model)| {
                        (session_id, request_id, provider, model)
                    })
                else {
                    return vec![];
                };
                vec![ServerFrame::event(ApiEvent::RequestUsage {
                    session_id: session_id.into(),
                    request_id: request_id.into(),
                    provider: provider.into(),
                    model: model.into(),
                    input: event["input"].as_u64(),
                    output: event["output"].as_u64(),
                    cache_read_input: event["cache_read_input"].as_u64(),
                    cache_creation_input: event["cache_creation_input"].as_u64(),
                })]
            }
            "cache_keepalive" => {
                let Some(session_id) = event["session_id"].as_str().filter(|id| !id.is_empty())
                else {
                    return vec![];
                };
                let Ok(usage) = serde_json::from_value(event["usage"].clone()) else {
                    return vec![];
                };
                vec![ServerFrame::event(ApiEvent::CacheKeepalive {
                    session_id: session_id.into(),
                    usage,
                })]
            }
            "kv_cache_miss" => vec![ServerFrame::event(ApiEvent::KvCacheMiss {
                session_id: session(self),
                reason: event["reason"].as_str().unwrap_or("unknown").to_string(),
                harness_caused: event["harness_caused"].as_bool().unwrap_or(false),
                missed_tokens: event["missed_tokens"].as_u64().unwrap_or(0),
                expected_tokens: event["expected_tokens"].as_u64().unwrap_or(0),
                read_tokens: event["read_tokens"].as_u64().unwrap_or(0),
                documented_cause: event["documented_cause"].as_str().map(str::to_string),
                message: event["message"]
                    .as_str()
                    .unwrap_or("KV cache miss")
                    .to_string(),
            })],
            // Replayed straight after the attach history, which the daemon sends
            // before the correlated `state` reply that binds this connection.
            // The event therefore names its own session; an unattributable one
            // is dropped rather than painted on whichever panel is listening.
            "context_occupancy" => {
                let session_id = event["session_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .or(self.session_id.as_deref());
                match (session_id, event["prompt_tokens"].as_u64()) {
                    (Some(session_id), Some(prompt_tokens)) => {
                        vec![ServerFrame::event(ApiEvent::ContextOccupancy {
                            session_id: session_id.to_string(),
                            prompt_tokens,
                        })]
                    }
                    _ => vec![],
                }
            }
            "done" => {
                let id = event["id"].as_u64().unwrap_or(0);
                if let Some(api_id) = self.take_simple(id, SimpleKind::OkOnDone) {
                    self.pending_control_done_ids.remove(&id);
                    return vec![ServerFrame::reply(api_id, ApiEvent::Ok)];
                }
                if let Some(api_id) = self.take_simple(id, SimpleKind::Detach) {
                    self.pending_control_done_ids.remove(&id);
                    self.session_id = None;
                    self.turn_stop_reported = false;
                    self.observed_turn_active = false;
                    self.text_message_id = None;
                    self.text_attempt.clear();
                    return vec![ServerFrame::reply(api_id, ApiEvent::Ok)];
                }
                // Subscribe and controls also emit `done`. Exclude their ids,
                // but retain session-scoped completions fanned out by another
                // attachment or a server-initiated turn (whose id is zero).
                let completed_message = self.pending_message_id == Some(id);
                let completed_idle_interrupt = self.pending_soft_interrupt_ids.contains(&id);
                let control_done = self.pending_control_done_ids.remove(&id);
                let observed_done =
                    self.observed_turn_active && !control_done && self.session_id.is_some();
                if completed_message || completed_idle_interrupt || observed_done {
                    self.observed_turn_active = false;
                    self.turn_stop_reported = false;
                    self.activity_version += 1;
                    self.pending_message_id = None;
                    // Any other retained soft interrupts were queued into the
                    // turn which just ended. Their ids will not emit `done`.
                    self.pending_soft_interrupt_ids.clear();
                    let mut frames = self.finish_text();
                    self.text_attempt.clear();
                    frames.push(ServerFrame::event(ApiEvent::TurnDone {
                        session_id: session(self),
                        pending_soft_interrupts: self.pending_soft_interrupts_at_done.take(),
                    }));
                    frames
                } else {
                    vec![]
                }
            }
            "provider_guardrail_retry" => {
                let Some(session_id) = self.session_id.clone() else {
                    return vec![];
                };
                let (Some(turn_id), Some(attempt), Some(max_attempts)) = (
                    event["turn_id"].as_str(),
                    event["attempt"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok()),
                    event["max_attempts"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok()),
                ) else {
                    return vec![];
                };
                if turn_id.is_empty() || attempt == 0 || attempt > max_attempts {
                    return vec![];
                }
                vec![ServerFrame::event(ApiEvent::ProviderGuardrailRetry {
                    session_id,
                    turn_id: turn_id.to_string(),
                    attempt,
                    max_attempts,
                })]
            }
            "turn_stopped" | "provider_guardrail" => {
                if self.session_id.is_none() || self.turn_stop_reported {
                    return vec![];
                }
                self.turn_stop_reported = true;
                self.observed_turn_active = true;
                let reason = if kind == "provider_guardrail" {
                    jcode_harness_api::TurnStopReason::ProviderGuardrail
                } else {
                    serde_json::from_value(event["reason"].clone())
                        .unwrap_or(jcode_harness_api::TurnStopReason::Unknown)
                };
                vec![ServerFrame::event(ApiEvent::TurnStopped {
                    session_id: session(self),
                    reason,
                    message: event["message"]
                        .as_str()
                        .unwrap_or("The turn stopped unexpectedly.")
                        .to_string(),
                    provider_stop_reason: event["provider_stop_reason"]
                        .as_str()
                        .or_else(|| event["stop_reason"].as_str())
                        .map(str::to_string),
                })]
            }
            "interrupted" => {
                self.activity_version += 1;
                let mut frames = vec![];
                // Older runtimes may only emit Interrupted. An idle cancel is
                // not an abnormal turn, and a late notice after Done is not a
                // second stop. Keep observed activity until Done for observers.
                if !self.turn_stop_reported
                    && (self.observed_turn_active || self.pending_message_id.is_some())
                {
                    self.turn_stop_reported = true;
                    frames.push(ServerFrame::event(ApiEvent::TurnStopped {
                        session_id: session(self),
                        reason: jcode_harness_api::TurnStopReason::Interrupted,
                        message: "The turn was interrupted by a cancellation request.".into(),
                        provider_stop_reason: None,
                    }));
                }
                frames.push(ServerFrame::event(ApiEvent::SessionStatus {
                    session_id: session(self),
                    status: "cancelled".into(),
                    pending_soft_interrupts: None,
                }));
                frames
            }
            "wake_requested" => vec![ServerFrame::event(ApiEvent::WakeRequested {
                session_id: event["session_id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| session(self)),
                reason: event["reason"].as_str().unwrap_or("").to_string(),
                notification: event["notification"].as_str().unwrap_or("").to_string(),
            })],
            "focus_requested" => vec![ServerFrame::event(ApiEvent::FocusRequested {
                session_id: event["session_id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| session(self)),
                from_session: event["from_session"].as_str().map(str::to_string),
                from_name: event["from_name"].as_str().map(str::to_string),
            })],
            "context_message_added" => {
                let id = event["id"].as_u64().unwrap_or(0);
                if self
                    .pending_no_reply_message_id
                    .is_some_and(|(legacy_id, _)| legacy_id == id)
                {
                    let (_, api_id) = self.pending_no_reply_message_id.take().unwrap();
                    vec![ServerFrame::reply(api_id, ApiEvent::Ok)]
                } else {
                    vec![]
                }
            }
            "side_question_forked" | "split_response" => self.translate_fork_reply(event),
            "pong" => self
                .take_simple(event["id"].as_u64().unwrap_or(0), SimpleKind::Ping)
                .map(|api_id| vec![ServerFrame::reply(api_id, ApiEvent::Pong)])
                .unwrap_or_default(),
            "compacted_history" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::CompactedHistory) else {
                    return vec![];
                };
                let raw_messages: Vec<Value> = match Self::daemon_list(event, "messages") {
                    Ok(messages) => messages,
                    Err(error) => return Self::invalid_daemon_event(Some(api_id), error),
                };
                let messages = raw_messages
                    .iter()
                    .map(|m| HistoryMessage {
                        attachments: serde_json::from_value(m["attachments"].clone())
                            .unwrap_or_default(),
                        // Statistics were optional in older daemon history. Malformed
                        // optional display metadata must not discard the transcript.
                        response_stats: serde_json::from_value(m["response_stats"].clone())
                            .inspect_err(|error| {
                                eprintln!("harness optional response statistics invalid: {error}")
                            })
                            .unwrap_or(None),
                        timestamp_ms: m["timestamp_ms"].as_i64(),
                        role: m["role"].as_str().unwrap_or("").to_string(),
                        content: m["content"].as_str().unwrap_or("").to_string(),
                        tool: Self::history_tool(m),
                    })
                    .collect();
                let images = match Self::daemon_list(event, "images") {
                    Ok(images) => images,
                    Err(error) => return Self::invalid_daemon_event(Some(api_id), error),
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::CompactedHistory {
                        session_id: event["session_id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| session(self)),
                        messages,
                        images,
                        compacted_total: event["compacted_total"].as_u64().unwrap_or(0) as usize,
                        compacted_visible: event["compacted_visible"].as_u64().unwrap_or(0)
                            as usize,
                        compacted_remaining: event["compacted_remaining"].as_u64().unwrap_or(0)
                            as usize,
                        compacted_hidden_prompts: event["compacted_hidden_prompts"]
                            .as_u64()
                            .unwrap_or(0)
                            as usize,
                    },
                )]
            }
            "history_page" => {
                let Some(api_id) =
                    self.take_simple(event["id"].as_u64().unwrap_or(0), SimpleKind::HistoryPage)
                else {
                    return vec![];
                };
                let fragments = match Self::daemon_list(event, "fragments") {
                    Ok(fragments) => fragments,
                    Err(error) => return Self::invalid_daemon_event(Some(api_id), error),
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::HistoryPage {
                        session_id: event["session_id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| session(self)),
                        fragments,
                        before: event["before"].as_str().map(str::to_string),
                    },
                )]
            }
            "history_image_chunk" => {
                let Some(api_id) = self.take_simple(
                    event["id"].as_u64().unwrap_or(0),
                    SimpleKind::HistoryImageChunk,
                ) else {
                    return vec![];
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::HistoryImageChunk {
                        session_id: event["session_id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| session(self)),
                        key: event["key"].as_str().unwrap_or("").to_string(),
                        offset: event["offset"].as_u64().unwrap_or(0) as usize,
                        total: event["total"].as_u64().unwrap_or(0) as usize,
                        data: event["data"].as_str().unwrap_or("").to_string(),
                    },
                )]
            }
            "session_images" => {
                let Some(api_id) =
                    self.take_simple(event["id"].as_u64().unwrap_or(0), SimpleKind::SessionImages)
                else {
                    return vec![];
                };
                let images = match Self::daemon_list(event, "images") {
                    Ok(images) => images,
                    Err(error) => return Self::invalid_daemon_event(Some(api_id), error),
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::SessionImages {
                        session_id: event["session_id"].as_str().unwrap_or("").to_owned(),
                        images,
                    },
                )]
            }
            "session_image" => {
                let Some(api_id) =
                    self.take_simple(event["id"].as_u64().unwrap_or(0), SimpleKind::SessionImage)
                else {
                    return vec![];
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::SessionImage {
                        session_id: event["session_id"].as_str().unwrap_or("").to_owned(),
                        image_id: event["image_id"].as_str().unwrap_or("").to_owned(),
                        media_type: event["media_type"].as_str().unwrap_or("").to_owned(),
                        offset: event["offset"].as_u64().unwrap_or(0) as usize,
                        total: event["total"].as_u64().unwrap_or(0) as usize,
                        data: event["data"].as_str().unwrap_or("").to_owned(),
                    },
                )]
            }
            "context_breakdown" => {
                let Some(api_id) = self.take_simple(
                    event["id"].as_u64().unwrap_or(0),
                    SimpleKind::ContextBreakdown,
                ) else {
                    return vec![];
                };
                let breakdown = match serde_json::from_value::<jcode_harness_api::ContextBreakdown>(
                    event.clone(),
                ) {
                    Ok(breakdown) => breakdown,
                    Err(_) => {
                        return Self::invalid_daemon_event(
                            Some(api_id),
                            "invalid context breakdown".into(),
                        );
                    }
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::ContextBreakdown { breakdown },
                )]
            }
            "shake_tools" => {
                let Some(api_id) =
                    self.take_simple(event["id"].as_u64().unwrap_or(0), SimpleKind::ShakeTools)
                else {
                    return vec![];
                };
                let (Some(session_id), Ok(result)) = (
                    event["session_id"].as_str(),
                    serde_json::from_value(event["result"].clone()),
                ) else {
                    return Self::invalid_daemon_event(
                        Some(api_id),
                        "invalid Shake tools result".into(),
                    );
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::ShakeTools {
                        session_id: session_id.to_owned(),
                        result,
                    },
                )]
            }
            "history" => {
                let id = event["id"].as_u64().unwrap_or(0);
                // The daemon volunteers the full session set on `history`,
                // which is the only place it appears: remember it so
                // `list_sessions` can answer with more than this connection.
                self.note_sessions(event);
                // A session that just sent its history is one whose transcript
                // the user is about to search, so index it now.
                self.note_session_transcript(event["session_id"].as_str().unwrap_or(""));
                if let Some(api_id) = self.take_simple(id, SimpleKind::OkOnHistory) {
                    return vec![ServerFrame::reply(api_id, ApiEvent::Ok)];
                }
                if self
                    .pending_recovery_history
                    .as_ref()
                    .is_some_and(|(subscribe_id, target)| {
                        *subscribe_id == id
                            && event["session_id"].as_str().is_some_and(|sid| {
                                !sid.is_empty()
                                    && target.as_deref().is_none_or(|target| target == sid)
                            })
                    })
                {
                    self.pending_recovery_history = None;
                    let mut frames: Vec<_> = Self::attachment_recovery(event).into_iter().collect();
                    frames.extend(Self::side_panel_frame(
                        event["session_id"].as_str().unwrap_or(""),
                        &event["side_panel"],
                    ));
                    frames.extend(Self::applet_frame(
                        event["session_id"].as_str().unwrap_or(""),
                        &event["applets"],
                    ));
                    return frames;
                }
                // The catalog probe rides the same `history` reply shape but
                // carries no messages: it is model identity, not transcript.
                if self.pending_model_probe == Some(id) {
                    self.pending_model_probe = None;
                    self.note_models(event);
                    return vec![
                        ServerFrame::event(self.model_info(session(self), event)),
                        ServerFrame::event(self.runtime_info()),
                    ];
                }
                // A `list_models` that arrived before the catalog did: the
                // client asked too early, so answer it now that it is here.
                if let Some(api_id) = self.take_simple(id, SimpleKind::Models) {
                    self.note_models(event);
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Models {
                            session_id: session(self),
                            models: self.available_models.clone(),
                            current: self.current_model.clone(),
                        },
                    )];
                }
                if let Some(api_id) = self.take_simple(id, SimpleKind::RuntimeInfo) {
                    self.note_models(event);
                    return vec![ServerFrame::reply(api_id, self.runtime_info())];
                }
                let Some(api_id) = self.take_simple(id, SimpleKind::History) else {
                    return vec![];
                };
                let mut messages: Vec<HistoryMessage> = event["messages"]
                    .as_array()
                    .map(|messages| {
                        messages
                            .iter()
                            .map(|m| HistoryMessage {
                                attachments: serde_json::from_value(m["attachments"].clone())
                                    .unwrap_or_default(),
                                response_stats: serde_json::from_value(m["response_stats"].clone())
                                    .unwrap_or(None),
                                timestamp_ms: m["timestamp_ms"].as_i64(),
                                role: m["role"].as_str().unwrap_or("").to_string(),
                                content: m["content"].as_str().unwrap_or("").to_string(),
                                tool: Self::history_tool(m),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // A stored terminal-looking row can still belong to a running
                // turn (e.g. an automatic continuation). Do not show a footer yet.
                if history_activity == Some(true) {
                    let start = messages.iter().rposition(|m| m.role == "user").unwrap_or(0);
                    for message in &mut messages[start..] {
                        message.response_stats = None;
                    }
                }
                let images = match Self::daemon_list(event, "images") {
                    Ok(images) => images,
                    Err(error) => return Self::invalid_daemon_event(Some(api_id), error),
                };
                let mut frames = vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::History {
                        session_id: session(self),
                        messages,
                        images,
                    },
                )];
                // A snapshot requested before newer stream/terminal events must
                // not resurrect a completed turn or stop a newly started one.
                let fresh_activity =
                    self.pending_activity_snapshots.remove(&id) == Some(self.activity_version);
                if fresh_activity && let Some(active) = history_activity {
                    self.observed_turn_active = active;
                    frames.push(ServerFrame::event(ApiEvent::SessionStatus {
                        session_id: session(self),
                        status: if active { "running" } else { "idle" }.into(),
                        pending_soft_interrupts: None,
                    }));
                }
                // Correlation establishes ownership, but reject a conflicting
                // explicit session id rather than hydrating the wrong panel.
                if event["session_id"]
                    .as_str()
                    .is_none_or(|sid| Some(sid) == self.session_id.as_deref())
                {
                    frames.extend(Self::side_panel_frame(&session(self), &event["side_panel"]));
                    frames.extend(Self::applet_frame(&session(self), &event["applets"]));
                }
                frames
            }
            // The model can change mid-session (`/model`, a cycle, or an auth
            // change re-resolving the route), so both pushes are forwarded.
            "context_cap_changed" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let reply_to = self.take_simple(id, SimpleKind::ContextCap);
                let changed = ApiEvent::ContextCapChanged {
                    session_id: event["session_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| session(self)),
                    max_context_tokens: event["max_context_tokens"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|n| *n != 0),
                    effective_budget: event["effective_budget"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .unwrap_or(0),
                };
                let mut frames = vec![ServerFrame::event(changed.clone())];
                if let Some(api_id) = reply_to {
                    frames.push(ServerFrame::reply(api_id, changed));
                }
                frames
            }
            "model_change_pending" => {
                let id = event["id"].as_u64().unwrap_or(0);
                // The switch is accepted, so the request is answered now
                // instead of timing out behind the running turn.
                let reply_to = self.take_simple(id, SimpleKind::Model);
                self.pending_model_switch = Some(id);
                let queued = ApiEvent::ModelSwitch {
                    session_id: session(self),
                    state: ModelSwitchState::Queued,
                    model: event["model"].as_str().map(str::to_string),
                    error: None,
                };
                let mut frames = vec![ServerFrame::event(queued.clone())];
                // The caller learns the switch is queued from its own reply,
                // so it does not apply an effort or speed meant for the new
                // model to the one that is still running.
                if let Some(api_id) = reply_to {
                    frames.push(ServerFrame::reply(api_id, queued));
                }
                frames
            }
            "model_change_cancelled" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let reply_to = self.take_simple(id, SimpleKind::ModelSwitchCancel);
                if event["pending_id"].as_u64() == self.pending_model_switch
                    || event["pending_id"].is_null()
                {
                    self.pending_model_switch = None;
                }
                let cancelled = ApiEvent::ModelSwitch {
                    session_id: session(self),
                    state: ModelSwitchState::Cancelled,
                    model: event["model"].as_str().map(str::to_string),
                    error: None,
                };
                let mut frames = vec![ServerFrame::event(cancelled)];
                if let Some(api_id) = reply_to {
                    frames.push(ServerFrame::reply(api_id, ApiEvent::Ok));
                }
                frames
            }
            "model_changed" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let reply_to = self.take_simple(id, SimpleKind::Model);
                let settles_pending = self.pending_model_switch == Some(id);
                if settles_pending {
                    self.pending_model_switch = None;
                }
                // The daemon reports a rejected switch in-band, as an `error`
                // field on the success event rather than as an error frame.
                // Relayed as a real error so a client sees a failed request
                // instead of a silent no-op that leaves the picker wrong.
                if let Some(error) = event["error"].as_str() {
                    let frame = ApiEvent::Error {
                        code: ErrorCode::InvalidRequest,
                        message: error.to_string(),
                    };
                    return match reply_to {
                        Some(api_id) => vec![ServerFrame::reply(api_id, frame)],
                        // A queued switch that failed to apply has no request
                        // left to answer, so it is reported as an event.
                        None if settles_pending => {
                            vec![ServerFrame::event(ApiEvent::ModelSwitch {
                                session_id: session(self),
                                state: ModelSwitchState::Failed,
                                model: None,
                                error: Some(error.to_string()),
                            })]
                        }
                        None => vec![],
                    };
                }
                if let Some(model) = event["model"].as_str() {
                    self.current_model = Some(model.to_string());
                }
                if !self.current_speed_options.is_empty() {
                    self.current_speed_options.clear();
                    self.model_catalog_loaded = false;
                }
                if let Some(provider) = event["provider_name"].as_str() {
                    self.note_provider(provider);
                }
                self.note_credential(event);
                // Newer daemons report the effort the switched-to model runs
                // with (`null` when the switch cleared it). Older ones omit
                // the key, so the cached value is kept as before.
                if event.get("reasoning_effort").is_some() {
                    self.current_effort = event["reasoning_effort"].as_str().map(str::to_string);
                }
                let info = ApiEvent::ModelInfo {
                    session_id: session(self),
                    provider: self.current_provider.clone(),
                    model: self.current_model.clone(),
                    reasoning_effort: self.current_effort.clone(),
                    auth_method: self.current_credential.clone(),
                    account_label: self.current_account.clone(),
                };
                // Both a reply and a broadcast: the caller needs its request
                // resolved, and every other client watching the session needs
                // to know the model moved under them.
                let mut frames = vec![ServerFrame::event(info)];
                if settles_pending {
                    frames.push(ServerFrame::event(ApiEvent::ModelSwitch {
                        session_id: session(self),
                        state: ModelSwitchState::Applied,
                        model: self.current_model.clone(),
                        error: None,
                    }));
                }
                if let Some(api_id) = reply_to {
                    frames.insert(0, ServerFrame::reply(api_id, ApiEvent::Ok));
                }
                frames
            }
            "status_detail"
                if self.current_speed_mode != jcode_harness_api::SpeedMode::Standard =>
            {
                event["detail"]
                    .as_str()
                    .map(|detail| {
                        ServerFrame::event(ApiEvent::ProviderStatusDetail {
                            session_id: session(self),
                            detail: detail.into(),
                        })
                    })
                    .into_iter()
                    .collect()
            }
            "speed_reported" => {
                let Ok(actual) = serde_json::from_value(event["actual"].clone()) else {
                    return vec![];
                };
                vec![ServerFrame::event(ApiEvent::SpeedModeReported {
                    session_id: session(self),
                    requested: serde_json::from_value(event["requested"].clone())
                        .unwrap_or_default(),
                    actual,
                })]
            }
            "service_tier_reported" => {
                use jcode_harness_api::SpeedMode;
                let mode = |tier: &str| match tier {
                    "standard" | "default" => Some(SpeedMode::Standard),
                    "fast" | "priority" => Some(SpeedMode::Fast),
                    "ultrafast" => Some(SpeedMode::Ultrafast),
                    _ => None,
                };
                event["tier"]
                    .as_str()
                    .and_then(mode)
                    .map(|actual| {
                        ServerFrame::event(ApiEvent::SpeedModeReported {
                            session_id: session(self),
                            requested: event["requested"].as_str().and_then(mode),
                            actual,
                        })
                    })
                    .into_iter()
                    .collect()
            }
            "speed_mode_changed" => {
                let id = event["id"].as_u64().unwrap_or_default();
                let reply_to = self.take_simple(id, SimpleKind::SpeedMode);
                if let Some(message) = event["error"].as_str() {
                    return reply_to
                        .map(|api_id| {
                            ServerFrame::reply(
                                api_id,
                                ApiEvent::Error {
                                    code: ErrorCode::InvalidRequest,
                                    message: message.into(),
                                },
                            )
                        })
                        .into_iter()
                        .collect();
                }
                self.note_speed(event);
                let changed = ApiEvent::SpeedModeChanged {
                    session_id: session(self),
                    speed_mode: self.current_speed_mode,
                    speed_options: self.current_speed_options.clone(),
                };
                return match reply_to {
                    Some(api_id) => vec![ServerFrame::reply(api_id, changed)],
                    None => vec![ServerFrame::event(changed)],
                };
            }
            "reasoning_effort_changed" => {
                let id = event["id"].as_u64().unwrap_or(0);
                // Remember the new effort even when the change was requested by
                // another client, so later identity events stay truthful.
                // A success without `effort` means provider default. The wire
                // value "none" is a separate explicit setting.
                let changed = event["error"].as_str().is_none() && {
                    let effort = event["effort"].as_str().map(str::to_string);
                    let model = event["model"].as_str().map(str::to_string);
                    let moved = self.current_effort != effort
                        || model
                            .as_ref()
                            .is_some_and(|m| self.current_model.as_ref() != Some(m));
                    self.current_effort = effort;
                    if let Some(model) = model {
                        self.current_model = Some(model);
                    }
                    moved
                };
                // A successful change is also broadcast as identity, mirroring
                // model_changed: every attached client needs to know the
                // effort moved under it, not only the one that asked.
                let info = changed.then(|| {
                    ServerFrame::event(ApiEvent::ModelInfo {
                        session_id: session(self),
                        provider: self.current_provider.clone(),
                        model: self.current_model.clone(),
                        reasoning_effort: self.current_effort.clone(),
                        auth_method: self.current_credential.clone(),
                        account_label: self.current_account.clone(),
                    })
                });
                let Some(api_id) = self.take_simple(id, SimpleKind::ReasoningEffort) else {
                    return info.into_iter().collect();
                };
                match event["error"].as_str() {
                    Some(error) => vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        },
                    )],
                    None => std::iter::once(ServerFrame::reply(api_id, ApiEvent::Ok))
                        .chain(info)
                        .collect(),
                }
            }
            // A completed compaction is a session event, not the request reply.
            // The divider body is formatted by the daemon with the same builder
            // the stored-session renderer uses, so the live row and the row that
            // comes back after a reload are identical. This bridge deliberately
            // links no application foundation, so it must not re-derive the text.
            "memory_injected" => {
                // Same contract as the compaction divider: the label and the
                // detail are composed by one shared formatter, and the daemon
                // stores the identical row, so a reload cannot disagree with
                // what this live event said was recalled.
                let count = event["count"].as_u64().unwrap_or(0) as usize;
                let display = event["display_prompt"].as_str().map(str::to_string);
                let display = display.unwrap_or_else(|| {
                    let prompt = event["prompt"].as_str().unwrap_or("");
                    if prompt.trim().is_empty() {
                        // The daemon stores this placeholder for an empty
                        // payload; use it so the live row matches the reloaded
                        // one even when nothing was recalled.
                        "# Memory\n\n## Notes\n1. (empty injection payload)".to_string()
                    } else {
                        prompt.to_string()
                    }
                });
                vec![ServerFrame::event(ApiEvent::MemoryInjected {
                    session_id: session(self),
                    count,
                    content: jcode_session_types::memory_injection_content(count, &display),
                })]
            }
            "memory_activity" => {
                // A snapshot the client missed is not worth reporting as a
                // protocol failure: activity is display-only.
                let Ok(activity) = serde_json::from_value(event["activity"].clone()) else {
                    return vec![];
                };
                vec![ServerFrame::event(ApiEvent::MemoryActivity {
                    session_id: session(self),
                    activity,
                })]
            }
            "compaction" => {
                let content = event["divider"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        let before = event["pre_tokens"]
                            .as_u64()
                            .map_or_else(|| "unknown".to_string(), |n| n.to_string());
                        let after = event["post_tokens"]
                            .as_u64()
                            .map_or_else(|| "unknown".to_string(), |n| n.to_string());
                        format!("compacted · {before}→{after}")
                    });
                vec![ServerFrame::event(ApiEvent::CompactionDivider {
                    session_id: session(self),
                    content,
                    running: false,
                    method: event["method"].as_str().map(str::to_string),
                    trigger: event["trigger"].as_str().map(str::to_string),
                    pre_tokens: event["pre_tokens"].as_u64(),
                    post_tokens: event["post_tokens"].as_u64(),
                })]
            }
            // The summary is being generated. Clients show the running divider
            // instead of leaving the transcript looking idle.
            "compaction_started" => {
                let trigger = event["trigger"].as_str().map(str::to_string);
                let pre_tokens = event["pre_tokens"].as_u64().filter(|tokens| *tokens > 0);
                let label = match (trigger.as_deref(), pre_tokens) {
                    (Some(trigger), Some(tokens)) => {
                        format!("compacting… · {trigger} · {tokens} tokens")
                    }
                    (Some(trigger), None) => format!("compacting… · {trigger}"),
                    (None, _) => "compacting…".to_string(),
                };
                vec![ServerFrame::event(ApiEvent::CompactionDivider {
                    session_id: session(self),
                    content: label,
                    running: true,
                    method: None,
                    trigger,
                    pre_tokens,
                    post_tokens: None,
                })]
            }
            // Correlated results acknowledge scheduling. Older daemons may
            // also reply with a refusal. Uncorrelated results report failure
            // after acceptance.
            "compact_result" => {
                let id = event["id"].as_u64().unwrap_or(0);
                if id == 0 && event["success"].as_bool() == Some(false) {
                    return vec![ServerFrame::event(ApiEvent::BackgroundProgress {
                        session_id: session(self),
                        task_id: "context-compaction".into(),
                        label: "Context compaction".into(),
                        percent: None,
                        summary: event["message"]
                            .as_str()
                            .unwrap_or("Compaction failed")
                            .to_string(),
                        detail: None,
                        done: true,
                    })];
                }
                let Some(api_id) = self.take_simple(id, SimpleKind::Compact) else {
                    return vec![];
                };
                let message = event["message"].as_str().unwrap_or("").to_string();
                if event["success"].as_bool() == Some(false) {
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        },
                    )];
                }
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::Compacted {
                        session_id: session(self),
                        message,
                    },
                )]
            }
            "session_renamed" => {
                let session_id = event["session_id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| session(self));
                self.note_session_title(&session_id, event["title"].as_str());
                vec![ServerFrame::event(ApiEvent::SessionRenamed {
                    session_id,
                    title: event["title"].as_str().map(str::to_string),
                    display_title: event["display_title"].as_str().unwrap_or("").to_string(),
                })]
            }
            "model_usage_updated" => {
                let route = &event["route"];
                let (Some(model), Some(provider), Some(api_method), Ok(usage)) = (
                    route["model"].as_str(),
                    route["provider"].as_str(),
                    route["api_method"].as_str(),
                    serde_json::from_value::<jcode_harness_api::ModelUsage>(route["usage"].clone()),
                ) else {
                    return vec![];
                };
                let observed = self
                    .model_usage_updates
                    .entry((model.into(), provider.into(), api_method.into()))
                    .or_default();
                observed.merge_observation(&usage);
                for cached in &mut self.available_routes {
                    if model == cached.model
                        && provider == cached.provider
                        && api_method == cached.api_method
                    {
                        cached
                            .usage
                            .get_or_insert_with(Default::default)
                            .merge_observation(observed);
                    }
                }
                vec![ServerFrame::event(self.runtime_info())]
            }
            "available_models_updated" => {
                self.note_models(event);
                vec![
                    ServerFrame::event(self.model_info(session(self), event)),
                    ServerFrame::event(self.runtime_info()),
                ]
            }
            "notification" => {
                let message = event["message"].as_str().unwrap_or("");
                let notification = &event["notification_type"];
                if notification["kind"] == "message"
                    && matches!(
                        notification["scope"].as_str(),
                        Some("swarm_worker_control" | "setting_change")
                    )
                    && event["from_session"] == "jcode"
                {
                    return vec![ServerFrame::event(ApiEvent::SwarmWorkerNotice {
                        session_id: session(self),
                        message: message.to_owned(),
                    })];
                }
                if notification["kind"] == "message" && notification["scope"] == "dm" {
                    return match event["from_session"].as_str().filter(|id| !id.is_empty()) {
                        Some(from_session) => vec![ServerFrame::event(ApiEvent::AgentMessage {
                            session_id: session(self),
                            from_session: from_session.to_owned(),
                            from_name: event["from_name"].as_str().map(str::to_owned),
                            message: message.to_owned(),
                        })],
                        None => vec![],
                    };
                }
                // Background-task traffic still arrives as rendered Markdown.
                match parse_background_notification(message) {
                    Some(mut progress) => {
                        progress.session_id = session(self);
                        vec![ServerFrame::event(progress.into_event())]
                    }
                    None => vec![],
                }
            }
            "ack" => {
                let id = event["id"].as_u64().unwrap_or(0);
                // The daemon acking the in-flight `message` is the proof the
                // agent has the text: report it as its own event so a client
                // can move a message from "sent" to "acknowledged" without
                // waiting for the first token of the reply.
                if self.pending_message_id == Some(id) {
                    // Acceptance starts a turn before any provider output. An
                    // older idle snapshot must not stop this pre-stream turn.
                    self.observed_turn_active = true;
                    self.activity_version += 1;
                    return vec![ServerFrame::event(ApiEvent::MessageAccepted {
                        session_id: session(self),
                    })];
                }
                let Some(index) = self
                    .pending_simple
                    .iter()
                    .position(|(legacy_id, _, _)| *legacy_id == id)
                else {
                    return vec![];
                };
                let (_, api_id, kind) = self.pending_simple.remove(index);
                match kind {
                    SimpleKind::Ok | SimpleKind::ToolControl | SimpleKind::QueueControl => {
                        vec![ServerFrame::reply(api_id, ApiEvent::Ok)]
                    }
                    SimpleKind::Credential {
                        provider,
                        configured,
                    } => vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::CredentialUpdated {
                            provider,
                            configured,
                        },
                    )],
                    // Acks are not terminal replies for richer request kinds.
                    other => {
                        self.pending_simple.push((id, api_id, other));
                        vec![]
                    }
                }
            }
            "error" => {
                let id = event["id"].as_u64().unwrap_or(0);
                if let Some(reply) = self.control_error_reply(id, event) {
                    return vec![reply];
                }
                self.activity_version += 1;
                self.observed_turn_active = false;
                let id = event["id"].as_u64().unwrap_or(0);
                self.pending_control_done_ids.remove(&id);
                self.pending_activity_snapshots.remove(&id);
                if let Some((state_id, api_id, target)) = self.pending_attach_id.as_ref()
                    && (self.pending_attach_subscribe_id == Some(id) || *state_id == id)
                {
                    let api_id = *api_id;
                    let code = if target.is_some() {
                        ErrorCode::UnknownSession
                    } else {
                        ErrorCode::Internal
                    };
                    self.pending_attach_id = None;
                    self.pending_attach_subscribe_id = None;
                    self.pending_model_probe = None;
                    self.pending_recovery_history = None;
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code,
                            message: event["message"].as_str().unwrap_or("").to_string(),
                        },
                    )];
                }
                self.pending_soft_interrupt_ids
                    .retain(|pending_id| *pending_id != id);
                let message = event["message"].as_str().unwrap_or("").to_string();
                // A turn that fails ends with `error` *instead of* `done`, so
                // the turn is over: forget the pending message, or a later
                // unrelated `done` carrying the same id would be reported as
                // this turn finishing.
                if self.pending_message_id == Some(id) {
                    self.pending_message_id = None;
                }
                let no_reply_api_id = self
                    .pending_no_reply_message_id
                    .filter(|(legacy_id, _)| *legacy_id == id)
                    .map(|(_, api_id)| api_id);
                if no_reply_api_id.is_some() {
                    self.pending_no_reply_message_id = None;
                }
                let fork_api_id = self
                    .pending_fork_id
                    .filter(|(legacy_id, _)| *legacy_id == id)
                    .map(|(_, api_id)| api_id);
                if fork_api_id.is_some() {
                    self.pending_fork_id = None;
                }
                // Route to a pending request when possible, else stream it.
                let reply_to = no_reply_api_id
                    .or(fork_api_id)
                    .or_else(|| {
                        self.pending_side_question_forks
                            .remove(&id)
                            .map(|(api_id, _)| api_id)
                    })
                    .or_else(|| {
                        self.pending_simple
                            .iter()
                            .position(|(legacy_id, _, _)| *legacy_id == id)
                            .map(|index| self.pending_simple.remove(index).1)
                    });
                let frame_event = ApiEvent::Error {
                    code: ErrorCode::Internal,
                    message,
                };
                let mut frames = vec![match reply_to {
                    Some(api_id) => ServerFrame::reply(api_id, frame_event),
                    None => ServerFrame::event(frame_event),
                }];
                if reply_to.is_none() && self.turn_stop_reported {
                    self.turn_stop_reported = false;
                    self.pending_message_id = None;
                    self.pending_soft_interrupt_ids.clear();
                    frames.extend(self.finish_text());
                    self.text_attempt.clear();
                    frames.push(ServerFrame::event(ApiEvent::TurnDone {
                        session_id: session(self),
                        pending_soft_interrupts: self.pending_soft_interrupts_at_done.take(),
                    }));
                }
                frames
            }
            "queued_prompt_rejected" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::QueueControl) else {
                    return vec![];
                };
                let skill = event["skill"].as_str().unwrap_or("");
                let code = if event["reason"] == "unknown_skill" {
                    ErrorCode::UnknownSkill
                } else {
                    ErrorCode::InvalidRequest
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::Error {
                        code,
                        message: format!("Skill '{skill}' is not installed on the server"),
                    },
                )]
            }
            "queued_prompts" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let prompts = match serde_json::from_value(event["prompts"].clone()) {
                    Ok(prompts) => prompts,
                    Err(_) => return vec![],
                };
                let payload = ApiEvent::QueuedPrompts {
                    session_id: session(self),
                    prompts,
                };
                match self.take_simple(id, SimpleKind::QueueSnapshot) {
                    Some(api_id) => vec![ServerFrame::reply(api_id, payload)],
                    None => vec![ServerFrame::event(payload)],
                }
            }
            "queued_prompt_removed" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::QueueRemoval) else {
                    return vec![];
                };
                let Ok(outcome) = serde_json::from_value(event["outcome"].clone()) else {
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "invalid queued prompt removal outcome".into(),
                        },
                    )];
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::QueuedPromptRemoved {
                        session_id: session(self),
                        prompt_id: event["prompt_id"].as_str().unwrap_or("").to_string(),
                        outcome,
                    },
                )]
            }
            "queued_prompt_edited" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::QueueEdit) else {
                    return vec![];
                };
                let Ok(outcome) = serde_json::from_value(event["outcome"].clone()) else {
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "invalid queued prompt edit outcome".into(),
                        },
                    )];
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::QueuedPromptEdited {
                        session_id: session(self),
                        prompt_id: event["prompt_id"].as_str().unwrap_or("").to_string(),
                        outcome,
                    },
                )]
            }
            "soft_interrupt_injected" => {
                // Daemon-originated injections (background task results,
                // system notes) are also turn boundaries an embedder may care
                // about, so forward every injection, not only id-bearing ones.
                let client_ids = event["client_ids"]
                    .as_array()
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                // Injection precedes the response that sees it: whatever text
                // was open belongs to the previous response.
                let mut frames = self.finish_text();
                self.observed_turn_active = true;
                self.activity_version += 1;
                frames.push(ServerFrame::event(ApiEvent::SoftInterruptInjected {
                    session_id: session(self),
                    content: event["content"].as_str().map(str::to_string),
                    client_ids,
                    point: event["point"].as_str().unwrap_or("").to_string(),
                    tools_skipped: event["tools_skipped"].as_u64().map(|n| n as u32),
                    display_role: event["display_role"].as_str().map(str::to_string),
                }));
                frames
            }
            "soft_interrupt_queue" => {
                let pending = event["pending"].as_u64().unwrap_or(0) as u32;
                self.pending_soft_interrupts_at_done = (pending > 0).then_some(pending);
                vec![]
            }
            "soft_interrupts_cancelled" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::SoftInterruptCancel) else {
                    return vec![];
                };
                let strings = |key: &str| -> Vec<String> {
                    event[key]
                        .as_array()
                        .map(|ids| {
                            ids.iter()
                                .filter_map(|id| id.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::SoftInterruptsCancelled {
                        cancelled: strings("cancelled"),
                        not_queued: strings("not_queued"),
                    },
                )]
            }
            "background_tool_result" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some(api_id) = self.take_simple(id, SimpleKind::BackgroundTool) else {
                    return vec![];
                };
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::BackgroundToolResult {
                        moved: event["moved"].as_bool().unwrap_or(false),
                        tool_call_id: event["tool_call_id"].as_str().map(str::to_string),
                        tool_name: event["tool_name"].as_str().map(str::to_string),
                    },
                )]
            }
            "swarm_status" => {
                let members = match Self::daemon_list::<Value>(event, "members") {
                    Ok(members) => members,
                    Err(message) => {
                        return vec![ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message,
                        })];
                    }
                };
                let updated_at_unix_ms = members
                    .iter()
                    .filter_map(|member| member["runtime"]["snapshot_at_unix_ms"].as_u64())
                    .max()
                    .unwrap_or_else(|| {
                        // Older daemons have no publication stamp. Preserve
                        // their receive order, including same-millisecond
                        // transitions, rather than creating equal LWW keys.
                        static LAST_MS: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        LAST_MS.fetch_max(Self::now_ms(), std::sync::atomic::Ordering::Relaxed);
                        LAST_MS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
                    });
                let members = members
                    .into_iter()
                    .filter_map(|member| {
                        Some(jcode_harness_api::SwarmMemberInfo {
                            session_id: member["session_id"].as_str()?.to_string(),
                            status: member["status"].as_str()?.to_string(),
                            parent_session_id: member["report_back_to_session_id"]
                                .as_str()
                                .map(str::to_owned),
                            agent_label: member["task_label"].as_str().map(str::to_owned),
                            model: member["runtime"]["model"].as_str().map(str::to_owned),
                            provider: member["runtime"]["provider"].as_str().map(str::to_owned),
                            effort: member["runtime"]["effort"].as_str().map(str::to_owned),
                        })
                    })
                    .collect();
                vec![ServerFrame::event(ApiEvent::SwarmStatus {
                    session_id: session(self),
                    updated_at_unix_ms,
                    members,
                })]
            }
            // Everything else on the legacy stream is not part of the stable
            // API surface yet; drop it.
            _ => vec![],
        };
        for frame in &mut frames {
            if let ApiEvent::Attached { session, .. } | ApiEvent::SessionForked { session } =
                &mut frame.event
            {
                jcode_harness_api::enrich_sessions_from_local_edit_stats(std::slice::from_mut(
                    session,
                ));
                jcode_harness_api::enrich_sessions_from_local_swarm_state(std::slice::from_mut(
                    session,
                ));
            }
        }
        frames
    }
}
