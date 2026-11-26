//! What the routing service needs to know about a session.

use super::Agent;
use crate::kv_cache_monitor::MissReason;
use jcode_base::routing_service::{CacheContext, CacheFactors, Seam};

impl Agent {
    /// The prompt cache a move would leave behind: its size and the provider's
    /// cache pricing, in input-token units.
    pub(crate) fn routing_cache(&self, provider: &str) -> CacheContext {
        let context_tokens = self
            .session
            .iter_messages()
            .rev()
            .find_map(|message| message.token_usage.as_ref())
            .map_or(0, |usage| {
                usage.prompt_tokens.unwrap_or(
                    usage.input_tokens
                        + usage.cache_read_input_tokens.unwrap_or(0)
                        + usage.cache_creation_input_tokens.unwrap_or(0),
                )
            }) as f64;
        CacheContext {
            seam: Seam::Start,
            context_tokens,
            factors: self.routing_cache_factors(provider),
            summary_tokens: None,
        }
    }

    fn routing_cache_factors(&self, provider: &str) -> CacheFactors {
        match provider {
            "claude" | "anthropic" => {
                let compaction = self.registry.compaction();
                let ttl = compaction
                    .try_read()
                    .ok()
                    .and_then(|manager| self.cache_expiry_ttl_secs(&manager));
                match ttl {
                    Some(ttl) if ttl <= 300 => CacheFactors::ANTHROPIC_5M,
                    _ => CacheFactors::ANTHROPIC_1H,
                }
            }
            _ => {
                let source = if provider == "openai" {
                    "openai:api-key"
                } else {
                    provider
                };
                jcode_base::provider::pricing::metered_pricing_for_source(
                    source,
                    &self.provider.model(),
                )
                .and_then(|price| {
                    let input = price
                        .input_price_per_mtok_micros
                        .filter(|input| *input > 0)?;
                    let read = price.cache_read_price_per_mtok_micros?;
                    Some(CacheFactors::openai(read as f64 / input as f64))
                })
                .unwrap_or(CacheFactors::NONE)
            }
        }
    }

    pub(crate) fn routing_parent_id(&self) -> Option<String> {
        self.session.parent_id().map(str::to_owned)
    }

    /// A spent allowance must reach routing on its first occurrence, because
    /// another account can serve this session.
    pub(crate) fn fail_fast_on_limits(&mut self) {
        self.routing_fail_fast_limits = true;
    }

    /// A spent allowance keeps the provider's own same-account retries.
    pub(crate) fn retry_limits_in_place(&mut self) {
        self.routing_fail_fast_limits = false;
    }

    /// Where the session stands relative to its prompt cache: a session with no
    /// history picks freely. A cold cache can move for free: the provider's
    /// TTL ran out, a compaction rewrote the cached prefix, or the model
    /// changed (the cache is per model). A warm one cannot.
    pub(crate) fn routing_seam(&self) -> Seam {
        // The session-context header is stored too, so count what the user sees.
        if self.visible_conversation_message_count() == 0 {
            return Seam::Start;
        }
        if self.request_cache_domain_switch() == Some(MissReason::ModelSwitch) {
            return Seam::Cold;
        }
        let compaction = self.registry.compaction();
        let Ok(manager) = compaction.try_read() else {
            return Seam::Warm;
        };
        if manager.prefix_rewritten_since_response() {
            return Seam::Cold;
        }
        let ttl = self.cache_expiry_ttl_secs(&manager);
        if manager
            .expired_cache_response(std::time::SystemTime::now(), ttl)
            .is_some()
        {
            Seam::Cold
        } else {
            Seam::Warm
        }
    }
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
