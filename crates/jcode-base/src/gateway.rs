//! WebSocket gateway for remote clients (iOS app, web).
//!
//! Accepts WebSocket connections over TCP and bridges them to the
//! existing newline-delimited JSON protocol used by Unix socket clients.
//! This lets iOS/web clients interact with jcode sessions identically
//! to TUI clients.
//!
//! Architecture:
//!   TCP :7643  →  WebSocket upgrade  →  UnixStream::pair()  →  handle_client()
//!
//! Each WebSocket client gets a virtual UnixStream pair. One end is handed
//! to the server's existing handle_client(); the other is bridged to WebSocket
//! frames by a relay task.

use anyhow::Result;
use futures::SinkExt;
use futures::stream::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use crate::logging;
mod auth;
pub mod control;
mod registry;
#[cfg(test)]
mod scoped_tests;
use auth::{
    AuthorizedDevice, WsAuth, WsAuthSource, authorize_ws_device, extract_ws_auth, ws_error_response,
};
#[cfg(test)]
pub(crate) use auth::{is_valid_hex_token, parse_bearer_token, parse_query_token};
pub use control::{
    PairingInvite, RemoteCommand, RemoteStatus, ToggleOutcome, create_pairing_invite,
    parse_remote_command, revoke_device, set_gateway_enabled,
};
pub use jcode_gateway_types::{DeviceScope, PairedDevice, PairingCode};
pub use registry::DeviceRegistry;

/// Default gateway port ("jc" on phone keypad = 52, but we use 7643)
pub const DEFAULT_PORT: u16 = 7643;
const WEBSOCKET_KEEPALIVE_INTERVAL_SECS: u64 = 20;
const MAX_HARNESS_CONNECTIONS: usize = 16;
type PairAttempts = Arc<tokio::sync::Mutex<std::collections::VecDeque<std::time::Instant>>>;

pub use crate::config::GatewayConfig;

// ---------------------------------------------------------------------------
// Gateway listener
// ---------------------------------------------------------------------------

/// Run the WebSocket gateway. Called from Server::run() as a spawned task.
///
/// For each incoming WebSocket connection:
/// 1. Extract auth token from the WebSocket upgrade request
/// 2. Validate against device registry
/// 3. Create a UnixStream::pair() - one end for the bridge, one for handle_client
/// 4. Spawn a relay task that converts WebSocket frames <-> newline-delimited JSON
/// 5. Return the server-side UnixStream for handle_client to consume
pub async fn run_gateway(
    config: GatewayConfig,
    client_tx: tokio::sync::mpsc::UnboundedSender<GatewayClient>,
    daemon_socket: std::path::PathBuf,
) -> Result<()> {
    let addr = format!("{}:{}", config.bind_addr, config.port);
    let listener = TcpListener::bind(&addr).await?;
    let harness_enabled = config.harness_enabled && listener.local_addr()?.ip().is_loopback();
    if config.harness_enabled && !harness_enabled {
        logging::warn("Harness gateway disabled: listener must bind to a loopback address");
    }
    logging::info(&format!("WebSocket gateway listening on {}", addr));

    let registry = Arc::new(tokio::sync::RwLock::new(DeviceRegistry::load()));
    let pair_attempts: PairAttempts = Arc::default();
    let harness_slots = Arc::new(tokio::sync::Semaphore::new(MAX_HARNESS_CONNECTIONS));

    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;
        let registry = Arc::clone(&registry);
        let client_tx = client_tx.clone();
        let daemon_socket = daemon_socket.clone();
        let pair_attempts = Arc::clone(&pair_attempts);
        let harness_slots = Arc::clone(&harness_slots);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                tcp_stream,
                peer_addr,
                registry,
                client_tx,
                harness_enabled,
                daemon_socket,
                pair_attempts,
                harness_slots,
            )
            .await
            {
                logging::error(&format!(
                    "Gateway connection error from {}: {}",
                    peer_addr, e
                ));
            }
        });
    }
}

