use crate::client::{ConnectOptions, JcodeClient, Transport};

impl JcodeClient {
    /// Connect to a separately paired Harness gateway.
    pub fn connect_paired_harness(
        base_url: &str,
        token: &str,
        options: ConnectOptions,
    ) -> Result<Self> {
        let transport = connect(base_url, token)?;
        Self::connect_with(transport, options)
    }
}
use crate::errors::{Error, ErrorKind, PairedFailure, Result};
use futures::{SinkExt, StreamExt};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::IpAddr;
#[cfg(any(not(unix), test))]
use std::net::TcpListener;
#[cfg(not(unix))]
use std::net::TcpStream as LocalStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream as LocalStream;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Credentials for the paired gateway. Treat this token as a full Harness credential.
pub struct PairedHarness {
    pub token: String,
}

fn endpoint(base: &str, path: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(base)
        .map_err(|_| Error::new(ErrorKind::InvalidOption, "Invalid gateway URL"))?;
    let loopback = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error::new(
            ErrorKind::InvalidOption,
            "Gateway URL must be an HTTPS origin, or loopback HTTP for testing",
        ));
    }
    url.set_path(path);
    Ok(url)
}

pub fn pair_harness(
    base_url: &str,
    code: &str,
    device_id: &str,
    device_name: &str,
) -> Result<PairedHarness> {
    let url = endpoint(base_url, "/pair")?;
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| {
            Error::new(
                ErrorKind::ConnectFailed,
                "Could not initialize pairing client",
            )
        })?;
    let health = client
        .get(endpoint(base_url, "/health")?)
        .send()
        .map_err(|_| Error::new(ErrorKind::ConnectFailed, "Could not reach pairing gateway"))?;
    #[derive(serde::Deserialize)]
    struct Health {
        #[serde(default)]
        harness_v1: bool,
    }
    if !health.status().is_success() || !read_json::<Health>(health)?.harness_v1 {
        return Err(Error::new(
            ErrorKind::ConnectFailed,
            "Harness pairing is not enabled on this server",
        ));
    }
    let response = client.post(url).json(&serde_json::json!({"code": code, "device_id": device_id, "device_name": device_name, "scope": "harness"})).send()
        .map_err(|_| Error::new(ErrorKind::ConnectFailed, "Could not reach pairing gateway"))?;
    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::ConnectFailed,
            format!("Pairing rejected ({})", response.status()),
        ));
    }
    #[derive(serde::Deserialize)]
    struct Response {
        token: String,
    }
    let response: Response = read_json(response)?;
    if response.token.len() != 64 || !response.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorKind::Protocol, "Invalid pairing token"));
    }
    Ok(PairedHarness {
        token: response.token,
    })
}

fn read_json<T: serde::de::DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
    const LIMIT: usize = 64 * 1024;
    let mut body = Vec::new();
    response
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| Error::new(ErrorKind::Protocol, "Could not read gateway response"))?;
    if body.len() > LIMIT {
        return Err(Error::new(
            ErrorKind::Protocol,
            "Gateway response too large",
        ));
    }
    serde_json::from_slice(&body)
        .map_err(|_| Error::new(ErrorKind::Protocol, "Invalid gateway response"))
}

struct PairedTransport(LocalStream);
impl Transport for PairedTransport {
    fn shutdown_handle(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let stream = self.0.try_clone().ok()?;
        Some(Arc::new(move || {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }))
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        let writer = self
            .0
            .try_clone()
            .map_err(|_| Error::new(ErrorKind::Transport, "Could not split paired transport"))?;
        Ok((Box::new(BufReader::new(self.0)), Box::new(writer)))
    }
}

#[cfg(unix)]
fn local_pair() -> std::io::Result<(LocalStream, LocalStream)> {
    LocalStream::pair()
}

#[cfg(not(unix))]
fn local_pair() -> std::io::Result<(LocalStream, LocalStream)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let client = LocalStream::connect(listener.local_addr()?)?;
    let (peer, address) = listener.accept()?;
    if address != client.local_addr()? {
        return Err(std::io::Error::other("Unexpected local transport peer"));
    }
    Ok((client, peer))
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn paired_error(failure: PairedFailure) -> Error {
    let message = match failure {
        PairedFailure::Unresolved => "The gateway's name did not resolve",
        PairedFailure::Refused => "The gateway refused the connection",
        PairedFailure::TimedOut => "The gateway did not answer in time",
        PairedFailure::Unreachable => "The network path to the gateway is unavailable",
        PairedFailure::Tls => "The secure connection to the gateway failed",
        PairedFailure::Revoked => "The gateway no longer accepts this device's token",
        PairedFailure::WrongOrigin => "This address does not serve a Harness gateway",
        PairedFailure::UpgradeRejected => "The gateway rejected the Harness connection",
        PairedFailure::TooOld => "The gateway is too old to serve Harness",
        PairedFailure::NoToken => "This device has no saved token for the gateway",
    };
    Error::new(ErrorKind::Paired(failure), message)
}

