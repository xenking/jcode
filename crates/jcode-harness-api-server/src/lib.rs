//! Harness API bridge: exposes the stable versioned harness API on its own
//! Unix socket and translates to the internal (legacy) jcode protocol.
//!
//! Architecture (milestone 2 of docs/HARNESS_API_AND_DESKTOP_REWRITE.md):
//! - Listens on `~/.jcode/jcode-api.sock` (or `JCODE_API_SOCKET`).
//! - For each API client, dials the legacy daemon socket (`JCODE_SOCKET` or
//!   `~/.jcode/jcode.sock`) and speaks `subscribe`/`message`/... on its
//!   behalf.
//! - Translation is JSON-to-JSON so this crate does not depend on the heavy
//!   internal protocol types and cannot be broken by additive internal
//!   changes.
//!
//! This keeps the daemon untouched while the API surface stabilizes. Once
//! proven, the same translation can move in-process behind a `hello` sniff on
//! the main socket.
// Tests hold the std env/home serialization lock across awaits on purpose.
#![cfg_attr(test, allow(clippy::await_holding_lock))]

pub mod allocator;
pub mod background_progress;
mod capabilities;
pub mod search_index;
pub mod timing;
pub mod translate;

#[cfg(test)]
#[path = "search_api_tests.rs"]
mod search_api_tests;

#[cfg(all(test, unix))]
mod session_presence_tests;
#[cfg(all(test, unix))]
mod stdio_tests;

use anyhow::{Context, Result};
use jcode_harness_api::{
    API_VERSION_MAJOR, ApiEvent, BackgroundTasksSnapshot, ErrorCode, ServerFrame,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
// Unix sockets on Unix, named pipes on Windows, one API. Without this the
// bridge simply did not compile for Windows, so the SDK could not run there at
// all.
use jcode_transport::{Listener, Stream};

// Socket paths live in `jcode-harness-api` so clients and the bridge can never
// resolve different directories (they once did, and the desktop app could not
// connect as a result).
pub use jcode_harness_api::{api_socket_path, legacy_socket_path};

/// Frames above this size are transient (history, transcripts, file reads).
/// Their buffers are released instead of pinning that capacity for the rest
/// of the connection, and the allocator is asked to return the pages.
const LARGE_FRAME_BYTES: usize = 256 * 1024;

fn release_large_frame(buffer: &mut Vec<u8>) {
    if buffer.capacity() > LARGE_FRAME_BYTES {
        *buffer = Vec::new();
        allocator::trim();
    }
}

mod session_controls;
use capabilities::DaemonCapabilities;
use session_controls::{DaemonControl, daemon_routing_view, daemon_side_questions};

const TASK_POLL_MIN: Duration = Duration::from_secs(2);
const TASK_POLL_QUIET_MAX: Duration = Duration::from_secs(30);
const TASK_POLL_RUNNING_MAX: Duration = Duration::from_secs(8);

struct TaskWatch {
    session_id: String,
    task_id: Option<String>,
    last: Option<BackgroundTasksSnapshot>,
    delay: Duration,
}

impl TaskWatch {
    async fn fetch(
        control: &mut DaemonControl,
        socket: &std::path::Path,
        session_id: &str,
        task_id: Option<&str>,
    ) -> Result<BackgroundTasksSnapshot> {
        Ok(serde_json::from_value(
            control
                .background_tasks(socket, session_id, task_id, false)
                .await?,
        )?)
    }

    fn accept(&mut self, snapshot: BackgroundTasksSnapshot, force: bool) -> Option<ApiEvent> {
        if !force && self.last.as_ref() == Some(&snapshot) {
            return None;
        }
        self.last = Some(snapshot.clone());
        Some(ApiEvent::BackgroundTasks {
            session_id: self.session_id.clone(),
            snapshot,
        })
    }

    fn next_delay(&mut self, changed: bool) -> Duration {
        self.delay = if changed {
            TASK_POLL_MIN
        } else {
            let running = self.last.as_ref().is_some_and(|snapshot| {
                snapshot
                    .tasks
                    .iter()
                    .any(|task| task.status == jcode_harness_api::BackgroundTaskStatus::Running)
            });
            (self.delay * 2).min(if running {
                TASK_POLL_RUNNING_MAX
            } else {
                TASK_POLL_QUIET_MAX
            })
        };
        self.delay
    }
}

async fn daemon_draft_models(socket: &std::path::Path) -> ApiEvent {
    let query = async {
        // A first-frame lightweight command closes its daemon stream after the
        // reply. Give it its own connection, never the client's session stream.
        let mut stream = Stream::connect(socket).await?;
        write_json_line(
            &mut stream,
            &serde_json::json!({"type":"comm_list_models", "id":1, "session_id":""}),
        )
        .await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while read_frame(&mut reader, &mut line).await? != 0 {
            let reply: Value = serde_json::from_str(&line)?;
            if reply["id"] != 1 {
                continue;
            }
            match reply["type"].as_str() {
                Some("comm_list_models_response") => {
                    return Ok(ApiEvent::DraftModels {
                        current: reply["current_model"].as_str().map(str::to_owned),
                        routes: if reply["model_routes"].is_null() {
                            Vec::new()
                        } else {
                            serde_json::from_value(reply["model_routes"].clone())?
                        },
                    });
                }
                Some("error") => anyhow::bail!(
                    "{}",
                    reply["message"].as_str().unwrap_or("catalog unavailable")
                ),
                _ => {}
            }
        }
        anyhow::bail!("daemon closed before returning the draft catalog")
    };
    match tokio::time::timeout(std::time::Duration::from_secs(4), query).await {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => ApiEvent::Error {
            code: ErrorCode::Internal,
            message: error.to_string(),
        },
        Err(_) => ApiEvent::Error {
            code: ErrorCode::Internal,
            message: "Draft model catalog timed out".into(),
        },
    }
}

/// Largest single request frame accepted from an API client, in bytes.
///
/// `read_line` grows its buffer until it finds a newline, so a client that
/// never sends one makes the bridge allocate without bound: one connection can
/// exhaust the host's memory, and the bridge serves every client on the
/// machine. A 32 MiB aggregate PDF budget expands to ~43 MiB of base64,
/// so 64 MiB leaves room for metadata while retaining a finite frame bound.
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Read one newline-delimited frame, refusing to buffer more than
/// `MAX_FRAME_BYTES`. Returns `Ok(0)` at end of stream, like `read_line`.
async fn read_frame<R>(reader: &mut R, line: &mut String) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    line.clear();
    let mut limited = tokio::io::AsyncReadExt::take(reader, MAX_FRAME_BYTES);
    let read = limited.read_line(line).await?;
    // A full buffer with no terminator means the frame exceeded the cap (or is
    // exactly at it and unterminated); either way it cannot be trusted.
    if read as u64 == MAX_FRAME_BYTES && !line.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    Ok(read)
}