/// Route an incoming TCP connection: either plain HTTP (pair/health) or WebSocket.
///
/// We peek at the first chunk to check for the Upgrade: websocket header.
/// Plain HTTP requests get handled inline; WebSocket connections proceed to
/// the existing auth + bridge flow.
async fn handle_connection(
    tcp_stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    registry: Arc<tokio::sync::RwLock<DeviceRegistry>>,
    client_tx: tokio::sync::mpsc::UnboundedSender<GatewayClient>,
    harness_enabled: bool,
    daemon_socket: std::path::PathBuf,
    pair_attempts: PairAttempts,
    harness_slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let mut peek_buf = [0u8; 16384];
    let n = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = tcp_stream.peek(&mut peek_buf).await?;
            if n == 0 || find_header_end(&peek_buf[..n]).is_some() || n == peek_buf.len() {
                return Ok::<usize, std::io::Error>(n);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    if n == 0 || find_header_end(&peek_buf[..n]).is_none() {
        anyhow::bail!("Incomplete gateway request headers");
    }
    let request_head = String::from_utf8_lossy(&peek_buf[..n]);

    let is_websocket = request_head.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.starts_with("upgrade:") && lower.contains("websocket")
    });

    if is_websocket {
        handle_ws_connection(
            tcp_stream,
            peer_addr,
            registry,
            client_tx,
            harness_enabled,
            daemon_socket,
            harness_slots,
        )
        .await
    } else {
        handle_http(
            tcp_stream,
            peer_addr,
            registry,
            harness_enabled,
            pair_attempts,
        )
        .await
    }
}

/// A gateway client ready to be plugged into handle_client
pub struct GatewayClient {
    /// The server-side end of the virtual Unix socket pair
    pub stream: crate::transport::Stream,
    /// Device info for this client
    pub device_name: String,
    /// Device ID
    pub device_id: String,
}

