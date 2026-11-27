//! Desktop state: typed access to a machine's `desktop_state_*` requests.
//!
//! The daemon keeps one keyed document per space (see `docs/DESKTOP_STATE.md`).
//! A state connection is dedicated: it never attaches a session. It works over
//! any transport that reaches the harness API (local bridge, SSH, paired
//! gateway), because it only uses [`JcodeClient`] requests and events.
//!
//! - [`DesktopStateClient`]: one connection. `get`, `subscribe`, `put`.
//! - [`DesktopStateMirror`]: the client's copy of a space, kept current from
//!   snapshots, pushes and put outcomes (a `full` conflict replaces it).
//! - [`DesktopStateSync`]: a connection that reconnects by itself, resubscribes
//!   and reports a fresh snapshot after every reconnect.
//!
//! A daemon without `desktop_state_v1` yields [`ErrorKind::UnsupportedCapability`]
//! telling the user to update Jcode on that machine; callers keep working
//! without sync and never restart the daemon.

use crate::client::{EventStream, JcodeClient};
use crate::errors::{Error, ErrorKind, Result};
use jcode_harness_api::{
    ApiEvent, ApiRequest, DESKTOP_STATE_CAPABILITY, DesktopStateChange, DesktopStateEntry,
    DesktopStateRejection,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UPDATE_MESSAGE: &str = "This machine's Jcode does not support Desktop state sync. Update Jcode on that machine; Desktop will not restart it.";

/// Every live entry of a space at one revision.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopStateSnapshot {
    pub space: String,
    pub rev: u64,
    pub entries: Vec<DesktopStateEntry>,
}

/// How a put ended. Only `Applied` changed the stored state.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopStatePut {
    /// Stored. `entries` are the keys that changed, at the new `rev`.
    Applied {
        rev: u64,
        entries: Vec<DesktopStateEntry>,
    },
    /// A key written changed after `expected_rev`; nothing for it was stored.
    /// `entries` is everything that changed since `expected_rev` (deletions
    /// have no value), or every live entry when `full`: replace what you hold,
    /// then merge your change on the new head and retry.
    Conflict {
        rev: u64,
        full: bool,
        entries: Vec<DesktopStateEntry>,
    },
    /// Refused outright; nothing was stored and retrying unchanged cannot help.
    Rejected {
        reason: DesktopStateRejection,
        message: String,
    },
}

/// Something another connection did to a subscribed space.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopStateUpdate {
    Changed {
        rev: u64,
        /// The writer's device label.
        device: String,
        entries: Vec<DesktopStateEntry>,
    },
    /// This subscriber fell behind: replace what you hold.
    Resync(DesktopStateSnapshot),
}

fn closed(client: &JcodeClient) -> Error {
    client
        .disconnect_reason()
        .unwrap_or_else(|| Error::new(ErrorKind::Disconnected, "the connection is closed"))
}

/// Whether `client`'s daemon advertises Desktop state.
pub fn supports_desktop_state(client: &JcodeClient) -> bool {
    client.supports(DESKTOP_STATE_CAPABILITY)
}

/// A connection used only for Desktop state.
#[derive(Clone)]
pub struct DesktopStateClient {
    client: JcodeClient,
}

impl DesktopStateClient {
    /// Wrap a fresh connection that has attached no session.
    pub fn new(client: JcodeClient) -> Result<Self> {
        if !supports_desktop_state(&client) {
            return Err(Error::new(ErrorKind::UnsupportedCapability, UPDATE_MESSAGE));
        }
        Ok(Self { client })
    }

    fn snapshot(&self, request: ApiRequest) -> Result<DesktopStateSnapshot> {
        match self.client.request(request)?.event {
            ApiEvent::DesktopState {
                space,
                rev,
                entries,
            } => Ok(DesktopStateSnapshot {
                space,
                rev,
                entries,
            }),
            ApiEvent::Error { code, message } => Err(Error::new(ErrorKind::Harness(code), message)),
            other => Err(Error::new(
                ErrorKind::UnexpectedReply,
                format!("unexpected reply to a Desktop state read: {other:?}"),
            )),
        }
    }

    /// Read every live entry of `space`.
    pub fn get(&self, space: &str) -> Result<DesktopStateSnapshot> {
        self.snapshot(ApiRequest::DesktopStateGet {
            space: space.to_owned(),
        })
    }

