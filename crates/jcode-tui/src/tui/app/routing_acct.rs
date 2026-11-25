//! `/acct` in the TUI. Parsing and completion live in `jcode-session-types`
//! so every client shares them.
pub(crate) use jcode_session_types::routing_acct::{complete_acct, parse_acct};

/// Routing account ids from the loaded config, in file order.
pub(crate) fn configured_account_ids() -> Vec<String> {
    crate::config::config()
        .routing
        .accounts
        .iter()
        .map(|account| account.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

impl super::App {
    /// Record the daemon's routing decision for the header chip. A move to a
    /// different account is also announced once, with its reason.
    pub(super) fn apply_routing_decision(
        &mut self,
        account: String,
        reason: String,
        warnings: usize,
        pending: Option<String>,
    ) {
        let moved = self
            .routing_chip
            .as_ref()
            .is_some_and(|chip| chip.account != account);
        if moved {
            self.push_display_message(super::DisplayMessage::system(format!(
                "Routing: now on {account} ({reason})"
            )));
        }
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
