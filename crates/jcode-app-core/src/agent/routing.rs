//! What the routing service needs to know about a session.

use super::Agent;
use crate::kv_cache_monitor::MissReason;
use jcode_base::routing_service::Seam;

impl Agent {
    pub(crate) fn routing_parent_id(&self) -> Option<String> {
        self.session.parent_id.clone()
    }

    /// Whether a spent allowance must reach routing on its first occurrence.
    pub(crate) fn set_routing_fail_fast_limits(&mut self, fail_fast: bool) {
        self.routing_fail_fast_limits = fail_fast;
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
