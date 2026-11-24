use crate::client::Transport;
use crate::errors::{Error, ErrorKind, Result};
use futures::{SinkExt, StreamExt};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
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
    let response = client.post(url).json(&serde_json::json!({"code": code, "device_id": device_id, "device_name": device_name, "scope": "harness"})).send()
        .map_err(|_| Error::new(ErrorKind::ConnectFailed, "Could not reach pairing gateway"))?;
    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::ConnectFailed,
            format!("Pairing rejected ({})", response.status()),
        ));
    }
    let mut body = Vec::new();
    response
        .take(65537)
        .read_to_end(&mut body)
        .map_err(|_| Error::new(ErrorKind::Protocol, "Could not read pairing response"))?;
    if body.len() > 65536 {
        return Err(Error::new(
            ErrorKind::Protocol,
            "Pairing response too large",
        ));
    }
    #[derive(serde::Deserialize)]
    struct Response {
        token: String,
    }
    let response: Response = serde_json::from_slice(&body)
        .map_err(|_| Error::new(ErrorKind::Protocol, "Invalid pairing response"))?;
    if response.token.len() != 64 || !response.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorKind::Protocol, "Invalid pairing token"));
    }
    Ok(PairedHarness {
        token: response.token,
    })
}

struct PairedTransport(TcpStream);
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

pub(crate) fn connect(base: &str, token: &str) -> Result<Box<dyn Transport>> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(
            ErrorKind::InvalidOption,
            "Invalid pairing token",
        ));
    }
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not create local transport"))?;
    let client = TcpStream::connect(
        listener
            .local_addr()
            .map_err(|_| Error::new(ErrorKind::Transport, "Could not create local transport"))?,
    )
    .map_err(|_| Error::new(ErrorKind::Transport, "Could not create local transport"))?;
    let expected_peer = client
        .local_addr()
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not identify local transport"))?;
    let (peer, address) = listener
        .accept()
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not create local transport"))?;
    if address != expected_peer {
        return Err(Error::new(
            ErrorKind::Transport,
            "Unexpected local transport peer",
        ));
    }
    peer.set_nonblocking(true)
        .map_err(|_| Error::new(ErrorKind::Transport, "Could not configure local transport"))?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        runtime.block_on(async move {
            let connected = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(request)).await;
            let ws = match connected { Ok(Ok((ws, _))) => ws, _ => { let _ = ready_tx.send(false); return; } };
            let Ok(stream) = tokio::net::TcpStream::from_std(peer) else { let _ = ready_tx.send(false); return; };
            if ready_tx.send(true).is_err() { return; }
            let (read, mut write) = stream.into_split();
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
    if ready_rx.recv_timeout(Duration::from_secs(16)) != Ok(true) {
        return Err(Error::new(
            ErrorKind::ConnectFailed,
            "Paired Harness connection failed",
        ));
    }
    Ok(Box::new(PairedTransport(client)))
}

#[cfg(test)]
mod tests {
    use super::*;
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
                let _ = ws.next().await;
            });
        });
        let client = crate::JcodeClient::connect_paired_harness(
            &format!("http://{address}"),
            &token,
            crate::ConnectOptions::default(),
        )
        .unwrap();
        client.ping().unwrap();
        drop(client);
        server.join().unwrap();
    }
}
