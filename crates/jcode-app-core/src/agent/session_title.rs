//! Post-turn session title generation.
//!
//! The timing rules live in `jcode_session_types::plan_title_update`; this is
//! the part that talks to a cheap model and publishes the result.

use super::*;
use crate::session_title as titles;

impl Agent {
    /// Generate or refine this session's title when the turn is a title moment.
    ///
    /// Never writes over a user rename or bookmark label, and never spends a
    /// call for internal-only transcripts. The title is persisted and, when a
    /// client event channel is given, announced with the same
    /// `SessionRenamed` event used by explicit renames so every list and index
    /// sees it.
    pub(crate) async fn refresh_session_title(
        &mut self,
        event_tx: Option<&mpsc::UnboundedSender<ServerEvent>>,
    ) {
        // Debug sessions include both test sessions and headless swarm workers.
        // Workers use their assigned task label instead of a second model call.
        if self.session.is_debug || self.session.is_ephemeral() {
            return;
        }
        let (action, source) = titles::plan_for_session(&self.session, self.auto_title_owned);
        let jcode_session_types::TitleAction::Generate(stage) = action else {
            return;
        };
        let Some(source) = source else {
            return;
        };
        let provider = Arc::clone(&self.provider);
        let generated = match titles::generate_title(&provider, &self.session, &source).await {
            Ok(title) => title,
            Err(error) => {
                logging::info(&format!(
                    "Session title not generated for {} ({stage:?}): {error}",
                    self.session.id
                ));
                return;
            }
        };
        match titles::apply_title(&mut self.session, &generated) {
            Ok(true) => {
                self.auto_title_owned = true;
                crate::session_list_cache::invalidate();
                logging::info(&format!(
                    "Session title set for {} ({stage:?}): {:?}",
                    self.session.id, generated
                ));
                if let Some(event_tx) = event_tx
                    && event_tx
                        .send(ServerEvent::SessionRenamed {
                            session_id: self.session.id.clone(),
                            title: Some(generated),
                            display_title: self.session.display_title_or_name().to_string(),
                        })
                        .is_err()
                {
                    logging::debug(&format!(
                        "Session title persisted for {}, but rename event receiver disconnected",
                        self.session.id
                    ));
                }
            }
            Ok(false) => {}
            Err(error) => logging::warn(&format!(
                "Failed to persist session title for {}: {error}",
                self.session.id
            )),
        }
    }
}

#[cfg(test)]
#[path = "session_title_tests.rs"]
mod tests;
