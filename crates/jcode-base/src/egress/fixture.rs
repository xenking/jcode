//! Loopback proxy and upstream servers for egress tests.
//!
//! [`Proxy`] records what a proxy sees (the absolute-form request line, or the
//! CONNECT target) and forwards to the real target on loopback. [`Upstream`]
//! answers any request with a canned JSON body and records what arrived.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};

async fn read_head(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            return Some((head, buf[end + 4..].to_vec()));
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A loopback HTTP proxy that forwards to loopback targets.
pub struct Proxy {
    pub url: String,
    pub port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::serve(listener)
    }

    /// Take over an address, for tests that bring a proxy up after it was down.
    pub async fn start_at(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        Self::serve(listener)
    }

    fn serve(listener: TcpListener) -> Self {
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let _ = handle(client, log).await;
                });
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            port,
            seen,
            task,
        }
    }

    /// First line of every request the proxy received, in order.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn saw(&self, needle: &str) -> bool {
        self.seen().iter().any(|line| line.contains(needle))
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(mut client: TcpStream, log: Arc<Mutex<Vec<String>>>) -> Option<()> {
    let (head, extra) = read_head(&mut client).await?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    log.lock().unwrap().push(request_line.clone());
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if method == "CONNECT" {
        let mut upstream = TcpStream::connect(target).await.ok()?;
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .ok()?;
        copy_bidirectional(&mut client, &mut upstream).await.ok()?;
        return Some(());
    }
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let mut upstream = TcpStream::connect(authority).await.ok()?;
    let mut forwarded = format!("{method} {path} HTTP/1.1\r\n");
    for line in lines {
        let name = line
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name != "connection" && name != "proxy-connection" {
            forwarded.push_str(line);
            forwarded.push_str("\r\n");
        }
    }
    forwarded.push_str("Connection: close\r\n\r\n");
    upstream.write_all(forwarded.as_bytes()).await.ok()?;
    upstream.write_all(&extra).await.ok()?;
    copy_bidirectional(&mut client, &mut upstream).await.ok()?;
    Some(())
}

/// A loopback upstream that answers with a canned JSON body, `200` unless routed.
pub struct Upstream {
    pub url: String,
    pub port: u16,
    seen: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

const JSON: &str = "application/json";

#[derive(Clone, Debug)]
pub struct Request {
    pub request_line: String,
    pub head: String,
}

impl Upstream {
    pub async fn start(body: &'static str) -> Self {
        Self::start_as(JSON, body).await
    }

    /// As [`Upstream::start`], with another content type (`text/event-stream`).
    pub async fn start_as(content_type: &'static str, body: &'static str) -> Self {
        Self::serve(content_type, move |_| (200, body.to_owned())).await
    }

    /// Answer by request path: `routes` is `(path suffix, status, body)`, first
    /// match wins, and any other path is a `404`.
    pub async fn start_routes(routes: &[(&str, u16, &str)]) -> Self {
        let routes: Vec<(String, u16, String)> = routes
            .iter()
            .map(|(path, status, body)| ((*path).to_owned(), *status, (*body).to_owned()))
            .collect();
        Self::serve(JSON, move |path| {
            routes
                .iter()
                .find(|(suffix, _, _)| path.ends_with(suffix.as_str()))
                .map_or((404, "{}".to_owned()), |(_, status, body)| {
                    (*status, body.clone())
                })
        })
        .await
    }

    async fn serve(
        content_type: &'static str,
        respond: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
    ) -> Self {
        let respond = Arc::new(respond);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    while let Some((head, _)) = read_head(&mut stream).await {
                        let request_line = head.lines().next().unwrap_or_default().to_owned();
                        let close = head.to_ascii_lowercase().contains("connection: close");
                        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
                        let (status, body) = respond(target.split('?').next().unwrap_or_default());
                        log.lock().unwrap().push(Request { request_line, head });
                        let response = format!(
                            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\n{}Content-Length: {}\r\n\r\n{body}",
                            if close { "Connection: close\r\n" } else { "" },
                            body.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() || close {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            port,
            seen,
            task,
        }
    }

    pub fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Write `egress` and `routing` tables to the active config home and reload.
/// `accounts` is `(id, egress name)`; `proxies` is `(name, url, required)`.
/// Hold [`crate::storage::lock_test_env`] and use an isolated `JCODE_HOME`.
pub fn install_config(proxies: &[(&str, &str, bool)], accounts: &[(&str, &str)]) {
    let mut text = String::new();
    for (name, url, required) in proxies {
        text.push_str(&format!(
            "[egress.{name}]\nurl = \"{url}\"\nrequired = {required}\n\n"
        ));
    }
    for (id, egress) in accounts {
        text.push_str(&format!(
            "[[routing.accounts]]\nid = \"{id}\"\negress = \"{egress}\"\n\n"
        ));
    }
    let path = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join("config.toml");
    std::fs::write(path, text).expect("write config");
    crate::config::invalidate_config_cache();
}
