//! Bundle-scoped bridge discovery. A listening socket is never identity evidence.
use crate::{Error, ErrorKind, Result};
use jcode_build_support::runtime_generation::{self as generation, ImmutableTarget};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Capabilities required by Desktop's bridge generation.
pub const BUNDLE_BRIDGE_CAPABILITIES: &[&str] = &["bridge_lifecycle_v1", "peer_user_message_v1"];

/// A stable endpoint for this exact bundle image, independent of older bridges.
/// Including the file identity also separates two builds installed at one path.
pub fn bundle_bridge_socket(base: &Path, target: &ImmutableTarget) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(target)
        .expect("immutable target is serializable")
        .hash(&mut hash);
    base.with_file_name(format!("jcode-api-{:016x}.sock", hash.finish()))
}

/// Verify the socket peer's executable, build and required protocol generation.
pub fn verify_bundle_bridge(
    socket: &Path,
    target: &ImmutableTarget,
    timeout: Duration,
) -> Result<()> {
    let mut stream = jcode_transport::SyncStream::connect(socket).map_err(failed)?;
    stream.set_read_timeout(Some(timeout)).map_err(failed)?;
    stream.set_write_timeout(Some(timeout)).map_err(failed)?;
    let peer = generation::socket_peer_pid(&stream);
    stream.write_all(b"{\"v\":1,\"id\":1,\"req\":\"hello\",\"client\":\"bundle-bridge-probe\",\"min_version\":1,\"max_version\":1}\n").map_err(failed)?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(failed)?;
    let frame: serde_json::Value = serde_json::from_str(&line).map_err(failed)?;
    let identity = frame
        .get("runtime_identity")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<generation::ServingIdentity>(value.clone()))
        .transpose()
        .map_err(failed)?;
    if frame["ev"] != "hello_ok" || frame["version"] != 1 {
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
    Ok(())
}

/// Ask a bridge to stop accepting new clients and drain existing connections.
/// Legacy bridges may reject this request. Never kill their active clients.
pub fn retire_bridge(socket: &Path, timeout: Duration) -> Result<()> {
    let mut stream = jcode_transport::SyncStream::connect(socket).map_err(failed)?;
    stream.set_read_timeout(Some(timeout)).map_err(failed)?;
    stream.set_write_timeout(Some(timeout)).map_err(failed)?;
    stream
        .write_all(b"{\"v\":1,\"id\":1,\"req\":\"retire_bridge\"}\n")
        .map_err(failed)?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(failed)?;
    let reply: serde_json::Value = serde_json::from_str(&line).map_err(failed)?;
    if reply["ev"] != "bridge_retiring" || reply["reply_to"] != 1 {
        return Err(failed("bridge does not support graceful retirement"));
    }
    Ok(())
}

fn failed(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::HandshakeFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
