use super::Client;
use crate::transport::Stream;
use anyhow::Result;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
fn resolved_socket_identity(path: &std::path::Path) -> PathBuf {
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

pub(super) fn sibling_socket_path(path: &std::path::Path) -> Option<PathBuf> {
    let filename = path.file_name()?.to_str()?;

    if let Some(base) = filename.strip_suffix("-debug.sock") {
        return Some(path.with_file_name(format!("{}.sock", base)));
    }

    if let Some(base) = filename.strip_suffix(".sock") {
        return Some(path.with_file_name(format!("{}-debug.sock", base)));
    }

    None
}

/// Remove a socket file and its sibling (main/debug) if present.
pub fn cleanup_socket_pair(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    if let Some(sibling) = sibling_socket_path(path) {
        let _ = std::fs::remove_file(sibling);
    }
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

pub(super) async fn socket_has_live_listener(path: &std::path::Path) -> bool {
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

#[cfg(unix)]
pub(super) fn same_socket_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    resolved_socket_identity(left) == resolved_socket_identity(right)
}

enum ServeListener {
    Missing,
    Live(Option<u32>),
}

async fn serve_listener(path: &std::path::Path) -> Result<ServeListener> {
    #[cfg(unix)]
    {
        match Stream::connect(path).await {
            Ok(peer) => Ok(ServeListener::Live(
                jcode_build_support::runtime_generation::socket_peer_pid(&peer.into_std()?),
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(ServeListener::Missing)
            }
            Err(error) => Err(anyhow::Error::new(error)
                .context(format!("Cannot check server socket {}", path.display()))),
        }
    }
    #[cfg(not(unix))]
    {
        if socket_has_live_listener(path).await {
            Ok(ServeListener::Live(
                crate::registry::find_server_by_socket_sync(path).map(|server| server.pid),
            ))
        } else {
            Ok(ServeListener::Missing)
        }
    }
}

pub(super) async fn reject_live_serve_socket(path: &std::path::Path) -> Result<()> {
    if let ServeListener::Live(pid) = serve_listener(path).await? {
        let owner = pid.map_or_else(|| " (PID unknown)".into(), |pid| format!(" (pid {pid})"));
        anyhow::bail!(
            "A live server is listening on {}{owner}. Use `jcode server reload`, `jcode server stop --force`, or `jcode serve --replace`.",
            path.display()
        );
    }
    Ok(())
}

/// Run with the startup daemon locks held. Never remove a live endpoint or a regular file.
pub(super) async fn clear_stale_serve_sockets(
    main: &std::path::Path,
    debug: &std::path::Path,
) -> Result<()> {
    for path in [main, debug] {
        reject_live_serve_socket(path).await?;
    }
    #[cfg(unix)]
    for path in [main, debug] {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                let kind = metadata.file_type();
                anyhow::ensure!(
                    kind.is_socket() || kind.is_symlink(),
                    "Not a socket: {}",
                    path.display()
                );
                std::fs::remove_file(path)?;
                crate::logging::info(&format!(
                    "Removed stale server socket at {}",
                    path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) async fn prepare_serve_replacement(
    path: &std::path::Path,
) -> Result<Option<(std::fs::File, std::fs::File)>> {
    let ServeListener::Live(pid) = serve_listener(path).await? else {
        return Ok(None);
    };
    #[cfg(not(unix))]
    {
        let _ = pid;
        anyhow::bail!("serve --replace cannot verify the owner of this listener on this platform");
    }
    #[cfg(unix)]
    {
        use anyhow::Context;
        let pid = pid
            .filter(|pid| *pid != std::process::id())
            .context("serve --replace could not verify a different live daemon PID")?;
        let gate = super::rollout::guard_serve_replacement(path)?;
        let registered = crate::registry::find_server_by_socket_sync(path)
            .is_some_and(|server| server.pid == pid);
        anyhow::ensure!(
            registered,
            "Refusing to replace unverified listener pid {pid} on {}",
            path.display()
        );
        anyhow::ensure!(
            matches!(serve_listener(path).await?, ServeListener::Live(Some(current)) if current == pid),
            "The listener on {} changed before replacement",
            path.display()
        );
        // Do not signal a process group or escalate on a cached PID.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context(format!("Could not stop daemon pid {pid}"));
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match serve_listener(path).await? {
                ServeListener::Missing => return Ok(Some(gate)),
                ServeListener::Live(Some(current)) if current == pid => {}
                ServeListener::Live(_) => anyhow::bail!(
                    "A different listener claimed {} during replacement",
                    path.display()
                ),
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Daemon pid {pid} did not exit after SIGTERM. Try jcode server stop --force."
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Reap a provably-stale socket left behind by a dead daemon.
///
/// Background: after an upgrade or crash, the runtime socket file can survive
/// even though the daemon that owned it is gone. A client that finds the socket
/// path present but cannot connect/handshake gets wedged into a connect-retry
/// loop and never recovers on its own (see issues #277 and #291).
///
/// Safety: a live daemon holds an exclusive `flock` on this socket's daemon
/// lock ([`daemon_lock_path_for`]) for its entire lifetime. So if (a) the socket
/// has no live listener AND (b) we can acquire that exclusive lock, then no
/// daemon is running and the socket is provably stale. Only then do we unlink
/// the socket pair (main + debug) and the lock file. This can never strand a
/// live daemon, because a live daemon would either still be answering on the
/// socket or still be holding the lock.
///
/// Returns true if a stale socket was reaped.
#[cfg(unix)]
pub async fn reap_stale_socket_if_dead(path: &std::path::Path) -> bool {
    // Nothing to reap if the path isn't even present.
    if !crate::transport::is_socket_path(path) {
        return false;
    }

    // If a listener answers, the daemon is alive; never touch it.
    if socket_has_live_listener(path).await {
        return false;
    }

    // Try to grab this socket's daemon lock. If a live daemon holds it, this
    // fails and we leave everything alone (the daemon may just be slow to
    // answer).
    let Ok(Some(_lock)) = try_acquire_daemon_locks(&daemon_lock_paths_for(path)) else {
        return false;
    };

    // Re-check the listener now that we hold the lock, to close the race where a
    // daemon bound the socket between our probe and acquiring the lock. (A real
    // daemon could not have given us the lock, but a brand-new daemon spawned in
    // the gap might be mid-startup before it acquired the lock itself.)
    if socket_has_live_listener(path).await {
        return false;
    }

    crate::logging::warn(&format!(
        "Reaping stale jcode socket with no live listener at {}",
        path.display()
    ));
    cleanup_socket_pair(path);
    // `_lock` (a DaemonLockGuard) removes the lock file when it drops at the end
    // of this scope, so the leftover lock is cleaned up too.
    true
}

#[cfg(not(unix))]
pub async fn reap_stale_socket_if_dead(_path: &std::path::Path) -> bool {
    // Windows named pipes do not leave filesystem socket nodes behind after a
    // process exits, so there is no stale artifact to reap. Probing and then
    // "cleaning" the pipe only consumes a live server instance temporarily.
    false
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

/// Lock path that enforces "one daemon per socket".
///
/// The shared socket keeps the historical runtime-dir-scoped name, so a daemon
/// from an older build that only knows that name is still respected across an
/// upgrade. A custom `--socket` path gets its own sibling lock, so an isolated
/// server can bind it while the shared daemon keeps running: a runtime-wide
/// lock made the second server impossible, which is why `run --socket X` could
/// never get a listener on `X` (issue #1748). The spawn lock
/// (`cli::dispatch::spawn_lock_path`) has always been socket-scoped; this
/// aligns the daemon lock with it.
#[cfg(unix)]
pub(super) fn daemon_lock_path_for(socket: &std::path::Path) -> PathBuf {
    if is_shared_socket(socket) {
        return crate::storage::runtime_dir().join("jcode-daemon.lock");
    }
    socket_scoped_lock_path(socket)
}

#[cfg(unix)]
fn socket_scoped_lock_path(socket: &std::path::Path) -> PathBuf {
    // Derive the lock from the resolved identity, not the spelling: two
    // spellings of one custom socket must contend for the same lock, or two
    // daemons could bind the same socket.
    let resolved = resolved_socket_identity(socket);
    let filename = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("jcode.sock");
    resolved.with_file_name(format!("{filename}.daemon.lock"))
}

/// The daemon lock this process actually acquired.
///
/// Recorded at acquisition rather than recomputed at exit: the lock path is
/// derived from the socket, and the filesystem under that socket can change
/// while the server runs. Recomputing could name a different file and leave
/// the real lock behind (review of #1768, finding 2).
#[cfg(unix)]
static HELD_DAEMON_LOCK: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

/// Every lock a server on `socket` must hold.
///
/// The shared socket takes both its runtime-dir lock and the socket-scoped
/// lock. Builds disagree about which socket is "shared" (a build that read
/// `$TMPDIR` saw the macOS user temp socket as custom and locked only
/// `jcode.sock.daemon.lock`), so taking one name let a newer build start a
/// second daemon on the same path, unlink the live daemon's socket, idle out,
/// and leave the machine with no reachable server.
#[cfg(unix)]
pub(super) fn daemon_lock_paths_for(socket: &std::path::Path) -> Vec<PathBuf> {
    let scoped = socket_scoped_lock_path(socket);
    let primary = daemon_lock_path_for(socket);
    if primary == scoped {
        vec![primary]
    } else {
        vec![primary, scoped]
    }
}

/// Remove the daemon lock this process holds, on a path that exits without
/// unwinding and so skips [`DaemonLockGuard`]'s `Drop`.
///
/// Only the holder may do this: unlinking a lock another process holds would
/// let a third process lock a fresh inode and start a second daemon. `flock`
/// is released by process exit either way, so a file left behind is litter
/// rather than a stuck lock: but a per-run socket would leave one per run.
#[cfg(unix)]
pub(super) fn cleanup_held_daemon_lock() {
    for path in HELD_DAEMON_LOCK.get().into_iter().flatten() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
pub(super) struct DaemonLockGuard {
    _file: std::fs::File,
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// All-or-nothing acquisition of several daemon locks: `None` (with every
/// lock taken so far released) as soon as one is held elsewhere.
#[cfg(unix)]
pub(super) fn try_acquire_daemon_locks(paths: &[PathBuf]) -> Result<Option<Vec<DaemonLockGuard>>> {
    let mut guards = Vec::with_capacity(paths.len());
    for path in paths {
        match try_acquire_daemon_lock(path)? {
            Some(guard) => guards.push(guard),
            None => return Ok(None),
        }
    }
    Ok(Some(guards))
}

#[cfg(unix)]
pub(super) fn try_acquire_daemon_lock(path: &std::path::Path) -> Result<Option<DaemonLockGuard>> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Ok(Some(DaemonLockGuard {
            _file: file,
            path: path.to_path_buf(),
        }))
    } else {
        Ok(None)
    }
}

#[cfg(unix)]
pub(super) fn acquire_daemon_lock_for(socket: &std::path::Path) -> Result<Vec<DaemonLockGuard>> {
    let paths = daemon_lock_paths_for(socket);
    let guards = try_acquire_daemon_locks(&paths)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Another jcode server process is already running on socket {}",
            socket.display()
        )
    })?;
    let _ = HELD_DAEMON_LOCK.set(paths);
    Ok(guards)
}

#[cfg(unix)]
pub(super) fn mark_close_on_exec<T: std::os::fd::AsRawFd>(io: &T) {
    let fd = io.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    }
}

pub fn set_socket_path(path: &str) {
    crate::env::set_var("JCODE_SOCKET", path);
}

/// Spawn a server child process and wait until it signals readiness.
///
/// Creates an anonymous pipe, passes the write-end fd to the child via
/// `JCODE_READY_FD`, and awaits a single byte on the read end. The server
/// calls `signal_ready_fd()` once its accept loops are spawned, so the future
/// resolves only after the daemon can start servicing client requests.
///
/// Falls back to a short poll loop if the pipe read times out (e.g. server
/// built without ready-fd support, or crash before bind).
#[cfg(unix)]
pub async fn spawn_server_notify(cmd: &mut std::process::Command) -> Result<std::process::Child> {
    let (child, _) = spawn_server_notify_impl(cmd, false).await?;
    Ok(child)
}

/// Start an owned daemon and reap it if it exits while its owner survives.
/// The owner's side of the lifetime channel closes even if it is killed.
#[cfg(unix)]
pub async fn spawn_owned_server_notify(cmd: &mut std::process::Command) -> Result<()> {
    let (mut child, owner) = spawn_server_notify_impl(cmd, true).await?;
    std::thread::spawn(move || {
        let _owner = owner;
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(unix)]
async fn spawn_server_notify_impl(
    cmd: &mut std::process::Command,
    owned: bool,
) -> Result<(std::process::Child, Option<std::os::unix::net::UnixStream>)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;

    let (owner, owned_server) = if owned {
        let (owner, server) = UnixStream::pair()?;
        let owner_fd = server.as_raw_fd();
        cmd.env(super::lifecycle::OWNER_FD_ENV, owner_fd.to_string());
        // The owner's endpoint stays CLOEXEC in every child. Inherit only
        // the server endpoint, and only in the server's initial exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(owner_fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        (Some(owner), Some(server))
    } else {
        (None, None)
    };

    // Create a pipe: fds[0] = read end, fds[1] = write end.
    // Use pipe2 with O_CLOEXEC on the read end (parent keeps it).
    // The write end needs CLOEXEC cleared so it survives exec in the child.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    let read_fd = fds[0];
    let write_fd = fds[1];

    // Set CLOEXEC on the read end (parent only)
    unsafe {
        let flags = libc::fcntl(read_fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(read_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }

    // Pass the write-end fd to the child and tell it the fd number.
    unsafe {
        cmd.pre_exec(move || {
            // Clear CLOEXEC on the write end so it survives exec
            let flags = libc::fcntl(write_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(write_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            libc::setsid();
            Ok(())
        });
    }
    cmd.env("JCODE_READY_FD", write_fd.to_string());

    // Close our copy of the write end so we get EOF if the child dies.
    let read_file = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let child = cmd.spawn();
    unsafe { libc::close(write_fd) };
    drop(owned_server);
    let mut child = child?;

    // Wait for the ready signal (or timeout / child death).
    let mut async_file = tokio::fs::File::from_std(read_file);
    let mut buf = [0u8; 1];
    match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::io::AsyncReadExt::read(&mut async_file, &mut buf),
    )
    .await
    {
        Ok(Ok(1)) => {
            crate::logging::info("Server signalled ready via pipe");
        }
        Ok(Ok(_)) => {
            if let Some(status) = child.try_wait()? {
                handle_server_start_exit(&mut child, status).await?;
            }
            crate::logging::info(
                "Server closed ready pipe without signalling; falling back to poll",
            );
            wait_for_server_ready(&socket_path(), Duration::from_secs(5)).await?;
        }
        Ok(Err(e)) => {
            crate::logging::info(&format!(
                "Ready pipe read error: {}; falling back to poll",
                e
            ));
            wait_for_server_ready(&socket_path(), Duration::from_secs(5)).await?;
        }
        Err(_) => {
            crate::logging::info("Timed out waiting for server ready signal; falling back to poll");
            wait_for_server_ready(&socket_path(), Duration::from_secs(5)).await?;
        }
    }

    if let Some(mut stderr) = child.stderr.take() {
        // The shared daemon outlives the spawning client. Keep draining the
        // stderr pipe after startup so later reloads cannot die on SIGPIPE
        // when they emit provider/model selection notices during boot.
        std::thread::spawn(move || {
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut stderr, &mut sink);
        });
    }

    Ok((child, owner))
}

/// Wait until a server socket is connectable and responds to a ping.
pub async fn wait_for_server_ready(path: &std::path::Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Ok(mut client)) = tokio::time::timeout(
            Duration::from_millis(250),
            Client::connect_with_path(path.to_path_buf()),
        )
        .await
            && let Ok(Ok(true)) =
                tokio::time::timeout(Duration::from_millis(250), client.ping()).await
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "Timed out waiting for responsive server socket {}",
        path.display()
    );
}

async fn probe_server_ready(path: &std::path::Path, ping_timeout: Duration) -> bool {
    let Ok(Ok(mut client)) =
        tokio::time::timeout(ping_timeout, Client::connect_with_path(path.to_path_buf())).await
    else {
        return false;
    };

    matches!(
        tokio::time::timeout(ping_timeout, client.ping()).await,
        Ok(Ok(true))
    )
}

pub async fn is_server_ready(path: &std::path::Path) -> bool {
    probe_server_ready(path, Duration::from_millis(50)).await
}

#[cfg(unix)]
pub(super) fn take_server_start_stderr(child: &mut std::process::Child) -> String {
    use std::io::Read;

    child
        .stderr
        .take()
        .and_then(|mut stderr| {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok()?;
            Some(buf)
        })
        .unwrap_or_default()
}

#[cfg(unix)]
pub(super) fn server_start_matches_existing_server(stderr_output: &str) -> bool {
    stderr_output.contains("Another jcode server process is already running")
        || stderr_output.contains("Refusing to replace active server socket")
}

#[cfg(any(unix, test))]
pub(super) async fn wait_for_existing_server(path: &std::path::Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if is_server_ready(path).await || has_live_listener(path).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
pub(super) fn format_server_start_error(
    status: std::process::ExitStatus,
    stderr_output: &str,
) -> String {
    if stderr_output.trim().is_empty() {
        format!(
            "Server exited before signalling ready ({}). Check logs at ~/.jcode/logs/",
            status
        )
    } else {
        format!(
            "Server exited before signalling ready ({}):\n{}",
            status,
            stderr_output.trim()
        )
    }
}

#[cfg(unix)]
pub(super) async fn handle_server_start_exit(
    child: &mut std::process::Child,
    status: std::process::ExitStatus,
) -> Result<()> {
    let stderr_output = take_server_start_stderr(child);
    if server_start_matches_existing_server(&stderr_output) {
        let socket_path = socket_path();
        if wait_for_existing_server(&socket_path, Duration::from_secs(5)).await {
            crate::logging::info(
                "Server spawn raced with an existing daemon; treating startup as successful",
            );
            return Ok(());
        }
    }

    anyhow::bail!(format_server_start_error(status, &stderr_output));
}

/// Write a single byte to the fd in `JCODE_READY_FD` and close it.
/// Called after startup plumbing is ready so the parent process knows the
/// server can accept and service client requests. The env var is cleared so child
/// processes (e.g. tool subprocesses) don't inherit a stale fd.
pub(super) fn signal_ready_fd() {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;

        if let Ok(fd_str) = std::env::var("JCODE_READY_FD") {
            crate::env::remove_var("JCODE_READY_FD");
            if let Ok(fd) = fd_str.parse::<i32>() {
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                let _ = std::io::Write::write_all(&mut file, b"R");
                // file is dropped here which closes the fd
            }
        }
    }
}
