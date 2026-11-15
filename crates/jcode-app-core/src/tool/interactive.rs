//! Live interactive input routes, independent of the Agent lock held by a turn.
//!
//! A turn may outlive its originating socket. A newly attached client must be
//! able to answer subsequent questions without waiting for that turn's lock.
use super::StdinInputRequest;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use tokio::sync::mpsc::UnboundedSender;

type Route = (u64, UnboundedSender<StdinInputRequest>);
static ROUTES: LazyLock<Mutex<HashMap<String, Vec<Route>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Attachment {
    session_id: String,
    id: u64,
}

impl Attachment {
    pub(crate) fn session_is(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }
}

pub(crate) fn attach(session_id: &str, sender: UnboundedSender<StdinInputRequest>) -> Attachment {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_owned())
        .or_default()
        .push((id, sender));
    Attachment {
        session_id: session_id.to_owned(),
        id,
    }
}

pub(crate) fn sender(session_id: &str) -> Option<UnboundedSender<StdinInputRequest>> {
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)?
        .iter()
        .rev()
        .find(|(_, sender)| !sender.is_closed())
        .map(|(_, sender)| sender.clone())
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let mut routes = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = routes.get_mut(&self.session_id) {
            entries.retain(|(id, _)| *id != self.id);
            if entries.is_empty() {
                routes.remove(&self.session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_live_attachment_wins_and_dropping_one_never_removes_the_other() {
        let (first, _first_rx) = tokio::sync::mpsc::unbounded_channel();
        let (second, second_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_guard = attach("interactive-test-session", first.clone());
        let second_guard = attach("interactive-test-session", second.clone());
        assert!(
            sender("interactive-test-session")
                .unwrap()
                .same_channel(&second)
        );
        assert!(sender("other-session").is_none());
        drop(second_rx);
        assert!(
            sender("interactive-test-session")
                .unwrap()
                .same_channel(&first)
        );
        drop(second_guard);
        assert!(
            sender("interactive-test-session")
                .unwrap()
                .same_channel(&first)
        );
        drop(first_guard);
        assert!(sender("interactive-test-session").is_none());
    }
}