    /// Read `space` and receive what other connections change from then on.
    pub fn subscribe(&self, space: &str) -> Result<(DesktopStateSnapshot, DesktopStateStream)> {
        // Listen before asking, so a push between the reply and this call
        // cannot be lost.
        let events = self.client.events(None);
        let snapshot = self.snapshot(ApiRequest::DesktopStateSubscribe {
            space: space.to_owned(),
        })?;
        let stream = DesktopStateStream {
            space: space.to_owned(),
            events,
            client: self.client.clone(),
        };
        Ok((snapshot, stream))
    }

    /// Apply `changes` (a change without a value deletes its key) on top of
    /// `expected_rev`, labelled `device` for other subscribers.
    pub fn put(
        &self,
        space: &str,
        device: &str,
        expected_rev: u64,
        changes: Vec<DesktopStateChange>,
    ) -> Result<DesktopStatePut> {
        let reply = self.client.request(ApiRequest::DesktopStatePut {
            space: space.to_owned(),
            device: device.to_owned(),
            expected_rev,
            changes,
        })?;
        match reply.event {
            ApiEvent::DesktopStateApplied { rev, entries, .. } => {
                Ok(DesktopStatePut::Applied { rev, entries })
            }
            ApiEvent::DesktopStateConflict {
                rev, full, entries, ..
            } => Ok(DesktopStatePut::Conflict { rev, full, entries }),
            ApiEvent::DesktopStateRejected { reason, message } => {
                Ok(DesktopStatePut::Rejected { reason, message })
            }
            ApiEvent::Error { code, message } => Err(Error::new(ErrorKind::Harness(code), message)),
            other => Err(Error::new(
                ErrorKind::UnexpectedReply,
                format!("unexpected reply to a Desktop state put: {other:?}"),
            )),
        }
    }

    /// Whether the connection has gone away.
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

/// The pushes of one subscribed space. Keeps its connection open.
pub struct DesktopStateStream {
    space: String,
    events: EventStream,
    client: JcodeClient,
}

impl DesktopStateStream {
    /// Wait up to `timeout` for the next update. `Ok(None)` is a timeout; an
    /// error means the connection is gone and must be re-established.
    pub fn next_timeout(&self, timeout: Duration) -> Result<Option<DesktopStateUpdate>> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(event) = self.events.next_timeout(remaining) else {
                return if self.client.is_closed() {
                    Err(closed(&self.client))
                } else {
                    Ok(None)
                };
            };
            let update = match event {
                ApiEvent::DesktopStateChanged {
                    space,
                    rev,
                    device,
                    entries,
                } if space == self.space => DesktopStateUpdate::Changed {
                    rev,
                    device,
                    entries,
                },
                ApiEvent::DesktopState {
                    space,
                    rev,
                    entries,
                } if space == self.space => DesktopStateUpdate::Resync(DesktopStateSnapshot {
                    space,
                    rev,
                    entries,
                }),
                _ => {
                    if remaining.is_zero() {
                        return Ok(None);
                    }
                    continue;
                }
            };
            return Ok(Some(update));
        }
    }
}

/// The client's copy of one space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesktopStateMirror {
    rev: u64,
    entries: BTreeMap<String, DesktopStateEntry>,
}

impl DesktopStateMirror {
    /// The revision this copy is at: use it as `expected_rev`.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key)?.value.as_ref()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Replace everything held.
    pub fn replace(&mut self, snapshot: &DesktopStateSnapshot) {
        self.entries.clear();
        self.rev = 0;
        self.merge(snapshot.rev, &snapshot.entries);
    }

    /// Fold in changed entries; an entry without a value deletes its key.
    pub fn merge(&mut self, rev: u64, entries: &[DesktopStateEntry]) {
        for entry in entries {
            if entry.value.is_some() {
                self.entries.insert(entry.key.clone(), entry.clone());
            } else {
                self.entries.remove(&entry.key);
            }
        }
        self.rev = self.rev.max(rev);
    }

    pub fn apply_update(&mut self, update: &DesktopStateUpdate) {
        match update {
            DesktopStateUpdate::Changed { rev, entries, .. } => self.merge(*rev, entries),
            DesktopStateUpdate::Resync(snapshot) => self.replace(snapshot),
        }
    }

