use super::*;

impl Agent {
    /// Clear conversation history
    pub fn clear(&mut self) {
        self.session.mark_closed();
        self.finish_concurrency_tracking();
        self.persist_session_best_effort("pre-clear session close state");

        let mut new_session = Session::create(None, None);
        // begin_concurrency_tracking below writes the presence markers.
        new_session.mark_active_unregistered();
        new_session.set_model(Some(self.provider_model()));
        new_session.set_provider_key(self.provider_key_for_new_session());
        // A cleared conversation keeps the kind of agent it was and where it ran.
        new_session.inherit_from(
            &self.session,
            crate::session::Inherit::BUILD_UNDER_TEST
                | crate::session::Inherit::WORKING_DIR
                | crate::session::Inherit::AGENT_PROFILE,
        );
        // Unlike a plain copy, this also tells presence the session is internal.
        new_session.set_debug(self.session.is_debug());
        new_session.ensure_initial_session_context_message();

        self.session = new_session;
        self.provider.set_session_id(self.session.id());
        self.begin_concurrency_tracking();
        self.refresh_profile_tool_policy();
        self.refresh_agents_md_snapshot();
        self.reconcile_explicit_provider_pin_route();
        self.reset_runtime_state_for_session_change();
        self.provider_session_id = None;
        self.seed_compaction_from_session();
    }

    /// Clear provider session so the next turn sends full context.
    pub fn reset_provider_session(&mut self) {
        self.provider_session_id = None;
        self.session.set_provider_session_id(None);
        self.persist_session_best_effort("provider session reset");
    }

    /// Rewind the conversation to a 1-based visible transcript message index.
    ///
    /// The index is interpreted against the same rendered transcript the TUI
    /// numbers in `/rewind` (user/assistant entries only, tool cards and
    /// system notices excluded). Mapping through raw stored messages instead
    /// would count tool-result messages the UI never numbers, sending
    /// `/rewind N` far earlier than the on-screen message N (issue #432).
    ///
    /// Provider-side resumable sessions are reset so the next request sends the
    /// truncated context from scratch instead of continuing from a stale upstream
    /// conversation.
    pub fn rewind_to_message(&mut self, message_index: usize) -> Result<usize, String> {
        let targets = self.session.rewind_target_stored_indices();
        let message_count = targets.len();
        if message_index == 0 || message_index > message_count {
            return Err(format!(
                "Invalid message number: {}. Valid range: 1-{}",
                message_index, message_count
            ));
        }
        self.rewind_stored_prefix(targets[message_index - 1] + 1, message_count)?;
        Ok(message_count - message_index)
    }

    pub fn rewind_transcript(
        &mut self,
        anchor: &jcode_session_types::RewindAnchor,
    ) -> Result<usize, String> {
        let (messages, _, _) = crate::session::render_messages_and_images_with_compacted_history(
            &self.session,
            usize::MAX,
        );
        let (keys, stored_indices): (Vec<_>, Vec<_>) = messages
            .into_iter()
            .filter_map(|message| {
                Some((
                    jcode_session_types::RewindMessageKey::new(&message.role, &message.content)?,
                    message.stored_index?,
                ))
            })
            .unzip();
        let target = anchor.resolve(&keys).ok_or_else(|| {
            "This rewind point no longer matches the session history or is ambiguous.".to_string()
        })?;
        let stored_index = stored_indices[target];
        let visible_count = self.session.rewind_target_count();
        let removed = stored_indices
            .iter()
            .filter(|&&index| index > stored_index)
            .count();
        self.rewind_stored_prefix(stored_index + 1, visible_count)?;
        Ok(removed)
    }

    fn rewind_stored_prefix(
        &mut self,
        stored_len: usize,
        message_count: usize,
    ) -> Result<(), String> {
        let compaction = self.registry.compaction();
        let mut manager = compaction
            .try_write()
            .map_err(|_| "Compaction state is busy. Try rewinding again.")?;
        let mut snapshot = RewindUndoSnapshot {
            messages: self.session.messages().to_vec(),
            compaction: self.session.compaction().cloned(),
            removed_compaction_events: Vec::new(),
            provider_session_id: self.provider_session_id.clone(),
            session_provider_session_id: self.session.provider_session_id().map(str::to_owned),
            visible_message_count: message_count,
        };
        if let Some(cutoff) = self
            .session
            .messages()
            .get(stored_len)
            .map(|message| message.timestamp)
        {
            if snapshot
                .compaction
                .as_ref()
                .is_some_and(|state| state.compacted_count > stored_len)
            {
                self.session.set_compaction(None);
            }
            // A later compaction can display a discarded summary or occupancy.
            let removed = self.session.drain_compaction_replay_events_since(cutoff);
            snapshot.removed_compaction_events.extend(removed);
            // Truncation forces a full snapshot, including the pruned replay.
            self.session.truncate_messages(stored_len);
            self.refresh_tool_call_snapshot();
        }
        self.rewind_undo_snapshot = Some(snapshot);
        self.session.touch();
        self.provider_session_id = None;
        self.session.set_provider_session_id(None);
        self.cache_tracker.reset();
        self.kv_cache_monitor.reset();
        self.locked_tools = None;
        self.reset_tool_output_tracking();
        self.reset_compaction_manager_from_session(&mut manager);
        drop(manager);
        self.persist_session_best_effort("conversation rewind");
        Ok(())
    }

    pub fn undo_rewind(&mut self) -> Result<usize, String> {
        let compaction = self.registry.compaction();
        let mut manager = compaction
            .try_write()
            .map_err(|_| "Compaction state is busy. Try undoing again.")?;
        let Some(snapshot) = self.rewind_undo_snapshot.take() else {
            return Err("No rewind to undo.".to_string());
        };

        let current_count = self.session.rewind_target_count();
        let restored = snapshot.visible_message_count.saturating_sub(current_count);
        self.session.set_compaction(snapshot.compaction);
        self.session
            .restore_replay_events(snapshot.removed_compaction_events);
        self.session.replace_messages(snapshot.messages);
        self.refresh_tool_call_snapshot();
        self.provider_session_id = snapshot.provider_session_id;
        self.session
            .set_provider_session_id(snapshot.session_provider_session_id);
        self.session.touch();
        self.cache_tracker.reset();
        self.kv_cache_monitor.reset();
        self.locked_tools = None;
        self.reset_tool_output_tracking();
        self.reset_compaction_manager_from_session(&mut manager);
        drop(manager);
        self.persist_session_best_effort("conversation rewind undo");
        Ok(restored)
    }
}
