//! A dedicated Desktop state connection.
//!
//! The first frame of the connection is a `desktop_state_*` request; from then
//! on the connection serves only those requests and pushes
//! `desktop_state_changed` for every space it subscribed to. It never attaches
//! a session, so a Desktop opens one per machine next to its session panels.

use crate::desktop_state::{DesktopStateHub, PutOutcome, SpaceSnapshot};
use crate::protocol::{DesktopStateRejection, Request, ServerEvent, decode_request, encode_event};
use anyhow::Result;
use jcode_session_types::desktop_state::{DesktopStateViolation, MAX_SPACES};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio::sync::broadcast::error::RecvError;

/// Largest request line: a put of a full space plus JSON framing.
const MAX_FRAME_BYTES: usize = 20 * 1024 * 1024;

static NEXT_ORIGIN: AtomicU64 = AtomicU64::new(1);

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, event: &ServerEvent) -> Result<()> {
    writer.write_all(encode_event(event).as_bytes()).await?;
    Ok(())
}

fn rejected(id: u64, violation: DesktopStateViolation) -> ServerEvent {
    ServerEvent::DesktopStateRejected {
        id,
        reason: violation.rejection,
        message: violation.message,
    }
}

fn unavailable(id: u64, error: &anyhow::Error) -> ServerEvent {
    crate::logging::warn(&format!("desktop state store: {error:#}"));
    ServerEvent::DesktopStateRejected {
        id,
        reason: DesktopStateRejection::Unavailable,
        message: "the Desktop state store is unavailable".into(),
    }
}

fn snapshot_event(id: u64, space: String, snapshot: SpaceSnapshot) -> ServerEvent {
    ServerEvent::DesktopState {
        id,
        space,
        rev: snapshot.rev,
        entries: snapshot.entries,
    }
}

/// The hub calls block on SQLite; keep them off the connection's executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work).await?
}

async fn snapshot(hub: &Arc<DesktopStateHub>, id: u64, space: String) -> ServerEvent {
    let (hub, name) = (Arc::clone(hub), space.clone());
    match blocking(move || hub.snapshot(&name)).await {
        Ok(Ok(snapshot)) => snapshot_event(id, space, snapshot),
        Ok(Err(violation)) => rejected(id, violation),
        Err(error) => unavailable(id, &error),
    }
}

/// What the connection does next: a decoded request, or a push to forward.
enum Next {
    Request(Request),
    Closed,
    Handled,
}

/// Serve `first` and everything after it on this connection until it closes.
pub(super) async fn serve<R, W>(
    first: Request,
    reader: &mut R,
    writer: &mut W,
    hub: Arc<DesktopStateHub>,
) -> Result<()>
where
    R: AsyncBufRead + AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let origin = NEXT_ORIGIN.fetch_add(1, Ordering::Relaxed);
    let mut changes = hub.subscribe();
    let mut subscribed: HashSet<String> = HashSet::new();
    let mut pending = Some(first);
    let mut line = String::new();
    loop {
        let request = match pending.take() {
            Some(request) => request,
            None => {
                let next = tokio::select! {
                    read = async {
                        let room = (MAX_FRAME_BYTES + 1).saturating_sub(line.len()) as u64;
                        (&mut *reader).take(room).read_line(&mut line).await
                    } => next_from_line(read?, &mut line, writer).await?,
                    change = changes.recv() => match change {
                        Ok(change) => {
                            if change.origin != origin && subscribed.contains(&change.space) {
                                send(writer, &ServerEvent::DesktopStateChanged {
                                    space: change.space.clone(),
                                    rev: change.rev,
                                    device: change.device.clone(),
                                    entries: change.entries.clone(),
                                }).await?;
                            }
                            Next::Handled
                        }
                        // Pushes were missed: say where every watched space
                        // stands now rather than leave the client stale.
                        Err(RecvError::Lagged(_)) => {
                            for space in subscribed.clone() {
                                send(writer, &snapshot(&hub, 0, space).await).await?;
                            }
                            Next::Handled
                        }
                        Err(RecvError::Closed) => Next::Closed,
                    },
                };
                match next {
                    Next::Request(request) => request,
                    Next::Handled => continue,
                    Next::Closed => return Ok(()),
                }
            }
        };
        let event = match request {
            Request::DesktopStateGet { id, space } => snapshot(&hub, id, space).await,
            Request::DesktopStateSubscribe { id, space } => {
                if !subscribed.contains(&space) && subscribed.len() >= MAX_SPACES {
                    ServerEvent::DesktopStateRejected {
                        id,
                        reason: DesktopStateRejection::TooLarge,
                        message: format!("at most {MAX_SPACES} subscribed spaces per connection"),
                    }
                } else {
                    // Subscribe before reading so no change falls in the gap;
                    // a duplicate is harmless because entries carry revisions.
                    subscribed.insert(space.clone());
                    snapshot(&hub, id, space).await
                }
            }
            Request::DesktopStatePut {
                id,
                space,
                device,
                expected_rev,
                changes,
            } => {
                let (hub, name) = (Arc::clone(&hub), space.clone());
                match blocking(move || hub.put(origin, &name, &device, expected_rev, &changes))
                    .await
                {
                    Ok(Ok(PutOutcome::Applied { rev, changed })) => {
                        ServerEvent::DesktopStateApplied {
                            id,
                            space,
                            rev,
                            entries: changed,
                        }
                    }
                    Ok(Ok(PutOutcome::Conflict { rev, full, entries })) => {
                        ServerEvent::DesktopStateConflict {
                            id,
                            space,
                            rev,
                            full,
                            entries,
                        }
                    }
                    Ok(Err(violation)) => rejected(id, violation),
                    Err(error) => unavailable(id, &error),
                }
            }
            other => ServerEvent::Error {
                id: other.id(),
                message: "A Desktop state connection serves only desktop_state requests".into(),
                retry_after_secs: None,
            },
        };
        send(writer, &event).await?;
    }
}