    /// Fold in a put outcome. A `full` conflict means the daemon forgot the
    /// deletions this copy was missing, so it replaces the copy.
    pub fn apply_put(&mut self, outcome: &DesktopStatePut) {
        match outcome {
            DesktopStatePut::Applied { rev, entries } => self.merge(*rev, entries),
            DesktopStatePut::Conflict {
                rev,
                full: true,
                entries,
            } => {
                self.entries.clear();
                self.rev = 0;
                self.merge(*rev, entries);
            }
            DesktopStatePut::Conflict { rev, entries, .. } => self.merge(*rev, entries),
            DesktopStatePut::Rejected { .. } => {}
        }
    }
}

/// What a [`DesktopStateSync`] reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncEvent {
    /// Connected and subscribed (first time, after a reconnect, or after a
    /// resync): replace what you hold with this.
    Snapshot(DesktopStateSnapshot),
    Changed {
        rev: u64,
        device: String,
        entries: Vec<DesktopStateEntry>,
    },
    /// The connection is down or could not be made; retrying. Reported once
    /// per outage.
    Lost(Error),
    /// The daemon cannot sync. The sync has stopped; nothing was changed there.
    Unsupported(Error),
}

/// Reconnect pacing.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// How often a quiet connection is checked for loss or a stop request.
    pub poll: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(10),
            poll: Duration::from_millis(100),
        }
    }
}

struct SyncShared {
    stop: AtomicBool,
    current: Mutex<Option<DesktopStateClient>>,
}

/// A subscribed space that survives connection loss: it reconnects with
/// backoff, resubscribes and reports a fresh [`SyncEvent::Snapshot`] each time.
/// Dropping it stops the background thread.
pub struct DesktopStateSync {
    space: String,
    device: String,
    shared: Arc<SyncShared>,
    events: Receiver<SyncEvent>,
}

impl DesktopStateSync {
    /// `connect` makes one new connection, however the machine is reached.
    pub fn start(
        connect: impl Fn() -> Result<JcodeClient> + Send + 'static,
        space: &str,
        device: &str,
        options: SyncOptions,
    ) -> Self {
        let shared = Arc::new(SyncShared {
            stop: AtomicBool::new(false),
            current: Mutex::new(None),
        });
        let (tx, events) = channel();
        let worker = (Arc::clone(&shared), space.to_owned());
        std::thread::spawn(move || run_sync(connect, worker.1, &worker.0, &tx, options));
        Self {
            space: space.to_owned(),
            device: device.to_owned(),
            shared,
            events,
        }
    }

    /// Wait up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<SyncEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Put on the current connection; `Disconnected` while it is down. A
    /// caller that lost a put merges the next snapshot and puts again.
    pub fn put(
        &self,
        expected_rev: u64,
        changes: Vec<DesktopStateChange>,
    ) -> Result<DesktopStatePut> {
        let client = self
            .shared
            .current
            .lock()
            .map_err(|_| Error::new(ErrorKind::Transport, "sync state poisoned"))?
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::Disconnected, "not connected"))?;
        client.put(&self.space, &self.device, expected_rev, changes)
    }
}

impl Drop for DesktopStateSync {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

fn pause(shared: &SyncShared, total: Duration) {
    let end = Instant::now() + total;
    while !shared.stop.load(Ordering::Acquire) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20).min(total));
    }
}

fn attach(
    connect: &impl Fn() -> Result<JcodeClient>,
    space: &str,
) -> Result<(DesktopStateClient, DesktopStateSnapshot, DesktopStateStream)> {
    let client = DesktopStateClient::new(connect()?)?;
    let (snapshot, stream) = client.subscribe(space)?;
    Ok((client, snapshot, stream))
}

