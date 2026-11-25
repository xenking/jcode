use super::{
    CompactionManager, CompactionProjection, CompactionReplay, Message, Role,
    format_compaction_plan, replay,
};

impl CompactionManager {
    pub fn messages_for_api_session(
        &mut self,
        session: &mut crate::session::Session,
        supports_image_input: bool,
        accepts_native: bool,
    ) -> CompactionReplay {
        let session_id = session.id().to_owned();
        let (checkpoint_changed, warn_missing_native_summary, mut appended_plan_update) =
            self.prepare_checkpoint(&session_id, session.provider_messages(), accepts_native);
        if let Some(message) = &mut appended_plan_update {
            self.append_plan_update(session, message);
        }
        let retained =
            replay::retained_checkpoint_messages(&session.messages(), self.compacted_count);
        let projection = self.project_messages(
            session.provider_messages(),
            supports_image_input,
            accepts_native,
            retained,
        );
        CompactionReplay {
            messages: projection.messages,
            checkpoint_changed,
            appended_plan_update,
            warn_missing_native_summary,
        }
    }

    /// Use the caller's materialized suffix when it owns a separate history.
    pub fn messages_for_api_session_with(
        &mut self,
        session: &mut crate::session::Session,
        all_messages: &[Message],
        supports_image_input: bool,
        accepts_native: bool,
    ) -> CompactionReplay {
        let (checkpoint_changed, warn_missing_native_summary, mut appended_plan_update) =
            self.prepare_checkpoint(session.id(), all_messages, accepts_native);
        let retained = if all_messages.len() == session.messages().len() {
            replay::retained_checkpoint_messages(&session.messages(), self.compacted_count)
        } else {
            Vec::new()
        };
        let mut projection =
            self.project_messages(all_messages, supports_image_input, accepts_native, retained);
        if let Some(message) = &mut appended_plan_update {
            self.append_plan_update(session, message);
            projection.messages.push(message.clone());
        }
        CompactionReplay {
            messages: projection.messages,
            checkpoint_changed,
            appended_plan_update,
            warn_missing_native_summary,
        }
    }

    fn append_plan_update(&mut self, session: &mut crate::session::Session, message: &mut Message) {
        // Give every emitted update a fixed position. A synthesized tail would
        // move behind the next assistant/tool result and break Responses replay.
        session.add_message_with_display_role(
            Role::User,
            message.content.clone(),
            Some(crate::session::StoredDisplayRole::System),
        );
        message.timestamp = session
            .messages()
            .last()
            .and_then(|stored| stored.timestamp);
        self.notify_message_added_with(message);
    }

    /// Assemble the current checkpoint without advancing compaction, reading a
    /// pending plan, or rewriting session state.
    pub fn inspect_provider_messages(
        &self,
        session: &crate::session::Session,
        all_messages: &[Message],
        supports_image_input: bool,
        accepts_native: bool,
    ) -> CompactionProjection {
        let retained =
            replay::retained_checkpoint_messages(&session.messages(), self.compacted_count);
        self.project_messages(all_messages, supports_image_input, accepts_native, retained)
    }

    fn prepare_checkpoint(
        &mut self,
        session_id: &str,
        all_messages: &[Message],
        accepts_native: bool,
    ) -> (bool, bool, Option<Message>) {
        self.check_and_apply_compaction_with(all_messages);
        let discarded = self.discard_oversized_openai_native_compaction();
        let mut checkpoint_changed = self.recover_native_text(all_messages, accepts_native);
        checkpoint_changed |= discarded || self.decisions_updated || self.last_compaction.is_some();
        let mut current_plan = None;
        if let Some(refresh_plan) = self
            .active_summary
            .as_ref()
            .map(|summary| !accepts_native || summary.openai_encrypted_content.is_none())
            && (refresh_plan || self.plan_context.is_none())
        {
            match format_compaction_plan(session_id) {
                Ok(plan) => {
                    if refresh_plan
                        && (!plan.is_empty()
                            || crate::todo::todos_exist(session_id).unwrap_or(false))
                    {
                        current_plan = replay::current_plan_update(
                            &plan,
                            self.plan_context.as_deref().unwrap_or(&plan),
                            self.active_messages(all_messages),
                        );
                    }
                    if self.plan_context.is_none() {
                        self.plan_context = Some(plan);
                        checkpoint_changed = true;
                    }
                }
                Err(error) if self.plan_context.is_none() => {
                    crate::logging::warn(&format!(
                        "[compaction] Could not load plan for {session_id}: {error}"
                    ));
                    // Keep the prefix stable if the store later becomes readable.
                    self.plan_context = Some(String::new());
                    checkpoint_changed = true;
                }
                _ => {}
            }
        }
        let warn_missing_native_summary = self.take_missing_native_summary_warning(accepts_native);
        (
            checkpoint_changed,
            warn_missing_native_summary,
            current_plan,
        )
    }
}
