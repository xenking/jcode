//! `/acct` in the TUI. Parsing and completion live in `jcode-session-types`
//! so every client shares them.
pub(crate) use jcode_session_types::routing_acct::{complete_acct, parse_acct};

/// Routing account ids from the loaded config, in file order.
pub(crate) fn configured_account_ids() -> Vec<String> {
    let (ids, _) = crate::config::config()
        .routing
        .each(|account| account.id.trim().to_string(), |_| ());
    ids.into_iter().filter(|id| !id.is_empty()).collect()
}

impl super::App {
    /// The daemon's answer to `/acct`. The routing table is shown on demand in
    /// a dismissable overlay and a one-line confirmation as a status notice;
    /// neither is written into the conversation.
    pub(super) fn show_routing_reply(&mut self, text: String) {
        if text.contains('\n') {
            self.model_status_content = text;
            self.model_status_title = "/acct status";
            self.model_status_scroll = Some(0);
        } else {
            self.set_status_notice(text);
        }
    }

    /// Record the daemon's routing decision for the header chip. The chat gets
    /// no line for it: the daemon writes one `old → new (cause)` row per real
    /// account change.
    pub(super) fn apply_routing_decision(
        &mut self,
        account: String,
        reason: String,
        warnings: usize,
        pending: Option<String>,
    ) {
        let chip = crate::tui::RoutingChip {
            account,
            reason,
            warnings,
            pending,
        };
        if self.routing_chip.as_ref() != Some(&chip) {
            self.routing_chip = Some(chip);
            crate::tui::ui::prepare::invalidate_header_prep_cache();
        }
        // A decision means routing runs in `auto` for this session, so the
        // daemon, not this client, rotates accounts when one fails.
        self.routing_owns_failover = true;
    }
}
