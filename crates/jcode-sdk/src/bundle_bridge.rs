//! Bundle-scoped bridge discovery. A listening socket is never identity evidence.
use crate::{Error, ErrorKind, Result};
use jcode_build_support::runtime_generation::{self as generation, ImmutableTarget};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Capabilities required by Desktop's bridge generation.
pub const BUNDLE_BRIDGE_CAPABILITIES: &[&str] = &["bridge_lifecycle_v1", "peer_user_message_v1"];

/// A stable endpoint for this exact bundle image, independent of older bridges.
/// Including the file identity also separates two builds installed at one path.
pub fn bundle_bridge_socket(base: &Path, target: &ImmutableTarget) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut hash);
    base.with_file_name(format!("jcode-api-{:016x}.sock", hash.finish()))
}

/// Verify the socket peer's executable, build and required protocol generation.
pub fn verify_bundle_bridge(
    socket: &Path,
    target: &ImmutableTarget,
    timeout: Duration,
) -> Result<()> {
    connect_verified(socket, target, Instant::now() + timeout).map(|_| ())
}

/// Verify the running bundle and its session index on the same connection.
pub fn verify_bundle_bridge_search(
    socket: &Path,
    target: &ImmutableTarget,
    deadline: Instant,
) -> Result<()> {
    use jcode_harness_api::{ApiEvent, ServerFrame, read_frame};
    let mut reader = connect_verified(
        socket,
        target,
        deadline.min(Instant::now() + Duration::from_secs(2)),
    )?;
    reader.get_mut().deadline = deadline.min(Instant::now() + Duration::from_secs(20));
    reader
        .get_mut()
        .write_all(
            b"{\"v\":1,\"id\":2,\"req\":\"search_sessions\",\"query\":\"rollout\",\"limit\":1}\n",
        )
        .map_err(probe_io)?;
    loop {
        let frame: ServerFrame = read_frame(&mut reader).map_err(|error| match error {
            jcode_harness_api::FrameError::Eof => {
                Error::new(ErrorKind::Disconnected, "bridge closed before search reply")
            }
            jcode_harness_api::FrameError::Io(error) => probe_io(error),
            other => Error::new(ErrorKind::Protocol, other.to_string()),
        })?;
        if frame.reply_to != Some(2) {
            continue;
        }
        return match frame.event {
            ApiEvent::SessionSearch { .. } => Ok(()),
            ApiEvent::Error { code, message } => Err(Error::new(ErrorKind::Harness(code), message)),
            other => Err(Error::new(
                ErrorKind::UnexpectedReply,
                format!("expected bridge session search, got {other:?}"),
            )),
        };
    }
}

fn connect_verified(
    socket: &Path,
    target: &ImmutableTarget,
    deadline: Instant,
) -> Result<BufReader<ProbeStream>> {
    let mut stream = ProbeStream::connect(socket, deadline).map_err(probe_io)?;
    let peer = generation::socket_peer_pid(&stream.socket);
    stream.write_all(b"{\"v\":1,\"id\":1,\"req\":\"hello\",\"client\":\"bundle-bridge-probe\",\"min_version\":1,\"max_version\":1}\n").map_err(probe_io)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if (&mut reader)
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(probe_io)?
        == 0
    {
        return Err(Error::new(
            ErrorKind::Disconnected,
            "bridge closed before hello reply",
        ));
    }
    let frame: serde_json::Value = serde_json::from_str(&line).map_err(failed)?;
    let identity = frame
        .get("runtime_identity")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<generation::ServingIdentity>(value.clone()))
        .transpose()
        .map_err(failed)?;
    if frame["ev"] != "hello_ok" || frame["version"] != 1 || frame["reply_to"] != 1 {
        return Err(failed(
            "bridge did not confirm the supported API generation",
        ));
    }
    match generation::assess_serving_target(target, identity.as_ref()) {
        generation::GenerationVerdict::Ready(identity)
            if peer.is_none_or(|pid| pid == identity.pid) => {}
        verdict => {
            return Err(failed(format!(
                "bundle bridge identity not confirmed: {verdict:?}"
            )));
        }
    }
    if !BUNDLE_BRIDGE_CAPABILITIES.iter().all(|required| {
        frame["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some(required)))
    }) {
        return Err(failed(
            "bundle bridge lacks the required capability generation",
        ));
    }
    Ok(reader)
}

