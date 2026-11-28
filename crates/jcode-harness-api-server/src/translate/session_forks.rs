use super::*;

impl BridgeState {
    pub(super) fn translate_fork_request(
        &mut self,
        req: &str,
        api_id: u64,
        request: &Value,
    ) -> Vec<Outbound> {
        match req {
            "fork_session" => {
                let id = self.legacy_id();
                self.pending_fork_id = Some((id, api_id));
                vec![Outbound::Legacy(json!({"type": "split", "id": id}))]
            }
            "fork_side_question" => {
                let Some(parent) = request["session_id"].as_str().filter(|id| !id.is_empty())
                else {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::InvalidRequest,
                        "Missing parent session ID",
                    );
                };
                if self.session_id.as_deref() != Some(parent) {
                    return Self::error_reply(
                        api_id,
                        ErrorCode::UnknownSession,
                        "Attach to the parent session before asking a side question.",
                    );
                }
                let id = self.legacy_id();
                self.pending_side_question_forks
                    .insert(id, (api_id, parent.to_string()));
                vec![Outbound::Legacy(
                    json!({"type": "split_side_question", "id": id}),
                )]
            }
            "transfer_session" => {
                let id = self.legacy_id();
                self.pending_fork_id = Some((id, api_id));
                vec![Outbound::Legacy(json!({"type": "transfer", "id": id}))]
            }
            _ => unreachable!("not a fork request"),
        }
    }

    pub(super) fn translate_fork_reply(&mut self, event: &Value) -> Vec<ServerFrame> {
        match event["type"].as_str().unwrap_or_default() {
            "side_question_forked" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some((api_id, parent)) = self.pending_side_question_forks.remove(&id) else {
                    return vec![];
                };
                match serde_json::from_value::<jcode_harness_api::SideQuestionTopic>(
                    event["topic"].clone(),
                ) {
                    Ok(topic) if topic.parent_session_id == parent => vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::SideQuestionForked { topic },
                    )],
                    Ok(_) => Self::invalid_daemon_event(
                        Some(api_id),
                        "Side-question fork returned a different parent".into(),
                    ),
                    Err(error) => Self::invalid_daemon_event(Some(api_id), error.to_string()),
                }
            }
            "split_response" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let Some((_, api_id)) = self
                    .pending_fork_id
                    .take()
                    .filter(|(legacy_id, _)| *legacy_id == id)
                else {
                    return vec![];
                };
                let session_id = event["new_session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let metadata = Self::resolve_session_metadata(&session_id);
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::SessionForked {
                        session: SessionInfo {
                            user_connected: self.user_connected_sessions.contains(&session_id),
                            edit_stats: None,
                            context: None,
                            parent_session_id: None,
                            agent_label: None,
                            swarm_status: None,
                            transcript_bytes: Self::transcript_bytes(&session_id),
                            saved: metadata.as_ref().is_some_and(|value| value.saved),
                            updated_at_ms: Self::session_modified_ms(&session_id)
                                .map(|value| value as i64),
                            last_active_at_ms: metadata
                                .as_ref()
                                .and_then(|value| value.last_active_at_ms),
                            session_id,
                            working_dir: metadata
                                .as_ref()
                                .and_then(|value| value.working_dir.clone()),
                            title: event["new_session_name"]
                                .as_str()
                                .map(str::to_string)
                                .or_else(|| {
                                    metadata
                                        .as_ref()
                                        .and_then(PersistedSessionMetadata::display_title)
                                }),
                            status: "idle".into(),
                            archived: false,
                            archived_at_ms: None,
                            save_label: metadata
                                .as_ref()
                                .and_then(PersistedSessionMetadata::save_label),
                        },
                    },
                )]
            }
            _ => unreachable!("not a fork reply"),
        }
    }

    pub(super) fn hidden_side_question_ids() -> BTreeSet<String> {
        let Some(home) = Self::jcode_home() else {
            return BTreeSet::new();
        };
        let path = home
            .join("state")
            .join(jcode_session_types::SIDE_QUESTION_VISIBILITY_DIR);
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()?.to_str()? != "json"
                    || !std::fs::read(&path).is_ok_and(|bytes| bytes == b"true")
                {
                    return None;
                }
                Some(path.file_stem()?.to_str()?.to_owned())
            })
            .collect()
    }
}
