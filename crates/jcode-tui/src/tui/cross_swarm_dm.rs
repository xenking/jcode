use std::collections::VecDeque;

const SWARM_LABEL: &str = " (swarm '";

pub(super) fn split_identity(identity: &str) -> Option<(&str, &str)> {
    let (sender, label) = identity.strip_suffix("')")?.split_once(SWARM_LABEL)?;
    (!sender.is_empty() && !sender.contains(char::is_whitespace) && !label.contains('\n'))
        .then_some((sender, label))
}

pub(super) fn parse(message: &str) -> Option<(&str, &str)> {
    let message = message.trim().strip_prefix("Cross-swarm DM from ")?;
    let (identity, body) = message.split_once("'): ")?;
    let identity = &message[..identity.len() + "')".len()];
    split_identity(identity)?;
    Some((identity, body.trim()))
}

#[derive(Default)]
pub(super) struct PendingReceipts(VecDeque<String>);

impl PendingReceipts {
    pub(super) fn record(&mut self, message: &str) {
        const MAX_RECEIPTS: usize = 32;
        if self.0.len() == MAX_RECEIPTS {
            self.0.pop_front();
        }
        self.0.push_back(message.trim().to_owned());
    }

    pub(super) fn consume(&mut self, message: &str) -> bool {
        let Some(index) = self.0.iter().position(|receipt| receipt == message.trim()) else {
            return false;
        };
        self.0.remove(index);
        true
    }
}
