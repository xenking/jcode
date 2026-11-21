//! Client for the local Desktop control socket. The caller chooses an explicit
//! endpoint so test instances and the user's running Desktop are never confused.
pub use jcode_harness_api::desktop_control::{
    DESKTOP_CONTROL_VERSION, DesktopAction, DesktopRequest, DesktopResponse, DesktopTarget,
    OpenDestination, RevealRow, WaitCondition,
};
#[cfg(unix)]
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    time::Duration,
};

/// Send one request and receive one response. WaitFor polls State on separate
/// sockets so other clients can continue using the Desktop while waiting.
#[cfg(unix)]
pub fn request(
    path: &Path,
    action: DesktopAction,
    timeout: Duration,
) -> io::Result<DesktopResponse> {
    let action = match action {
        DesktopAction::WaitFor {
            target,
            condition,
            timeout_ms,
        } => {
            if !(1..=120_000).contains(&timeout_ms) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "timeout_ms must be between 1 and 120000",
                ));
            }
            return wait_for(
                path,
                target,
                condition,
                timeout.min(Duration::from_millis(timeout_ms)),
            );
        }
        action => action,
    };
    use std::os::unix::net::UnixStream;
    let mut socket = UnixStream::connect(path)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    serde_json::to_writer(
        &mut socket,
        &DesktopRequest {
            v: DESKTOP_CONTROL_VERSION,
            action,
        },
    )?;
    socket.write_all(b"\n")?;
    let mut line = String::new();
    let n = BufReader::new(socket)
        .take(8 * 1024 * 1024)
        .read_line(&mut line)?;
    if n == 0 || !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete Desktop response",
        ));
    }
    serde_json::from_str(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Poll semantic state without blocking the Desktop event loop. Timeouts are bounded.
#[cfg(unix)]
pub fn wait_for(
    path: &Path,
    target: DesktopTarget,
    condition: WaitCondition,
    timeout: Duration,
) -> io::Result<DesktopResponse> {
    if timeout.is_zero() || timeout > Duration::from_secs(120) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout must be between 1ms and 120s",
        ));
    }
    if let WaitCondition::SelectorVisible { selector } = &condition
        && !selector.starts_with("tool:")
        && !selector.starts_with("row:")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only transcript selector visibility is supported",
        ));
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "wait_for timed out",
            ));
        }
        let response = request(
            path,
            DesktopAction::State {
                target: target.clone(),
            },
            remaining.min(Duration::from_secs(5)),
        )?;
        match &response {
            DesktopResponse::Ok { result } if !condition_met(&condition, result) => {}
            _ => return Ok(response),
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
}

/// Interpret a condition against semantic Desktop state.
pub fn condition_met(condition: &WaitCondition, state: &serde_json::Value) -> bool {
    fn contains(value: &serde_json::Value, text: &str) -> bool {
        match value {
            serde_json::Value::String(value) => value.contains(text),
            serde_json::Value::Array(values) => values.iter().any(|value| contains(value, text)),
            serde_json::Value::Object(values) => values.values().any(|value| contains(value, text)),
            _ => false,
        }
    }
    let panel = &state["panel"];
    match condition {
        WaitCondition::SessionIdle {} => panel["status"].as_str() == Some("idle"),
        WaitCondition::TextContains { text } => {
            contains(&panel["loaded_transcript_items"], text)
                || contains(&panel["streaming_text"], text)
        }
        WaitCondition::SelectorVisible { selector } => panel["visible_transcript_items"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["selector"].as_str() == Some(selector))
            }),
    }
}

#[cfg(unix)]
pub fn socket_dir() -> std::path::PathBuf {
    std::env::var_os("JCODE_DESKTOP_CONTROL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!("/tmp/jdc-{}", unsafe { libc::geteuid() }))
        })
}

/// Discover private endpoints. Reject an unsafe directory rather than trusting
/// filenames planted by another local user.
#[cfg(unix)]
pub fn endpoints() -> io::Result<Vec<std::path::PathBuf>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let dir = socket_dir();
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Desktop control directory",
        ));
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_socket()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn wait_polls_state_until_selector_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for visible in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: DesktopRequest = serde_json::from_str(&line).unwrap();
                assert!(matches!(request.action, DesktopAction::State { .. }));
                let rows = if visible {
                    serde_json::json!([{"selector":"tool:c1"}])
                } else {
                    serde_json::json!([])
                };
                writeln!(stream, "{}", serde_json::json!({"status":"ok", "result":{"panel":{"visible_transcript_items":rows}}})).unwrap();
            }
        });
        let result = request(
            &path,
            DesktopAction::WaitFor {
                target: DesktopTarget {
                    window_id: "w".into(),
                    session_id: "s".into(),
                },
                condition: WaitCondition::SelectorVisible {
                    selector: "tool:c1".into(),
                },
                timeout_ms: 2000,
            },
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(matches!(result, DesktopResponse::Ok { .. }));
        server.join().unwrap();
    }

    #[test]
    fn request_round_trips_through_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let request: DesktopRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.v, DESKTOP_CONTROL_VERSION);
            assert!(matches!(request.action, DesktopAction::List {}));
            writeln!(
                stream,
                "{}",
                serde_json::json!({"status":"ok", "result":{"windows":[]}})
            )
            .unwrap();
        });
        assert_eq!(
            request(&path, DesktopAction::List {}, Duration::from_secs(2)).unwrap(),
            DesktopResponse::Ok {
                result: serde_json::json!({"windows":[]})
            }
        );
        server.join().unwrap();
    }
}
