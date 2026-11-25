//! Automatic advisor review at turn end (`[agents.advisor] auto_review`).
//!
//! pi_agent_rust reviews every non-trivial turn. Here the review runs off the
//! turn path: a NOTE is only logged, while a CONCERN or BLOCKER is delivered
//! as a completed background task that wakes the agent, so it acts on the
//! verdict even when the turn already ended.

use super::Agent;
use crate::advisor::{self, AdvisorSettings, VerdictLevel};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// At most this many delivered verdicts per session per window, so a review
/// can never drive the agent in a loop.
const MAX_DELIVERIES: usize = 3;
const DELIVERY_WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct DeliveryLog {
    times: VecDeque<Instant>,
    advice: VecDeque<u64>,
}

fn delivery_logs() -> &'static Mutex<HashMap<String, DeliveryLog>> {
    static LOGS: OnceLock<Mutex<HashMap<String, DeliveryLog>>> = OnceLock::new();
    LOGS.get_or_init(Mutex::default)
}

fn advice_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.trim().hash(&mut hasher);
    hasher.finish()
}

/// Whether a verdict may wake the session now. Repeated advice is dropped.
pub(crate) fn admit_delivery(session_id: &str, advice: &str, now: Instant) -> bool {
    let Ok(mut logs) = delivery_logs().lock() else {
        return false;
    };
    let log = logs.entry(session_id.to_string()).or_default();
    while log
        .times
        .front()
        .is_some_and(|time| now.duration_since(*time) > DELIVERY_WINDOW)
    {
        log.times.pop_front();
    }
    let hash = advice_hash(advice);
    if log.times.len() >= MAX_DELIVERIES || log.advice.contains(&hash) {
        return false;
    }
    log.times.push_back(now);
    log.advice.push_back(hash);
    if log.advice.len() > 16 {
        log.advice.pop_front();
    }
    true
}

impl Agent {
    /// Review the turn that started at `start_message_index`, off the turn path.
    pub(super) fn spawn_advisor_auto_review(&self, turn_ok: bool, start_message_index: usize) {
        if !turn_ok {
            return;
        }
        let Some(settings) = AdvisorSettings::from_config() else {
            return;
        };
        if !settings.auto_review
            || !crate::tool::session_allows_tool(self.session_id(), "advisor")
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let messages: Vec<_> = self
            .session
            .messages()
            .iter()
            .map(|message| message.to_message())
            .collect();
        let session_id = self.session_id().to_string();
        let plan = advisor::plan_lines(&session_id);
        // Include the user message that started the turn.
        let since = start_message_index.saturating_sub(1);
        let digest = advisor::build_digest(&messages, since, &plan, settings.digest_tokens);
        // Like pi: a turn without tool calls is not worth a review. A turn that
        // already consulted the advisor got its review.
        let consulted = messages.iter().skip(since).any(|message| {
            message.content.iter().any(|block| {
                matches!(block, crate::message::ContentBlock::ToolUse { name, .. } if name == "advisor")
            })
        });
        if digest.tool_calls == 0 || consulted {
            return;
        }
        let provider = Arc::clone(&self.provider);
        tokio::spawn(async move {
            let question = "Review the agent's last turn (the steps after the latest user \
                            message). Flag only what it should act on now.";
            let consultation =
                match advisor::consult(&provider, &settings, &digest, Some(question)).await {
                    Ok(consultation) => consultation,
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "[advisor] auto review failed for {session_id}: {error}"
                        ));
                        return;
                    }
                };
            crate::logging::info(&format!(
                "[advisor] auto review {session_id}: {} ({} in / {} out)",
                consultation.verdict.level.as_str(),
                consultation.input_tokens,
                consultation.output_tokens
            ));
            if consultation.verdict.level == VerdictLevel::Note
                || !admit_delivery(&session_id, &consultation.verdict.advice, Instant::now())
            {
                return;
            }
            let text = format!(
                "{}\n\nThe advisor reviewed your last turn. Act on this before continuing, or \
                 explain why it does not apply.",
                advisor::format_for_agent(&consultation)
            );
            crate::background::global()
                .spawn_with_notify(
                    "advisor",
                    Some("advisor review".into()),
                    &session_id,
                    true,
                    true,
                    move |output_path| async move {
                        tokio::fs::write(&output_path, text).await?;
                        Ok(crate::background::TaskResult::completed(Some(0)))
                    },
                )
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliveries_are_rate_limited_and_deduplicated() {
        let session = "advisor_review_rate_limit";
        let start = Instant::now();
        assert!(admit_delivery(session, "first", start));
        assert!(!admit_delivery(session, "first", start), "repeat dropped");
        assert!(admit_delivery(session, "second", start));
        assert!(admit_delivery(session, "third", start));
        assert!(!admit_delivery(session, "fourth", start), "window is full");
        let later = start + DELIVERY_WINDOW + Duration::from_secs(1);
        assert!(admit_delivery(session, "fourth", later));
    }
}
