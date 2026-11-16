//! Socket adapter. A live connection or a legacy Pong is not identity evidence.
use super::{ServingIdentity, validate_serving_identity};
use anyhow::{Context, Result, bail};
use jcode_transport::SyncStream;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

pub fn probe_socket_identity(socket: &Path, timeout: Duration) -> Result<ServingIdentity> {
    let stream = SyncStream::connect(socket)
        .with_context(|| format!("cannot observe serving generation on {}", socket.display()))?;
    probe_stream_identity(stream, timeout)
}

pub(super) fn probe_stream_identity(
    mut stream: SyncStream,
    timeout: Duration,
) -> Result<ServingIdentity> {
    stream.set_read_timeout(Some(timeout))?;
    #[cfg(unix)]
    let peer = socket_peer_pid(&stream);
    #[cfg(not(unix))]
    let peer: Option<u32> = None;
    stream.write_all(b"{\"type\":\"ping\",\"id\":1}\n")?;
    // Identity frames are tiny. Bound hostile/corrupt peers and ignored events.
    let mut reader = BufReader::new(stream).take(64 * 1024);
    for _ in 0..8 {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("listener closed before reporting its serving generation");
        }
        let event: serde_json::Value = serde_json::from_str(&line)?;
        if event["id"].as_u64() != Some(1) {
            continue;
        }
        if event["type"] == "ack" {
            continue;
        }
        if event["type"] != "pong" {
            bail!(
                "listener did not answer Ping with serving generation: {}",
                event["type"]
            );
        }
        let raw = event
            .get("daemon_identity")
            .filter(|value| !value.is_null())
            .context("listener does not report serving identity (legacy daemon)")?;
        let identity: ServingIdentity = serde_json::from_value(raw.clone())?;
        validate_serving_identity(&identity)?;
        if peer.is_some_and(|pid| pid != identity.pid) {
            bail!(
                "serving identity PID {} differs from socket peer {:?}",
                identity.pid,
                peer
            );
        }
        return Ok(identity);
    }
    bail!("listener did not report serving generation within the bounded Ping response")
}

#[cfg(unix)]
pub fn socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: both writable buffers have the size supplied to getsockopt.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        (rc == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: ucred is a C struct with a valid zero representation.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        // SAFETY: both writable buffers have the size supplied to getsockopt.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        None
    }
}

#[cfg(not(unix))]
pub fn socket_peer_pid(_stream: &SyncStream) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_identity_is_unknown_not_a_serving_generation() {
        let valid = ServingIdentity {
            pid: 42,
            binary_path: "/versions/current/jcode".into(),
            git_hash: "abc123".into(),
            version_label: None,
            reload_id: Some("reload-id".into()),
            generation: 2,
        };
        assert!(validate_serving_identity(&valid).is_ok());
        let mut bad = valid.clone();
        bad.binary_path = "relative/jcode".into();
        assert!(validate_serving_identity(&bad).is_err());
        bad = valid.clone();
        bad.git_hash = "unknown".into();
        assert!(validate_serving_identity(&bad).is_err());
        bad.git_hash = "not-a-git-hash".into();
        assert!(validate_serving_identity(&bad).is_err());
        bad = valid.clone();
        bad.generation = 0;
        assert!(validate_serving_identity(&bad).is_err());
        bad = valid.clone();
        bad.pid = 0;
        assert!(validate_serving_identity(&bad).is_err());
        bad = valid;
        bad.reload_id = Some(" ".into());
        assert!(validate_serving_identity(&bad).is_err());
    }
}