/// Handle a single incoming TCP connection: upgrade to WebSocket, auth, bridge.
#[expect(
    clippy::result_large_err,
    reason = "WebSocket handshake callback must return Tungstenite ErrorResponse directly"
)]
async fn handle_ws_connection(
    tcp_stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    registry: Arc<tokio::sync::RwLock<DeviceRegistry>>,
    client_tx: tokio::sync::mpsc::UnboundedSender<GatewayClient>,
    harness_enabled: bool,
    daemon_socket: std::path::PathBuf,
    harness_slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    // Perform WebSocket handshake with a callback to inspect headers.
    // Prefer Authorization headers, but continue accepting ?token= for browser clients.
    // Token validation happens HERE, before the handshake completes, so
    // revoked/unknown tokens receive a proper 401 instead of a silent
    // accept-then-drop (which clients cannot distinguish from network flakes).
    let auth = Arc::new(std::sync::Mutex::new(
        None::<(
            WsAuth,
            AuthorizedDevice,
            bool,
            Option<tokio::sync::OwnedSemaphorePermit>,
        )>,
    ));
    let auth_cb = Arc::clone(&auth);

    let ws_stream = tokio_tungstenite::accept_hdr_async(
        tcp_stream,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let harness = request.uri().path() == "/harness/v1" && harness_enabled;
            if request.uri().path() != "/ws" && !harness {
                return Err(ws_error_response(
                    404,
                    "Not Found",
                    "WebSocket endpoint not found",
                ));
            }

            let ws_auth = extract_ws_auth(request)?;
            if harness
                && (ws_auth.source != WsAuthSource::Header || request.uri().query().is_some())
            {
                return Err(ws_error_response(
                    401,
                    "Unauthorized",
                    "Harness requires Bearer authorization",
                ));
            }
            // Reload from disk to pick up newly paired or revoked devices.
            let registry = DeviceRegistry::load();
            let device = authorize_ws_device(&registry, &ws_auth.token)?;
            let expected = if harness {
                jcode_gateway_types::DeviceScope::Harness
            } else {
                jcode_gateway_types::DeviceScope::Phone
            };
            if registry
                .validate_token(&ws_auth.token)
                .is_none_or(|device| device.scope != expected)
            {
                return Err(ws_error_response(401, "Unauthorized", "Wrong token scope"));
            }
            let permit = if harness {
                Some(
                    Arc::clone(&harness_slots)
                        .try_acquire_owned()
                        .map_err(|_| {
                            ws_error_response(
                                503,
                                "Service Unavailable",
                                "Harness connection limit reached",
                            )
                        })?,
                )
            } else {
                None
            };
            let mut guard = auth_cb
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some((ws_auth, device, harness, permit));
            Ok(response)
        },
    )
    .await?;

    let (auth, device, harness, _permit) = auth
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .ok_or_else(|| anyhow::anyhow!("No auth token provided"))?;
    let token = auth.token;
    let (device_name, device_id) = (device.name, device.id);

    if auth.source == WsAuthSource::Query {
        logging::info(&format!(
            "Gateway: {} connected with deprecated query token auth",
            peer_addr
        ));
    }

    {
        let mut reg = registry.write().await;
        // Reload from disk so the shared registry matches what we validated.
        *reg = DeviceRegistry::load();
        reg.touch_device(&token);
    }

    logging::info(&format!(
        "Gateway: {} connected (device: {}, addr: {})",
        device_name, device_id, peer_addr
    ));

    // Create a virtual Unix socket pair
    let (server_stream, bridge_stream) = crate::transport::stream_pair()
        .map_err(|e| anyhow::anyhow!("Failed to create socket pair: {}", e))?;

    // Send the server-side stream to the main server loop for handle_client
    let harness_task = if harness {
        let (read, write) = server_stream.into_split();
        Some(tokio::spawn(async move {
            let _ = jcode_harness_api_server::run_bridge_stream(read, write, daemon_socket).await;
        }))
    } else {
        client_tx.send(GatewayClient {
            stream: server_stream,
            device_name: device_name.clone(),
            device_id,
        })?;
        None
    };

    // Bridge WebSocket frames <-> newline-delimited JSON on the bridge stream
    let (ws_sink, ws_source) = ws_stream.split();
    let ws_sink = Arc::new(tokio::sync::Mutex::new(ws_sink));

    let (bridge_reader, bridge_writer) = bridge_stream.into_split();
    let mut bridge_reader = BufReader::new(bridge_reader);
    let bridge_writer = Arc::new(tokio::sync::Mutex::new(bridge_writer));

    // Task 1: WebSocket → Unix socket (client requests)
    let writer_for_ws = Arc::clone(&bridge_writer);
    let sink_for_ping = Arc::clone(&ws_sink);
    let sink_for_unix = Arc::clone(&ws_sink);
    let sink_for_keepalive = Arc::clone(&ws_sink);
    let ws_to_unix = tokio::spawn(async move {
        let mut ws_source = ws_source;
        while let Some(msg) = ws_source.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let mut writer = writer_for_ws.lock().await;
                    if text.ends_with('\n') {
                        if writer.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    } else {
                        if writer.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(data)) => {
                    let mut sink = sink_for_ping.lock().await;
                    let _ = sink.send(Message::Pong(data)).await;
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let keepalive_device_name = device_name.clone();
    let keepalive = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(WEBSOCKET_KEEPALIVE_INTERVAL_SECS));
        loop {
            interval.tick().await;
            let mut sink = sink_for_keepalive.lock().await;
            if sink.send(Message::Ping(Vec::new())).await.is_err() {
                logging::info(&format!(
                    "Gateway: stopping keepalive for {} after ping send failure",
                    keepalive_device_name
                ));
                break;
            }
        }
    });

    // Task 2: Unix socket → WebSocket (server events)
    let unix_to_ws = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match bridge_reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end().to_string();
                    if !trimmed.is_empty() {
                        let mut sink = sink_for_unix.lock().await;
                        if sink.send(Message::Text(trimmed)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait for either direction to finish
    tokio::pin!(ws_to_unix);
    tokio::pin!(unix_to_ws);
    tokio::pin!(keepalive);

    let revoked = async {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let expected = if harness {
                jcode_gateway_types::DeviceScope::Harness
            } else {
                jcode_gateway_types::DeviceScope::Phone
            };
            if DeviceRegistry::load()
                .validate_token(&token)
                .is_none_or(|device| device.scope != expected)
            {
                break;
            }
        }
    };

    tokio::select! {
        _ = &mut ws_to_unix => {}
        _ = &mut unix_to_ws => {}
        _ = &mut keepalive => {}
        _ = revoked => {}
    }

    ws_to_unix.abort();
    unix_to_ws.abort();
    keepalive.abort();
    if let Some(task) = harness_task {
        task.abort();
    }

    logging::info(&format!("Gateway: {} disconnected", device_name));
    Ok(())
}

/// Finds the end of HTTP headers (`\r\n\r\n`), returning the offset of the
/// terminator start.
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

