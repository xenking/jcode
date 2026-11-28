use super::{Role, Session, is_scheduled_task_message};

impl Session {
    pub(crate) fn catalog_activity(&self) -> jcode_session_types::SessionCatalogActivity {
        use jcode_session_types::{SessionCatalogActivity, SessionOrigin, legacy_agent_session};
        let belongs_to_session = |time: chrono::DateTime<chrono::Utc>| {
            self.parent_id.is_none() || time >= self.created_at
        };
        let is_agent = self.agent_started
            || self.is_swarm_worker
            || self.hidden
            || self.side_question_message_start.is_some()
            || legacy_agent_session(
                self.parent_id.is_some(),
                self.title.as_deref(),
                self.system_prompt.as_deref(),
            )
            || (self.parent_id.is_some()
                && self.messages.iter().rev().any(|message| {
                    message.timestamp.is_some_and(belongs_to_session)
                        && is_scheduled_task_message(message)
                }));
        SessionCatalogActivity {
            origin: if is_agent {
                SessionOrigin::Agent
            } else {
                SessionOrigin::User
            },
            last_agent_activity_at_ms: self
                .messages
                .iter()
                .rev()
                .filter(|message| message.role == Role::Assistant)
                .filter_map(|message| message.timestamp)
                .find(|time| belongs_to_session(*time))
                .map(|time| time.timestamp_millis()),
            last_message_at_ms: Some(
                self.messages
                    .iter()
                    .filter(|message| message.counts_as_catalog_message())
                    .filter_map(|message| message.timestamp)
                    .filter(|time| belongs_to_session(*time))
                    .max()
                    .map(|time| time.timestamp_millis()),
            ),
        }
    }
}