/// Cancellation-safe variant for the multiplexed client loop. Tokio guarantees
/// `read_until` retains bytes already appended to `frame` when another select
/// branch wins, unlike `read_line` with a temporary String.
async fn read_frame_bytes<R>(reader: &mut R, frame: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    // A cancelled read retains its prefix. Limit the remaining bytes, not
    // each invocation separately, or repeated cancellation defeats the cap.
    let remaining = MAX_FRAME_BYTES.saturating_sub(frame.len() as u64);
    let mut limited = tokio::io::AsyncReadExt::take(reader, remaining);
    limited.read_until(b'\n', frame).await?;
    if frame.len() as u64 >= MAX_FRAME_BYTES && !frame.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    Ok(frame.len())
}

/// Run the bridge accept loop forever.
#[cfg(unix)]
pub(crate) struct InstanceLock {
    _file: std::fs::File,
}

/// Take the exclusive bridge lock beside the API socket, or report that a live
/// bridge already holds it. `flock` is released by the kernel when the holder
/// dies, so a crashed bridge never wedges the next one out.
#[cfg(unix)]
pub(crate) fn single_instance_lock(api_socket: &std::path::Path) -> Result<Option<InstanceLock>> {
    use std::os::fd::AsRawFd;

    let path = api_socket.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open bridge lock {}", path.display()))?;
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    // Never unlink a lock inode: a successor may already be waiting on it.
    Ok(taken.then_some(InstanceLock { _file: file }))
}

fn capture_bridge_identity() -> Result<()> {
    use jcode_build_support::runtime_generation;
    runtime_generation::serving_identity(jcode_build_meta::git_hash())?;
    if let Some(warning) = runtime_generation::identity_warning() {
        anyhow::bail!("invalid inherited API bridge identity: {warning}");
    }
    Ok(())
}