fn http_response(status: u16, status_text: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\n\r\n{}",
        status, status_text, body.len(), body
    ).into_bytes()
}

/// Handle a plain HTTP request (not WebSocket).
/// Supports:
///   GET  /health  - server status
///   POST /pair    - exchange pairing code for auth token
///   OPTIONS *     - CORS preflight
async fn handle_http(
    mut tcp_stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    registry: Arc<tokio::sync::RwLock<DeviceRegistry>>,
    harness_enabled: bool,
    pair_attempts: PairAttempts,
) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0usize;
    // Read until end of headers. Clients like URLSession may deliver headers
    // and body in separate TCP segments, so a single read is not enough.
    let header_end = loop {
        if filled == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        let n = tcp_stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            break None;
        }
        filled += n;
        if let Some(pos) = find_header_end(&buf[..filled]) {
            break Some(pos);
        }
        if filled > 64 * 1024 {
            anyhow::bail!("HTTP request headers too large from {}", peer_addr);
        }
    };
    let Some(header_end) = header_end else {
        anyhow::bail!("HTTP connection closed before headers from {}", peer_addr);
    };

    // Read the remaining body bytes per Content-Length, if any.
    let headers_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = headers_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    let expected_total = body_start.saturating_add(content_length.min(1024 * 1024));
    while filled < expected_total {
        if filled == buf.len() {
            buf.resize(expected_total.max(buf.len() * 2), 0);
        }
        let n = tcp_stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let request = String::from_utf8_lossy(&buf[..filled]);

    let first_line = request.lines().next().unwrap_or("");
    let (method, path) = {
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() >= 2 {
            (parts[0], parts[1])
        } else {
            ("", "")
        }
    };

    // Strip query params from path for matching
    let path_base = path.split('?').next().unwrap_or(path);

    logging::info(&format!(
        "Gateway HTTP: {} {} from {}",
        method, path_base, peer_addr
    ));

    let response = match (method, path_base) {
        ("GET", "/health") => {
            let body = serde_json::json!({
                "status": "ok",
                "version": jcode_build_meta::version(),
                "gateway": true,
                "harness_v1": harness_enabled,
                "working_dir": default_client_working_dir(),
            });
            http_response(200, "OK", &body.to_string())
        }

        ("POST", "/pair") => {
            // Extract JSON body (after \r\n\r\n)
            let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
            handle_pair_request(body_str, &registry, harness_enabled, &pair_attempts).await
        }

        ("OPTIONS", _) => {
            // CORS preflight
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Max-Age: 86400\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string().into_bytes()
        }

        _ => {
            let body = serde_json::json!({"error": "Not found"});
            http_response(404, "Not Found", &body.to_string())
        }
    };

    tcp_stream.write_all(&response).await?;
    tcp_stream.shutdown().await?;
    Ok(())
}

/// Handle POST /pair request.
///
/// Expected JSON body:
/// ```json
/// {
///   "code": "123456",
///   "device_id": "uuid-here",
///   "device_name": "Jeremy's iPhone",
///   "apns_token": "optional-apns-token"
/// }
/// ```
///
/// Returns:
/// ```json
/// {
///   "token": "hex-auth-token",
///   "server_name": "jcode",
///   "server_version": "v0.4.0"
/// }
/// ```
async fn handle_pair_request(
    body: &str,
    registry: &Arc<tokio::sync::RwLock<DeviceRegistry>>,
    harness_enabled: bool,
    pair_attempts: &PairAttempts,
) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct PairRequest {
        code: String,
        device_id: String,
        device_name: String,
        apns_token: Option<String>,
        #[serde(default)]
        scope: jcode_gateway_types::DeviceScope,
    }

    let req: PairRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            let body = serde_json::json!({"error": format!("Invalid JSON: {}", e)});
            return http_response(400, "Bad Request", &body.to_string());
        }
    };

    if req.scope == jcode_gateway_types::DeviceScope::Harness && !harness_enabled {
        return http_response(
            403,
            "Forbidden",
            r#"{"error":"Harness requires a loopback listener"}"#,
        );
    }

    {
        let mut attempts = pair_attempts.lock().await;
        let now = std::time::Instant::now();
        while attempts
            .front()
            .is_some_and(|time| now.duration_since(*time) >= Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= 10 {
            return http_response(
                429,
                "Too Many Requests",
                r#"{"error":"Too many pairing attempts"}"#,
            );
        }
        attempts.push_back(now);
    }

    let mut reg = registry.write().await;

    // Reload from disk - pairing codes are generated by `jcode pair` CLI
    *reg = DeviceRegistry::load();

    if !reg.validate_code_scoped(&req.code, req.scope) {
        let body = serde_json::json!({"error": "Invalid or expired pairing code"});
        return http_response(401, "Unauthorized", &body.to_string());
    }

    let token = reg.pair_device_scoped(
        req.device_id.clone(),
        req.device_name.clone(),
        req.apns_token,
        req.scope,
    );

    logging::info(&format!(
        "Gateway: paired device '{}' ({})",
        req.device_name, req.device_id
    ));

    let body = serde_json::json!({
        "token": token,
        "server_name": "jcode",
        "server_version": jcode_build_meta::version(),
        "working_dir": default_client_working_dir(),
    });
    http_response(200, "OK", &body.to_string())
}