async fn next_from_line<W: AsyncWrite + Unpin>(
    read: usize,
    line: &mut String,
    writer: &mut W,
) -> Result<Next> {
    if read == 0 {
        return Ok(Next::Closed);
    }
    if !line.ends_with('\n') {
        if line.len() > MAX_FRAME_BYTES {
            let error = ServerEvent::Error {
                id: 0,
                message: "Desktop state request is too large".into(),
                retry_after_secs: None,
            };
            send(writer, &error).await?;
        }
        // Either too large, or a frame cut short by end of stream.
        return Ok(Next::Closed);
    }
    let decoded = decode_request(line);
    line.clear();
    match decoded {
        Ok(request) => Ok(Next::Request(request)),
        Err(error) => {
            let error = ServerEvent::Error {
                id: 0,
                message: format!("Invalid request: {error}"),
                retry_after_secs: None,
            };
            send(writer, &error).await?;
            Ok(Next::Handled)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_state::DesktopStateStore;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

    struct Client {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Client {
        /// A connection whose first frame is `first`, served on `hub`.
        async fn connect(hub: &Arc<DesktopStateHub>, first: Value) -> Self {
            let (client, server) = tokio::io::duplex(8 * 1024 * 1024);
            let (server_read, mut server_write) = tokio::io::split(server);
            let (reader, writer) = tokio::io::split(client);
            let first = decode_request(&first.to_string()).unwrap();
            let hub = Arc::clone(hub);
            tokio::spawn(async move {
                let mut server_read = BufReader::new(server_read);
                serve(first, &mut server_read, &mut server_write, hub).await
            });
            Self {
                reader: BufReader::new(reader),
                writer,
            }
        }

        async fn send(&mut self, request: Value) {
            self.writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn next(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                .await
                .expect("an event")
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn silent(&mut self) -> bool {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_millis(150), self.reader.read_line(&mut line))
                .await
                .is_err()
        }
    }

    fn hub() -> (tempfile::TempDir, Arc<DesktopStateHub>) {
        let dir = tempfile::tempdir().unwrap();
        let store = DesktopStateStore::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, Arc::new(DesktopStateHub::new(store)))
    }

    fn subscribe(id: u64) -> Value {
        json!({"type":"desktop_state_subscribe","id":id,"space":"main"})
    }

    fn put(id: u64, device: &str, expected_rev: u64, key: &str, value: Value) -> Value {
        json!({"type":"desktop_state_put","id":id,"space":"main","device":device,
               "expected_rev":expected_rev,"changes":[{"key":key,"value":value}]})
    }

    fn draft(text: &str) -> Value {
        json!({"v":1,"text":text,"cursor":0})
    }

    #[tokio::test]
    async fn a_put_by_one_client_is_pushed_to_the_other_and_a_stale_put_conflicts() {
        let (_dir, hub) = hub();
        let mut a = Client::connect(&hub, subscribe(1)).await;
        let mut b = Client::connect(&hub, subscribe(1)).await;
        assert_eq!(a.next().await["rev"], 0);
        assert_eq!(b.next().await["rev"], 0);

        a.send(put(2, "laptop", 0, "draft/p1", draft("hello")))
            .await;
        let applied = a.next().await;
        assert_eq!(
            (applied["type"].as_str(), applied["rev"].as_u64()),
            (Some("desktop_state_applied"), Some(1))
        );

        let pushed = b.next().await;
        assert_eq!(pushed["type"], "desktop_state_changed");
        assert_eq!(
            (pushed["rev"].as_u64(), pushed["device"].as_str()),
            (Some(1), Some("laptop"))
        );
        assert_eq!(pushed["entries"][0]["value"], draft("hello"));
        // The writer already has its answer and is not pushed its own change.
        assert!(a.silent().await);

        // B still believes the space is at revision 0.
        b.send(put(3, "desktop", 0, "draft/p1", draft("mine")))
            .await;
        let conflict = b.next().await;
        assert_eq!(conflict["type"], "desktop_state_conflict");
        assert_eq!(conflict["rev"], 1);
        assert_eq!(conflict["entries"][0]["value"], draft("hello"));
        assert!(a.silent().await, "a refused put is not pushed");

        // Based on the head, the same write goes through and A hears of it.
        b.send(put(4, "desktop", 1, "draft/p1", draft("mine")))
            .await;
        assert_eq!(b.next().await["type"], "desktop_state_applied");
        assert_eq!(a.next().await["entries"][0]["value"], draft("mine"));
    }

    #[tokio::test]
    async fn a_get_does_not_subscribe_and_a_new_subscriber_starts_from_the_snapshot() {
        let (_dir, hub) = hub();
        let mut writer = Client::connect(&hub, subscribe(1)).await;
        writer.next().await;
        writer
            .send(put(2, "laptop", 0, "draft/p1", draft("a")))
            .await;
        writer.next().await;

        let mut reader = Client::connect(
            &hub,
            json!({"type":"desktop_state_get","id":1,"space":"main"}),
        )
        .await;
        let snapshot = reader.next().await;
        assert_eq!(
            (snapshot["type"].as_str(), snapshot["rev"].as_u64()),
            (Some("desktop_state"), Some(1))
        );
        assert_eq!(snapshot["entries"][0]["key"], "draft/p1");

        writer
            .send(put(3, "laptop", 1, "draft/p1", draft("b")))
            .await;
        writer.next().await;
        assert!(reader.silent().await, "a get is not a subscription");
    }

    #[tokio::test]
    async fn rejected_puts_store_nothing_and_say_why() {
        let (_dir, hub) = hub();
        let mut client = Client::connect(&hub, subscribe(1)).await;
        client.next().await;
        for (key, value, reason) in [
            (
                "panel/p1",
                json!({"v":1,"kind":"compose","launch":{"env":{"K":"v"}}}),
                "invalid",
            ),
            (
                "draft/p1",
                json!({"v":1,"text":"x".repeat(5 * 1024 * 1024)}),
                "too_large",
            ),
            ("settings", json!({"v":1}), "invalid"),
        ] {
            client.send(put(2, "laptop", 0, key, value)).await;
            let rejected = client.next().await;
            assert_eq!(rejected["type"], "desktop_state_rejected", "{key}");
            assert_eq!(rejected["reason"], reason, "{key}");
        }
        client
            .send(json!({"type":"desktop_state_get","id":9,"space":"main"}))
            .await;
        let snapshot = client.next().await;
        assert_eq!(
            (
                snapshot["rev"].as_u64(),
                snapshot["entries"].as_array().map(Vec::len)
            ),
            (Some(0), Some(0))
        );
    }

    #[tokio::test]
    async fn other_requests_get_an_error_and_unsubscribed_spaces_are_not_pushed() {
        let (_dir, hub) = hub();
        let mut watcher = Client::connect(&hub, subscribe(1)).await;
        watcher.next().await;
        watcher.send(json!({"type":"ping","id":5})).await;
        let error = watcher.next().await;
        assert_eq!(
            (error["type"].as_str(), error["id"].as_u64()),
            (Some("error"), Some(5))
        );

        let mut other = Client::connect(
            &hub,
            json!({"type":"desktop_state_subscribe","id":1,"space":"elsewhere"}),
        )
        .await;
        other.next().await;
        watcher
            .send(put(6, "laptop", 0, "draft/p1", draft("a")))
            .await;
        watcher.next().await;
        assert!(other.silent().await);
    }
}