/// Connect the websocket, naming the first stage that fails. The name lookup
/// is its own stage so an unresolvable host fails at once instead of waiting
/// out the connection deadline.
async fn dial(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    origin: &str,
    deadline: Duration,
) -> Result<Socket> {
    let uri = request.uri();
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_owned();
    let secure = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let started = tokio::time::Instant::now();
    match tokio::time::timeout(deadline, tokio::net::lookup_host((host.as_str(), port))).await {
        Err(_) => return Err(paired_error(PairedFailure::TimedOut)),
        Ok(Err(_)) => return Err(paired_error(PairedFailure::Unresolved)),
        Ok(Ok(mut addresses)) => {
            if addresses.next().is_none() {
                return Err(paired_error(PairedFailure::Unresolved));
            }
        }
    }
    let remaining = deadline.saturating_sub(started.elapsed());
    match tokio::time::timeout(remaining, tokio_tungstenite::connect_async(request)).await {
        Err(_) => Err(paired_error(PairedFailure::TimedOut)),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(error)) => Err(paired_error(classify_handshake(error, origin, secure).await)),
    }
}

async fn classify_handshake(
    error: tokio_tungstenite::tungstenite::Error,
    origin: &str,
    secure: bool,
) -> PairedFailure {
    use std::io::ErrorKind as Io;
    use tokio_tungstenite::tungstenite::Error as Ws;
    match error {
        Ws::Http(response) => match response.status().as_u16() {
            401 | 403 => PairedFailure::Revoked,
            404 => classify_missing_route(origin).await,
            _ => PairedFailure::UpgradeRejected,
        },
        Ws::Tls(_) => PairedFailure::Tls,
        Ws::Io(error) => match error.kind() {
            Io::ConnectionRefused => PairedFailure::Refused,
            Io::TimedOut => PairedFailure::TimedOut,
            Io::InvalidData | Io::UnexpectedEof if secure => PairedFailure::Tls,
            _ if secure && looks_like_tls_failure(&error) => PairedFailure::Tls,
            _ => PairedFailure::Unreachable,
        },
        // The reply was not a usable HTTP/1.1 answer at all (for example an
        // HTTP/1.0 error page). Ask `/health` whether a Harness gateway is there.
        Ws::Protocol(_) | Ws::HttpFormat(_) => classify_missing_route(origin).await,
        _ => PairedFailure::UpgradeRejected,
    }
}

fn looks_like_tls_failure(error: &std::io::Error) -> bool {
    let text = format!("{error:?}").to_ascii_lowercase();
    ["certificate", "handshake", "tls", "rustls", "peer"]
        .iter()
        .any(|word| text.contains(word))
}

/// The upgrade route is missing or its answer was unusable. A gateway that serves `/health` without
/// `harness_v1` is merely old. Anything else is the wrong address.
async fn classify_missing_route(origin: &str) -> PairedFailure {
    #[derive(serde::Deserialize)]
    struct Health {
        #[serde(default)]
        harness_v1: bool,
    }
    let Ok(url) = endpoint(origin, "/health") else {
        return PairedFailure::WrongOrigin;
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return PairedFailure::WrongOrigin;
    };
    let Ok(response) = client.get(url).send().await else {
        return PairedFailure::WrongOrigin;
    };
    if !response.status().is_success() {
        return PairedFailure::WrongOrigin;
    }
    match response.json::<Health>().await {
        Ok(health) if !health.harness_v1 => PairedFailure::TooOld,
        Ok(_) => PairedFailure::UpgradeRejected,
        Err(_) => PairedFailure::WrongOrigin,
    }
}

const CONNECT_DEADLINE: Duration = Duration::from_secs(15);

/// Select one rustls provider before building a wss connector. When both `ring` and
/// `aws-lc-rs` are enabled in the dependency graph (the Desktop build), rustls panics
/// instead of choosing, which killed the connect thread and surfaced as
/// "Paired Harness connection failed".
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

