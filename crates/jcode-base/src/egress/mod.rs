//! Credential-scoped outbound transport, independent of the routing mode.
//!
//! `[[routing.accounts]] egress = "<name>"` sends every request made with that
//! credential (inference, token refresh, usage and quota meters) through the
//! proxy `[egress.<name>]`. Credentials without egress keep the shared client.
//!
//! Resolution is by credential: the exact account id (`claude:team`) wins over
//! the bare provider entry (`claude`). A `required` proxy that is unreachable
//! never lets a request leave directly. A proxy with `required = false` falls
//! back to the direct client while it is unreachable.

use anyhow::{Result, bail};
use jcode_routing_policy::{AccountId, Availability, config::EgressConfig};
use reqwest::{Client, Proxy, Url};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

use crate::config::Config;

const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const PROBE_TTL: Duration = Duration::from_secs(5);
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PROXY_HEAD: usize = 16 * 1024;

/// The selected route for one request, including any transport retries.
#[derive(Clone, Default)]
pub struct Transport(Option<Arc<Endpoint>>);

impl Transport {
    pub fn is_proxied(&self) -> bool {
        self.0.is_some()
    }

    /// The proxy client, or the shared direct client.
    pub fn client(&self) -> Client {
        self.client_or(&jcode_provider_core::shared_http_client())
    }

    /// The proxy client, or `direct` for a credential without egress.
    pub fn client_or(&self, direct: &Client) -> Client {
        match &self.0 {
            Some(endpoint) => endpoint.client.clone(),
            None => direct.clone(),
        }
    }

    /// Route a custom client builder (own timeouts, redirect policy) through
    /// the proxy. A direct transport leaves the builder untouched.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
        match &self.0 {
            Some(endpoint) => Ok(builder.proxy(Proxy::all(endpoint.url.clone())?)),
            None => Ok(builder),
        }
    }

    /// A client for transport-fault retries: never reuses a pooled connection.
    pub fn retry_client(&self) -> Client {
        match &self.0 {
            Some(endpoint) => endpoint.fresh_client.clone(),
            None => jcode_provider_core::fresh_transport_client(),
        }
    }

    /// Open a TCP stream to `host:port` for transports that do not use
    /// reqwest (WebSocket). Through the proxy for an assigned credential,
    /// otherwise direct. HTTP CONNECT and SOCKS5 proxies are supported. An
    /// `https://` proxy is refused rather than bypassed.
    pub async fn connect_tcp(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let Some(endpoint) = &self.0 else {
            return TcpStream::connect((host, port)).await;
        };
        timeout(TUNNEL_TIMEOUT, endpoint.tunnel(host, port))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "egress proxy tunnel timed out"))?
    }
}

struct Endpoint {
    url: Url,
    host: String,
    port: u16,
    client: Client,
    fresh_client: Client,
    health: tokio::sync::Mutex<Option<(Instant, bool)>>,
}

fn proxy_client(url: &Url, pooled: bool) -> Result<Client> {
    let builder = jcode_provider_core::reqwest_builder()
        .proxy(Proxy::all(url.clone())?)
        .user_agent(jcode_provider_core::JCODE_USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(15))
        .http2_keep_alive_while_idle(true);
    let builder = if pooled {
        builder
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
    } else {
        builder.pool_max_idle_per_host(0)
    };
    Ok(builder.build()?)
}

impl Endpoint {
    fn for_url(raw: &str) -> Result<Arc<Self>> {
        static CACHE: OnceLock<Mutex<HashMap<Url, Arc<Endpoint>>>> = OnceLock::new();
        let url = Url::parse(raw.trim())?;
        let mut cache = CACHE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(endpoint) = cache.get(&url) {
            return Ok(Arc::clone(endpoint));
        }

        let default_port = match url.scheme() {
            "http" => 80,
            "https" => 443,
            "socks5" | "socks5h" => 1080,
            _ => bail!("unsupported proxy scheme"),
        };
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("missing proxy host"))?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = url.port().unwrap_or(default_port);
        let endpoint = Arc::new(Self {
            client: proxy_client(&url, true)?,
            fresh_client: proxy_client(&url, false)?,
            url: url.clone(),
            host,
            port,
            health: tokio::sync::Mutex::default(),
        });
        cache.insert(url, Arc::clone(&endpoint));
        Ok(endpoint)
    }

    async fn reachable(&self) -> bool {
        let mut health = self.health.lock().await;
        if let Some((checked, reachable)) = *health
            && checked.elapsed() < PROBE_TTL
        {
            return reachable;
        }
        let reachable = timeout(
            PROBE_TIMEOUT,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        *health = Some((Instant::now(), reachable));
        reachable
    }

    async fn tunnel(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).await?;
        match self.url.scheme() {
            "http" => http_connect(&mut stream, &self.url, host, port).await?,
            "socks5" | "socks5h" => socks5_connect(&mut stream, &self.url, host, port).await?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "WebSocket through an https egress proxy is not supported",
                ));
            }
        }
        Ok(stream)
    }
}

fn credentials(url: &Url) -> Option<(String, String)> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    let decode = |raw: &str| {
        urlencoding::decode(raw)
            .map(|text| text.into_owned())
            .unwrap_or_else(|_| raw.to_owned())
    };
    Some((
        decode(url.username()),
        decode(url.password().unwrap_or_default()),
    ))
}

fn tunnel_error(message: &str) -> io::Error {
    io::Error::other(message.to_owned())
}

