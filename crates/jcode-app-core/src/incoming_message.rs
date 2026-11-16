//! Wake signal for blocking tool waits when a user message is queued mid-turn.
//!
//! A user prompt sent while a turn is running is parked in the session's soft
//! interrupt queue and only injected once the running tool returns. A tool that
//! blocks for minutes (`bg action=wait`) would therefore hide the prompt until
//! the user cancels. Blocking waits select on [`Watch::arrived`] and return
//! early so the model handles the message at once.
//!
//! Queue push sites call [`notify`] after queueing a user message. [`bind`]
//! lets a wait also notice a message that was queued before it started.

use jcode_agent_runtime::{SoftInterruptMessage, SoftInterruptQueue, SoftInterruptSource};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Notify;

/// Safety net for lost wakeups, session renames and unbound queues.
const RECHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Entry {
    notify: Notify,
    /// Bumped by every [`notify`], so a watch sees messages queued after it
    /// was created even when no queue is bound.
    seq: AtomicU64,
    queue: Mutex<Option<Weak<Mutex<Vec<SoftInterruptMessage>>>>>,
}

static ENTRIES: LazyLock<Mutex<HashMap<String, Arc<Entry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn entry(session_id: &str) -> Arc<Entry> {
    let mut entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    entries.entry(session_id.to_owned()).or_default().clone()
}

/// Remember the session's soft interrupt queue so waits can see messages that
/// were queued before they started. Cheap and idempotent.
pub fn bind(session_id: &str, queue: &SoftInterruptQueue) {
    let entry = entry(session_id);
    *entry.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(queue));
}

/// Obtain the live queue without taking the agent lock.
pub(crate) fn bound_queue(session_id: &str) -> Option<SoftInterruptQueue> {
    let entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = entries.get(session_id)?;
    let queue = entry.queue.lock().unwrap_or_else(|e| e.into_inner());
    queue.as_ref().and_then(Weak::upgrade)
}

/// A user message was queued for `session_id`: wake its blocking waits.
pub fn notify(session_id: &str) {
    let entry = entry(session_id);
    entry.seq.fetch_add(1, Ordering::SeqCst);
    entry.notify.notify_waiters();
}

/// Notify for `source`. Only user prompts wake waits. Notifications are
/// delivered at the next safe point like before.
pub fn notify_if_user(session_id: &str, source: SoftInterruptSource) {
    if source == SoftInterruptSource::User {
        notify(session_id);
    }
}

/// Drop the bookkeeping for a closed session.
pub fn forget(session_id: &str) {
    ENTRIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
}

fn queue_has_user_message(entry: &Entry) -> bool {
    let queue = entry
        .queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(Weak::upgrade);
    queue.is_some_and(|queue| {
        queue
            .lock()
            .map(|queue| {
                queue.iter().any(|message| {
                    message.source == SoftInterruptSource::User
                        && message.mode == jcode_agent_runtime::QueuedMessageMode::Steer
                })
            })
            .unwrap_or(false)
    })
}

/// Created when a blocking wait starts.
pub struct Watch {
    entry: Arc<Entry>,
    seen: u64,
}

pub fn watch(session_id: &str) -> Watch {
    let entry = entry(session_id);
    let seen = entry.seq.load(Ordering::SeqCst);
    Watch { entry, seen }
}

impl Watch {
    /// A user message is queued, or arrived after this watch was created.
    pub fn is_pending(&self) -> bool {
        self.entry.seq.load(Ordering::SeqCst) != self.seen || queue_has_user_message(&self.entry)
    }

    /// Resolves as soon as [`is_pending`](Self::is_pending) holds.
    pub async fn arrived(&self) {
        loop {
            let mut notified = std::pin::pin!(self.entry.notify.notified());
            // Register before checking so a notify in between is not lost.
            notified.as_mut().enable();
            if self.is_pending() {
                return;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(RECHECK_INTERVAL) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message() -> SoftInterruptMessage {
        SoftInterruptMessage {
            system_reminder: None,
            content: "hello".into(),
            images: Vec::new(),
            urgent: false,
            source: SoftInterruptSource::User,
            mode: jcode_agent_runtime::QueuedMessageMode::Steer,
            client_id: None,
        }
    }

    #[tokio::test]
    async fn notify_wakes_a_parked_watch() {
        let sid = "incoming_message_notify";
        let watch = watch(sid);
        assert!(!watch.is_pending());
        let waiter = tokio::spawn(async move { watch.arrived().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        notify(sid);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("notify must wake the watch")
            .unwrap();
    }

    #[tokio::test]
    async fn bound_queue_message_is_seen_before_the_wait_starts() {
        let sid = "incoming_message_bound";
        let queue: SoftInterruptQueue = Arc::new(Mutex::new(Vec::new()));
        bind(sid, &queue);
        queue.lock().unwrap().push(user_message());
        let watch = watch(sid);
        tokio::time::timeout(Duration::from_millis(100), watch.arrived())
            .await
            .expect("already queued user message must resolve at once");
    }

    #[tokio::test]
    async fn system_messages_do_not_wake_waits() {
        let sid = "incoming_message_system";
        let queue: SoftInterruptQueue = Arc::new(Mutex::new(Vec::new()));
        bind(sid, &queue);
        let mut message = user_message();
        message.source = SoftInterruptSource::System;
        queue.lock().unwrap().push(message);
        notify_if_user(sid, SoftInterruptSource::System);
        let watch = watch(sid);
        assert!(!watch.is_pending());
    }
}