pub async fn run_bridge(api_socket: PathBuf, legacy_socket: PathBuf) -> Result<()> {
    capture_bridge_identity()
        .context("capture API bridge generation before publishing its listener")?;
    // Only one bridge may own the socket.
    //
    // This is the fix for clients seeing "disconnected: harness API stream
    // closed" at random: every desktop client spawned a bridge on demand, and
    // each new bridge unlinked the live socket and bound its own. The older
    // bridges kept running with their connected clients, but the *pathname*
    // now pointed at the newest one, and each reconnect churned the same way.
    // Whoever lost the race had its clients dropped. Refusing to start when a
    // live bridge holds the lock makes on-demand spawning idempotent, which is
    // what every caller already assumes.
    #[cfg(unix)]
    let _lock = match single_instance_lock(&api_socket)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "harness API bridge: another bridge already owns {}; exiting",
                api_socket.display()
            );
            return Ok(());
        }
    };
    // A stale socket file blocks bind on Unix. On Windows there is no file to
    // remove: the pipe namespace is not the filesystem. Safe to unlink here
    // only because we hold the exclusive lock above, so no live bridge owns it.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&api_socket);
    if let Some(parent) = api_socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // `mut` only on Windows: the named-pipe listener republishes a pipe
    // instance on every accept, so accepting takes `&mut self`. Unix's
    // UnixListener::accept takes `&self`, and an unconditional `mut` there is
    // an unused_mut warning, so the binding is declared per platform rather
    // than warning on every build.
    #[cfg(windows)]
    let mut listener = Listener::bind(&api_socket)
        .with_context(|| format!("bind API socket {}", api_socket.display()))?;
    #[cfg(unix)]
    let listener = Listener::bind(&api_socket)
        .with_context(|| format!("bind API socket {}", api_socket.display()))?;
    // Restrict the socket to its owner, matching the daemon socket it fronts.
    //
    // Without this the bridge widens access to everything behind it: the
    // daemon socket is 0600, but a default-umask bind here produced 0755, so
    // any local user could drive sessions, read transcripts, and spend the
    // owner's provider tokens. A bridge must never be more permissive than
    // the thing it bridges to.
    //
    // Unix only: a Windows named pipe carries an ACL rather than a file mode,
    // and the transport applies it when publishing the pipe.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&api_socket, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict API socket {}", api_socket.display()))?;
    }
    eprintln!(
        "harness API bridge: listening on {} -> {}",
        api_socket.display(),
        legacy_socket.display()
    );
    drop(search_index::warm_on_start());
    let retiring = Arc::new(AtomicBool::new(false));
    let mut clients = tokio::task::JoinSet::new();
    let idle_timeout = std::time::Duration::from_secs(30);
    let mut idle_since = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    #[cfg(unix)]
    let parent = unsafe { libc::getppid() };
    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                #[cfg(unix)]
                if unsafe { libc::getppid() } != parent {
                    retiring.store(true, Ordering::Relaxed);
                }
                if retiring.load(Ordering::Relaxed)
                    || (clients.is_empty() && idle_since.elapsed() >= idle_timeout) {
                    break;
                }
            }
            Some(_) = clients.join_next(), if !clients.is_empty() => {
                if clients.is_empty() { idle_since = tokio::time::Instant::now(); }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let legacy = legacy_socket.clone();
                let retiring = Arc::clone(&retiring);
                clients.spawn(async move {
                    if let Err(error) = handle_api_client(stream, legacy, retiring).await {
                        eprintln!("harness API bridge: client ended: {error:#}");
                    }
                    allocator::trim();
                });
            }
        }
    }
    // Withdraw discovery before draining, without interrupting existing streams.
    drop(listener);
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&api_socket);
        drop(_lock);
    }
    while clients.join_next().await.is_some() {}
    Ok(())
}

async fn handle_api_client(
    stream: Stream,
    legacy_socket: PathBuf,
    retiring: Arc<AtomicBool>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    run_bridge_stream_inner(read_half, write_half, legacy_socket, Some(retiring)).await
}

/// Serve the process's stdin/stdout as one API connection.
///
/// Read stdin on a plain thread, not Tokio's blocking pool: Tokio stdin cannot
/// be cancelled and otherwise prevents process exit when the daemon disconnects
/// while SSH still holds stdin open. This entrypoint is for a CLI process only.
#[cfg(unix)]
pub async fn run_bridge_stdio(legacy_socket: PathBuf) -> Result<()> {
    capture_bridge_identity().context("capture stdio bridge generation before accepting input")?;
    drop(search_index::warm_on_start());
    let (input, mut writer) = std::os::unix::net::UnixStream::pair()?;
    input.set_nonblocking(true)?;
    let input = tokio::net::UnixStream::from_std(input)?;
    std::thread::Builder::new()
        .name("api-stdin".into())
        .spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut writer);
            let _ = writer.shutdown(std::net::Shutdown::Write);
        })?;
    run_bridge_stream(input, tokio::io::stdout(), legacy_socket).await
}

/// Serve one stable API connection over a duplex byte stream, including SSH
/// stdio. No API listener or filesystem socket is created for this connection.
pub async fn run_bridge_stream<R, W>(
    read_half: R,
    write_half: W,
    legacy_socket: PathBuf,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    run_bridge_stream_inner(read_half, write_half, legacy_socket, None).await
}

