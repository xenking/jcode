//! A loopback HTTP endpoint that records what callers send, for usage tests.

use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct Loopback {
    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub(super) base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Loopback {
    /// Serve every request. `answer` maps `"<METHOD> <target>"` to a status and
    /// JSON body.
    pub(super) async fn start(
        answer: impl Fn(&str) -> (u16, Value) + Send + Sync + 'static,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 16384];
                let n = stream.read(&mut bytes).await.unwrap();
                let raw = String::from_utf8_lossy(&bytes[..n]).into_owned();
                let (status, body) = answer(&request_line(&raw));
                log.lock().unwrap().push(raw);
                let body = body.to_string();
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }

    /// Every request received so far, as `"<METHOD> <target>"`.
    pub(super) fn lines(&self) -> Vec<String> {
        self.raw().iter().map(|raw| request_line(raw)).collect()
    }

    pub(super) fn raw(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request_line(raw: &str) -> String {
    let mut parts = raw.lines().next().unwrap_or_default().split(' ');
    format!(
        "{} {}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    )
}

/// One request header's value, case-insensitively.
pub(super) fn header(raw: &str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    raw.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .starts_with(&prefix)
            .then(|| line[prefix.len()..].trim().to_owned())
    })
}
