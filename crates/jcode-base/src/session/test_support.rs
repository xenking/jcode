//! Fixture hooks for tests that need to build sessions in states the
//! production API cannot reach (for example hand-edited transcripts).
//!
//! Compiled only for this crate's tests and for downstream test targets that
//! enable the `test-support` feature.

use super::{Session, StoredMessage};

impl Session {
    pub fn messages_mut_for_tests(&mut self) -> &mut Vec<StoredMessage> {
        &mut self.messages
    }

    pub fn set_id_for_tests(&mut self, id: String) {
        self.id = id;
    }

    pub fn set_saved_for_tests(&mut self, saved: bool) {
        self.saved = saved;
    }

    pub fn set_custom_title_for_tests(&mut self, title: Option<String>) {
        self.custom_title = title;
    }

    pub fn set_save_label_for_tests(&mut self, label: Option<String>) {
        self.save_label = label;
    }

    pub fn set_replay_events_for_tests(&mut self, events: Vec<super::StoredReplayEvent>) {
        self.replay_events = events;
    }

    pub fn set_is_canary_for_tests(&mut self, is_canary: bool) {
        self.is_canary = is_canary;
    }

    pub fn set_testing_build_for_tests(&mut self, build: Option<String>) {
        self.testing_build = build;
    }

    pub fn speed_selection_mut_for_tests(&mut self) -> &mut Option<super::StoredSpeedSelection> {
        &mut self.speed_selection
    }

    pub fn replay_events_mut_for_tests(&mut self) -> &mut Vec<super::StoredReplayEvent> {
        &mut self.replay_events
    }

    pub fn set_created_at_for_tests(&mut self, at: chrono::DateTime<chrono::Utc>) {
        self.created_at = at;
    }

    pub fn set_updated_at_for_tests(&mut self, at: chrono::DateTime<chrono::Utc>) {
        self.updated_at = at;
    }

    pub fn set_last_active_at_for_tests(&mut self, at: Option<chrono::DateTime<chrono::Utc>>) {
        self.last_active_at = at;
    }

    pub fn set_last_provider_request_at_for_tests(
        &mut self,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.last_provider_request_at = at;
    }

    pub fn set_last_provider_cache_domain_for_tests(
        &mut self,
        domain: Option<crate::kv_cache_monitor::RequestCacheDomain>,
    ) {
        self.last_provider_cache_domain = domain;
    }

    pub fn set_status_for_tests(&mut self, status: super::SessionStatus) {
        self.status = status;
    }
}