struct ProbeStream {
    socket: jcode_transport::SyncStream,
    deadline: Instant,
}

fn time_left(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "bridge probe deadline elapsed",
        ))
    } else {
        Ok(remaining)
    }
}

impl ProbeStream {
    fn connect(path: &Path, deadline: Instant) -> io::Result<Self> {
        #[cfg(unix)]
        let socket = {
            let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
            socket.connect_timeout(&socket2::SockAddr::unix(path)?, time_left(deadline)?)?;
            socket.set_nonblocking(true)?;
            socket.into()
        };
        #[cfg(not(unix))]
        let socket = jcode_transport::SyncStream::connect(path)?;
        Ok(Self { socket, deadline })
    }

    #[cfg(unix)]
    fn io_until<T>(
        &mut self,
        events: libc::c_short,
        mut operation: impl FnMut(&mut jcode_transport::SyncStream) -> io::Result<T>,
    ) -> io::Result<T> {
        use std::os::fd::AsRawFd;
        let mut fd = libc::pollfd {
            fd: self.socket.as_raw_fd(),
            events,
            revents: 0,
        };
        loop {
            time_left(self.deadline)?;
            match operation(&mut self.socket) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                outcome => return outcome,
            }
            let timeout = time_left(self.deadline)?
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            // SAFETY: poll receives a pointer to one initialized pollfd.
            if unsafe { libc::poll(&mut fd, 1, timeout) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

impl Read for ProbeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return self.io_until(libc::POLLIN, |socket| socket.read(buffer));
        #[cfg(not(unix))]
        {
            self.socket
                .set_read_timeout(Some(time_left(self.deadline)?))?;
            self.socket.read(buffer)
        }
    }
}

impl Write for ProbeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return self.io_until(libc::POLLOUT, |socket| socket.write(buffer));
        #[cfg(not(unix))]
        {
            self.socket
                .set_write_timeout(Some(time_left(self.deadline)?))?;
            self.socket.write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

fn probe_io(error: io::Error) -> Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::NotFound
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::NotConnected
        | io::ErrorKind::UnexpectedEof => ErrorKind::Disconnected,
        _ => ErrorKind::Transport,
    };
    Error::new(kind, error.to_string())
}

/// Ask a bridge to stop accepting new clients and drain existing connections.
/// Legacy bridges may reject this request. Never kill their active clients.
pub fn retire_bridge(socket: &Path, timeout: Duration) -> Result<()> {
    retire_bridge_inner(socket, None, timeout)
}

/// Retire only the process observed during discovery. The peer PID is checked
/// on the same connection that carries the request, closing the discovery race.
pub fn retire_bridge_if_pid(socket: &Path, expected_pid: u32, timeout: Duration) -> Result<()> {
    retire_bridge_inner(socket, Some(expected_pid), timeout)
}

fn retire_bridge_inner(socket: &Path, expected_pid: Option<u32>, timeout: Duration) -> Result<()> {
    let mut stream = jcode_transport::SyncStream::connect(socket).map_err(retirement_io)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(retirement_io)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(retirement_io)?;
    if let Some(expected) = expected_pid
        && generation::socket_peer_pid(&stream) != Some(expected)
    {
        return Err(failed(
            "bridge socket peer changed since discovery; retirement not sent",
        ));
    }
    stream
        .write_all(b"{\"v\":1,\"id\":1,\"req\":\"retire_bridge\"}\n")
        .map_err(retirement_io)?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(retirement_io)?;
    let reply: serde_json::Value = serde_json::from_str(&line)
        .map_err(|error| Error::new(ErrorKind::Protocol, error.to_string()))?;
    if reply["ev"] != "bridge_retiring" || reply["reply_to"] != 1 {
        return Err(Error::new(
            ErrorKind::UnsupportedCapability,
            "bridge does not support graceful retirement",
        ));
    }
    Ok(())
}