pub(crate) fn connect(base: &str, token: &str) -> Result<Box<dyn Transport>> {
    connect_within(base, token, CONNECT_DEADLINE)
}

fn connect_within(base: &str, token: &str, deadline: Duration) -> Result<Box<dyn Transport>> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(
            ErrorKind::InvalidOption,
            "Invalid pairing token",
        ));
    }
    ensure_crypto_provider();
    let mut url = endpoint(base, "/harness/v1")?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| Error::new(ErrorKind::InvalidOption, "Invalid gateway scheme"))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| Error::new(ErrorKind::InvalidOption, "Invalid gateway URL"))?;
    let mut authorization: tokio_tungstenite::tungstenite::http::HeaderValue =
        format!("Bearer {token}")
            .parse()
            .map_err(|_| Error::new(ErrorKind::InvalidOption, "Invalid pairing token"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("Authorization", authorization);
    let (client, peer) = local_pair()
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not create local transport"))?;
    peer.set_nonblocking(true)
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not configure local transport"))?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let origin = base.to_owned();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => {
                let _ = ready_tx.send(Err(Error::new(
                    ErrorKind::Transport,
                    "Could not start the connection runtime",
                )));
                return;
            }
        };
        runtime.block_on(async move {
            let ws = match dial(request, &origin, deadline).await {
                Ok(ws) => ws,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            #[cfg(unix)]
            let stream = tokio::net::UnixStream::from_std(peer);
            #[cfg(not(unix))]
            let stream = tokio::net::TcpStream::from_std(peer);
            let Ok(stream) = stream else {
                let _ = ready_tx.send(Err(Error::new(
                    ErrorKind::Transport,
                    "Could not configure local transport",
                )));
                return;
            };
            if ready_tx.send(Ok(())).is_err() { return; }
            let (read, mut write) = tokio::io::split(stream);
            let mut lines = tokio::io::BufReader::new(read).lines();
            let (mut sink, mut source) = ws.split();
            loop {
                tokio::select! {
                    line = lines.next_line() => match line {
                        Ok(Some(line)) if line.len() <= 256 * 1024 * 1024 => if sink.send(Message::Text(line)).await.is_err() { break },
                        _ => break,
                    },
                    message = source.next() => match message {
                        Some(Ok(Message::Text(text))) => { if write.write_all(text.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { break; } },
                        Some(Ok(Message::Ping(bytes))) => if sink.send(Message::Pong(bytes)).await.is_err() { break },
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {},
                    }
                }
            }
        });
    });
    match ready_rx.recv_timeout(deadline + Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(paired_error(PairedFailure::TimedOut)),
    }
    Ok(Box::new(PairedTransport(client)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connect_installs_a_process_crypto_provider() {
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn urls_require_tls_or_literal_loopback_and_no_extra_components() {
        assert!(endpoint("https://work.example", "/pair").is_ok());
        assert!(endpoint("http://127.0.0.1:1234", "/pair").is_ok());
        for url in [
            "http://work.example",
            "http://localhost",
            "https://user@work.example",
            "https://work.example/path",
            "https://work.example/?token=x",
        ] {
            assert!(endpoint(url, "/pair").is_err());
        }
    }

    #[test]
    fn pairing_refuses_redirects() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let target = destination.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target}/pair\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert!(pair_harness(&format!("http://{address}"), "123456", "id", "desktop").is_err());
        server.join().unwrap();
        assert!(destination.accept().is_err());
    }

    #[test]
    fn websocket_refuses_redirects() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let target = destination.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: ws://{target}/harness/v1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert!(connect(&format!("http://{address}"), &"a".repeat(64)).is_err());
        server.join().unwrap();
        assert!(destination.accept().is_err());
    }

    #[test]
    fn client_sends_bearer_and_speaks_harness_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let token = "a".repeat(64);
        let expected = token.clone();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.uri().path(), "/harness/v1");
                    assert!(request.uri().query().is_none());
                    assert_eq!(request.headers()["authorization"], format!("Bearer {expected}"));
                    Ok(response)
                }).await.unwrap();
                let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
                assert_eq!(hello["req"], "hello");
                ws.send(Message::Text(serde_json::json!({"v":1,"reply_to":hello["id"],"ev":"hello_ok","version":1,"server":"test","capabilities":[]}).to_string())).await.unwrap();
                let ping = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let ping: serde_json::Value = serde_json::from_str(&ping).unwrap();
                assert_eq!(ping["req"], "ping");
                ws.send(Message::Text(serde_json::json!({"v":1,"reply_to":ping["id"],"ev":"pong"}).to_string())).await.unwrap();
                let pending = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let pending: serde_json::Value = serde_json::from_str(&pending).unwrap();
                assert_eq!(pending["req"], "ping");
                ws.close(None).await.unwrap();
            });
        });
        let client = crate::JcodeClient::connect_paired_harness(
            &format!("http://{address}"),
            &token,
            crate::ConnectOptions::default(),
        )
        .unwrap();
        client.ping().unwrap();
        assert!(client.ping().is_err());
        drop(client);
        server.join().unwrap();
    }

    /// Answer each connection with `reply(request_line_and_headers)`, then close.
    fn serve(reply: impl Fn(&str) -> String + Send + 'static, connections: usize) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buffer = [0; 4096];
                let n = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..n]).into_owned();
                let _ = stream.write_all(reply(&request).as_bytes());
            }
        });
        address
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn failed_kind(base: &str, deadline: Duration) -> (ErrorKind, Duration) {
        let started = std::time::Instant::now();
        let error = match connect_within(base, &"a".repeat(64), deadline) {
            Ok(_) => panic!("connect unexpectedly succeeded"),
            Err(error) => error,
        };
        (error.kind, started.elapsed())
    }

    #[test]
    fn unresolvable_name_fails_at_once_as_unresolved() {
        let (kind, elapsed) = failed_kind("https://gateway.invalid", Duration::from_secs(10));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::Unresolved));
        assert!(elapsed < Duration::from_secs(8), "waited {elapsed:?}");
    }

    #[test]
    fn closed_port_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (kind, elapsed) = failed_kind(&format!("http://{address}"), Duration::from_secs(10));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::Refused));
        assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
    }

    #[test]
    fn silent_gateway_times_out_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (kind, elapsed) = failed_kind(&format!("http://{address}"), Duration::from_millis(400));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::TimedOut));
        assert!(elapsed < Duration::from_secs(3), "waited {elapsed:?}");
        drop(listener);
    }

    #[test]
    fn revoked_or_unknown_token_is_revoked() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let address = serve(move |_| http(status, "{\"error\":\"Unknown or revoked auth token; re-pair this device\"}"), 1);
            let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
            assert_eq!(kind, ErrorKind::Paired(PairedFailure::Revoked), "{status}");
        }
    }

    #[test]
    fn missing_route_on_a_gateway_without_harness_is_too_old() {
        let address = serve(
            |request| {
                if request.starts_with("GET /health") {
                    http("200 OK", "{\"status\":\"ok\"}")
                } else {
                    http("404 Not Found", "{}")
                }
            },
            2,
        );
        let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::TooOld));
    }

    #[test]
    fn missing_route_on_a_foreign_origin_is_wrong_origin() {
        let address = serve(|_| http("404 Not Found", "nope"), 2);
        let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::WrongOrigin));
    }

    #[test]
    fn http_1_0_not_found_is_still_wrong_origin() {
        // Some servers answer HTTP/1.0. The websocket client reports that as a
        // protocol error, not a status, so the health probe must still run.
        let address = serve(|_| "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(), 2);
        let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::WrongOrigin));
    }

    #[test]
    fn unusable_upgrade_answer_is_upgrade_rejected() {
        let address = serve(|_| http("200 OK", "<html>proxy</html>"), 1);
        let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::UpgradeRejected));
        let address = serve(|_| http("502 Bad Gateway", ""), 1);
        let (kind, _) = failed_kind(&format!("http://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::UpgradeRejected));
    }

    #[test]
    fn plain_text_server_on_an_https_origin_is_a_tls_failure() {
        let address = serve(|_| "HTTP/1.1 400 Bad Request\r\n\r\n".into(), 1);
        let (kind, _) = failed_kind(&format!("https://{address}"), Duration::from_secs(5));
        assert_eq!(kind, ErrorKind::Paired(PairedFailure::Tls));
    }

    #[test]
    fn failure_messages_carry_no_token() {
        let address = serve(|_| http("401 Unauthorized", "{}"), 1);
        let error = connect_within(&format!("http://{address}"), &"b".repeat(64), Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(!error.to_string().contains(&"b".repeat(64)));
    }
}