async fn run_bridge_stream_inner<R, W>(
    read_half: R,
    mut write_half: W,
    legacy_socket: PathBuf,
    retiring: Option<Arc<AtomicBool>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let runtime_identity =
        jcode_build_support::runtime_generation::serving_identity(jcode_build_meta::git_hash())
            .context("capture API bridge serving generation")?;
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let mut pending = timing::Pending::default();

    // 1. Handshake: first frame must be hello with a compatible version.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame(&mut reader, &mut line),
    )
    .await
    .context("API client hello timed out")??;
    // A malformed first frame used to abort the task, closing the connection
    // with no reply at all: the client saw only an EOF and could not tell a
    // protocol mistake from a crashed bridge. Say what was wrong, then close.
    let hello: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(error) => {
            let frame = ServerFrame::event(ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                message: format!("first frame must be a JSON `hello`: {error}"),
            });
            write_json_line(&mut write_half, &frame).await?;
            return Ok(());
        }
    };
    let reply_to = hello["id"].as_u64().unwrap_or(0);
    if hello["req"] == "retire_bridge"
        && let Some(retiring) = retiring
    {
        retiring.store(true, Ordering::Relaxed);
        write_json_line(
            &mut write_half,
            &serde_json::json!({
                "v": API_VERSION_MAJOR, "reply_to": reply_to, "ev": "bridge_retiring"
            }),
        )
        .await?;
        return Ok(());
    }
    let client_name = hello["client"].as_str().unwrap_or_default();
    let compatible = hello["req"] == "hello"
        && hello["min_version"].as_u64().unwrap_or(0) <= u64::from(API_VERSION_MAJOR)
        && hello["max_version"].as_u64().unwrap_or(0) >= u64::from(API_VERSION_MAJOR);
    if !compatible {
        let frame = ServerFrame::reply(
            reply_to,
            ApiEvent::Error {
                code: ErrorCode::UnsupportedVersion,
                message: format!(
                    "bridge speaks API v{API_VERSION_MAJOR}; this client asked for v{}..=v{}",
                    hello["min_version"].as_u64().unwrap_or(0),
                    hello["max_version"].as_u64().unwrap_or(0),
                ),
            },
        );
        write_json_line(&mut write_half, &frame).await?;
        return Ok(());
    }
    let hello_at = std::time::Instant::now();
    // Do not claim a usable connection before the native daemon is reachable.
    let mut legacy = Stream::connect(&legacy_socket)
        .await
        .with_context(|| format!("connect legacy socket {}", legacy_socket.display()))?;
    let daemon_connected_at = std::time::Instant::now();
    let mut control = DaemonControl::default();
    let daemon_caps = control
        .capabilities(&legacy_socket)
        .await
        .unwrap_or_default();
    let control = Arc::new(tokio::sync::Mutex::new(control));
    let mut openrig_host_cap = None;
    if daemon_caps
        .iter()
        .any(|cap| cap == "openrig_host_identity_v1")
    {
        let probe = async {
            write_json_line(
                &mut legacy,
                &serde_json::json!({"type":"openrig_host_identity", "id":0}),
            )
            .await?;
            let mut reader = BufReader::new(legacy);
            let mut line = String::new();
            read_frame(&mut reader, &mut line).await?;
            let reply: Value = serde_json::from_str(&line)?;
            if reply["type"] != "pong" || reply["id"] != 0 {
                anyhow::bail!("invalid OpenRig identity reply");
            }
            let host = reply["capabilities"].as_array().and_then(|caps| {
                let mut claims = caps
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|cap| cap.starts_with("openrig_self_host_id="));
                let claim = claims.next()?;
                claims.next().is_none().then(|| claim.to_owned())
            });
            Ok::<_, anyhow::Error>((reader.into_inner(), host))
        };
        let (stream, host) =
            tokio::time::timeout(std::time::Duration::from_secs(2), probe).await??;
        legacy = stream;
        openrig_host_cap = host;
    }
    timing::log(&format!(
        "hello client={client_name} first_frame={:.1}ms daemon_connect={:.1}ms daemon_caps={:.1}ms",
        timing::ms(pending.connection_age_at(hello_at)),
        timing::ms(daemon_connected_at.saturating_duration_since(hello_at)),
        timing::ms(daemon_connected_at.elapsed()),
    ));
    let daemon = DaemonCapabilities::from_daemon(&daemon_caps);
    let hello_ok = ServerFrame::reply(
        reply_to,
        ApiEvent::HelloOk {
            runtime_identity: Some(runtime_identity),
            version: API_VERSION_MAJOR,
            server: format!("jcode-harness-api-bridge/{}", env!("CARGO_PKG_VERSION")),
            capabilities: daemon.hello_capabilities(openrig_host_cap),
        },
    );
    write_json_line(&mut write_half, &hello_ok).await?;

    // 2. Dial the legacy daemon for this client.
    let (legacy_read, mut legacy_write) = legacy.into_split();
    let mut legacy_reader = BufReader::new(legacy_read);

    let mut state = translate::BridgeState::open(&daemon, &hello, client_name);

    // 3. Pump both directions in one select loop so translation state stays
    //    single-threaded.
    let mut api_frame = Vec::new();
    let mut legacy_frame = Vec::new();
    let mut task_watch: Option<TaskWatch> = None;
    let mut task_poll = None;
    let task_tick = tokio::time::sleep(TASK_POLL_MIN);
    tokio::pin!(task_tick);
    loop {
        tokio::select! {
            // A busy daemon can keep the legacy socket continuously readable.
            // Prefer client requests so locally answered operations such as
            // list_sessions cannot sit behind an unbounded event broadcast.
            biased;
            n = read_frame_bytes(&mut reader, &mut api_frame) => {
                let n = match n {
                    Ok(n) => n,
                    // An oversized frame is unrecoverable: the stream is now
                    // mid-frame with no way to resynchronise. Report and close.
                    Err(error) => {
                        let frame = ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                        return Ok(());
                    }
                };
                if n == 0 { return Ok(()); }
                if api_frame.iter().all(u8::is_ascii_whitespace) {
                    api_frame.clear();
                    continue;
                }
                let parsed = serde_json::from_slice(&api_frame);
                api_frame.clear();
                release_large_frame(&mut api_frame);
                let received = std::time::Instant::now();
                let request: Value = match parsed {
                    Ok(value) => value,
                    Err(error) => {
                        // No `reply_to`: the id lived in the frame that failed
                        // to parse, so there is nothing to correlate against.
                        let frame = ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                        continue;
                    }
                };
                // The initial bounded probe may have timed out while the daemon
                // was busy. A missing advertised capability is only a snapshot,
                // not a permanent rejection. Recheck on a disposable connection
                // before dispatch, without reattaching or replaying anything on
                // the live session stream.
                if request["req"] == "runtime_command" && !state.runtime_commands_available() {
                    match DaemonControl::default().capabilities(&legacy_socket).await {
                        Some(capabilities) => {
                            state.note_runtime_commands(&capabilities);
                        }
                        None => {
                            write_json_line(&mut write_half, &ServerFrame::reply(
                                request["id"].as_u64().unwrap_or(0),
                                ApiEvent::Error {
                                    code: ErrorCode::Internal,
                                    message: "Could not verify runtime command support because the daemon capability probe failed. Retry the command once the runtime is responsive. The command was not executed.".into(),
                                },
                            )).await?;
                            continue;
                        }
                    }
                }
                if request["req"] == "session_context_breakdown" && !daemon.context_breakdown() {
                    write_json_line(&mut write_half, &ServerFrame::reply(
                        request["id"].as_u64().unwrap_or(0),
                        ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: "This runtime does not support context breakdown.".into(),
                        },
                    )).await?;
                    continue;
                }
                if request["req"] == "session_shake_tools" && !daemon.shake_tools() {
                    write_json_line(&mut write_half, &ServerFrame::reply(
                        request["id"].as_u64().unwrap_or(0),
                        ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: "This runtime does not support manual Shake tools.".into(),
                        },
                    )).await?;
                    continue;
                }
                if request["req"] == "get_routing_view" {
                    let id = request["id"].as_u64().unwrap_or(0);
                    let event = if daemon.routing() {
                        daemon_routing_view(&legacy_socket).await
                    } else {
                        ApiEvent::Error { code: ErrorCode::UnknownRequest, message: "This runtime does not support account routing.".into() }
                    };
                    write_json_line(&mut write_half, &ServerFrame::reply(id, event)).await?;
                    continue;
                }
                if matches!(request["req"].as_str(), Some("list_side_questions" | "get_side_question_history" | "promote_side_question")) {
                    let id = request["id"].as_u64().unwrap_or(0);
                    let event = if daemon.side_questions() {
                        daemon_side_questions(&legacy_socket, &request).await
                    } else {
                        ApiEvent::Error { code: ErrorCode::UnknownRequest, message: "This runtime does not support side questions.".into() }
                    };
                    write_json_line(&mut write_half, &ServerFrame::reply(id, event)).await?;
                    continue;
                }
                // Never wait on a task poll here: it is driven by this same
                // select loop. Use the last presence snapshot until it finishes.
                if request["req"] == "list_sessions" && daemon.user_connections()
                    && let Ok(mut control) = control.try_lock()
                {
                    state.set_user_connected_sessions(control.user_connections(&legacy_socket).await);
                }
                if request["req"] == "list_draft_models" {
                    let id = request["id"].as_u64().unwrap_or(0);
                    let event = daemon_draft_models(&legacy_socket).await;
                    write_json_line(&mut write_half, &ServerFrame::reply(id, event)).await?;
                    continue;
                }
                if matches!(request["req"].as_str(), Some("watch_background_tasks" | "cancel_background_task")) {
                    use jcode_harness_api::{ApiRequest, ClientFrame};
                    let id = request["id"].as_u64().unwrap_or(0);
                    let event = if daemon.background_tasks() {
                        match serde_json::from_value::<ClientFrame>(request).map(|frame| frame.request) {
                            Ok(ApiRequest::WatchBackgroundTasks { session_id, task_id }) if
                                state.session_id.as_ref().is_none_or(|attached| attached == &session_id) => {
                                task_poll = None;
                                let mut watch = TaskWatch { session_id, task_id, last: None, delay: TASK_POLL_MIN };
                                let event = TaskWatch::fetch(&mut *control.lock().await, &legacy_socket, &watch.session_id, watch.task_id.as_deref()).await
                                    .map(|snapshot| watch.accept(snapshot, true).expect("forced snapshot"));
                                task_watch = Some(watch);
                                task_tick.as_mut().reset(tokio::time::Instant::now() + TASK_POLL_MIN);
                                match event {
                                    Ok(event) => event,
                                    Err(error) => ApiEvent::Error { code: ErrorCode::Internal, message: error.to_string() },
                                }
                            }
                            Ok(ApiRequest::CancelBackgroundTask { session_id, task_id }) if
                                state.session_id.as_ref().is_none_or(|attached| attached == &session_id) => {
                                match DaemonControl::default().background_tasks(&legacy_socket, &session_id, Some(&task_id), true).await {
                                    Ok(_) => ApiEvent::Ok,
                                    Err(error) => ApiEvent::Error { code: ErrorCode::Internal, message: error.to_string() },
                                }
                            }
                            _ => ApiEvent::Error { code: ErrorCode::InvalidRequest, message: "Invalid background task request for this connection".into() },
                        }
                    } else {
                        ApiEvent::Error { code: ErrorCode::UnknownRequest, message: "The native daemon does not support background task inspection".into() }
                    };
                    write_json_line(&mut write_half, &ServerFrame::reply(id, event)).await?;
                    continue;
                }
                if matches!(request["req"].as_str(), Some("attach_session" | "create_session" | "detach_session")) {
                    task_watch = None;
                    task_poll = None;
                }
                // Translation may inspect persisted session/archive files. Tell
                // Tokio before entering that synchronous region so it can keep
                // the accept loop and fresh-client handshakes scheduled.
                let translate_started = std::time::Instant::now();
                let outbound = tokio::task::block_in_place(|| {
                    state.api_request_to_legacy(&request)
                });
                if let Some(id) = request["id"].as_u64() {
                    pending.begin(
                        id,
                        request["req"].as_str().unwrap_or("?"),
                        received,
                        translate_started.elapsed(),
                    );
                }
                let heavy = allocator::request_is_heavy(&request);
                drop(request);
                for out in outbound {
                    match out {
                        translate::Outbound::Legacy(value) => {
                            write_json_line(&mut legacy_write, &value).await?;
                        }
                        translate::Outbound::Reply(frame) => {
                            write_frame_timed(&mut write_half, &frame, &mut pending, "bridge").await?;
                        }
                    }
                }
                if heavy {
                    allocator::trim();
                }
            }
            _ = &mut task_tick, if task_watch.is_some() && task_poll.is_none() => {
                if let Some(watch) = &task_watch {
                    let socket = legacy_socket.clone();
                    let session_id = watch.session_id.clone();
                    let task_id = watch.task_id.clone();
                    let control = Arc::clone(&control);
                    task_poll = Some(Box::pin(async move {
                        TaskWatch::fetch(&mut *control.lock().await, &socket, &session_id, task_id.as_deref()).await
                    }));
                }
            }
            snapshot = async { task_poll.as_mut().expect("pending task poll").await }, if task_poll.is_some() => {
                task_poll = None;
                if let Some(watch) = &mut task_watch {
                    let event = snapshot.ok().and_then(|snapshot| watch.accept(snapshot, false));
                    let delay = watch.next_delay(event.is_some());
                    task_tick.as_mut().reset(tokio::time::Instant::now() + delay);
                    if let Some(event) = event {
                        write_json_line(&mut write_half, &ServerFrame::event(event)).await?;
                    }
                }
            }
            n = read_frame_bytes(&mut legacy_reader, &mut legacy_frame) => {
                if n? == 0 {
                    let frame = ServerFrame::event(ApiEvent::Error {
                        code: ErrorCode::Internal,
                        message: "daemon connection closed".into(),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    return Ok(());
                }
                if legacy_frame.iter().all(u8::is_ascii_whitespace) {
                    legacy_frame.clear();
                    continue;
                }
                let parsed = serde_json::from_slice(&legacy_frame);
                let large = legacy_frame.capacity() > LARGE_FRAME_BYTES;
                legacy_frame.clear();
                let event: Value = match parsed {
                    Ok(value) => value,
                    Err(_) => {
                        release_large_frame(&mut legacy_frame);
                        continue;
                    }
                };
                // Only a few replies read persisted session files. Streaming
                // deltas are pure translation, and routing each one through
                // block_in_place handed the worker core to a fresh blocking
                // thread per event: a busy session grew the bridge to 69 idle
                // threads, each pinning its own allocator arena.
                let translate_started = std::time::Instant::now();
                let frames = if translate::legacy_event_may_block(&event) {
                    tokio::task::block_in_place(|| state.legacy_event_to_api(&event))
                } else {
                    state.legacy_event_to_api(&event)
                };
                let translated = translate_started.elapsed();
                if translated > timing::SLOW_TRANSLATE {
                    timing::log(&format!(
                        "slow_event {} translate={:.1}ms",
                        event["type"].as_str().unwrap_or("?"),
                        timing::ms(translated),
                    ));
                }
                drop(event);
                for frame in frames {
                    write_frame_timed(&mut write_half, &frame, &mut pending, "daemon").await?;
                }
                if large {
                    release_large_frame(&mut legacy_frame);
                }
            }
        }
    }
}

