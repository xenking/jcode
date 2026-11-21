//! Connected harness client: handshake, reply correlation, event streaming.
//!
//! Mirrors the TypeScript SDK's `JcodeClient` capability for capability. The
//! shapes differ where Rust and TS differ honestly (`Result` instead of
//! throwing, channels instead of `EventEmitter`), but every method here has a
//! counterpart there and vice versa; `parity.rs` enforces that.
//!
//! Blocking, thread-based, and `Clone`. A desktop app has a UI thread that
//! must never block on a socket and a worker that reads it forever, so the
//! client is a handle both can hold: one reader thread owns the stream and
//! fans frames out to whoever asked for them.

use crate::errors::{Error, ErrorKind, Result};
use crate::launch::{LaunchOptions, LaunchedInstance, ensure_runtime, launch_instance};
use crate::ssh::{SshConnectOptions, SshProcess, SshTransport};
use jcode_harness_api::{
    API_VERSION_MAJOR, ApiEvent, ApiRequest, ClientFrame, HistoryMessage, ModelRouteInfo,
    PermissionDecision, ServerFrame, SessionInfo, SessionToolDefinition, TextMatch,
    ToolConfiguration, api_socket_path, read_frame, write_frame,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How a client reaches the harness.
pub struct ConnectOptions {
    /// Defaults to the resolved harness API socket path.
    pub socket_path: Option<std::path::PathBuf>,
    /// Client identity sent in the handshake, e.g. "my-app/1.0".
    pub client_name: String,
    /// How long a request waits for its reply. `None` disables the timeout.
    pub request_timeout: Option<Duration>,
    /// Start the daemon and bridge if they are not already listening.
    ///
    /// On by default: an app that only works once the user has launched two
    /// daemons by hand is indistinguishable from a broken one.
    pub ensure_runtime: bool,
    /// Desktop inline Markdown response renderer. Other clients leave false.
    pub supports_user_response: bool,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            socket_path: None,
            client_name: concat!("jcode-sdk-rs/", env!("CARGO_PKG_VERSION")).to_string(),
            request_timeout: Some(Duration::from_secs(30)),
            ensure_runtime: true,
            supports_user_response: false,
        }
    }
}

/// Options fixed when a session is created.
#[derive(Clone, Debug, Default)]
pub struct CreateSessionOptions {
    /// Working directory for the new session. Omit to use the runtime default.
    pub working_dir: Option<String>,
    /// Replace the entire assembled system prompt, not just its base text.
    ///
    /// This bypasses the default prompt and assembled instruction/context additions.
    /// `None` keeps normal prompt assembly. `Some(String::new())` explicitly
    /// overrides it with an empty prompt. Immutable after creation and persisted
    /// by the runtime for resume.
    pub system_prompt: Option<String>,
}

/// A duplex byte transport. Lets tests and future WebSockets plug in.
pub trait Transport: Send {
    /// A handle which interrupts both halves after `split`. Custom transports
    /// may omit this; native sockets provide it so Drop closes promptly.
    fn shutdown_handle(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        None
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)>;
}

/// A Unix-domain socket transport, the default.
type PlatformUnixStream = jcode_transport::SyncStream;

pub struct UnixTransport(PlatformUnixStream);

impl UnixTransport {
    pub fn connect(path: &std::path::Path) -> Result<Self> {
        // A bare `No such file or directory` names the syscall and hides the
        // cause: the bridge is not running. Connecting is the first thing
        // anyone does with this SDK, so say what to do about it.
        let stream = PlatformUnixStream::connect(path).map_err(|cause| {
            Error::new(
                ErrorKind::ConnectFailed,
                match cause.kind() {
                    std::io::ErrorKind::NotFound => format!(
                        "harness not running (no socket at {}). Start `jcode-harness-api-bridge` \
                         or connect with ensure_runtime.",
                        path.display()
                    ),
                    // Also seen briefly while the bridge restarts, so do not
                    // claim the socket is stale.
                    std::io::ErrorKind::ConnectionRefused => format!(
                        "harness refused the connection at {}. It may be restarting or stopped.",
                        path.display()
                    ),
                    std::io::ErrorKind::PermissionDenied => format!(
                        "permission denied on {}. The socket belongs to another user.",
                        path.display()
                    ),
                    _ => format!("could not connect to {}: {cause}", path.display()),
                },
            )
        })?;
        Ok(Self(stream))
    }
}

impl Transport for UnixTransport {
    fn shutdown_handle(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        #[cfg(unix)]
        {
            let socket = self.0.try_clone().ok()?;
            Some(Arc::new(move || {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }))
        }
        #[cfg(windows)]
        {
            let pipe = self.0.try_clone().ok()?;
            Some(Arc::new(move || {
                let _ = pipe.shutdown(std::net::Shutdown::Both);
            }))
        }
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        let writer = self
            .0
            .try_clone()
            .map_err(|e| Error::new(ErrorKind::Transport, e.to_string()))?;
        Ok((Box::new(BufReader::new(self.0)), Box::new(writer)))
    }
}

/// A subscription to the event stream.
///
/// Streaming events are fanned out to every live subscription, so an app can
/// have one loop rendering the attached session while another waits for a
/// single acknowledgement without either stealing frames from the other.
pub struct EventStream {
    rx: Receiver<ApiEvent>,
    id: u64,
    inner: Arc<Inner>,
}

impl EventStream {
    /// Block for the next event. `None` once the connection closes.
    pub fn next(&self) -> Option<ApiEvent> {
        self.rx.recv().ok()
    }

