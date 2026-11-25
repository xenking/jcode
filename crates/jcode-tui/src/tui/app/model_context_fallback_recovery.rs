//! Provider fallback and context recovery.

use super::*;

impl App {}

impl App {}

impl App {
    /// Attempt automatic compaction and retry when context limit is exceeded.
    /// Returns true if the retry succeeded.
    pub(in super::super) async fn try_auto_compact_and_retry(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) -> bool {
        if self.is_remote || !self.provider.supports_compaction() {
            return false;
        }

        self.push_display_message(DisplayMessage::system(
            "⚠️ Context limit exceeded - auto-compacting and retrying...".to_string(),
        ));

        // Force the compaction manager to think we're at the limit
        let compaction = self.registry.compaction();
        let compact_started = match compaction.try_write() {
            Ok(mut manager) => {
                let mut provider_messages = self.materialized_provider_messages();
                manager.update_observed_input_tokens(self.context_limit);
                let usage = manager.context_usage_with(&provider_messages);
                if usage > 1.5 {
                    let recovery = manager.recover_within_budget(&mut provider_messages);
                    if recovery.did_anything() {
                        self.messages = provider_messages;
                        self.sync_session_compaction_state_from_manager(&manager);
                        drop(manager);
                        self.reset_state_for_compaction_retry();

                        self.push_display_message(DisplayMessage::system(format!(
                            "{} Retrying...",
                            recovery.summary_line(usage)
                        )));
                        return self.run_compaction_retry_turn(terminal, event_stream).await;
                    }
                    false
                } else {
                    match manager.force_compact_with(&provider_messages, self.provider.clone()) {
                        Ok(()) => true,
                        Err(_) => match manager.hard_compact_with(&provider_messages) {
                            Ok(_) => {
                                self.sync_session_compaction_state_from_manager(&manager);
                                drop(manager);
                                self.reset_state_for_compaction_retry();

                                self.push_display_message(DisplayMessage::system(
                                    "✓ Context compacted (emergency). Retrying...".to_string(),
                                ));
                                return self
                                    .run_compaction_retry_turn(terminal, event_stream)
                                    .await;
                            }
                            Err(_) => false,
                        },
                    }
                }
            }
            Err(_) => false,
        };

        if !compact_started {
            return false;
        }

        // Wait for compaction to finish (up to 60s), reacting to Bus event
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        self.status = ProcessingStatus::RunningTool("compacting context...".to_string());
        let mut bus_rx = Bus::global().subscribe();

        loop {
            if std::time::Instant::now() >= deadline {
                self.push_display_message(DisplayMessage::error(
                    "Auto-compaction timed out.".to_string(),
                ));
                return false;
            }

            // Redraw UI while we wait
            if let Err(error) = terminal.draw(|frame| crate::tui::ui::draw(frame, self)) {
                crate::logging::warn(&format!("Cannot draw compaction progress: {error}"));
                return false;
            }

            let compaction = self.registry.compaction();
            let done = if let Ok(mut manager) = compaction.try_write() {
                let provider_messages = self.materialized_provider_messages();
                if let Some(event) = manager.poll_compaction_event_with(&provider_messages) {
                    self.sync_session_compaction_state_from_manager(&manager);
                    self.handle_compaction_event(event, None);
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if done {
                break;
            }

            // Wait for Bus notification or timeout (instead of sleep-polling)
            let timeout = tokio::time::sleep(Duration::from_secs(1));
            tokio::select! {
                _ = bus_rx.recv() => {}
                _ = timeout => {}
            }
        }

        self.push_display_message(DisplayMessage::system(
            "✓ Context compacted. Retrying...".to_string(),
        ));

        self.reset_state_for_compaction_retry();

        // Retry the turn
        self.run_compaction_retry_turn(terminal, event_stream).await
    }
}
