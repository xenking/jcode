//! One-shot client asking the daemon that owns the Claude login to renew it.
//! Only that daemon rotates the refresh grant (see `claim_claude_daemon_refresh`).
use crate::protocol::{Request, ServerEvent};
use anyhow::{Result, bail};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
// The daemon waits up to 40s for the refresh lock, then one 15s exchange.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// `Ok(false)`: no daemon is listening. `Ok(true)`: it renewed the login; the
/// caller re-reads the credential store. `Err`: it was reached but refused or
/// failed. Tokens never cross the socket.
pub(super) async fn request_refresh(label: &str) -> Result<bool> {
    let path = crate::storage::socket_path();
    let connect = jcode_transport::Stream::connect(&path);
    let Ok(Ok(stream)) = tokio::time::timeout(CONNECT_TIMEOUT, connect).await else {
        return Ok(false);
    };
    tokio::time::timeout(REPLY_TIMEOUT, ask(stream, label))
        .await
        .map_err(|_| {
            anyhow::anyhow!("the jcode daemon did not answer the refresh request in time")
        })?
}

async fn ask(stream: jcode_transport::Stream, label: &str) -> Result<bool> {
    const ID: u64 = 1;
    let (reader, mut writer) = stream.into_split();
    let request = Request::RefreshAuth {
        id: ID,
        provider: "claude".into(),
        account_label: Some(label.into()),
    };
    writer
        .write_all((serde_json::to_string(&request)? + "\n").as_bytes())
        .await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<ServerEvent>(&line) {
            Ok(ServerEvent::Done { id: ID }) => return Ok(true),
            Ok(ServerEvent::Error {
                id: ID, message, ..
            }) => bail!("{message}"),
            _ => {}
        }
    }
    bail!("the jcode daemon closed the connection without answering the refresh request")
}