// ---------------------------------------------------------------------------
// Reachable-host resolution
// ---------------------------------------------------------------------------

/// Placeholder returned when no reachable host name can be determined.
pub const UNKNOWN_CONNECT_HOST: &str = "<this-machine>";

/// Parse the machine's own MagicDNS name out of `tailscale status --json`.
pub fn parse_tailscale_dns_name(status_json: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(status_json).ok()?;
    let dns_name = value
        .get("Self")?
        .get("DNSName")?
        .as_str()?
        .trim()
        .trim_end_matches('.')
        .to_string();

    if dns_name.is_empty() {
        None
    } else {
        Some(dns_name)
    }
}

/// Best-effort MagicDNS name for this machine, if Tailscale is installed.
pub fn detect_tailscale_dns_name() -> Option<String> {
    let output = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_tailscale_dns_name(&output.stdout)
}

/// Resolve the address a remote client should actually dial.
///
/// A wildcard `bind_addr` says nothing about how to reach this machine, so fall
/// back through the explicit override, Tailscale MagicDNS, and finally the
/// system hostname.
pub fn resolve_connect_host(bind_addr: &str) -> String {
    if bind_addr != "0.0.0.0" && bind_addr != "::" {
        return bind_addr.to_string();
    }

    if let Some(host) = std::env::var("JCODE_GATEWAY_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return host;
    }

    if let Some(host) = detect_tailscale_dns_name() {
        return host;
    }

    system_hostname().unwrap_or_else(|| UNKNOWN_CONNECT_HOST.to_string())
}

/// The system hostname, via `$HOSTNAME` and then the `hostname` command.
///
/// `$HOSTNAME` is a shell variable that is frequently not exported, so it
/// cannot be the only source.
fn system_hostname() -> Option<String> {
    if let Some(host) = std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(host);
    }

    match std::process::Command::new("hostname").output() {
        Ok(output) if output.status.success() => {
            let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if host.is_empty() { None } else { Some(host) }
        }
        Ok(output) => {
            logging::info(&format!(
                "gateway: `hostname` exited with {}; cannot infer a reachable address",
                output.status
            ));
            None
        }
        Err(error) => {
            logging::info(&format!(
                "gateway: could not run `hostname` ({error}); cannot infer a reachable address"
            ));
            None
        }
    }
}

/// Working directory advertised to gateway clients.
///
/// `Subscribe` requires an absolute working directory, but a phone or browser
/// has no directory on the host, so it cannot invent one. The gateway therefore
/// publishes the server's own cwd (falling back to the home directory) in the
/// `/health` and `/pair` responses, and remote clients echo it back on
/// subscribe. Without this, every real-server subscribe from a remote client
/// fails with "Subscribe requires the client's working directory".
fn default_client_working_dir() -> String {
    match std::env::current_dir() {
        Ok(path) => Some(path),
        Err(error) => {
            crate::logging::warn(&format!(
                "Gateway working directory unavailable, using home: {error}"
            ));
            None
        }
    }
    .filter(|p| p.is_absolute())
    .or_else(dirs::home_dir)
    .map(|p| p.to_string_lossy().to_string())
    .unwrap_or_else(|| "/".to_string())
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod gateway_tests;