fn run_sync(
    connect: impl Fn() -> Result<JcodeClient>,
    space: String,
    shared: &SyncShared,
    tx: &Sender<SyncEvent>,
    options: SyncOptions,
) {
    let mut backoff = options.initial_backoff;
    let mut reported_lost = false;
    let set_current = |client: Option<DesktopStateClient>| {
        if let Ok(mut current) = shared.current.lock() {
            *current = client;
        }
    };
    while !shared.stop.load(Ordering::Acquire) {
        let (client, snapshot, stream) = match attach(&connect, &space) {
            Ok(attached) => attached,
            Err(error) if error.kind == ErrorKind::UnsupportedCapability => {
                let _ = tx.send(SyncEvent::Unsupported(error));
                return;
            }
            Err(error) => {
                if !std::mem::replace(&mut reported_lost, true) {
                    let _ = tx.send(SyncEvent::Lost(error));
                }
                pause(shared, backoff);
                backoff = (backoff * 2).min(options.max_backoff);
                continue;
            }
        };
        backoff = options.initial_backoff;
        set_current(Some(client));
        if tx.send(SyncEvent::Snapshot(snapshot)).is_err() {
            return;
        }
        let lost = loop {
            if shared.stop.load(Ordering::Acquire) {
                set_current(None);
                return;
            }
            let event = match stream.next_timeout(options.poll) {
                Ok(None) => continue,
                Ok(Some(DesktopStateUpdate::Changed {
                    rev,
                    device,
                    entries,
                })) => SyncEvent::Changed {
                    rev,
                    device,
                    entries,
                },
                Ok(Some(DesktopStateUpdate::Resync(snapshot))) => SyncEvent::Snapshot(snapshot),
                Err(error) => break error,
            };
            if tx.send(event).is_err() {
                return;
            }
        };
        set_current(None);
        reported_lost = true;
        if tx.send(SyncEvent::Lost(lost)).is_err() {
            return;
        }
        pause(shared, backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(key: &str, rev: u64, value: Option<Value>) -> DesktopStateEntry {
        DesktopStateEntry {
            key: key.to_owned(),
            rev,
            value,
        }
    }

    #[test]
    fn mirror_follows_pushes_and_deletions() {
        let mut mirror = DesktopStateMirror::default();
        mirror.replace(&DesktopStateSnapshot {
            space: "s".into(),
            rev: 3,
            entries: vec![entry("a", 1, Some(json!(1))), entry("b", 3, Some(json!(2)))],
        });
        mirror.apply_update(&DesktopStateUpdate::Changed {
            rev: 4,
            device: "d".into(),
            entries: vec![entry("a", 4, None), entry("c", 4, Some(json!(3)))],
        });
        assert_eq!(mirror.rev(), 4);
        assert_eq!(mirror.keys().collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    fn full_conflict_replaces_the_copy_but_a_delta_conflict_merges() {
        let mut mirror = DesktopStateMirror::default();
        mirror.merge(
            2,
            &[
                entry("gone", 1, Some(json!(1))),
                entry("kept", 2, Some(json!(2))),
            ],
        );
        let mut merged = mirror.clone();
        let delta = DesktopStatePut::Conflict {
            rev: 5,
            full: false,
            entries: vec![entry("new", 5, Some(json!(3)))],
        };
        merged.apply_put(&delta);
        assert_eq!(merged.keys().collect::<Vec<_>>(), ["gone", "kept", "new"]);
        mirror.apply_put(&DesktopStatePut::Conflict {
            rev: 5,
            full: true,
            entries: vec![
                entry("kept", 2, Some(json!(2))),
                entry("new", 5, Some(json!(3))),
            ],
        });
        assert_eq!(mirror.rev(), 5);
        assert_eq!(mirror.keys().collect::<Vec<_>>(), ["kept", "new"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_daemon_without_the_capability_is_refused_with_the_update_message() {
        use crate::client::{ConnectOptions, Transport};
        use jcode_harness_api::{ClientFrame, ServerFrame, read_frame, write_frame};
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        struct Pipe(UnixStream);
        impl Transport for Pipe {
            fn split(self: Box<Self>) -> Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
                let writer = self.0.try_clone().expect("clone");
                Ok((Box::new(BufReader::new(self.0)), Box::new(writer)))
            }
        }
        let (client_end, server_end) = UnixStream::pair().expect("pair");
        std::thread::spawn(move || {
            let mut reader = BufReader::new(server_end.try_clone().expect("clone"));
            let mut writer = server_end;
            let hello: ClientFrame = read_frame(&mut reader).expect("hello");
            let mut reply = ServerFrame::reply(
                hello.id,
                ApiEvent::HelloOk {
                    version: 1,
                    server: "old".into(),
                    runtime_identity: None,
                    capabilities: vec!["queued_prompts".into()],
                },
            );
            reply.reply_to = Some(hello.id);
            write_frame(&mut writer, &reply).expect("reply");
            // Hold the connection open until the client hangs up.
            let _ = read_frame::<_, ClientFrame>(&mut reader);
        });
        let client =
            JcodeClient::connect_with(Box::new(Pipe(client_end)), ConnectOptions::default())
                .expect("connect");
        assert!(!supports_desktop_state(&client));
        let error = DesktopStateClient::new(client).err().expect("refused");
        assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
        assert!(error.message.contains("Update Jcode on that machine"));
    }
}
