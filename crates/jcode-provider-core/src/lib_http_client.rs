use super::*;
use std::sync::{Arc, OnceLock};

#[cfg(test)]
static NATIVE_ROOT_LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn load_tls_config() -> rustls::ClientConfig {
    #[cfg(test)]
    NATIVE_ROOT_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let loaded = rustls_native_certs::load_native_certs();
    if !loaded.errors.is_empty() {
        jcode_logging::warn(&format!(
            "TLS: native root certificate load errors: {:?}",
            loaded.errors
        ));
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(loaded.certs);
    if roots.is_empty() {
        jcode_logging::warn("TLS: no usable native roots, falling back to bundled WebPKI roots");
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS provider supports default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

fn new_tls_config() -> rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    let mut config = CONFIG.get_or_init(load_tls_config).clone();
    // Share certificate verification, not session tickets across different
    // clients or ALPN profiles. A transport retry starts with an empty cache.
    config.resumption = rustls::client::Resumption::default();
    config
}

/// HTTP/2 and HTTP/1.1 builder with the process-wide TLS configuration.
/// Configure TLS-specific options on the rustls config, not on this builder.
pub fn reqwest_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(new_tls_config())
}

/// HTTP/1.1-only builder. Reqwest cannot change ALPN on a preconfigured TLS config.
pub fn reqwest_http1_builder() -> reqwest::ClientBuilder {
    let mut config = new_tls_config();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .http1_only()
        .use_preconfigured_tls(config)
}

#[cfg(feature = "blocking-http")]
pub fn reqwest_blocking_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().use_preconfigured_tls(new_tls_config())
}

/// TLS for raw WebSocket upgrades: preserve the native trust config but do not
/// advertise HTTP/2 to servers when the caller sends an HTTP/1.1 upgrade.
pub fn websocket_tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let mut config = new_tls_config();
        config.alpn_protocols.clear();
        Arc::new(config)
    }))
}

/// Read an HTTP error body without hiding failures behind an empty string.
///
/// This is useful after a non-success status when the response is about to be
/// converted into an error. If reading the body itself fails, the returned text
/// preserves that failure so callers can include it in their error message.
pub async fn http_error_body(response: reqwest::Response, context: &str) -> String {
    match response.text().await {
        Ok(body) => body,
        Err(err) => format!("<failed to read {context} response body: {err}>"),
    }
}

/// Shared HTTP client for all generic provider requests. Reuse a single instance to share its
/// connection pool. Provider-specific transports can override the User-Agent on individual
/// requests to match an official client.
pub fn shared_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest_builder()
                .user_agent(JCODE_USER_AGENT)
                .connect_timeout(Duration::from_secs(15))
                .tcp_keepalive(Some(Duration::from_secs(30)))
                // Proactively detect half-dead pooled HTTP/2 connections before we
                // reuse them. Without keepalive pings, a stale multiplexed connection
                // (common behind NAT/VPN/proxy or flaky Wi-Fi) surfaces as
                // "http2 error: stream error received: unspecific protocol error".
                // Pinging while idle lets reqwest drop the connection instead.
                .http2_keep_alive_interval(Some(Duration::from_secs(30)))
                .http2_keep_alive_timeout(Duration::from_secs(15))
                .http2_keep_alive_while_idle(true)
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(8)
                .build()
                .unwrap_or_else(|err| {
                    eprintln!("jcode: failed to build shared provider HTTP client: {err}");
                    reqwest_builder()
                        .user_agent(JCODE_USER_AGENT)
                        .build()
                        .expect("fallback provider HTTP client should build")
                })
        })
        .clone()
}

/// Fresh HTTP client for transport-fault retries.
///
/// Retrying on the shared pooled client can reuse *other* idle connections
/// established through the same broken network path (corrupting middlebox,
/// flaky NAT/VPN) that produced a TLS fault like `BadRecordMac` - so the
/// retry fails the same way. This client disables connection pooling, which
/// guarantees the retry opens a brand-new TCP+TLS connection (the property
/// that makes transport-fault retries actually succeed). TLS trust is shared,
/// but TLS session tickets from other connections are not reused for the retry.
pub fn fresh_transport_client() -> reqwest::Client {
    reqwest_builder()
        .user_agent(JCODE_USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Some(Duration::from_secs(30)))
        .http2_keep_alive_timeout(Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        // No pooled reuse: every request gets a fresh connection.
        .pool_max_idle_per_host(0)
        .build()
        .unwrap_or_else(|_| shared_http_client())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn fresh_transports_load_native_roots_once() {
        for _ in 0..5 {
            let _client = fresh_transport_client();
        }
        let _client = shared_http_client();
        let _client = reqwest_http1_builder().build().unwrap();
        assert!(websocket_tls_config().alpn_protocols.is_empty());
        assert_eq!(NATIVE_ROOT_LOADS.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[ignore = "requires public HTTPS access to api.openai.com"]
    async fn negotiates_expected_http_versions() {
        for (client, version) in [
            (fresh_transport_client(), reqwest::Version::HTTP_2),
            (
                reqwest_http1_builder().build().unwrap(),
                reqwest::Version::HTTP_11,
            ),
        ] {
            let response = client
                .get("https://api.openai.com/v1/models")
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
            assert_eq!(response.version(), version);
        }
    }
}
