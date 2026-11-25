//! Where the shared jcode server listens and how lower layers reach it.
//!
//! Kept apart from `server` so tools and the agent can dial the daemon without
//! depending on the daemon's own implementation.

use crate::transport::Stream;
use anyhow::Result;
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("JCODE_SOCKET") {
        return PathBuf::from(custom);
    }
    default_socket_path()
}

/// The shared-daemon socket for this runtime dir, ignoring any
/// `--socket`/`JCODE_SOCKET` override.
pub fn default_socket_path() -> PathBuf {
    crate::storage::runtime_dir().join("jcode.sock")
}

/// Resolve a socket path to a comparable identity: absolute, with its parent
/// directory's symlinks collapsed.
///
/// `JCODE_SOCKET` carries a user-supplied *spelling*, and several spellings
/// name the same socket: `jcode.sock` relative to the runtime dir, a path
/// through a symlinked parent, a `..` segment. Comparing spellings would call
/// those "custom" and hand the shared socket to a process-scoped server that
/// then takes it down with the run, disconnecting every other client.
///
/// Only the *parent* is resolved; the final component is kept as written, even
/// when it currently exists as a symlink to another socket. Binding always
/// unlinks and recreates that last component (see `Server::run`), so the
/// socket a server ends up owning is the one named here: not whatever the
/// name happened to point at beforehand. Following the final link would pin
/// the daemon lock to the old target and then publish a different socket,
/// locking a path nobody is listening on (review of #1768, finding 2).
///
/// Falls back to the plain absolute path when the parent is absent too: two
/// absent paths still compare consistently with each other.
pub(crate) fn resolved_socket_identity(path: &std::path::Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(file_name)) => match std::fs::canonicalize(parent) {
            Ok(canonical_parent) => canonical_parent.join(file_name),
            Err(_) => absolute,
        },
        _ => absolute,
    }
}

/// True when `path` is the shared daemon socket for this runtime dir, however
/// it happens to be spelled.
pub fn is_shared_socket(path: &std::path::Path) -> bool {
    resolved_socket_identity(path) == resolved_socket_identity(&default_socket_path())
}

/// True when `--socket`/`JCODE_SOCKET` points somewhere other than the shared
/// daemon socket for this runtime dir.
///
/// A custom socket means "this process wants its own server", which is what
/// lets an isolated server be started for it without disturbing the shared
/// daemon (issue #1748).
pub fn socket_path_is_custom() -> bool {
    !is_shared_socket(&socket_path())
}

/// Debug socket path for testing/introspection
/// Derived from main socket path
pub fn debug_socket_path() -> PathBuf {
    let main_path = socket_path();
    let filename = main_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("jcode.sock");
    let debug_filename = filename.replace(".sock", "-debug.sock");
    main_path.with_file_name(debug_filename)
}

/// Connect to a socket path.
///
/// Do not unlink the path on connection-refused here. A client-side cleanup can
/// strand a live daemon behind an unlinked Unix socket pathname, leaving the
/// process running with the daemon lock held while new clients can no longer
/// discover or connect to it.
pub async fn connect_socket(path: &std::path::Path) -> Result<Stream> {
    match Stream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused && path.exists() => {
            Err(anyhow::Error::new(err).context(format!(
                "Socket exists but refused the connection at {}. Retry, or remove it after confirming no jcode server is running.",
                path.display()
            )))
        }
        Err(err) if err.raw_os_error() == Some(libc::EMFILE) => Err(anyhow::anyhow!(
            "{} ({})",
            err,
            crate::util::process_fd_diagnostic_snapshot()
        )),
        Err(err) => Err(err.into()),
    }
}

pub async fn socket_has_live_listener(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        // `is_socket_path` performs one non-blocking named-pipe open and treats
        // ERROR_PIPE_BUSY as live. Do not follow it with a second connect: the
        // first probe can temporarily occupy the only published pipe instance
        // before the accept loop replaces it, making that second connect wait
        // forever inside the Windows ERROR_PIPE_BUSY retry loop.
        crate::transport::is_socket_path(path)
    }

    #[cfg(not(windows))]
    {
        crate::transport::is_socket_path(path) && Stream::connect(path).await.is_ok()
    }
}

/// Return true if a live server process is listening on the socket path.
///
/// This is intentionally weaker than [`is_server_ready`]: a live listener may
/// still be finishing startup or be temporarily too busy to answer a ping
/// within the short readiness timeout. Callers that must avoid spawning a
/// duplicate daemon should prefer this check over a ping-only probe.
pub async fn has_live_listener(path: &std::path::Path) -> bool {
    socket_has_live_listener(path).await
}
