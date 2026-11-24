//! What the routing service needs to know about a session.

use super::Agent;
use jcode_base::routing_service::Seam;

impl Agent {
    pub(crate) fn routing_parent_id(&self) -> Option<String> {
        self.session.parent_id.clone()
    }

    /// Where the session stands relative to its prompt cache: a session with no
    /// history picks freely, a cold cache can move for free, a warm one cannot.
    pub(crate) fn routing_seam(&self) -> Seam {
        if self.message_count() == 0 {
            return Seam::Start;
        }
        let compaction = self.registry.compaction();
        let Ok(manager) = compaction.try_read() else {
            return Seam::Warm;
        };
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