fn retirement_io(error: std::io::Error) -> Error {
    let kind = if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        ErrorKind::Timeout
    } else {
        ErrorKind::Transport
    };
    Error::new(kind, error.to_string())
}

fn failed(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::HandshakeFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bundle_handshake_requires_path_build_capabilities_and_socket_peer() {
        for mismatch in ["none", "path", "hash", "capabilities", "pid", "protocol"] {
            let root = tempfile::tempdir().unwrap();
            let api = root.path().join("api.sock");
            let mut target = generation::pin_path(&std::env::current_exe().unwrap()).unwrap();
            target.git_hash = Some("abcdef123".into());
            let mut identity = generation::ServingIdentity {
                pid: std::process::id(),
                binary_path: target.binary_path.clone(),
                git_hash: "abcdef123".into(),
                version_label: None,
                reload_id: None,
                generation: 1,
            };
            match mismatch {
                "path" => identity.binary_path = root.path().join("old-bundle/jcode"),
                "hash" => identity.git_hash = "987654321".into(),
                "pid" => identity.pid += 1,
                _ => {}
            }
            let caps = if mismatch == "capabilities" {
                &[][..]
            } else {
                BUNDLE_BRIDGE_CAPABILITIES
            };
            let response = serde_json::json!({"v":1,"reply_to":1,"ev":"hello_ok",
                "version": if mismatch == "protocol" { 2 } else { 1 }, "server":"fixture",
                "runtime_identity":identity,"capabilities":caps});
            let listener = std::os::unix::net::UnixListener::bind(&api).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                writeln!(stream, "{response}").unwrap();
            });
            let result = verify_bundle_bridge(&api, &target, Duration::from_secs(1));
            assert_eq!(result.is_ok(), mismatch == "none", "{mismatch}: {result:?}");
            server.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn changed_peer_is_not_sent_a_retirement_request() {
        let root = tempfile::tempdir().unwrap();
        let api = root.path().join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&api).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // macOS may reject SO_RCVTIMEO after the expected peer has already
            // closed. The client always closes after its bounded PID check.
            let mut line = String::new();
            assert_eq!(
                BufReader::new(stream).read_line(&mut line).unwrap(),
                0,
                "retirement must not be sent to a replacement peer"
            );
        });
        let error =
            retire_bridge_if_pid(&api, std::process::id() + 1, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::HandshakeFailed);
        server.join().unwrap();
    }

    #[test]
    fn non_utf8_bundle_paths_get_stable_distinct_endpoints() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().unwrap();
        let mut target = generation::pin_path(&std::env::current_exe().unwrap()).unwrap();
        target.binary_path = root
            .path()
            .join(std::ffi::OsString::from_vec(b"jcode-\xff".to_vec()));
        let base = root.path().join("api.sock");
        let socket = bundle_bridge_socket(&base, &target);
        assert_eq!(socket, bundle_bridge_socket(&base, &target));
        let other = root.path().join("jcode-other");
        std::fs::write(&other, b"binary").unwrap();
        assert_ne!(
            socket,
            bundle_bridge_socket(&base, &generation::pin_path(&other).unwrap())
        );
    }

    #[test]
    fn old_bundle_and_replaced_image_get_distinct_endpoints() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old/jcode");
        let new = root.path().join("new/jcode");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        let a = generation::pin_path(&old).unwrap();
        let b = generation::pin_path(&new).unwrap();
        let base = root.path().join("api.sock");
        assert_ne!(
            bundle_bridge_socket(&base, &a),
            bundle_bridge_socket(&base, &b)
        );
        assert_eq!(
            bundle_bridge_socket(&base, &a),
            bundle_bridge_socket(&base, &a)
        );
        std::fs::write(&new, b"newer image").unwrap();
        let c = generation::pin_path(&new).unwrap();
        assert_ne!(
            bundle_bridge_socket(&base, &b),
            bundle_bridge_socket(&base, &c)
        );
    }
}
