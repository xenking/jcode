//! API request -> legacy daemon request translation.

use super::*;

impl BridgeState {
    pub(super) fn translate_request(&mut self, request: &Value) -> Vec<Outbound> {
        let api_id = request["id"].as_u64().unwrap_or(0);
        let req = request["req"].as_str().unwrap_or("");

        if req.starts_with("desktop_state_") {
            return self.desktop_state_request(request, api_id);
        }

        if matches!(req, "routing_gate_answer" | "routing_intent") && !self.routing_supported {
            return Self::error_reply(
                api_id,
                ErrorCode::UnknownRequest,
                "This runtime does not support account routing.",
            );
        }

        if req == "fork_side_question" && !self.side_questions_supported {
            return Self::error_reply(
                api_id,
                ErrorCode::UnknownRequest,
                "This runtime does not support side questions.",
            );
        }

        // Stateful requests only mean something once this connection is
        // attached. Forwarding one before then is not merely useless: the
        // daemon answers "Client must Subscribe with a working_dir before
        // sending stateful requests" and *closes the connection*, so a client
        // that mistypes a session id loses every other session it was
        // streaming, and the SDK reports a bare EPIPE. Answer locally instead,
        // with the code that actually says what went wrong.
        // `pending_attach_id` means a subscribe is already on the wire, so the
        // daemon will have a session by the time this arrives: a client that
        // pipelines `create_session` and `send_message` without awaiting must
        // still work. Only a connection that never asked to attach is refused.
        if self.session_id.is_none()
            && self.pending_attach_id.is_none()
            && REQUIRES_ATTACH.contains(&req)
        {
            let requested = request["session_id"].as_str().unwrap_or("");
            return vec![Outbound::Reply(ServerFrame::reply(
                api_id,
                ApiEvent::Error {
                    code: ErrorCode::UnknownSession,
                    message: if requested.is_empty() {
                        format!(
                            "`{req}` needs an attached session; call create_session or attach_session first"
                        )
                    } else {
                        format!(
                            "not attached to session `{requested}`; call attach_session first (it is not attached, or does not exist)"
                        )
                    },
                },
            ))];
        }

        // A request naming a session other than the attached one would be
        // silently applied to the attached session, because the legacy
        // protocol has no session field: `clear` on a typo'd id would wipe the
        // wrong transcript. Refuse rather than destroy the wrong thing.
        if let Some(attached) = self.session_id.as_deref()
            && REQUIRES_ATTACH.contains(&req)
            && let Some(requested) = request["session_id"].as_str()
            && !requested.is_empty()
            && requested != attached
        {
            return vec![Outbound::Reply(ServerFrame::reply(
                api_id,
                ApiEvent::Error {
                    code: ErrorCode::UnknownSession,
                    message: format!(
                        "this connection is attached to `{attached}`, not `{requested}`; attach to it first or use another connection"
                    ),
                },
            ))];
        }

        if req == "runtime_command" {
            if !self.runtime_commands_supported {
                return Self::error_reply(
                    api_id,
                    ErrorCode::UnknownRequest,
                    "The connected daemon does not support structured commands. Updating Desktop alone does not replace a running daemon. When active work has finished, update Jcode on the runtime host, then run `jcode server reload` for a state-preserving upgrade and reconnect Desktop. Do not stop or kill the daemon while sessions are active.",
                );
            }
            let parsed = serde_json::from_value::<jcode_harness_api::ApiRequest>(request.clone());
            let Ok(jcode_harness_api::ApiRequest::RuntimeCommand {
                command,
                args,
                session_id,
            }) = parsed
            else {
                return Self::error_reply(
                    api_id,
                    ErrorCode::InvalidRequest,
                    "Invalid runtime command request",
                );
            };
            if session_id.is_empty() {
                return Self::error_reply(
                    api_id,
                    ErrorCode::InvalidRequest,
                    "session_id must not be empty",
                );
            }
            let id = self.legacy_id();
            self.pending_simple
                .push((id, api_id, SimpleKind::RuntimeCommand));
            return vec![Outbound::Legacy(
                json!({"type": "runtime_command", "id": id, "command": command, "args": args}),
            )];
        }
        if matches!(req, "configure_tools" | "list_tools" | "tool_result") {
            // Validate the typed surface even though this bridge accepts raw JSON.
            // Never coerce a malformed result/configuration into a valid mutation.
            if let Err(error) =
                serde_json::from_value::<jcode_harness_api::ApiRequest>(request.clone())
            {
                return Self::error_reply(api_id, ErrorCode::InvalidRequest, &error.to_string());
            }
            if request["session_id"].as_str().is_none_or(str::is_empty) {
                return Self::error_reply(
                    api_id,
                    ErrorCode::InvalidRequest,
                    "session_id must not be empty",
                );
            }
            if let Some((_, _, Some(target))) = &self.pending_attach_id
                && self.session_id.is_none()
                && request["session_id"].as_str() != Some(target.as_str())
            {
                return Self::error_reply(
                    api_id,
                    ErrorCode::UnknownSession,
                    "request does not match the pending attachment",
                );
            }
            if !self.session_tools_supported {
                return Self::error_reply(
                    api_id,
                    ErrorCode::UnknownRequest,
                    "daemon does not advertise the session_tools capability",
                );
            }
            let id = self.legacy_id();
            let mut legacy = json!({"type": req, "id": id});
            match req {
                "configure_tools" => legacy["tools"] = request["tools"].clone(),
                "tool_result" => {
                    legacy["call_id"] = request["call_id"].clone();
                    legacy["output"] = request["output"].clone();
                    if let Some(error) = request.get("error") {
                        legacy["error"] = error.clone();
                    }
                }
                _ => {}
            }
            self.pending_simple.push((
                id,
                api_id,
                if req == "list_tools" {
                    SimpleKind::Tools
                } else {
                    SimpleKind::ToolControl
                },
            ));
            return vec![Outbound::Legacy(legacy)];
        }

        match req {
            "archive_session" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                if Self::session_record_path(session_id).is_none_or(|path| !path.is_file()) {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::UnknownSession,
                        "session does not exist",
                    );
                }
                let _write_guard = Self::state_write_guard();
                let mut archive = Self::load_archive_state();
                archive
                    .sessions
                    .insert(session_id.to_string(), Self::now_ms());
                match Self::save_archive_state(&archive) {
                    Ok(()) => vec![Outbound::Reply(ServerFrame::reply(api_id, ApiEvent::Ok))],
                    Err(message) => Self::error_reply(api_id, ErrorCode::Internal, &message),
                }
            }
            "restore_session" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                if Self::session_record_path(session_id).is_none_or(|path| !path.is_file()) {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::UnknownSession,
                        "session does not exist",
                    );
                }
                let _write_guard = Self::state_write_guard();
                let mut archive = Self::load_archive_state();
                archive.sessions.remove(session_id);
                match Self::save_archive_state(&archive) {
                    Ok(()) => vec![Outbound::Reply(ServerFrame::reply(api_id, ApiEvent::Ok))],
                    Err(message) => Self::error_reply(api_id, ErrorCode::Internal, &message),
                }
            }
            "set_retention_policy" => {
                let days = match request.get("archive_after_days") {
                    None | Some(Value::Null) => None,
                    Some(value) => match value.as_u64() {
                        Some(days @ 1..=36_500) => Some(days as u32),
                        _ => {
                            return Self::error_reply(
                                api_id,
                                ErrorCode::InvalidRequest,
                                "archive_after_days must be 1..=36500, or null to disable",
                            );
                        }
                    },
                };
                let _write_guard = Self::state_write_guard();
                let mut archive = Self::load_archive_state();
                archive.archive_after_days = days;
                match Self::save_archive_state(&archive) {
                    Ok(()) => vec![Outbound::Reply(ServerFrame::reply(api_id, ApiEvent::Ok))],
                    Err(message) => Self::error_reply(api_id, ErrorCode::Internal, &message),
                }
            }
            "create_session" | "attach_session" => {
                let id = self.legacy_id();
                let state_id = self.legacy_id();
                let catalog_id = self.legacy_id();
                let requested_session = (req == "attach_session")
                    .then(|| request["session_id"].as_str().map(str::to_string))
                    .flatten();
                self.pending_recovery_history = Some((id, requested_session.clone()));
                self.pending_attach_id = Some((state_id, api_id, requested_session));
                self.pending_attach_subscribe_id = Some(id);
                self.pending_model_probe = Some(catalog_id);
                self.pending_create_dir = None;
                let mut subscribe = json!({
                    "type": "subscribe",
                    "id": id,
                    "supports_pdf_panels": true,
                    "supports_user_response": self.supports_user_response,
                    "supports_applets": self.supports_applets,
                    // API clients have no account-router countdown of their
                    // own, so the daemon switches pool accounts and retries
                    // usage-limited turns for them, like the TUI does.
                    "supports_server_failover": true,
                    "supports_turn_identity": true,
                });
                if self.bounded_history_supported
                    && self
                        .client_name
                        .as_deref()
                        .is_some_and(|name| name.starts_with("jcode-desktop-"))
                {
                    subscribe["history_base"] =
                        json!({"count": 0, "fingerprint": 0, "recent_messages": 0});
                }
                // Mobile sockets disappear when the OS suspends the app or
                // changes networks. The daemon must own their session lifetime.
                // Preserve the existing desktop and unknown SDK client policy.
                if self.client_name.as_deref().is_some_and(|name| {
                    matches!(name.split('/').next(), Some("jcode-ios" | "jcode-android"))
                }) {
                    subscribe["continue_on_disconnect"] = json!(true);
                }
                if self.crash_on_disconnect {
                    subscribe["crash_on_disconnect"] = json!(true);
                }
                if let Some(name) = self.client_name.as_deref().filter(|n| !n.is_empty()) {
                    subscribe["client_name"] = json!(name);
                }
                // Sessions rooted inside a jcode checkout are self-dev
                // sessions: the daemon only enables the self-dev tools and
                // prompt when the subscribe says so, and a client that opens
                // the repo without saying so gets an agent that cannot build
                // the very app it is running in.
                if req == "create_session" {
                    if let Some(prompt) = request["system_prompt"].as_str() {
                        subscribe["system_prompt"] = json!(prompt);
                    }
                    let working_dir =
                        request["working_dir"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| {
                                std::env::current_dir()
                                    .ok()
                                    .map(|d| d.display().to_string())
                            });
                    subscribe["working_dir"] = json!(working_dir);
                    self.pending_create_dir = working_dir.clone();
                    if working_dir
                        .as_deref()
                        .is_some_and(Self::path_is_inside_jcode_repo)
                    {
                        subscribe["selfdev"] = json!(true);
                    }
                } else if let Some(target) = request["session_id"].as_str() {
                    // The daemon owns both live and persisted session roots.
                    // Empty panels have no file yet, and a cached disk cwd may
                    // be older than the live Agent. Never override either with
                    // the bridge's process cwd just to bootstrap attachment.
                    subscribe["target_session_id"] = json!(target);
                }
                // The daemon assigns the session during subscribe but reports
                // the id via `state`, so chase the subscribe with get_state.
                // The model identity arrives the same way, via the catalog
                // reply, so ask for it now rather than making the client poll.
                vec![
                    Outbound::Legacy(subscribe),
                    Outbound::Legacy(json!({"type": "state", "id": state_id})),
                    Outbound::Legacy(
                        json!({"type": "get_model_catalog", "id": catalog_id, "subscribe_usage_updates": true}),
                    ),
                ]
            }
            "send_message" | "send_skill_message" => {
                if request["attachments"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                    && !self.file_attachments_supported
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "daemon does not advertise file_attachments_v1",
                    );
                }
                let id = self.legacy_id();
                let no_reply = request["no_reply"].as_bool().unwrap_or(false);
                if no_reply {
                    self.pending_no_reply_message_id = Some((id, api_id));
                } else {
                    self.pending_message_id = Some(id);
                }
                let mut message = json!({
                    "type": "message",
                    "id": id,
                    "content": request["content"].as_str().unwrap_or(""),
                });
                if no_reply {
                    message["no_reply"] = json!(true);
                }
                if let Some(reminder) = request["system_reminder"].as_str() {
                    message["system_reminder"] = json!(reminder);
                }
                if let Some(skill) = request["active_skill"].as_str() {
                    message["active_skill"] = json!(skill);
                }
                if let Some(images) = request["images"].as_array()
                    && !images.is_empty()
                {
                    message["images"] = json!(images);
                }
                if let Some(attachments) = request["attachments"].as_array()
                    && !attachments.is_empty()
                {
                    message["attachments"] = json!(attachments);
                }
                vec![Outbound::Legacy(message)]
            }
            "fork_session" | "fork_side_question" | "transfer_session" => {
                self.translate_fork_request(req, api_id, request)
            }
            "cancel" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ok));
                vec![Outbound::Legacy(json!({"type": "cancel", "id": id}))]
            }
            "force_stop_turn" => {
                if !self.force_stop_turn_supported {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::UnknownRequest,
                        "daemon does not advertise force_stop_turn_v1",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ok));
                vec![Outbound::Legacy(
                    json!({"type": "force_stop_turn", "id": id, "session_id": request["session_id"]}),
                )]
            }
            "soft_interrupt" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ok));
                self.pending_soft_interrupt_ids.push(id);
                let mut interrupt = json!({
                    "type": "soft_interrupt",
                    "id": id,
                    "content": request["content"].as_str().unwrap_or(""),
                    "urgent": request["urgent"].as_bool().unwrap_or(false),
                });
                if let Some(images) = request["images"].as_array()
                    && !images.is_empty()
                {
                    interrupt["images"] = json!(images);
                }
                if let Some(client_id) = request["client_id"].as_str() {
                    // Older daemons ignore the field, which is harmless here.
                    interrupt["client_id"] = json!(client_id);
                }
                vec![Outbound::Legacy(interrupt)]
            }
            "queue_prompt"
            | "set_queued_prompt_mode"
            | "interrupt_queued_prompt"
            | "interrupt_queued_prompts"
            | "get_queued_prompts"
            | "remove_queued_prompt"
            | "reorder_queued_prompts"
            | "set_queued_prompt_editing"
            | "edit_queued_prompt" => {
                if !self.queued_prompts_supported {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "daemon does not advertise queued_prompts".into(),
                        },
                    ))];
                }
                if matches!(req, "queue_prompt" | "set_queued_prompt_mode")
                    && request["mode"] == "queue"
                    && !self.queued_prompt_fifo_supported
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "daemon does not advertise queued_prompt_fifo_v1",
                    );
                }
                if req == "queue_prompt"
                    && request["active_skill"].is_string()
                    && !self.queued_prompt_active_skill_supported
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "daemon does not advertise queued_prompt_active_skill",
                    );
                }
                if matches!(req, "queue_prompt" | "edit_queued_prompt")
                    && request["attachments"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                    && !self.file_attachments_supported
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "daemon does not advertise file_attachments_v1",
                    );
                }
                if req == "interrupt_queued_prompts"
                    && !self.queued_prompt_batch_interrupt_supported
                {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "daemon does not advertise queued_prompt_batch_interrupt_v1"
                                .into(),
                        },
                    ))];
                }
                if matches!(req, "remove_queued_prompt" | "reorder_queued_prompts")
                    && !self.queued_prompt_mutations_supported
                {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "daemon does not advertise queued_prompt_mutations_v1".into(),
                        },
                    ))];
                }
                if matches!(req, "set_queued_prompt_editing" | "edit_queued_prompt")
                    && !self.queued_prompt_editing_supported
                {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "daemon does not advertise queued_prompt_editing".into(),
                        },
                    ))];
                }
                let id = self.legacy_id();
                let kind = if req == "get_queued_prompts" {
                    SimpleKind::QueueSnapshot
                } else if req == "remove_queued_prompt" {
                    SimpleKind::QueueRemoval
                } else if req == "edit_queued_prompt" {
                    SimpleKind::QueueEdit
                } else {
                    SimpleKind::QueueControl
                };
                self.pending_simple.push((id, api_id, kind));
                let mut forwarded = request.clone();
                forwarded["type"] = json!(req);
                forwarded["id"] = json!(id);
                if let Some(object) = forwarded.as_object_mut() {
                    object.remove("req");
                    object.remove("session_id");
                }
                vec![Outbound::Legacy(forwarded)]
            }
            "clear" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                vec![Outbound::Legacy(json!({"type": "clear", "id": id}))]
            }
            "rewind" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::OkOnHistory));
                vec![Outbound::Legacy(json!({
                    "type": "rewind",
                    "id": id,
                    "message_index": request["message_index"].as_u64().unwrap_or(1),
                }))]
            }
            "rewind_transcript" => {
                let target = match RewindAnchor::deserialize(&request["target"]) {
                    Ok(target) => target,
                    Err(error) => {
                        return Self::error_reply(
                            api_id,
                            ErrorCode::InvalidRequest,
                            &error.to_string(),
                        );
                    }
                };
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::OkOnHistory));
                vec![Outbound::Legacy(
                    json!({"type": "rewind_transcript", "id": id, "target": target}),
                )]
            }
            "get_history" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::History));
                vec![Outbound::Legacy(json!({"type": "get_history", "id": id}))]
            }
            "get_history_page" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::HistoryPage));
                vec![Outbound::Legacy(
                    json!({"type":"get_history_page", "id":id, "before":request["before"], "limit":request["limit"]}),
                )]
            }
            "get_history_image_chunk" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::HistoryImageChunk));
                vec![Outbound::Legacy(
                    json!({"type":"get_history_image_chunk", "id":id, "key":request["key"], "offset":request["offset"]}),
                )]
            }
            "list_session_images" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::SessionImages));
                vec![Outbound::Legacy(
                    json!({"type":"list_session_images", "id":id, "session_id":request["session_id"]}),
                )]
            }
            "get_session_image" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::SessionImage));
                vec![Outbound::Legacy(
                    json!({"type":"get_session_image", "id":id, "session_id":request["session_id"], "image_id":request["image_id"], "offset":request["offset"]}),
                )]
            }
            "session_context_breakdown" => {
                if ["session_id", "model", "cwd"]
                    .into_iter()
                    .any(|key| !request[key].is_null() && !request[key].is_string())
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "session_id, model, and cwd must be strings or null",
                    );
                }
                if ["summary_only", "include_snapcompact"]
                    .into_iter()
                    .any(|key| !request[key].is_null() && !request[key].is_boolean())
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "summary_only and include_snapcompact must be booleans",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ContextBreakdown));
                vec![Outbound::Legacy(json!({
                    "type": "session_context_breakdown", "id": id,
                    "session_id": request["session_id"], "model": request["model"], "cwd": request["cwd"],
                    "summary_only": request["summary_only"].as_bool().unwrap_or(false),
                    "include_snapcompact": request["include_snapcompact"].as_bool().unwrap_or(false),
                }))]
            }
            "session_shake_tools" => {
                let Ok(jcode_harness_api::ApiRequest::SessionShakeTools { session_id, action }) =
                    serde_json::from_value(request.clone())
                else {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Shake requires a session ID and a preview or apply action",
                    );
                };
                if self
                    .session_id
                    .as_ref()
                    .is_some_and(|attached| attached != &session_id)
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Shake target does not match the attached session",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ShakeTools));
                vec![Outbound::Legacy(json!({
                    "type": "session_shake_tools", "id": id, "session_id": session_id, "action": action,
                }))]
            }
            "get_compacted_history" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::CompactedHistory));
                vec![Outbound::Legacy(json!({
                    "type": "get_compacted_history", "id": id,
                    "visible_messages": request["visible_messages"].as_u64().unwrap_or(64),
                    "recent_history": false,
                }))]
            }
            // Answered from the stored record rather than the daemon: the
            // legacy protocol can only speak about the attached session, and
            // attaching to a session merely to read it would disturb the very
            // thing being previewed.
            "peek_session" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let limit = request["limit"].as_u64().unwrap_or(PEEK_LIMIT) as usize;
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    ApiEvent::History {
                        session_id: session_id.to_string(),
                        messages: Self::stored_tail(session_id, limit),
                        images: Vec::new(),
                    },
                ))]
            }
            // Answered locally before attach. The daemon treats `ping` as a
            // "lightweight control" request: when it arrives as the first
            // frame on a connection it is answered and the connection is then
            // closed, which would tear down the client's whole session. A
            // liveness probe must never cost the caller its connection, and
            // reaching the bridge already proves the socket is alive.
            "ping" if self.session_id.is_none() => {
                vec![Outbound::Reply(ServerFrame::reply(api_id, ApiEvent::Pong))]
            }
            "ping" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ping));
                vec![Outbound::Legacy(json!({"type": "ping", "id": id}))]
            }
            "list_sessions" => {
                let list_started = std::time::Instant::now();
                // A fresh API connection has not received a daemon `state`
                // snapshot. Start with every persisted record, then merge the
                // live snapshot so unattached dashboards and global event
                // subscribers discover complete session state.
                let limit = request["limit"].as_u64().map(|limit| limit as usize);
                let hidden = Self::hidden_side_question_ids();
                let scan_limit = limit.map(|limit| limit.saturating_add(hidden.len()));
                let mut ids: BTreeSet<String> =
                    Self::stored_session_ids(scan_limit).into_iter().collect();
                if limit.is_some() {
                    // Bookmarks are how users find old work, so a recency
                    // limit must never hide them from pickers.
                    ids.extend(Self::saved_session_ids());
                }
                let ids_loaded = list_started.elapsed();
                ids.extend(self.known_sessions.iter().cloned());
                if let Some(attached) = self.session_id.clone() {
                    ids.insert(attached);
                }
                ids.retain(|id| !hidden.contains(id));
                if let Some(limit) = limit {
                    let mut recent: Vec<_> = ids.into_iter().collect();
                    recent.sort_unstable_by_key(|id| {
                        std::cmp::Reverse(Self::session_modified_ms(id))
                    });
                    recent.truncate(limit);
                    ids = recent.into_iter().collect();
                }
                // Connected user clients must remain discoverable even when
                // their idle session is older than the persisted recency page.
                ids.extend(
                    self.user_connected_sessions
                        .iter()
                        .filter(|id| !hidden.contains(*id))
                        .cloned(),
                );
                let mut indexed_metadata = Self::indexed_session_metadata();
                let legacy_activity = Self::backfill_session_activity(&ids, &mut indexed_metadata);
                let metadata: BTreeMap<String, PersistedSessionMetadata> = ids
                    .iter()
                    .filter_map(|id| {
                        indexed_metadata
                            .get(id)
                            .map(|entry| {
                                let mut metadata = PersistedSessionMetadata::from(entry);
                                // Rows indexed before labels were tracked have
                                // no label. Only saved sessions can carry one,
                                // so the record read stays rare.
                                if metadata.saved && metadata.save_label.is_none() {
                                    metadata.save_label = Self::resolve_session_metadata(id)
                                        .and_then(|record| record.save_label);
                                }
                                metadata
                            })
                            .or_else(|| Self::resolve_session_metadata(id))
                            .map(|metadata| (id.clone(), metadata))
                    })
                    .collect();
                let mut metadata = metadata;
                Self::backfill_prompt_titles(&ids, &indexed_metadata, &mut metadata);
                let metadata_loaded = list_started.elapsed();
                for id in &ids {
                    if !self.session_dirs.contains_key(id)
                        && let Some(dir) = metadata
                            .get(id)
                            .and_then(|metadata| metadata.working_dir.clone())
                    {
                        self.session_dirs.insert(id.clone(), dir);
                    }
                }
                let _write_guard = Self::state_write_guard();
                let mut archive = Self::load_archive_state();
                if let Some(days) = archive.archive_after_days {
                    let cutoff = Self::now_ms().saturating_sub(u64::from(days) * 86_400_000);
                    let mut changed = false;
                    for id in &ids {
                        if self.session_id.as_ref() == Some(id)
                            || self.user_connected_sessions.contains(id)
                            || archive.sessions.contains_key(id)
                        {
                            continue;
                        }
                        if Self::session_modified_ms(id).is_some_and(|modified| modified < cutoff) {
                            archive.sessions.insert(id.clone(), Self::now_ms());
                            changed = true;
                        }
                    }
                    if changed && let Err(message) = Self::save_archive_state(&archive) {
                        return Self::error_reply(api_id, ErrorCode::Internal, &message);
                    }
                }
                let include_archived = request["include_archived"].as_bool().unwrap_or(false);
                let mut sessions: Vec<_> = ids
                    .into_iter()
                    .filter(|session_id| {
                        include_archived || !archive.sessions.contains_key(session_id)
                    })
                    .map(|session_id| SessionInfo {
                        user_connected: self.user_connected_sessions.contains(&session_id),
                        edit_stats: None,
                        context: catalog_activity::context_for_session(
                            &session_id,
                            &indexed_metadata,
                            &legacy_activity,
                        ),
                        parent_session_id: None,
                        agent_label: None,
                        swarm_status: None,
                        working_dir: self.session_dirs.get(&session_id).cloned(),
                        title: metadata
                            .get(&session_id)
                            .and_then(PersistedSessionMetadata::display_title),
                        status: if self.session_id.as_ref() == Some(&session_id) {
                            "attached".into()
                        } else {
                            "idle".into()
                        },
                        transcript_bytes: Self::transcript_bytes(&session_id),
                        saved: metadata.get(&session_id).is_some_and(|value| value.saved),
                        save_label: metadata
                            .get(&session_id)
                            .and_then(PersistedSessionMetadata::save_label),
                        updated_at_ms: metadata
                            .get(&session_id)
                            .and_then(|value| value.updated_at_ms)
                            .or_else(|| {
                                Self::session_modified_ms(&session_id).map(|value| value as i64)
                            }),
                        last_active_at_ms: metadata
                            .get(&session_id)
                            .and_then(|value| value.last_active_at_ms),
                        archived: archive.sessions.contains_key(&session_id),
                        archived_at_ms: archive.sessions.get(&session_id).copied(),
                        session_id,
                    })
                    .collect();
                jcode_harness_api::enrich_sessions_from_local_swarm_state(&mut sessions);
                jcode_harness_api::enrich_sessions_from_local_edit_stats(&mut sessions);
                // Swarm workers are other sessions. Their model, effort and
                // spend live in their own records, so the catalog carries them
                // rather than making every client re-read transcripts.
                let workers = jcode_harness_api::local_worker_meta(&sessions);
                let completed = list_started.elapsed();
                eprintln!(
                    "harness API bridge: list_sessions ids={:.1}ms metadata={:.1}ms total={:.1}ms count={}",
                    ids_loaded.as_secs_f64() * 1_000.0,
                    metadata_loaded.as_secs_f64() * 1_000.0,
                    completed.as_secs_f64() * 1_000.0,
                    sessions.len()
                );
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    ApiEvent::Sessions { sessions, workers },
                ))]
            }
            // Answered from the cached catalog. The daemon pushes it on attach
            // and on every change, so asking again would add a round trip to
            // an interaction (opening a picker) that must feel instant.
            "list_models" => {
                // Attach pushes the catalog, but a client that asks in the
                // same breath as attaching can beat it. Returning an empty
                // list would look like "no models exist", so ask the daemon
                // and answer when the catalog lands.
                if !self.model_catalog_loaded {
                    let id = self.legacy_id();
                    self.pending_simple.push((id, api_id, SimpleKind::Models));
                    return vec![Outbound::Legacy(
                        json!({"type": "get_model_catalog", "id": id, "subscribe_usage_updates": true}),
                    )];
                }
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    ApiEvent::Models {
                        session_id: self.session_id.clone().unwrap_or_default(),
                        models: self.available_models.clone(),
                        current: self.current_model.clone(),
                    },
                ))]
            }
            "get_runtime_info" => {
                if !self.model_catalog_loaded {
                    let id = self.legacy_id();
                    self.pending_simple
                        .push((id, api_id, SimpleKind::RuntimeInfo));
                    return vec![Outbound::Legacy(
                        json!({"type": "get_model_catalog", "id": id, "subscribe_usage_updates": true}),
                    )];
                }
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    self.runtime_info(),
                ))]
            }
            "notify_auth_changed" => {
                let provider = request["provider"].as_str().unwrap_or("");
                if provider.is_empty()
                    || provider.len() > 64
                    || !provider
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "invalid auth provider identifier",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ok));
                vec![Outbound::Legacy(json!({
                    "type": "notify_auth_changed", "id": id, "provider": provider
                }))]
            }
            "invalidate_usage" => {
                let legacy_type = match request["provider"].as_str().unwrap_or("") {
                    "claude" | "anthropic" => "invalidate_anthropic_usage",
                    "openai" => "invalidate_openai_usage",
                    _ => {
                        return Self::error_reply(
                            api_id,
                            ErrorCode::InvalidRequest,
                            "unsupported usage provider; supported: claude, openai",
                        );
                    }
                };
                let account_label = request["account_label"].as_str();
                if account_label.is_some_and(|label| {
                    label.is_empty() || label.len() > 128 || label.chars().any(char::is_control)
                }) {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "invalid account label",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Ok));
                vec![Outbound::Legacy(json!({
                    "type": legacy_type, "id": id, "account_label": account_label
                }))]
            }
            "set_api_key" | "clear_api_key" => {
                let provider = request["provider"].as_str().unwrap_or("");
                let Some((provider, env_keys, file_name)) = Self::credential_binding(provider)
                else {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "unsupported API-key provider; supported: claude-api, openai-api, openrouter, cursor, gemini, jcode, or any API-key catalog provider such as opencode-go",
                    );
                };
                let configured = req == "set_api_key";
                let key = request["api_key"].as_str().unwrap_or("");
                if configured
                    && (key.trim().is_empty()
                        || key.trim() != key
                        || key.contains(['\n', '\r', '\0']))
                {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "api_key must be a non-empty, trimmed, non-NUL single line",
                    );
                }
                if let Err(message) =
                    Self::write_credential(file_name, &env_keys, configured.then_some(key))
                {
                    return Self::error_reply(api_id, ErrorCode::Internal, &message);
                }
                let id = self.legacy_id();
                self.pending_simple.push((
                    id,
                    api_id,
                    SimpleKind::Credential {
                        provider: provider.to_string(),
                        configured,
                    },
                ));
                vec![Outbound::Legacy(json!({
                    "type": "notify_auth_changed",
                    "id": id,
                    "provider": provider,
                    "auth": {
                        "provider": provider,
                        "credential_source": "api_key_file",
                        "auth_method": "remote_tui_paste_api_key"
                    }
                }))]
            }
            "read_file" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let relative = request["path"].as_str().unwrap_or("");
                let max = request["max_bytes"]
                    .as_u64()
                    .unwrap_or(DEFAULT_FILE_BYTES)
                    .min(MAX_FILE_BYTES);
                match Self::read_session_file(session_id, relative, max) {
                    Ok((content, size, truncated)) => vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::FileContent {
                            session_id: session_id.to_string(),
                            path: relative.to_string(),
                            content,
                            size,
                            truncated,
                        },
                    ))],
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "find_files" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let query = request["query"].as_str().unwrap_or("");
                let limit = request["limit"]
                    .as_u64()
                    .unwrap_or(DEFAULT_FIND_LIMIT as u64)
                    .min(MAX_FIND_LIMIT as u64) as usize;
                match Self::find_session_files(session_id, query, limit) {
                    Ok(paths) => vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Files {
                            session_id: session_id.to_string(),
                            paths,
                        },
                    ))],
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "search_text" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let query = request["query"].as_str().unwrap_or("");
                let under = request["path"].as_str();
                let limit = request["limit"]
                    .as_u64()
                    .unwrap_or(DEFAULT_FIND_LIMIT as u64)
                    .min(MAX_FIND_LIMIT as u64) as usize;
                match Self::search_session_text(session_id, query, under, limit) {
                    Ok(matches) => vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::TextMatches {
                            session_id: session_id.to_string(),
                            matches,
                        },
                    ))],
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "get_todo_state" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                match Self::todo_home(session_id) {
                    Ok(home) => {
                        let mut state = jcode_todo_policy::store::load_snapshot(&home, session_id);
                        state.pending_swarm_await = Some(
                            !jcode_harness_api::pending_awaits_for_session(session_id).is_empty(),
                        );
                        vec![Outbound::Reply(ServerFrame::reply(
                            api_id,
                            ApiEvent::TodoState {
                                session_id: session_id.to_string(),
                                state,
                            },
                        ))]
                    }
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "ack_todo_follow_up" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let effects: jcode_todo_policy::TodoEffects =
                    match serde_json::from_value(request["effects"].clone()) {
                        Ok(effects) => effects,
                        Err(error) => {
                            return Self::error_reply(
                                api_id,
                                ErrorCode::InvalidRequest,
                                &format!("invalid effects: {error}"),
                            );
                        }
                    };
                let result = Self::todo_home(session_id).and_then(|home| {
                    jcode_todo_policy::store::apply_effects(&home, session_id, effects)
                        .map_err(|error| (ErrorCode::Internal, error.to_string()))
                });
                match result {
                    Ok(()) => vec![Outbound::Reply(ServerFrame::reply(api_id, ApiEvent::Ok))],
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "file_status" => {
                let session_id = request["session_id"].as_str().unwrap_or("");
                let relative = request["path"].as_str().unwrap_or("");
                match Self::session_file_status(session_id, relative) {
                    Ok((exists, kind, size, modified_ms)) => {
                        vec![Outbound::Reply(ServerFrame::reply(
                            api_id,
                            ApiEvent::FileStatus {
                                session_id: session_id.to_string(),
                                path: relative.to_string(),
                                exists,
                                kind,
                                size,
                                modified_ms,
                            },
                        ))]
                    }
                    Err((code, message)) => Self::error_reply(api_id, code, &message),
                }
            }
            "set_context_cap" => {
                let cap = &request["max_context_tokens"];
                if !cap.is_null() && cap.as_u64().and_then(|n| usize::try_from(n).ok()).is_none() {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "max_context_tokens must be a non-negative integer or null",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ContextCap));
                vec![Outbound::Legacy(json!({
                    "type": "set_context_cap", "id": id,
                    "session_id": request["session_id"],
                    "max_context_tokens": cap,
                }))]
            }
            "set_model" => {
                let model = request["model"].as_str().unwrap_or("");
                if model.is_empty() {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "set_model needs a non-empty `model`".into(),
                        },
                    ))];
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Model));
                vec![Outbound::Legacy(json!({
                    "type": "set_model",
                    "id": id,
                    "model": model,
                }))]
            }
            "cancel_pending_model" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ModelSwitchCancel));
                vec![Outbound::Legacy(json!({
                    "type": "cancel_pending_model_change",
                    "id": id,
                }))]
            }
            "set_speed_mode" => {
                let speed_mode = match serde_json::from_value::<jcode_harness_api::SpeedMode>(
                    request["speed_mode"].clone(),
                ) {
                    Ok(mode) => mode,
                    Err(_) => {
                        return Self::error_reply(
                            api_id,
                            ErrorCode::InvalidRequest,
                            "speed_mode must be standard, fast, or ultrafast",
                        );
                    }
                };
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::SpeedMode));
                vec![Outbound::Legacy(
                    json!({"type": "set_speed_mode", "id": id, "speed_mode": speed_mode}),
                )]
            }
            "set_reasoning_effort" => {
                let Some(effort) = request["effort"].as_str() else {
                    return vec![Outbound::Reply(ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "set_reasoning_effort needs a string `effort`".into(),
                        },
                    ))];
                };
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ReasoningEffort));
                vec![Outbound::Legacy(json!({
                    "type": "set_reasoning_effort",
                    "id": id,
                    "effort": effort,
                }))]
            }
            "compact" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Compact));
                vec![Outbound::Legacy(json!({"type": "compact", "id": id}))]
            }
            "search_sessions" => {
                let query = request["query"].as_str().unwrap_or("");
                if query.trim().is_empty() {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "query must not be empty",
                    );
                }
                let limit = request["limit"].as_u64().map(|limit| limit as u32);
                let started = std::time::Instant::now();
                let results = crate::search_index::search(query, limit);
                crate::timing::log(&format!(
                    "search_sessions query={:?} titles={} messages={} in {:.1}ms",
                    query,
                    results.titles.len(),
                    results.messages.len(),
                    crate::timing::ms(started.elapsed())
                ));
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    ApiEvent::SessionSearch {
                        query: results.query,
                        titles: results.titles,
                        messages: results.messages,
                        indexed_sessions: results.indexed_sessions,
                        truncated: results.truncated,
                    },
                ))]
            }
            "rename_session" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                let mut rename = json!({"type": "rename_session", "id": id});
                // An absent title clears it and restores the generated one, so
                // null and "" must stay distinguishable on the wire.
                if let Some(title) = request["title"].as_str() {
                    rename["title"] = json!(title);
                }
                vec![Outbound::Legacy(rename)]
            }
            "applet_action" | "close_applet" => {
                let session_id = request["session_id"]
                    .as_str()
                    .filter(|sid| !sid.is_empty())
                    .map(str::to_string)
                    .or_else(|| self.session_id.clone());
                let instance = request["instance"].as_str().unwrap_or("");
                let (Some(session_id), false) = (session_id, instance.is_empty()) else {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "session_id and instance are required",
                    );
                };
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                let mut out = json!({
                    "type": req,
                    "id": id,
                    "session_id": session_id,
                    "instance": instance,
                });
                if req == "applet_action" {
                    if !request["action"].is_object() {
                        self.pending_simple.pop();
                        return Self::error_reply(
                            api_id,
                            ErrorCode::InvalidRequest,
                            "applet_action requires an action object",
                        );
                    }
                    out["action"] = request["action"].clone();
                    out["state"] = request.get("state").cloned().unwrap_or(json!({}));
                    if let Some(key) = request["source_key"].as_str() {
                        out["source_key"] = json!(key);
                    }
                }
                vec![Outbound::Legacy(out)]
            }
            "set_session_env" => {
                let valid = request
                    .get("env")
                    .and_then(Value::as_object)
                    .is_some_and(|env| env.values().all(Value::is_string));
                if !valid {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "set_session_env requires an env object containing string values",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                vec![Outbound::Legacy(json!({
                    "type": "set_session_env", "id": id, "env": request["env"].clone()
                }))]
            }
            "set_session_saved" => {
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                let mut save = json!({
                    "type": "set_session_saved",
                    "id": id,
                    "saved": request["saved"].as_bool().unwrap_or(true),
                });
                if let Some(label) = request["label"].as_str() {
                    save["label"] = json!(label);
                }
                vec![Outbound::Legacy(save)]
            }
            "rewind_undo" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::OkOnHistory));
                vec![Outbound::Legacy(json!({"type": "rewind_undo", "id": id}))]
            }
            "cancel_soft_interrupts" => {
                let client_ids = match request.get("client_ids") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(ids)) if ids.iter().all(Value::is_string) => Some(
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect::<Vec<_>>(),
                    ),
                    Some(_) => {
                        return Self::error_reply(
                            api_id,
                            ErrorCode::InvalidRequest,
                            "`client_ids` must be an array of strings",
                        );
                    }
                };
                let id = self.legacy_id();
                match client_ids {
                    Some(client_ids) => {
                        // An older daemon ignores the filter and would clear
                        // the whole queue: refuse instead of over-cancelling.
                        if !self.soft_interrupt_ids_supported {
                            return Self::error_reply(
                                api_id,
                                ErrorCode::UnknownRequest,
                                "daemon does not advertise the soft_interrupt_ids capability",
                            );
                        }
                        self.pending_simple
                            .push((id, api_id, SimpleKind::SoftInterruptCancel));
                        vec![Outbound::Legacy(json!({
                            "type": "cancel_soft_interrupts",
                            "id": id,
                            "client_ids": client_ids,
                        }))]
                    }
                    None => {
                        self.pending_simple.push((id, api_id, SimpleKind::Ok));
                        vec![Outbound::Legacy(
                            json!({"type": "cancel_soft_interrupts", "id": id}),
                        )]
                    }
                }
            }
            "background_tool" => {
                let id = self.legacy_id();
                let kind = if self.soft_interrupt_ids_supported {
                    SimpleKind::BackgroundTool
                } else {
                    SimpleKind::Ok
                };
                self.pending_simple.push((id, api_id, kind));
                vec![Outbound::Legacy(
                    json!({"type": "background_tool", "id": id}),
                )]
            }
            "detach_session" => {
                let id = self.legacy_id();
                // Done is the release barrier. The daemon's initial Ack is
                // sent before it releases SDK callback ownership.
                self.pending_simple.push((id, api_id, SimpleKind::Detach));
                vec![Outbound::Legacy(
                    json!({"type": "prepare_disconnect", "id": id}),
                )]
            }
            "routing_gate_answer" => {
                let gate_id = request["gate_id"].as_str().unwrap_or("").trim();
                if gate_id.is_empty() {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Missing routing gate ID",
                    );
                }
                if !request["choice"].is_object() {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Missing routing gate choice",
                    );
                }
                let id = self.legacy_id();
                // The daemon reports done, or an error when no gate is waiting.
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                vec![Outbound::Legacy(json!({
                    "type": "routing_gate_answer",
                    "id": id,
                    "gate_id": gate_id,
                    "choice": request["choice"],
                }))]
            }
            "routing_intent" => {
                if !request["action"]["action"].is_string() {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Missing routing action",
                    );
                }
                let id = self.legacy_id();
                self.pending_simple.push((id, api_id, SimpleKind::Routing));
                vec![Outbound::Legacy(json!({
                    "type": "routing_intent",
                    "id": id,
                    "action": request["action"],
                }))]
            }
            "ask_response" => {
                let id = self.legacy_id();
                // The daemon acks receipt, then reports done or error once it
                // knows whether a waiting question took the answer.
                self.pending_simple.push((id, api_id, SimpleKind::OkOnDone));
                vec![Outbound::Legacy(json!({
                    "type": "ask_user_response",
                    "id": id,
                    "request_id": request["request_id"].as_str().unwrap_or(""),
                    "response": request.get("response").cloned().unwrap_or_else(|| json!({})),
                }))]
            }
            "permission_response" => {
                // The legacy protocol does not surface permission prompts on
                // this path, so the bridge never emits `permission_request`
                // and there is nothing for a response to answer. Say that,
                // rather than "not supported", which reads like a bug the
                // caller should work around. Clients discover this up front
                // via the absence of the `permissions` capability in `hello`.
                vec![Outbound::Reply(ServerFrame::reply(
                    api_id,
                    ApiEvent::Error {
                        code: ErrorCode::InvalidRequest,
                        message: "this server does not issue permission prompts \
                                  (no `permissions` capability), so there is nothing to respond to"
                            .into(),
                    },
                ))]
            }
            other => vec![Outbound::Reply(ServerFrame::reply(
                api_id,
                ApiEvent::Error {
                    code: ErrorCode::UnknownRequest,
                    message: format!("unknown request: {other}"),
                },
            ))],
        }
    }
}
