//! Presence and identity are separate observations. Failure to answer Ping is
//! never proof that a listener is absent or permission to spawn a competitor.
use super::{ServingIdentity, socket_adapter};
use jcode_transport::SyncStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum SocketObservation {
    Absent { reason: String },
    Serving(ServingIdentity),
    Unknown { reason: String },
}

pub fn observe_socket(socket: &Path, timeout: Duration) -> SocketObservation {
    let stream = match SyncStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return SocketObservation::Absent {
                reason: format!("no listener at {}: {error}", socket.display()),
            };
        }
        Err(error) => {
            return SocketObservation::Unknown {
                reason: format!(
                    "socket observation failed for {}: {error}",
                    socket.display()
                ),
            };
        }
    };
    match socket_adapter::probe_stream_identity(stream, timeout) {
        Ok(identity) => SocketObservation::Serving(identity),
        Err(error) => SocketObservation::Unknown {
            reason: format!("listener generation unknown: {error:#}"),
        },
    }
}

pub fn replacement_spawn_permission(
    observation: &SocketObservation,
    owner_alive: bool,
) -> Result<(), String> {
    match observation {
        SocketObservation::Absent { .. } if !owner_alive => Ok(()),
        SocketObservation::Absent { .. } => {
            Err("handoff owner is still alive, awaiting its replacement listener".into())
        }
        SocketObservation::Serving(identity) => Err(format!(
            "listener pid {} generation {} still owns the socket",
            identity.pid, identity.generation
        )),
        SocketObservation::Unknown { reason } => Err(reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_socket_requires_dead_handoff_owner_before_replacement() {
        let absent = SocketObservation::Absent {
            reason: "fixture".into(),
        };
        assert!(replacement_spawn_permission(&absent, true).is_err());
        assert!(replacement_spawn_permission(&absent, false).is_ok());
        assert!(
            replacement_spawn_permission(
                &SocketObservation::Unknown {
                    reason: "legacy listener".into()
                },
                false,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_live_listener_is_unknown_not_permission_to_spawn() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream.write_all(b"{\"type\":\"pong\",\"id\":1}\n").unwrap();
        });
        let observation = observe_socket(&socket, Duration::from_secs(1));
        assert!(
            matches!(&observation, SocketObservation::Unknown { reason } if reason.contains("legacy"))
        );
        assert!(replacement_spawn_permission(&observation, false).is_err());
        server.join().unwrap();
    }
}