async fn http_connect(
    stream: &mut TcpStream,
    proxy: &Url,
    host: &str,
    port: u16,
) -> io::Result<()> {
    use base64::Engine as _;
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some((user, password)) = credentials(proxy) {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Read byte by byte so no tunnelled data is consumed with the response head.
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_PROXY_HEAD {
            return Err(tunnel_error("egress proxy response head too large"));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(tunnel_error("egress proxy closed during CONNECT"));
        }
        head.push(byte[0]);
    }
    let status = head
        .split(|b| *b == b' ')
        .nth(1)
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(200..=299) => Ok(()),
        Some(407) => Err(tunnel_error("egress proxy rejected the credentials")),
        _ => Err(tunnel_error("egress proxy refused CONNECT")),
    }
}

async fn socks5_connect(
    stream: &mut TcpStream,
    proxy: &Url,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let auth = credentials(proxy);
    let greeting: &[u8] = if auth.is_some() {
        &[5, 2, 0, 2]
    } else {
        &[5, 1, 0]
    };
    stream.write_all(greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    match (choice, &auth) {
        ([5, 0], _) => {}
        ([5, 2], Some((user, password))) => {
            let (user, password) = (user.as_bytes(), password.as_bytes());
            let (Ok(user_len), Ok(password_len)) =
                (u8::try_from(user.len()), u8::try_from(password.len()))
            else {
                return Err(tunnel_error("egress proxy credentials too long"));
            };
            let mut message = vec![1, user_len];
            message.extend_from_slice(user);
            message.push(password_len);
            message.extend_from_slice(password);
            stream.write_all(&message).await?;
            let mut verdict = [0u8; 2];
            stream.read_exact(&mut verdict).await?;
            if verdict[1] != 0 {
                return Err(tunnel_error("egress proxy rejected the credentials"));
            }
        }
        _ => return Err(tunnel_error("egress proxy offered no usable SOCKS5 auth")),
    }

    let mut request = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let Ok(len) = u8::try_from(host.len()) else {
                return Err(tunnel_error("target host name too long"));
            };
            request.push(3);
            request.push(len);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[1] != 0 {
        return Err(tunnel_error("egress proxy refused the SOCKS5 connect"));
    }
    let skip = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        _ => return Err(tunnel_error("egress proxy sent a malformed SOCKS5 reply")),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

fn assignment<'a>(
    config: &'a Config,
    provider: &str,
    label: Option<&str>,
) -> Option<(&'a str, Option<&'a EgressConfig>)> {
    let find = |id: &str| {
        config.routing.accounts.iter().find_map(|account| {
            (account.id.trim() == id)
                .then_some(account.egress.as_deref())
                .flatten()
        })
    };
    let name = label
        .and_then(|label| find(&format!("{provider}:{label}")))
        .or_else(|| find(provider))?;
    Some((name, config.egress.get(name)))
}

async fn select(config: &Config, provider: &str, label: Option<&str>) -> Result<Transport> {
    let Some((name, spec)) = assignment(config, provider, label) else {
        return Ok(Transport::default());
    };
    // Even an unrelated validation error in [routing] must not remove an
    // account's transport restriction. Never include the proxy URL in errors.
    let Some(spec) = spec else {
        bail!("egress proxy '{name}' is not configured");
    };
    let endpoint = Endpoint::for_url(&spec.url)
        .map_err(|_| anyhow::anyhow!("egress proxy '{name}' has an invalid URL"))?;
    if endpoint.reachable().await {
        Ok(Transport(Some(endpoint)))
    } else if spec.required {
        bail!("required egress proxy '{name}' is unreachable")
    } else {
        crate::logging::warn(&format!(
            "optional egress proxy '{name}' is unreachable, sending directly"
        ));
        Ok(Transport::default())
    }
}

/// The transport for one credential. `label` is `None` for an unlabelled
/// credential. Fails for a required proxy that is unreachable, so the caller
/// never falls back to a direct connection.
pub async fn transport(provider: &str, label: Option<&str>) -> Result<Transport> {
    select(crate::config::config(), provider, label).await
}

pub async fn transport_for(account: &AccountId) -> Result<Transport> {
    transport(account.provider(), account.label()).await
}

/// The transport of the API key that serves `provider` requests right now:
/// the active labelled key, else the unlabelled one.
pub async fn transport_for_active_key(provider: &str) -> Result<Transport> {
    let account = crate::provider_catalog::active_api_key_account(provider);
    let label = account.as_ref().and_then(AccountId::label);
    transport(provider, label).await
}

/// The transport of the key one runtime is bound to: its own account when it
/// has one, else the globally active key.
pub async fn transport_for_binding(
    provider: &str,
    binding: &jcode_provider_core::ApiKeyBinding,
) -> Result<Transport> {
    match binding {
        jcode_provider_core::ApiKeyBinding::Follow => transport_for_active_key(provider).await,
        jcode_provider_core::ApiKeyBinding::Account(label) => {
            transport(provider, label.as_deref()).await
        }
    }
}

/// An HTTP client for one credential: the proxy client, or the shared direct one.
pub async fn client(provider: &str, label: Option<&str>) -> Result<Client> {
    Ok(transport(provider, label).await?.client())
}

/// Report the credential's egress availability without contacting its vendor.
pub async fn availability(account: &AccountId) -> Availability {
    availability_in(crate::config::config(), account).await
}

async fn availability_in(config: &Config, account: &AccountId) -> Availability {
    if select(config, account.provider(), account.label())
        .await
        .is_ok()
    {
        Availability::Ready
    } else {
        Availability::EgressDown
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

#[cfg(test)]
mod tests;
