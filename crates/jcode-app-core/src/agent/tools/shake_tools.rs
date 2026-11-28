use super::*;
use crate::compaction::{ShakeOutcome, ShakeReason};
use anyhow::Context as _;
use jcode_session_types::{
    ContextActionMeasurement, ShakeToolsAction, ShakeToolsResult, ShakeToolsStats,
};
use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;
use std::time::SystemTime;

static CONFIRMATIONS: LazyLock<RandomState> = LazyLock::new(RandomState::new);

impl Agent {
    pub(crate) fn shake_tools(
        &mut self,
        action: ShakeToolsAction,
        now: SystemTime,
    ) -> Result<(ShakeToolsResult, Option<CompactionEvent>)> {
        let compaction = self.registry.compaction();
        let Ok(mut manager) = compaction.try_write() else {
            anyhow::bail!("Session is busy. Try Shake again when it is idle.");
        };
        if manager.is_compacting() {
            anyhow::bail!("Session is compacting. Try Shake again after it finishes.");
        }
        let config = manager.manual_tool_shake_config();
        let start = manager.active_start(self.session.provider_messages());
        let outcome = self.session.preview_shake_stored_messages(start, &config);
        let ShakeOutcome::Shaken {
            regions_elided,
            tokens_saved,
        } = outcome
        else {
            return Ok((ShakeToolsResult::NoChange, None));
        };
        let stats = ShakeToolsStats {
            outputs_elided: regions_elided,
            estimated_tokens_saved: tokens_saved as u64,
        };
        let cache_at_risk = self.provider_session_id.is_some()
            || self.session.provider_session_id().is_some()
            || manager
                .expired_cache_response(now, self.cache_expiry_ttl_secs(&manager))
                .is_none();
        let prefix = self
            .session
            .provider_message_prefix_hashes()
            .last()
            .copied();
        let confirmation = format!(
            "{:016x}",
            CONFIRMATIONS.hash_one((
                self.session.id(),
                prefix,
                start,
                config,
                self.session.last_provider_request_at(),
                self.provider.name(),
                self.provider.model(),
                &self.provider_session_id,
                &self.session.provider_session_id(),
                cache_at_risk,
            ))
        );
        match action {
            ShakeToolsAction::Preview => {
                return Ok((
                    ShakeToolsResult::Preview {
                        confirmation,
                        cache_at_risk,
                        stats,
                    },
                    None,
                ));
            }
            ShakeToolsAction::Apply {
                confirmation: supplied,
            } if supplied != confirmation => {
                anyhow::bail!(
                    "The Shake preview is no longer current. Preview again before confirming."
                );
            }
            ShakeToolsAction::Apply { .. } => {}
        }

        let started = Instant::now();
        let pre = manager.measure_token_estimate_with(self.session.provider_messages()) as u64;
        let mut staged = self.session.clone();
        anyhow::ensure!(
            staged.shake_stored_messages(start, &config) == outcome,
            "The Shake plan changed before it could be saved."
        );
        let post = manager.measure_token_estimate_with(staged.provider_messages()) as u64;
        let reason = ShakeReason::Manual;
        let event = CompactionEvent {
            trigger: reason.label().into(),
            method: Some("shake".into()),
            pre_tokens: Some(pre),
            post_tokens: Some(post),
            tokens_saved: Some(pre.saturating_sub(post)),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            messages_dropped: None,
            messages_compacted: Some(regions_elided),
            summary_chars: None,
            active_messages: None,
        };
        staged.forget_provider_session();
        staged.record_compaction_event(&event, Some("shake"), None);
        staged
            .save_rewritten_context()
            .context("Could not save shaken tool outputs")?;
        self.session = staged;
        manager.record_shake(reason, outcome);
        drop(manager);
        self.note_compaction_applied();
        Ok((
            ShakeToolsResult::Applied {
                stats,
                measurement: Some(ContextActionMeasurement {
                    before_tokens: pre,
                    after_tokens: post,
                }),
            },
            Some(event),
        ))
    }
}