    /// Block for the next event, up to `timeout`.
    pub fn next_timeout(&self, timeout: Duration) -> Option<ApiEvent> {
        match self.rx.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Iterator for EventStream {
    type Item = ApiEvent;
    fn next(&mut self) -> Option<ApiEvent> {
        EventStream::next(self)
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        if let Ok(mut subs) = self.inner.subscribers.lock() {
            subs.retain(|(id, _, _)| *id != self.id);
        }
    }
}

/// Discovery and buffering controls for [`JcodeClient::global_events`].
#[derive(Debug, Clone, Copy)]
pub struct GlobalEventsOptions {
    /// How often persisted sessions are rescanned. Zero performs one scan.
    pub discovery_interval: Duration,
    /// Maximum events waiting for the consumer before the stream fails loudly.
    pub max_buffered_events: usize,
}

impl Default for GlobalEventsOptions {
    fn default() -> Self {
        Self {
            discovery_interval: Duration::from_secs(1),
            max_buffered_events: 10_000,
        }
    }
}

struct GlobalEventControl {
    stopped: AtomicBool,
    terminal_error: Mutex<Option<Error>>,
    children: Mutex<HashMap<String, JcodeClient>>,
    tx: SyncSender<ApiEvent>,
    max_buffered_events: usize,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

/// Events fanned in from every persisted and newly-created session.
///
/// Delivery begins when each per-session child attaches. Ordering is preserved
/// within a session; no total order across sessions is promised. Dropping this
/// stream cancels discovery and closes all child connections.
pub struct GlobalEventStream {
    rx: Receiver<ApiEvent>,
    control: Arc<GlobalEventControl>,
    discovery: Option<std::thread::JoinHandle<()>>,
}

impl GlobalEventStream {
    /// Block for the next event, returning the terminal stream error once.
    pub fn next(&self) -> Result<Option<ApiEvent>> {
        loop {
            if let Some(error) = take_global_error(&self.control) {
                return Err(error);
            }
            if self.control.stopped.load(Ordering::Acquire) {
                return Ok(None);
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => {
                    if let Some(error) = take_global_error(&self.control) {
                        return Err(error);
                    }
                    if self.control.stopped.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    return Ok(Some(event));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.control.stopped.load(Ordering::Acquire) {
                        if let Some(error) = take_global_error(&self.control) {
                            return Err(error);
                        }
                        return Ok(None);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }

    /// Wait up to `timeout` for an event. `Ok(None)` means timeout or shutdown.
    pub fn next_timeout(&self, timeout: Duration) -> Result<Option<ApiEvent>> {
        if let Some(error) = take_global_error(&self.control) {
            return Err(error);
        }
        if self.control.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(event) => {
                if let Some(error) = take_global_error(&self.control) {
                    Err(error)
                } else if self.control.stopped.load(Ordering::Acquire) {
                    Ok(None)
                } else {
                    Ok(Some(event))
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(error) = take_global_error(&self.control) {
                    Err(error)
                } else {
                    Ok(None)
                }
            }
        }
    }
}

impl Iterator for GlobalEventStream {
    type Item = Result<ApiEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        GlobalEventStream::next(self).transpose()
    }
}

impl Drop for GlobalEventStream {
    fn drop(&mut self) {
        stop_global_stream(&self.control, None);
        if let Some(discovery) = self.discovery.take() {
            let _ = discovery.join();
        }
    }
}

fn take_global_error(control: &GlobalEventControl) -> Option<Error> {
    control.terminal_error.lock().ok()?.take()
}

fn stop_global_stream(control: &GlobalEventControl, error: Option<Error>) {
    if let Some(error) = error
        && let Ok(mut terminal) = control.terminal_error.lock()
        && terminal.is_none()
    {
        *terminal = Some(error);
    }
    control.stopped.store(true, Ordering::Release);
    control.wake.notify_all();
    let children = control
        .children
        .lock()
        .ok()
        .map(|mut children| std::mem::take(&mut *children));
    drop(children);
}

/// A live subscription: (id, session filter, sink).
type Subscriber = (u64, Option<String>, Sender<ApiEvent>);

struct Inner {
    writer: Mutex<Box<dyn Write + Send>>,
    /// Requests waiting for their `reply_to` frame.
    pending: Mutex<HashMap<u64, Sender<ServerFrame>>>,
    /// Live subscriptions: (id, session filter, sink).
    subscribers: Mutex<Vec<Subscriber>>,
    next_id: AtomicU64,
    next_sub: AtomicU64,
    closed: AtomicBool,
    disconnect_reason: Mutex<Option<Error>>,
    request_timeout: Option<Duration>,
    socket_path: std::path::PathBuf,
    client_name: String,
    native_socket: bool,
    ssh_process: Option<Arc<SshProcess>>,
    shutdown: Option<Arc<dyn Fn() + Send + Sync>>,
    client_handles: AtomicUsize,
}

/// Connected harness client.
///
/// Replies are correlated by the `reply_to` id the server echoes; anything
/// without one is a stream event and goes to every subscriber.
pub struct JcodeClient {
    inner: Arc<Inner>,
    ssh_options: Option<SshConnectOptions>,
    instance: Option<Arc<LaunchedInstance>>,
    /// State directory of the private instance this client owns, if any.
    pub instance_home: Option<std::path::PathBuf>,
    /// Server identity from the handshake, e.g. "jcode-harness-api-bridge/0.1.0".
    pub server: String,
    /// Self-reported executing bridge image, absent on older bridges.
    pub runtime_identity: Option<crate::api::ServingIdentity>,
    /// Capability strings advertised by the server.
    pub capabilities: Vec<String>,
}

impl Clone for JcodeClient {
    fn clone(&self) -> Self {
        self.inner.client_handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
            ssh_options: self.ssh_options.clone(),
            instance: self.instance.clone(),
            instance_home: self.instance_home.clone(),
            server: self.server.clone(),
            runtime_identity: self.runtime_identity.clone(),
            capabilities: self.capabilities.clone(),
        }
    }
}

impl Drop for JcodeClient {
    fn drop(&mut self) {
        if self.inner.client_handles.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some(shutdown) = &self.inner.shutdown
        {
            shutdown();
        }
    }
}

impl JcodeClient {
    /// OpenRig identity reported by the connected daemon, absent for older peers.
    pub fn openrig_self_host_id(&self) -> Option<&str> {
        let mut ids = self
            .capabilities
            .iter()
            .filter_map(|cap| cap.strip_prefix("openrig_self_host_id="));
        let id = ids.next()?;
        if ids.next().is_some()
            || id.is_empty()
            || id.trim() != id
            || id.chars().any(char::is_control)
        {
            return None;
        }
        Some(id)
    }

    /// Retain this client's shared SSH master for reconnecting independent API
    /// channels. Returns None for isolated or already closed SSH channels.
    #[cfg(unix)]
    pub fn shared_ssh_transport(&self) -> Option<crate::SharedSshTransport> {
        self.inner
            .ssh_process
            .as_ref()
            .and_then(|process| process.shared_transport())
    }

    /// Connect to a remote shared harness using system SSH credentials/config.
    ///
    /// The remote must have `jcode api --stdio`. Dropping the last client clone
    /// kills and reaps SSH, but leaves the remote shared daemon running. This
    /// never launches a local runtime or falls back to local session data.
    pub fn connect_ssh(options: SshConnectOptions) -> Result<Self> {
        let transport = SshTransport::spawn(&options)?;
        Self::over_ssh(transport, options)
    }

    pub(crate) fn over_ssh(transport: SshTransport, options: SshConnectOptions) -> Result<Self> {
        let process = Arc::clone(&transport.process);
        // Also interrupts a blocked hello write, not just the reply wait.
        let (deadline, watchdog) = process.startup_deadline(options.connect_timeout);
        let connect = ConnectOptions {
            socket_path: None,
            client_name: options.client_name.clone(),
            supports_user_response: options.supports_user_response,
            request_timeout: options.request_timeout,
            ensure_runtime: false,
        };
        // A remote connection has no local socket path. Do not resolve the
        // user's local configuration just to manufacture one.
        let result = Self::over(
            Box::new(transport),
            &connect,
            std::path::PathBuf::new(),
            false,
            Some((options, Arc::clone(&process))),
        );
        let _ = deadline.send(());
        let _ = watchdog.join();
        let result = if process.timed_out.load(Ordering::Acquire) {
            Err(Error::new(
                ErrorKind::StartupTimeout,
                "SSH startup and handshake timed out",
            ))
        } else {
            result
        };
        result.map_err(|error| {
            process.shutdown();
            let message = if error.kind == ErrorKind::Disconnected {
                process.diagnostic()
            } else {
                format!("{}; {}", error.message, process.diagnostic())
            };
            Error::new(
                if error.kind == ErrorKind::Timeout || process.timed_out.load(Ordering::Acquire) {
                    ErrorKind::StartupTimeout
                } else {
                    error.kind
                },
                message,
            )
        })
    }

    /// Start and own a private jcode instance, then connect to it.
    ///
    /// Its state and sockets are isolated from the user's interactive jcode.
    /// The last clone of this client shuts the instance down through Drop.
    pub fn launch(options: LaunchOptions) -> Result<Self> {
        let instance = Arc::new(launch_instance(&options)?);
        let connect = ConnectOptions {
            socket_path: Some(instance.socket_path.clone()),
            supports_user_response: false,
            client_name: options.client_name.clone(),
            request_timeout: options.request_timeout,
            ensure_runtime: false,
        };
        let mut client = Self::connect(connect)?;
        client.instance_home = Some(instance.jcode_home.clone());
        client.instance = Some(instance);
        Ok(client)
    }

    /// Connect to the jcode running on this machine.
    ///
    /// Use this to automate the user's own jcode: a desktop app, an editor
    /// plugin, a status dashboard. It shares the user's live sessions.
    pub fn connect(options: ConnectOptions) -> Result<Self> {
        let path = options.socket_path.clone().unwrap_or_else(api_socket_path);
        if options.ensure_runtime {
            ensure_runtime(&LaunchOptions::default(), &|_| {})?;
        }
        Self::over(
            Box::new(UnixTransport::connect(&path)?),
            &options,
            path,
            true,
            None,
        )
    }

    /// Connect over a caller-supplied transport. The seam tests use.
    pub fn connect_with(transport: Box<dyn Transport>, options: ConnectOptions) -> Result<Self> {
        let path = options.socket_path.clone().unwrap_or_else(api_socket_path);
        Self::over(transport, &options, path, false, None)
    }

    fn over(
        transport: Box<dyn Transport>,
        options: &ConnectOptions,
        socket_path: std::path::PathBuf,
        native_socket: bool,
        ssh: Option<(SshConnectOptions, Arc<SshProcess>)>,
    ) -> Result<Self> {
        let hello_timeout = ssh
            .as_ref()
            .map(|(options, _)| options.connect_timeout)
            .or(options.request_timeout);
        let (ssh_options, ssh_process) = match ssh {
            Some((options, process)) => (Some(options), Some(process)),
            None => (None, None),
        };
        let shutdown = transport.shutdown_handle();
        let (reader, writer) = transport.split()?;
        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            next_sub: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            disconnect_reason: Mutex::new(None),
            request_timeout: options.request_timeout,
            socket_path,
            client_name: options.client_name.clone(),
            native_socket,
            ssh_process,
            shutdown,
            client_handles: AtomicUsize::new(1),
        });
        spawn_reader(Arc::clone(&inner), reader);

        let mut client = Self {
            inner,
            ssh_options,
            instance: None,
            instance_home: None,
            server: String::new(),
            runtime_identity: None,
            capabilities: Vec::new(),
        };
        let frame = client.request_with_timeout(
            ApiRequest::Hello {
                min_version: API_VERSION_MAJOR,
                max_version: API_VERSION_MAJOR,
                client: options.client_name.clone(),
                supports_user_response: options.supports_user_response,
            },
            hello_timeout,
        )?;
        match frame.event {
            ApiEvent::HelloOk {
                server,
                capabilities,
                runtime_identity,
                ..
            } => {
                client.server = server;
                client.runtime_identity = runtime_identity;
                client.capabilities = capabilities;
                Ok(client)
            }
            ApiEvent::Error { code, message } => Err(Error::new(ErrorKind::Harness(code), message)),
            other => Err(Error::new(
                ErrorKind::HandshakeFailed,
                format!("unexpected reply to hello: {other:?}"),
            )),
        }
    }

    /// The socket this client is talking to. Empty for SSH connections.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.inner.socket_path
    }

    /// Whether the server advertises a capability.
    ///
    /// Capabilities are how a client learns what this particular server can do
    /// before depending on it. `permissions`, for instance, is absent from the
    /// current bridge: it never issues permission prompts, so a client that
    /// waits for one waits forever. Check rather than assume.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Send a raw request and wait for its reply frame.
    pub fn request(&self, request: ApiRequest) -> Result<ServerFrame> {
        self.request_with_timeout(request, self.inner.request_timeout)
    }

    fn request_with_timeout(
        &self,
        request: ApiRequest,
        timeout: Option<Duration>,
    ) -> Result<ServerFrame> {
        if let ApiRequest::AskResponse { response, .. } = &request
            && response.answers.as_ref().is_some_and(|answers| {
                answers.iter().any(|answer| {
                    !answer.other_images.is_empty()
                        || answer
                            .comment_images
                            .values()
                            .any(|images| !images.is_empty())
                })
            })
            && !self.supports("ask_response_images")
        {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "This server cannot receive images in ask answers. Update the jcode runtime and API bridge before sending; your answer has not been sent.",
            ));
        }
        if matches!(
            &request,
            ApiRequest::QueuePrompt {
                active_skill: Some(_),
                ..
            }
        ) && !self.supports("queued_prompt_active_skill")
        {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "This server cannot preserve queued skills. Update the jcode runtime and API bridge before sending; your prompt has not been sent.",
            ));
        }
        let (tx, rx) = channel();
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.pending.lock().map_err(poisoned)?.insert(id, tx);
        if let Err(error) = self.write(ClientFrame::new(id, request.clone())) {
            self.inner.pending.lock().map_err(poisoned)?.remove(&id);
            return Err(error);
        }
        let received = match timeout {
            Some(timeout) => rx.recv_timeout(timeout).map_err(|error| match error {
                RecvTimeoutError::Timeout => Error::new(
                    ErrorKind::Timeout,
                    format!("no reply to {} within {timeout:?}", request_name(&request)),
                ),
                RecvTimeoutError::Disconnected => self.disconnect_error(),
            }),
            None => rx.recv().map_err(|_| self.disconnect_error()),
        };
        if received.is_err() {
            self.inner.pending.lock().map_err(poisoned)?.remove(&id);
        }
        received
    }

    /// Write a request without expecting a request-level reply.
    pub fn notify(&self, request: ApiRequest) -> Result<()> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.write(ClientFrame::new(id, request))
    }

    fn write(&self, frame: ClientFrame) -> Result<()> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(self.disconnect_error());
        }
        let mut writer = self.inner.writer.lock().map_err(poisoned)?;
        write_frame(&mut *writer, &frame).map_err(|error| {
            if let Some(process) = &self.inner.ssh_process {
                process.shutdown();
                Error::new(
                    ErrorKind::Disconnected,
                    format!("{error}; {}", process.diagnostic()),
                )
            } else {
                Error::from(error)
            }
        })
    }

    /// The terminal reader failure, once known. Malformed frames carry
    /// `ErrorKind::Protocol` so callers can stop retrying corrupt history.
    /// An ordinary EOF remains reconnectable (`ErrorKind::Disconnected`).
    pub fn disconnect_reason(&self) -> Option<Error> {
        // A poisoned lock still holds the recorded cause. Report it rather
        // than degrading to a generic disconnect.
        match self.inner.disconnect_reason.lock() {
            Ok(reason) => reason.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn disconnect_error(&self) -> Error {
        if let Some(mut error) = self.disconnect_reason() {
            if let Some(process) = &self.inner.ssh_process {
                error.message = format!("{}; {}", error.message, process.diagnostic());
            }
            return error;
        }
        match &self.inner.ssh_process {
            Some(process) => Error::new(ErrorKind::Disconnected, process.diagnostic()),
            None => closed_error(),
        }
    }

    /// Send a request, failing when the server replies with an error frame.
    fn request_ok(&self, request: ApiRequest) -> Result<ServerFrame> {
        let frame = self.request(request)?;
        match frame.event {
            ApiEvent::Error { code, message } => Err(Error::new(ErrorKind::Harness(code), message)),
            _ => Ok(frame),
        }
    }

    /// Subscribe to stream events, optionally filtered to one session.
    ///
    /// Frames that arrive between reads are buffered, so a consumer that does
    /// slow work in the loop body does not silently drop deltas.
    pub fn events(&self, session_id: Option<&str>) -> EventStream {
        let (tx, rx) = channel();
        let id = self.inner.next_sub.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut subs) = self.inner.subscribers.lock() {
            subs.push((id, session_id.map(str::to_string), tx));
        }
        EventStream {
            rx,
            id,
            inner: Arc::clone(&self.inner),
        }
    }

    /// Stream events from all persisted and newly-created sessions.
    ///
    /// The bridge attaches one session per connection, so this discovers
    /// sessions repeatedly, owns one native child connection per session, and
    /// fans their events into a bounded queue. A disconnected child is removed
    /// and attached again by a later discovery pass.
    pub fn global_events(&self, options: GlobalEventsOptions) -> Result<GlobalEventStream> {
        if !self.inner.native_socket && self.ssh_options.is_none() {
            return Err(Error::new(
                ErrorKind::UnsupportedTransport,
                "global_events requires a native socket or SSH connection; custom transports cannot be cloned into per-session child connections",
            ));
        }
        if self.is_closed() {
            return Err(self.disconnect_error());
        }
        if options.max_buffered_events == 0 {
            return Err(Error::new(
                ErrorKind::InvalidOption,
                "max_buffered_events must be positive",
            ));
        }

        let (tx, rx) = sync_channel(options.max_buffered_events);
        let control = Arc::new(GlobalEventControl {
            stopped: AtomicBool::new(false),
            terminal_error: Mutex::new(None),
            children: Mutex::new(HashMap::new()),
            tx,
            max_buffered_events: options.max_buffered_events,
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
        });
        let parent = self.clone();
        let discovery_control = Arc::clone(&control);
        let discovery = std::thread::spawn(move || {
            discover_global_sessions(parent, discovery_control, options.discovery_interval);
        });
        Ok(GlobalEventStream {
            rx,
            control,
            discovery: Some(discovery),
        })
    }

    // --- Curated surface -----------------------------------------------------

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        match self
            .request_ok(ApiRequest::ListSessions {
                include_archived: false,
                limit: None,
            })?
            .event
        {
            ApiEvent::Sessions { sessions, .. } => Ok(sessions),
            other => Err(unexpected("sessions", &other)),
        }
    }

    /// Search every stored session's titles and transcripts.
    ///
    /// The reply is a distinct event rather than a list so a client can apply
    /// its own debounce without the transport second-guessing it.
    pub fn search_sessions(
        &self,
        query: &str,
        limit: Option<u32>,
    ) -> Result<jcode_harness_api::SessionSearchResults> {
        match self
            .request_ok(ApiRequest::SearchSessions {
                query: query.to_string(),
                limit,
            })?
            .event
        {
            ApiEvent::SessionSearch {
                query,
                titles,
                messages,
                indexed_sessions,
                truncated,
            } => Ok(jcode_harness_api::SessionSearchResults {
                query,
                titles,
                messages,
                indexed_sessions,
                truncated,
            }),
            other => Err(unexpected("session_search", &other)),
        }
    }

    /// List only the newest persisted sessions, for bounded dashboard startup.
    pub fn list_sessions_limited(&self, limit: u32) -> Result<Vec<SessionInfo>> {
        match self
            .request_ok(ApiRequest::ListSessions {
                include_archived: false,
                limit: Some(limit),
            })?
            .event
        {
            ApiEvent::Sessions { sessions, .. } => Ok(sessions),
            other => Err(unexpected("sessions", &other)),
        }
    }

    /// Reversibly hide a session from the default list. Its transcript remains
    /// available for a later restore.
    pub fn archive_session(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::ArchiveSession {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    /// Put an archived session back in the default session list.
    pub fn restore_session(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::RestoreSession {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    /// Automatically archive inactive sessions after `archive_after_days`.
    /// `None` disables automatic archival.
    pub fn set_retention_policy(&self, archive_after_days: Option<u32>) -> Result<()> {
        self.request_ok(ApiRequest::SetRetentionPolicy { archive_after_days })
            .map(drop)
    }

    /// Create a session with the normal assembled system prompt.
    pub fn create_session(&self, working_dir: Option<String>) -> Result<SessionInfo> {
        self.create_session_with_options(CreateSessionOptions {
            working_dir,
            ..Default::default()
        })
    }

    /// Create a session with optional full system prompt replacement.
    /// See [`CreateSessionOptions::system_prompt`] for override semantics.
    pub fn create_session_with_options(
        &self,
        options: CreateSessionOptions,
    ) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::CreateSession {
                working_dir: options.working_dir,
                system_prompt: options.system_prompt,
            })?
            .event
        {
            ApiEvent::Attached { session } => Ok(session),
            other => Err(unexpected("attached", &other)),
        }
    }

    pub fn attach_session(&self, session_id: &str) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::AttachSession {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::Attached { session } => Ok(session),
            other => Err(unexpected("attached", &other)),
        }
    }

    /// Clone a session's complete persisted context into a new idle session.
    pub fn transfer_session(&self, session_id: &str) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::TransferSession {
                session_id: session_id.into(),
            })?
            .event
        {
            ApiEvent::SessionForked { session } => Ok(session),
            other => Err(unexpected("session_forked", &other)),
        }
    }

    pub fn fork_session(&self, session_id: &str) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::ForkSession {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::SessionForked { session } => Ok(session),
            other => Err(unexpected("session_forked", &other)),
        }
    }

    pub fn detach_session(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::DetachSession {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    /// Submit a hidden continuation without a user transcript row or accept wait.
    /// The caller, not the bridge, decides when recovery is appropriate.
    pub fn send_system_reminder(&self, session_id: &str, reminder: &str) -> Result<()> {
        self.notify(ApiRequest::SendMessage {
            session_id: session_id.to_string(),
            content: String::new(),
            system_reminder: Some(reminder.to_string()),
            images: Vec::new(),
            attachments: Vec::new(),
            no_reply: false,
        })
    }

    /// Send a user message.
    ///
    /// The harness does not reply to `send_message` at the request level: it
    /// acknowledges by emitting `message_accepted` once the agent has the
    /// message. Waiting for a reply here would always time out, so the frame
    /// is written and the acknowledgement event is awaited instead. Pass a
    /// `None` timeout for pure fire-and-forget.
    pub fn send_message(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        wait_for_accept: Option<Duration>,
    ) -> Result<()> {
        self.send_message_with_reminder(session_id, content, images, None, wait_for_accept)
    }

    /// Send a prompt with a skill resolved and activated by the daemon, not the client.
    pub fn send_skill_message(
        &self,
        session_id: &str,
        content: &str,
        active_skill: &str,
        images: Vec<(String, String)>,
        system_reminder: Option<&str>,
    ) -> Result<()> {
        self.notify(ApiRequest::SendSkillMessage {
            session_id: session_id.to_string(),
            content: content.to_string(),
            active_skill: active_skill.to_string(),
            system_reminder: system_reminder.map(str::to_owned),
            images,
            attachments: Vec::new(),
        })
    }

    /// [`send_message`](Self::send_message) with a hidden system reminder
    /// for this turn only, such as a collection's instructions. The reminder
    /// guides the model without appearing in the visible transcript.
    pub fn send_message_with_reminder(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        system_reminder: Option<&str>,
        wait_for_accept: Option<Duration>,
    ) -> Result<()> {
        self.send_message_with_attachments_and_reminder(
            session_id,
            content,
            images,
            Vec::new(),
            system_reminder,
            wait_for_accept,
        )
    }

    pub fn send_message_with_attachments(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        attachments: Vec<jcode_harness_api::Attachment>,
        wait_for_accept: Option<Duration>,
    ) -> Result<()> {
        self.send_message_with_attachments_and_reminder(
            session_id,
            content,
            images,
            attachments,
            None,
            wait_for_accept,
        )
    }

    fn send_message_with_attachments_and_reminder(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        attachments: Vec<jcode_harness_api::Attachment>,
        system_reminder: Option<&str>,
        wait_for_accept: Option<Duration>,
    ) -> Result<()> {
        // Subscribe *before* writing: an ack that lands between the write and
        // the subscribe would otherwise be missed and time out a healthy turn.
        let stream = wait_for_accept.map(|_| self.events(Some(session_id)));
        self.notify(ApiRequest::SendMessage {
            session_id: session_id.to_string(),
            content: content.to_string(),
            system_reminder: system_reminder
                .map(str::trim)
                .filter(|reminder| !reminder.is_empty())
                .map(str::to_owned),
            images,
            attachments,
            no_reply: false,
        })?;
        if let (Some(stream), Some(timeout)) = (stream, wait_for_accept) {
            let deadline = std::time::Instant::now() + timeout;
            while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
                match stream.next_timeout(remaining) {
                    // Returning on timeout rather than failing keeps a missing
                    // ack from failing an otherwise healthy turn: the stream is
                    // the source of truth.
                    None => break,
                    Some(ApiEvent::MessageAccepted { .. }) => break,
                    Some(_) => continue,
                }
            }
        }
        Ok(())
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::Cancel {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    pub fn force_stop_turn(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::ForceStopTurn {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    pub fn soft_interrupt(&self, session_id: &str, content: &str, urgent: bool) -> Result<()> {
        self.soft_interrupt_with_images(session_id, content, Vec::new(), urgent)
    }

    /// Inject text and image attachments at the next safe point without cancelling.
    pub fn soft_interrupt_with_images(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        urgent: bool,
    ) -> Result<()> {
        self.soft_interrupt_with_id(session_id, content, images, urgent, None)
    }

    /// Queue a soft interrupt tagged with an opaque `client_id`.
    ///
    /// The id comes back in [`ApiEvent::SoftInterruptInjected`] when the model
    /// receives the message, and [`Self::cancel_soft_interrupt`] can take it
    /// back before then. Needs the `soft_interrupt_ids` capability; an older
    /// server accepts the message but drops the id.
    pub fn soft_interrupt_with_id(
        &self,
        session_id: &str,
        content: &str,
        images: Vec<(String, String)>,
        urgent: bool,
        client_id: Option<&str>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::SoftInterrupt {
            session_id: session_id.to_string(),
            content: content.to_string(),
            images,
            urgent,
            client_id: client_id.map(str::to_string),
        })
        .map(drop)
    }

    /// Queue a prompt on the daemon, preserving its mode and caller ID.
    pub fn queue_prompt(
        &self,
        session_id: &str,
        prompt: jcode_harness_api::QueuedPrompt,
    ) -> Result<()> {
        self.queue_prompt_with_reminder(session_id, prompt, None)
    }

    /// Queue a prompt with a hidden instruction for the next standalone turn.
    pub fn queue_prompt_with_reminder(
        &self,
        session_id: &str,
        prompt: jcode_harness_api::QueuedPrompt,
        system_reminder: Option<&str>,
    ) -> Result<()> {
        self.queue_prompt_with_options(session_id, prompt, system_reminder)
    }

    /// Queue a prompt with a skill resolved and validated by the daemon.
    pub fn queue_prompt_with_skill(
        &self,
        session_id: &str,
        mut prompt: jcode_harness_api::QueuedPrompt,
        active_skill: Option<&str>,
    ) -> Result<()> {
        prompt.active_skill = active_skill.map(str::to_owned);
        self.queue_prompt_with_options(session_id, prompt, None)
    }

    fn queue_prompt_with_options(
        &self,
        session_id: &str,
        prompt: jcode_harness_api::QueuedPrompt,
        system_reminder: Option<&str>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::QueuePrompt {
            session_id: session_id.into(),
            client_id: prompt.client_id,
            content: prompt.content,
            images: prompt.images,
            attachments: prompt.attachments,
            mode: prompt.mode,
            system_reminder: system_reminder.map(str::to_owned),
            active_skill: prompt.active_skill,
        })
        .map(drop)
    }

    pub fn set_queued_prompt_mode(
        &self,
        session_id: &str,
        client_id: &str,
        mode: jcode_harness_api::QueuedPromptMode,
    ) -> Result<()> {
        self.request_ok(ApiRequest::SetQueuedPromptMode {
            session_id: session_id.into(),
            client_id: client_id.into(),
            mode,
        })
        .map(drop)
    }

    pub fn interrupt_queued_prompt(&self, session_id: &str, client_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::InterruptQueuedPrompt {
            session_id: session_id.into(),
            client_id: client_id.into(),
        })
        .map(drop)
    }

    pub fn interrupt_queued_prompts(&self, session_id: &str, ids: Vec<String>) -> Result<()> {
        self.request_ok(ApiRequest::InterruptQueuedPrompts {
            session_id: session_id.into(),
            ids,
        })
        .map(drop)
    }

    pub fn get_queued_prompts(
        &self,
        session_id: &str,
    ) -> Result<Vec<jcode_harness_api::QueuedPrompt>> {
        match self
            .request_ok(ApiRequest::GetQueuedPrompts {
                session_id: session_id.into(),
            })?
            .event
        {
            ApiEvent::QueuedPrompts { prompts, .. } => Ok(prompts),
            other => Err(unexpected("queued_prompts", &other)),
        }
    }

    pub fn remove_queued_prompt(
        &self,
        session_id: &str,
        client_id: &str,
        prompt_id: &str,
    ) -> Result<jcode_harness_api::QueuedPromptRemovalOutcome> {
        match self
            .request_ok(ApiRequest::RemoveQueuedPrompt {
                session_id: session_id.into(),
                client_id: client_id.into(),
                prompt_id: prompt_id.into(),
            })?
            .event
        {
            ApiEvent::QueuedPromptRemoved { outcome, .. } => Ok(outcome),
            other => Err(unexpected("queued_prompt_removed", &other)),
        }
    }

    pub fn reorder_queued_prompts(
        &self,
        session_id: &str,
        client_id: &str,
        order: Vec<String>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::ReorderQueuedPrompts {
            session_id: session_id.into(),
            client_id: client_id.into(),
            order,
        })
        .map(drop)
    }

    /// Hold a queued prompt open for editing (or release the hold) so the
    /// daemon cannot deliver it while the user edits it.
    pub fn set_queued_prompt_editing(
        &self,
        session_id: &str,
        client_id: &str,
        prompt_id: &str,
        editing: bool,
    ) -> Result<()> {
        self.request_ok(ApiRequest::SetQueuedPromptEditing {
            session_id: session_id.into(),
            client_id: client_id.into(),
            prompt_id: prompt_id.into(),
            editing,
        })
        .map(drop)
    }

    /// Replace a still-queued prompt in place, preserving its position, mode,
    /// and active skill.
    pub fn edit_queued_prompt(
        &self,
        session_id: &str,
        client_id: &str,
        prompt_id: &str,
        content: String,
        images: Vec<(String, String)>,
    ) -> Result<jcode_harness_api::QueuedPromptEditOutcome> {
        self.edit_queued_prompt_with_attachments(
            session_id,
            client_id,
            prompt_id,
            content,
            images,
            Vec::new(),
        )
    }

    pub fn edit_queued_prompt_with_attachments(
        &self,
        session_id: &str,
        client_id: &str,
        prompt_id: &str,
        content: String,
        images: Vec<(String, String)>,
        attachments: Vec<jcode_harness_api::Attachment>,
    ) -> Result<jcode_harness_api::QueuedPromptEditOutcome> {
        match self
            .request_ok(ApiRequest::EditQueuedPrompt {
                session_id: session_id.into(),
                client_id: client_id.into(),
                prompt_id: prompt_id.into(),
                content,
                images,
                attachments,
            })?
            .event
        {
            ApiEvent::QueuedPromptEdited { outcome, .. } => Ok(outcome),
            other => Err(unexpected("queued_prompt_edited", &other)),
        }
    }

    pub fn get_history(&self, session_id: &str) -> Result<Vec<HistoryMessage>> {
        self.get_history_with_images(session_id)
            .map(|(messages, _)| messages)
    }

    /// Fetch one bounded page, rebuilding records split over multiple frames.
    pub fn get_history_page(
        &self,
        session_id: &str,
        before: Option<String>,
    ) -> Result<crate::HistoryPage> {
        crate::bounded_history::page(self, session_id, before)
    }

    /// Fetch a lazy history image in bounded chunks.
    pub fn get_history_image(&self, session_id: &str, key: &str) -> Result<String> {
        crate::bounded_history::image(self, session_id, key)
    }

    pub fn get_history_with_images(
        &self,
        session_id: &str,
    ) -> Result<(Vec<HistoryMessage>, Vec<jcode_harness_api::RenderedImage>)> {
        match self
            .request_ok(ApiRequest::GetHistory {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::History {
                messages, images, ..
            } => Ok((messages, images)),
            other => Err(unexpected("history", &other)),
        }
    }

    /// Fetch an expanded compacted-history window from the attached session.
    pub fn get_compacted_history(
        &self,
        session_id: &str,
        visible_messages: usize,
    ) -> Result<ApiEvent> {
        let event = self
            .request_ok(ApiRequest::GetCompactedHistory {
                session_id: session_id.to_string(),
                visible_messages,
            })?
            .event;
        if matches!(event, ApiEvent::CompactedHistory { .. }) {
            Ok(event)
        } else {
            Err(unexpected("compacted_history", &event))
        }
    }

    pub fn peek_session(
        &self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<HistoryMessage>> {
        match self
            .request_ok(ApiRequest::PeekSession {
                session_id: session_id.to_string(),
                limit,
            })?
            .event
        {
            ApiEvent::History { messages, .. } => Ok(messages),
            other => Err(unexpected("history", &other)),
        }
    }

    pub fn clear(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::Clear {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    pub fn rewind(&self, session_id: &str, message_index: usize) -> Result<()> {
        self.request_ok(ApiRequest::Rewind {
            session_id: session_id.to_string(),
            message_index,
        })
        .map(drop)
    }

    pub fn rewind_transcript(&self, session_id: &str, target: crate::api::RewindAnchor) -> Result<()> {
        self.request_ok(ApiRequest::RewindTranscript {
            session_id: session_id.to_string(),
            target,
        })
        .map(drop)
    }

    /// Answer an `AskRequest`. `AskResponse::default()` declines.
    pub fn respond_to_ask(
        &self,
        session_id: &str,
        request_id: &str,
        response: jcode_harness_api::AskResponse,
    ) -> Result<()> {
        self.request_ok(ApiRequest::AskResponse {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            response,
        })
        .map(drop)
    }

    pub fn respond_to_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.request_ok(ApiRequest::PermissionResponse {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            decision,
        })
        .map(drop)
    }

    /// Routes and default model for a new local conversation. Does not create one.
    pub fn list_draft_models(
        &self,
    ) -> Result<(Option<String>, Vec<jcode_harness_api::ModelRouteInfo>)> {
        if !self.supports("draft_model_catalog") {
            return Ok((None, Vec::new()));
        }
        match self.request_ok(ApiRequest::ListDraftModels)?.event {
            ApiEvent::DraftModels { current, routes } => Ok((current, routes)),
            other => Err(unexpected("draft_models", &other)),
        }
    }

    /// Models this session can switch to, and which one is serving it.
    pub fn list_models(&self, session_id: &str) -> Result<(Vec<String>, Option<String>)> {
        match self
            .request_ok(ApiRequest::ListModels {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::Models {
                models, current, ..
            } => Ok((models, current)),
            other => Err(unexpected("models", &other)),
        }
    }

    /// Runtime identity, route catalog, protocol metadata, and a live health
    /// check for a session.
    pub fn runtime_command(
        &self,
        session_id: &str,
        command: jcode_harness_api::RuntimeCommand,
        args: Vec<String>,
    ) -> Result<jcode_harness_api::RuntimeCommandOutput> {
        // Hello capabilities describe a point-in-time probe. A busy daemon may
        // omit this capability temporarily, and an updated bridge can recheck
        // it without disturbing the attached session. Send the typed request
        // once and let the bridge enforce support before forwarding it.
        match self
            .request_ok(ApiRequest::RuntimeCommand {
                session_id: session_id.into(),
                command,
                args,
            })?
            .event
        {
            ApiEvent::RuntimeCommandResult {
                session_id,
                command,
                data,
                message,
            } => Ok(jcode_harness_api::RuntimeCommandOutput {
                session_id,
                command,
                data,
                message,
            }),
            other => Err(unexpected("runtime_command_result", &other)),
        }
    }

    pub fn get_runtime_info(&self, session_id: &str) -> Result<RuntimeInfo> {
        self.ping()?;
        match self
            .request_ok(ApiRequest::GetRuntimeInfo {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::RuntimeInfo {
                session_id,
                provider,
                model,
                reasoning_effort,
                auth_method,
                account_label,
                routes,
            } => {
                let mut providers = Vec::new();
                if let Some(provider) = provider.as_ref() {
                    providers.push(provider.clone());
                }
                for route in &routes {
                    if !providers.contains(&route.provider) {
                        providers.push(route.provider.clone());
                    }
                }
                Ok(RuntimeInfo {
                    server: self.server.clone(),
                    protocol_version: API_VERSION_MAJOR,
                    capabilities: self.capabilities.clone(),
                    healthy: true,
                    session_id,
                    provider,
                    model,
                    reasoning_effort,
                    auth_method,
                    account_label,
                    providers,
                    routes,
                })
            }
            other => Err(unexpected("runtime_info", &other)),
        }
    }

    /// Persist an API key in jcode's owner-only provider store and hot-reload
    /// provider credentials.
    pub fn set_api_key(&self, provider: &str, api_key: &str) -> Result<()> {
        match self
            .request_ok(ApiRequest::SetApiKey {
                provider: provider.to_string(),
                api_key: api_key.to_string(),
            })?
            .event
        {
            ApiEvent::CredentialUpdated { .. } => Ok(()),
            other => Err(unexpected("credential_updated", &other)),
        }
    }

    /// Reload credentials saved by an out-of-band OAuth login. This does not
    /// send a chat message or transport any credential material.
    pub fn notify_auth_changed(&self, provider: &str) -> Result<()> {
        match self
            .request_ok(ApiRequest::NotifyAuthChanged {
                provider: provider.to_string(),
            })?
            .event
        {
            ApiEvent::Ok => Ok(()),
            other => Err(unexpected("ok", &other)),
        }
    }

    /// Tell the daemon a banked usage reset was redeemed for one subscription
    /// login (`claude` or `openai`), so it refetches quota instead of keeping a
    /// pre-reset cooldown. Carries no credentials and never redeems anything.
    pub fn invalidate_usage(&self, provider: &str, account_label: Option<&str>) -> Result<()> {
        match self
            .request_ok(ApiRequest::InvalidateUsage {
                provider: provider.to_string(),
                account_label: account_label.map(str::to_string),
            })?
            .event
        {
            ApiEvent::Ok => Ok(()),
            other => Err(unexpected("ok", &other)),
        }
    }

    /// Remove a persisted API-key credential and hot-reload provider
    /// credentials.
    pub fn clear_api_key(&self, provider: &str) -> Result<()> {
        match self
            .request_ok(ApiRequest::ClearApiKey {
                provider: provider.to_string(),
            })?
            .event
        {
            ApiEvent::CredentialUpdated { .. } => Ok(()),
            other => Err(unexpected("credential_updated", &other)),
        }
    }

    /// Read one UTF-8 file under the session working directory.
    pub fn read_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: Option<u64>,
    ) -> Result<FileContent> {
        match self
            .request_ok(ApiRequest::ReadFile {
                session_id: session_id.to_string(),
                path: path.to_string(),
                max_bytes,
            })?
            .event
        {
            ApiEvent::FileContent {
                path,
                content,
                size,
                truncated,
                ..
            } => Ok(FileContent {
                path,
                content,
                size,
                truncated,
            }),
            other => Err(unexpected("file_content", &other)),
        }
    }

    /// Find files by case-insensitive path substring under the session root.
    pub fn find_files(
        &self,
        session_id: &str,
        query: &str,
        limit: Option<u32>,
    ) -> Result<Vec<String>> {
        match self
            .request_ok(ApiRequest::FindFiles {
                session_id: session_id.to_string(),
                query: query.to_string(),
                limit,
            })?
            .event
        {
            ApiEvent::Files { paths, .. } => Ok(paths),
            other => Err(unexpected("files", &other)),
        }
    }

    /// Search UTF-8 files under the session root for a literal string.
    pub fn search_text(
        &self,
        session_id: &str,
        query: &str,
        options: SearchTextOptions,
    ) -> Result<Vec<TextMatch>> {
        match self
            .request_ok(ApiRequest::SearchText {
                session_id: session_id.to_string(),
                query: query.to_string(),
                path: options.path,
                limit: options.limit,
            })?
            .event
        {
            ApiEvent::TextMatches { matches, .. } => Ok(matches),
            other => Err(unexpected("text_matches", &other)),
        }
    }

    /// Read safe filesystem metadata for a path under the session root.
    /// The session's persisted todo state for the auto-poke and quality-gate
    /// policy. Requires the `todo_state` capability.
    pub fn todo_state(&self, session_id: &str) -> Result<jcode_todo_policy::TodoSnapshot> {
        match self
            .request_ok(ApiRequest::GetTodoState {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::TodoState { state, .. } => Ok(state),
            other => Err(unexpected("todo_state", &other)),
        }
    }

    /// Apply a follow-up decision's persistent effects. A no-op when empty.
    pub fn ack_todo_follow_up(
        &self,
        session_id: &str,
        effects: jcode_todo_policy::TodoEffects,
    ) -> Result<()> {
        if effects.is_empty() {
            return Ok(());
        }
        self.request_ok(ApiRequest::AckTodoFollowUp {
            session_id: session_id.to_string(),
            effects,
        })
        .map(drop)
    }

    pub fn file_status(&self, session_id: &str, path: &str) -> Result<FileStatus> {
        match self
            .request_ok(ApiRequest::FileStatus {
                session_id: session_id.to_string(),
                path: path.to_string(),
            })?
            .event
        {
            ApiEvent::FileStatus {
                path,
                exists,
                kind,
                size,
                modified_ms,
                ..
            } => Ok(FileStatus {
                path,
                exists,
                kind,
                size,
                modified_ms,
            }),
            other => Err(unexpected("file_status", &other)),
        }
    }

    /// Configure the tools available to a session before starting a turn.
    ///
    /// Custom tool invocations arrive as [`ApiEvent::ToolCall`] on [`Self::events`].
    /// Subscribe before sending a message and answer each invocation with
    /// [`Self::submit_tool_result`]. The SDK does not execute custom tools itself.
    pub fn configure_tools(&self, session_id: &str, tools: ToolConfiguration) -> Result<()> {
        match self
            .request_ok(ApiRequest::ConfigureTools {
                session_id: session_id.to_string(),
                tools,
            })?
            .event
        {
            ApiEvent::Ok => Ok(()),
            other => Err(unexpected("ok", &other)),
        }
    }

    /// List the effective tool definitions available to a session.
    pub fn list_tools(&self, session_id: &str) -> Result<Vec<SessionToolDefinition>> {
        match self
            .request_ok(ApiRequest::ListTools {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::Tools { tools, .. } => Ok(tools),
            other => Err(unexpected("tools", &other)),
        }
    }

    /// Complete a custom [`ApiEvent::ToolCall`] using its session and call ids.
    ///
    /// Pass textual output (serialize structured results as JSON) and `None`
    /// for success, or `Some(message)` to report a tool execution failure.
    pub fn submit_tool_result(
        &self,
        session_id: &str,
        call_id: &str,
        output: &str,
        error: Option<String>,
    ) -> Result<()> {
        match self
            .request_ok(ApiRequest::ToolResult {
                session_id: session_id.to_string(),
                call_id: call_id.to_string(),
                output: output.to_string(),
                error,
            })?
            .event
        {
            ApiEvent::Ok => Ok(()),
            other => Err(unexpected("ok", &other)),
        }
    }

    /// Switch the session to a different model. `model` is an id from
    /// `list_models`.
    pub fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
        self.request_ok(ApiRequest::SetModel {
            session_id: session_id.to_string(),
            model: model.to_string(),
        })
        .map(drop)
    }

    /// Override the context budget. None or zero clears it. Returns the
    /// authoritative effective budget after applying the override.
    pub fn set_context_cap(
        &self,
        session_id: &str,
        max_context_tokens: Option<usize>,
    ) -> Result<usize> {
        match self
            .request_ok(ApiRequest::SetContextCap {
                session_id: session_id.to_string(),
                max_context_tokens,
            })?
            .event
        {
            ApiEvent::ContextCapChanged {
                effective_budget, ..
            } => Ok(effective_budget),
            other => Err(unexpected("context_cap_changed", &other)),
        }
    }

    /// Set how much the model deliberates before answering. The accepted set
    /// is per-provider, so this takes a string rather than a union that would
    /// go stale.
    pub fn set_reasoning_effort(&self, session_id: &str, effort: &str) -> Result<()> {
        self.request_ok(ApiRequest::SetReasoningEffort {
            session_id: session_id.to_string(),
            effort: effort.to_string(),
        })
        .map(drop)
    }

    /// Queue compaction without waiting for an active turn to finish. The
    /// reply acknowledges the request. Progress and outcome arrive separately
    /// as compaction divider or background progress events.
    pub fn compact(&self, session_id: &str) -> Result<String> {
        match self
            .request_ok(ApiRequest::Compact {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::Compacted { message, .. } => Ok(message),
            other => Err(unexpected("compacted", &other)),
        }
    }

    /// Set a session's title. `None` restores the generated one.
    pub fn rename_session(&self, session_id: &str, title: Option<String>) -> Result<()> {
        self.request_ok(ApiRequest::RenameSession {
            session_id: session_id.to_string(),
            title,
        })
        .map(drop)
    }

    /// Replace the session's process-local environment overlay. Empty clears it.
    pub fn set_session_env(
        &self,
        session_id: &str,
        env: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::SetSessionEnv {
            session_id: session_id.to_string(),
            env,
        })
        .map(drop)
    }

    /// Bookmark or unbookmark a session. A label also becomes its title.
    pub fn set_session_saved(
        &self,
        session_id: &str,
        saved: bool,
        label: Option<String>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::SetSessionSaved {
            session_id: session_id.to_string(),
            saved,
            label,
        })
        .map(drop)
    }

    /// Report a user action in an agent applet instance.
    pub fn applet_action(
        &self,
        session_id: &str,
        instance: &str,
        action: jcode_applet_types::Action,
        state: serde_json::Value,
        source_key: Option<String>,
    ) -> Result<()> {
        self.request_ok(ApiRequest::AppletAction {
            session_id: session_id.to_string(),
            instance: instance.to_string(),
            action,
            state,
            source_key,
        })
        .map(drop)
    }

    /// Close an agent applet instance. The agent is not woken.
    pub fn close_applet(&self, session_id: &str, instance: &str) -> Result<()> {
        self.request_ok(ApiRequest::CloseApplet {
            session_id: session_id.to_string(),
            instance: instance.to_string(),
        })
        .map(drop)
    }

    /// Restore the history the last `rewind` removed.
    pub fn rewind_undo(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::RewindUndo {
            session_id: session_id.to_string(),
        })
        .map(drop)
    }

    /// Drop soft interrupts that are queued but not yet delivered.
    pub fn cancel_soft_interrupts(&self, session_id: &str) -> Result<()> {
        self.request_ok(ApiRequest::CancelSoftInterrupts {
            session_id: session_id.to_string(),
            client_ids: None,
        })
        .map(drop)
    }

    /// Take back specific queued soft interrupts by `client_id`.
    ///
    /// Returns `(cancelled, not_queued)`. The server decides against injection
    /// atomically, so an id in `cancelled` was never seen by the model, and an
    /// id in `not_queued` was already delivered (or never queued).
    pub fn cancel_soft_interrupts_by_id(
        &self,
        session_id: &str,
        client_ids: &[&str],
    ) -> Result<(Vec<String>, Vec<String>)> {
        let request = ApiRequest::CancelSoftInterrupts {
            session_id: session_id.to_string(),
            client_ids: Some(client_ids.iter().map(|id| id.to_string()).collect()),
        };
        match self.request_ok(request)?.event {
            ApiEvent::SoftInterruptsCancelled {
                cancelled,
                not_queued,
            } => Ok((cancelled, not_queued)),
            other => Err(unexpected("soft_interrupts_cancelled", &other)),
        }
    }

    /// Take back one queued soft interrupt. `true` means it was removed before
    /// the model saw it, `false` that it was already delivered (or unknown).
    pub fn cancel_soft_interrupt(&self, session_id: &str, client_id: &str) -> Result<bool> {
        self.cancel_soft_interrupts_by_id(session_id, &[client_id])
            .map(|(cancelled, _)| !cancelled.is_empty())
    }

    /// Move the running tool call to the background (the TUI's Alt+B).
    pub fn background_tool(&self, session_id: &str) -> Result<()> {
        self.background_tool_result(session_id).map(drop)
    }

    /// Move the running tool call to the background and report what moved.
    ///
    /// Returns `(moved, tool_call_id)`. A server without the
    /// `background_tool_result` capability answers only `Ok`, reported here as
    /// `(false, None)` even if a tool was moved.
    pub fn background_tool_result(&self, session_id: &str) -> Result<(bool, Option<String>)> {
        match self
            .request_ok(ApiRequest::BackgroundTool {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::BackgroundToolResult {
                moved,
                tool_call_id,
                ..
            } => Ok((moved, tool_call_id)),
            _ => Ok((false, None)),
        }
    }

    pub fn ping(&self) -> Result<()> {
        self.request_ok(ApiRequest::Ping).map(drop)
    }

    /// Send a message and collect the assistant reply until the turn ends.
    ///
    /// The convenience path for scripts: one call in, the text and tool calls
    /// of one turn out. Streaming consumers should use `events()` instead.
    pub fn run(&self, session_id: &str, content: &str, options: RunOptions) -> Result<TurnResult> {
        let stream = self.events(Some(session_id));
        self.send_message(
            session_id,
            content,
            options.images.clone(),
            Some(Duration::from_secs(10)),
        )?;
        let mut result = TurnResult::default();
        let mut text_stream = TextCollector::default();
        while let Some(event) = stream.next() {
            if let Some(on_event) = &options.on_event {
                on_event(&event);
            }
            match event {
                ApiEvent::TextDelta {
                    text, message_id, ..
                } => {
                    text_stream.message(message_id).0.text.push_str(&text);
                }
                ApiEvent::TextReplace {
                    text, message_id, ..
                } => {
                    text_stream.message(message_id).0.text = text;
                }
                ApiEvent::TextDone { message_id, .. } => {
                    if let Some(index) = text_stream.index(&message_id) {
                        text_stream.parts[index].1 = true;
                    }
                }
                ApiEvent::ReasoningDelta { text, .. } => result.reasoning.push_str(&text),
                ApiEvent::ToolDone {
                    call_id,
                    name,
                    output,
                    error,
                    ..
                } => result.tool_calls.push(ToolCall {
                    call_id,
                    name,
                    output,
                    error,
                }),
                ApiEvent::TokenUsage {
                    input,
                    output,
                    cache_read_input,
                    cache_creation_input,
                    effective_budget,
                    ..
                } => {
                    result.usage = Some(Usage {
                        input,
                        output,
                        cache_read_input,
                        cache_creation_input,
                        effective_budget,
                    })
                }
                ApiEvent::PermissionRequest { request_id, .. } if options.auto_approve => {
                    self.respond_to_permission(session_id, &request_id, PermissionDecision::Allow)?;
                }
                ApiEvent::TurnStopped {
                    reason, message, ..
                } => {
                    result.stop_reason = Some(reason);
                    result.stop_message = Some(message);
                }
                ApiEvent::TurnDone { .. } => {
                    text_stream.finish(&mut result);
                    return Ok(result);
                }
                ApiEvent::Error { code, message } => {
                    return Err(Error::new(ErrorKind::Harness(code), message));
                }
                _ => {}
            }
        }
        Err(self.disconnect_error())
    }

    /// Whether the connection has been closed or lost.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Relaxed)
    }
}

mod types;
pub use types::*;

#[derive(Default)]
struct TextCollector {
    parts: Vec<(AssistantTextMessage, bool)>,
}

impl TextCollector {
    fn index(&self, id: &Option<String>) -> Option<usize> {
        self.parts
            .iter()
            .rposition(|(part, done)| &part.message_id == id && (id.is_some() || !done))
    }

    fn message(&mut self, id: Option<String>) -> &mut (AssistantTextMessage, bool) {
        let index = self.index(&id).unwrap_or_else(|| {
            self.parts.push((
                AssistantTextMessage {
                    message_id: id,
                    text: String::new(),
                },
                false,
            ));
            self.parts.len() - 1
        });
        &mut self.parts[index]
    }

    fn finish(self, result: &mut TurnResult) {
        result.text = self
            .parts
            .iter()
            .map(|(part, _)| part.text.as_str())
            .collect();
        result.messages = self
            .parts
            .into_iter()
            .filter_map(|(part, done)| (done && !part.text.is_empty()).then_some(part))
            .collect();
        result.final_text = result
            .messages
            .last()
            .map(|part| part.text.clone())
            .unwrap_or_else(|| result.text.clone());
    }
}

#[path = "client/global_session_workers.rs"]
mod global_session_workers;
use global_session_workers::discover_global_sessions;

#[path = "client_reader.rs"]
mod reader;
use reader::*;
/// The session an event belongs to, when it names one.
fn event_session(event: &ApiEvent) -> Option<&str> {
    use ApiEvent::*;
    match event {
        TextDelta { session_id, .. }
        | TextDone { session_id, .. }
        | TextReplace { session_id, .. }
        | ReasoningDelta { session_id, .. }
        | ReasoningDone { session_id, .. }
        | ToolStart { session_id, .. }
        | ToolInputDelta { session_id, .. }
        | ToolExec { session_id, .. }
        | ToolDone { session_id, .. }
        | ToolCall { session_id, .. }
        | Tools { session_id, .. }
        | SidePanelState { session_id, .. }
        | TokenUsage { session_id, .. }
        | RequestUsage { session_id, .. }
        | ContextCapChanged { session_id, .. }
        | ContextOccupancy { session_id, .. }
        | KvCacheMiss { session_id, .. }
        | MemoryInjected { session_id, .. }
        | MemoryActivity { session_id, .. }
        | TurnDone { session_id, .. }
        | SoftInterruptInjected { session_id, .. }
        | TurnStopped { session_id, .. }
        | BackgroundProgress { session_id, .. }
        | BackgroundTasks { session_id, .. }
        | PeerUserMessage { session_id, .. }
        | AgentMessage { session_id, .. }
        | MessageAccepted { session_id, .. }
        | AskRequest { session_id, .. }
        | PermissionRequest { session_id, .. }
        | SessionStatus { session_id, .. }
        | SwarmStatus { session_id, .. }
        | SessionRecovery { session_id, .. }
        | ModelInfo { session_id, .. }
        | RuntimeInfo { session_id, .. }
        | ConnectionPhase { session_id, .. }
        | Models { session_id, .. }
        | Compacted { session_id, .. }
        | SessionRenamed { session_id, .. }
        | History { session_id, .. } => Some(session_id.as_str()),
        Attached { session } => Some(session.session_id.as_str()),
        _ => None,
    }
}

fn closed_error() -> Error {
    Error::new(ErrorKind::Disconnected, "harness connection closed")
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::new(ErrorKind::Transport, "harness client state was poisoned")
}

fn unexpected(want: &str, got: &ApiEvent) -> Error {
    Error::new(
        ErrorKind::UnexpectedReply,
        format!("expected {want}, got {got:?}"),
    )
}

/// The wire name of a request, for error text.
fn request_name(request: &ApiRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| {
            value
                .get("req")
                .and_then(|r| r.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "request".to_string())
}

#[cfg(test)]
mod event_session_tests {
    use super::*;

    #[test]
    fn peer_message_is_scoped_to_its_session() {
        let event = ApiEvent::PeerUserMessage {
            session_id: "session-a".into(),
            content: "hello".into(),
        };
        assert_eq!(event_session(&event), Some("session-a"));
    }

    #[test]
    fn request_usage_is_scoped_to_its_session() {
        let event = ApiEvent::RequestUsage {
            session_id: "session-b".into(),
            request_id: "request-1".into(),
            provider: "anthropic".into(),
            model: "model".into(),
            input: Some(1),
            output: Some(2),
            cache_read_input: Some(0),
            cache_creation_input: None,
        };
        assert_eq!(event_session(&event), Some("session-b"));
    }
}