/// Write a frame and, if it answers a timed request, log that request.
async fn write_frame_timed<W>(
    writer: &mut W,
    frame: &ServerFrame,
    pending: &mut timing::Pending,
    source: &str,
) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    if let Some(reply_to) = frame.reply_to {
        // Avoid a second serialization of large replies just for the name.
        // Missing event names remain empty timing labels, not request failures.
        let event = line
            .split_once("\"ev\":\"")
            .and_then(|(_, rest)| rest.split('"').next())
            .unwrap_or("");
        pending.finish(reply_to, event, line.len(), source);
    }
    Ok(())
}

async fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: ?Sized + serde::Serialize,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(all(test, unix))]
mod runtime_command_compat_tests;

#[cfg(all(test, unix))]
mod public_acceptance_tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsString;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    #[tokio::test(flavor = "multi_thread")]
    async fn stdio_stream_handshake_ping_and_eof() {
        let path = std::env::temp_dir().join(format!(
            "jcode-stdio-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let (client, bridge) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(bridge);
        let task = tokio::spawn(run_bridge_stream(read, write, path.clone()));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut reader = BufReader::new(client_read);
        write_json_line(&mut client_write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"stdio-test"})).await.unwrap();
        let (_daemon, _) = listener.accept().await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["ev"],
            "hello_ok"
        );
        write_json_line(&mut client_write, &json!({"v":1,"id":2,"req":"ping"}))
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let pong: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(pong["ev"], "pong");
        assert_eq!(pong["reply_to"], 2);
        drop(client_write);
        drop(reader);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stdio_does_not_confirm_handshake_when_native_daemon_is_missing() {
        let (mut client, bridge) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(bridge);
        let task = tokio::spawn(run_bridge_stream(
            read,
            write,
            PathBuf::from("/nonexistent-jcode-stdio-test/daemon.sock"),
        ));
        write_json_line(
            &mut client,
            &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1}),
        )
        .await
        .unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("connect legacy socket"));
    }

    struct JcodeHomeGuard(Option<OsString>);

    impl Drop for JcodeHomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
                None => unsafe { std::env::remove_var("JCODE_HOME") },
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn public_socket_keeps_its_attachment_after_another_sessions_state() {
        let _home_lock = translate::jcode_home_test_lock();
        let root = std::env::temp_dir().join(format!(
            "jcode-api-attachment-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let previous_home = std::env::var_os("JCODE_HOME");
        unsafe { std::env::set_var("JCODE_HOME", &root) };
        let _home_guard = JcodeHomeGuard(previous_home);
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("session_alpha.json"),
            json!({"working_dir": "/workspace/alpha", "messages": []}).to_string(),
        )
        .unwrap();
        let api_path = root.join("api.sock");
        let legacy_path = root.join("legacy.sock");
        let legacy_listener = UnixListener::bind(&legacy_path).unwrap();

        let fake_daemon = tokio::spawn(async move {
            let (stream, _) = legacy_listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut attached_state_id = None;
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                match request["type"].as_str() {
                    Some("state") if attached_state_id.is_none() => {
                        let id = request["id"].as_u64().unwrap();
                        attached_state_id = Some(id);
                        write_json_line(
                            &mut write,
                            &json!({"type":"state", "id":id, "session_id":"session_alpha"}),
                        )
                        .await
                        .unwrap();
                    }
                    Some("message") => {
                        if request["content"] == "public boundary prompt" {
                            continue;
                        }
                        assert_eq!(request["content"], "hello");
                        assert_eq!(request["active_skill"], "poteto-mode");
                        return;
                    }
                    _ => {}
                }
                if attached_state_id.is_some() {
                    write_json_line(
                        &mut write,
                        &json!({"type":"state", "id":999_999, "session_id":"session_beta"}),
                    )
                    .await
                    .unwrap();
                    attached_state_id = None;
                }
            }
            panic!("API connection closed before the message reached the attached session");
        });

        let bridge = tokio::spawn(run_bridge(api_path.clone(), legacy_path));
        let stream = loop {
            match UnixStream::connect(&api_path).await {
                Ok(stream) => break stream,
                Err(_) => tokio::task::yield_now().await,
            }
        };
        let (read, mut write) = stream.into_split();
        let mut replies = BufReader::new(read).lines();
        for request in [
            json!({"id":1, "req":"hello", "min_version":1, "max_version":1, "client":"acceptance-test"}),
            json!({"id":2, "req":"attach_session", "session_id":"session_alpha"}),
        ] {
            write_json_line(&mut write, &request).await.unwrap();
            replies
                .next_line()
                .await
                .unwrap()
                .expect("public API reply");
        }
        write_json_line(
            &mut write,
            &json!({"id":3, "req":"send_message", "session_id":"session_alpha", "content":"public boundary prompt"}),
        )
        .await
        .unwrap();

        write_json_line(
            &mut write,
            &json!({"id":4, "req":"send_skill_message", "session_id":"session_alpha", "content":"hello", "active_skill":"poteto-mode"}),
        )
        .await
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), fake_daemon)
            .await
            .expect("message should cross the API/legacy boundary")
            .unwrap();
        bridge.abort();
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
#[path = "framing_tests.rs"]
mod framing_tests;

