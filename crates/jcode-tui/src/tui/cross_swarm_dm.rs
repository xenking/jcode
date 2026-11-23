use crate::protocol::NotificationType;
use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};

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
struct PendingReceipts(VecDeque<String>);

impl PendingReceipts {
    fn record(&mut self, message: &str) {
        const MAX_RECEIPTS: usize = 32;
        if self.0.len() == MAX_RECEIPTS {
            self.0.pop_front();
        }
        self.0.push_back(message.trim().to_owned());
    }

    fn consume(&mut self, message: &str) -> bool {
        let Some(index) = self.0.iter().position(|receipt| receipt == message.trim()) else {
            return false;
        };
        self.0.remove(index);
        true
    }
}

#[derive(Default)]
pub(super) struct SwarmNotifications {
    last_by_sender: HashMap<String, Instant>,
    receipts: PendingReceipts,
}

impl SwarmNotifications {
    pub(super) fn note_sender(&mut self, sender: &str) -> Option<f32> {
        let now = Instant::now();
        self.last_by_sender
            .insert(sender.to_owned(), now)
            .map(|previous| now.duration_since(previous).as_secs_f32())
    }

    pub(super) fn clear_receipts(&mut self) {
        self.receipts = PendingReceipts::default();
    }

    pub(super) fn record_dm_receipt(&mut self, notification: &NotificationType, message: &str) {
        if matches!(notification, NotificationType::Message { scope: Some(scope), .. } if scope == "dm")
            && parse(message).is_some()
        {
            self.receipts.record(message);
        }
    }

    pub(super) fn consume_dm_receipt(&mut self, message: &str) -> bool {
        parse(message).is_some() && self.receipts.consume(message)
    }
}
