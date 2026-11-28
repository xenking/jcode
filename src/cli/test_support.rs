use crate::protocol::{DaemonIdentity, Request, ServerEvent};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) struct ReloadIdentityListener(tokio::task::JoinHandle<()>);

impl Drop for ReloadIdentityListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn reload_identity_listener(socket: &Path, reload_id: &str) -> ReloadIdentityListener {
    // Windows named-pipe listeners accept through `&mut self`.
    #[allow(unused_mut)]
    let mut listener = crate::transport::Listener::bind(socket).expect("bind replacement listener");
    let reload_id = reload_id.to_owned();
    ReloadIdentityListener(tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept identity probe");
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(reader)
                .read_line(&mut line)
                .await
                .expect("read identity probe")
                == 0
            {
                continue;
            }
            let request = serde_json::from_str(&line).expect("decode identity probe");
            let Request::Ping { id } = request else {
                panic!("expected identity ping");
            };
            let reply = ServerEvent::Pong {
                id,
                native_ssh_protocol: None,
                capabilities: Vec::new(),
                user_connected_session_ids: None,
                daemon_identity: Some(DaemonIdentity {
                    pid: std::process::id(),
                    binary_path: "/fixture/versions/new/jcode".into(),
                    git_hash: "abcdef123".into(),
                    version_label: Some("new".into()),
                    reload_id: Some(reload_id.clone()),
                    generation: 2,
                }),
            };
            let mut response = serde_json::to_vec(&reply).expect("encode identity pong");
            response.push(b'\n');
            writer
                .write_all(&response)
                .await
                .expect("send identity pong");
        }
    }))
}
