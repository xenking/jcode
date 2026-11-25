//! Provider transport prewarming and post-compaction request metadata.

use super::*;

impl Agent {
    pub(in crate::agent) async fn prepare_stream_request(
        &mut self,
        event_tx: &crate::stream_batching::BatchingSender,
    ) -> Result<(
        Vec<ToolDefinition>,
        crate::prompt::SplitSystemPrompt,
        Vec<Message>,
    )> {
        // Start provider transport setup before deriving and potentially
        // compacting the request history. This is the first point where the
        // stable request settings are available.
        let mut tools = self.tool_definitions().await;
        self.freeze_static_prompt();
        self.record_prompt_inputs()?;
        let mut split_prompt = self.build_system_prompt_split(None);
        if !self.is_swarm_worker() {
            self.provider
                .prewarm(&tools, &split_prompt.static_part)
                .await;
        }
        let (messages, compaction_event) = self.messages_for_provider(Some(event_tx)).await?;
        if let Some(event) = compaction_event {
            let mut notification = crate::compaction::compaction_server_event(&event);
            if let ServerEvent::Compaction { divider, .. } = &mut notification
                && let Some(notice) = self.route_switch_compaction_notice(&event)
            {
                logging::info(&notice);
                *divider = Some(notice);
            }
            // Reset cache tracker and tool lock on compaction since the message history changes
            self.cache_tracker.reset();
            self.kv_cache_monitor.reset();
            self.locked_tools = None;
            logging::info(&format!(
                "Context compacted ({}{})",
                event.trigger,
                event
                    .pre_tokens
                    .map(|t| format!(" {} tokens", t))
                    .unwrap_or_default()
            ));
            let _ = event_tx.send(notification);
            if let Some(usage) = crate::agent::compaction_context_usage_event(
                event.post_tokens,
                Some(self.effective_context_budget().await),
            ) {
                let _ = event_tx.send(usage);
            }
            // Compaction clears the tool lock, so rebuild the foreground
            // request metadata rather than relying on the pre-compaction snapshot.
            tools = self.tool_definitions().await;
            split_prompt = self.build_system_prompt_split(None);
        }

        let tools = self.prepare_tools_for_request(tools, &messages)?;
        Ok((tools, split_prompt, messages))
    }
}
