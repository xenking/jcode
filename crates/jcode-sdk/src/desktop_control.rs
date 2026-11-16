//! Client for the local Desktop control socket. The caller chooses an explicit
//! endpoint so test instances and the user's running Desktop are never confused.
pub use jcode_harness_api::desktop_control::*;
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    time::Duration,
};

/// Send one NDJSON request and receive one response. Each call owns its socket.
#[cfg(unix)]
pub fn request(
    path: &Path,
    action: DesktopAction,
    timeout: Duration,
) -> io::Result<DesktopResponse> {
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
