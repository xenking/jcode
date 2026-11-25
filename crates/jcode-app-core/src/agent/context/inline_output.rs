//! `Agent` entry points to its presentation layer ([`super::inline_tail::InlineOutput`]).

use super::Agent;

impl Agent {
    /// Whether this session streams a public output tail to the bus. Owned
    /// workers retain a tail in every spawn mode so forced stops can still
    /// deliver recent output when transcript collection cannot finish.
    pub(crate) fn inline_output_tap(&self) -> bool {
        self.inline_output
            .is_tapped(self.session.parent_id.is_some())
    }

    /// Publish the current rolling activity tail to the bus for the
    /// coordinator's inline gallery. No-op unless the inline tap is enabled.
    pub(crate) fn publish_inline_tail(&self) {
        self.inline_output
            .publish(&self.session.id, self.session.parent_id.is_some());
    }

    /// Mark this session as an inline swarm worker. The streaming loop then
    /// publishes a throttled output tail to the global bus so a coordinator can
    /// render a live inline gallery viewport for it.
    pub fn tap_inline_output(&mut self) {
        self.inline_output.tap();
    }
}