#[cfg(all(test, unix))]
mod single_instance_tests {
    /// Two bridges must never both own the API socket.
    ///
    /// They used to: `run_bridge` unlinked whatever socket file was there and
    /// bound its own, so every on-demand spawn silently evicted the live
    /// bridge and its clients reported "harness API stream closed".
    #[test]
    fn a_second_bridge_cannot_take_the_socket() {
        let dir = std::env::temp_dir().join(format!("jcode-bridge-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("jcode-api.sock");

        let first = super::single_instance_lock(&socket).unwrap();
        assert!(first.is_some(), "the first bridge must take the lock");
        assert!(
            super::single_instance_lock(&socket).unwrap().is_none(),
            "a second bridge must be refused while the first is alive"
        );

        // Once the owner is gone the lock is available again, so a crashed
        // bridge never wedges its replacement out.
        drop(first);
        assert!(super::single_instance_lock(&socket).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod socket_permission_tests {
    /// The API socket must never be more permissive than the daemon socket it
    /// fronts.
    ///
    /// This regressed once: `UnixListener::bind` applies the process umask, so
    /// the socket landed at 0755 while the daemon socket it bridges to is
    /// 0600. Every guarantee behind the daemon socket was then reachable by
    /// any local user, including reading transcripts and spending the owner's
    /// provider tokens.
    #[tokio::test]
    async fn the_api_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("jcode-api-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let api_socket = dir.join("api.sock");
        let legacy_socket = dir.join("daemon.sock");

        let bridge_socket = api_socket.clone();
        let handle = tokio::spawn(async move {
            let _ = super::run_bridge(bridge_socket, legacy_socket).await;
        });

        // Wait for the bind, which happens before the accept loop.
        let mut mode = None;
        for _ in 0..100 {
            if let Ok(meta) = std::fs::metadata(&api_socket) {
                mode = Some(meta.permissions().mode() & 0o777);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            mode,
            Some(0o600),
            "API socket must be owner-only (0600); a wider mode exposes every \
             session behind the bridge to other local users"
        );
    }
}

#[cfg(all(test, unix))]
mod fragmented_reply_tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_history_replies_survive_api_requests_between_fragments() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let path = std::env::temp_dir().join(format!("jcode-fragments-{}-{}.sock", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            let listener = UnixListener::bind(&path).unwrap();
            let (client, bridge) = tokio::io::duplex(4 * 1024 * 1024);
            let (bridge_read, bridge_write) = tokio::io::split(bridge);
            let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, path.clone()));
            let (client_read, mut client_write) = tokio::io::split(client);
            let mut replies = BufReader::new(client_read).lines();
            write_json_line(&mut client_write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1})).await.unwrap();
            let (daemon, _) = listener.accept().await.unwrap();
            let (daemon_read, mut daemon_write) = daemon.into_split();
            let mut requests = BufReader::new(daemon_read).lines();
            assert_eq!(serde_json::from_str::<Value>(&replies.next_line().await.unwrap().unwrap()).unwrap()["ev"], "hello_ok");
            write_json_line(&mut client_write, &json!({"v":1,"id":2,"req":"attach_session","session_id":"fragment-session"})).await.unwrap();
            for _ in 0..3 {
                let request: Value = serde_json::from_str(&requests.next_line().await.unwrap().unwrap()).unwrap();
                if request["type"] == "state" {
                    write_json_line(&mut daemon_write, &json!({"type":"state","id":request["id"],"session_id":"fragment-session"})).await.unwrap();
                }
            }
            loop {
                let frame: Value = serde_json::from_str(&replies.next_line().await.unwrap().unwrap()).unwrap();
                if frame["ev"] == "attached" { break; }
            }
            for id in 3..6 {
                write_json_line(&mut client_write, &json!({"v":1,"id":id,"req":"get_history","session_id":"fragment-session"})).await.unwrap();
            }
            let mut history_ids = Vec::new();
            for _ in 0..3 {
                let request: Value = serde_json::from_str(&requests.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request["type"], "get_history");
                history_ids.push(request["id"].clone());
            }
            for (index, id) in history_ids.into_iter().enumerate() {
                // Larger than the socket send buffer, forcing the bridge to
                // consume an unterminated prefix before API activity cancels it.
                let text = "x".repeat(1024 * 1024);
                let prefix = format!("{{\"type\":\"history\",\"id\":{id},\"session_id\":\"fragment-session\",\"messages\":[{{\"role\":\"assistant\",\"content\":\"{text}");
                daemon_write.write_all(prefix.as_bytes()).await.unwrap();
                let ping_id = 20 + index;
                write_json_line(&mut client_write, &json!({"v":1,"id":ping_id,"req":"ping"})).await.unwrap();
                let ping: Value = serde_json::from_str(&requests.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(ping["type"], "ping");
                daemon_write.write_all(b"\"}]}\n").await.unwrap();
                write_json_line(&mut daemon_write, &json!({"type":"pong","id":ping["id"]})).await.unwrap();
                let mut history_seen = false;
                let mut pong_seen = false;
                while !history_seen || !pong_seen {
                    let reply: Value = serde_json::from_str(&replies.next_line().await.unwrap().unwrap()).unwrap();
                    if reply["reply_to"] == index + 3 {
                        assert_eq!(reply["ev"], "history");
                        assert_eq!(reply["messages"][0]["content"], text);
                        history_seen = true;
                    }
                    if reply["reply_to"] == ping_id {
                        assert_eq!(reply["ev"], "pong");
                        pong_seen = true;
                    }
                }
            }
            drop(client_write);
            drop(replies);
            task.await.unwrap().unwrap();
            drop(listener);
            std::fs::remove_file(path).unwrap();
        }).await.expect("fragmented history requests must all receive correlated replies");
    }
}
