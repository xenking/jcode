use super::fixture::{Proxy, Upstream};
use super::*;
use jcode_routing_policy::Availability;
use tokio::net::TcpListener;

fn config(toml: &str) -> Config {
    toml::from_str(toml).expect("test config")
}

async fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn account(id: &str) -> AccountId {
    AccountId::parse(id).unwrap()
}

async fn transport_in(config: &Config, id: &str) -> Result<Transport> {
    let id = account(id);
    select(config, id.provider(), id.label()).await
}

fn assigned(proxy_url: &str, required: bool) -> Config {
    config(&format!(
        r#"
[egress.corp]
url = "{proxy_url}"
required = {required}

[[routing.accounts]]
id = "claude:team"
egress = "corp"

[[routing.accounts]]
id = "claude:solo"
"#
    ))
}

#[tokio::test]
async fn assigned_account_goes_through_the_proxy_and_others_go_direct() {
    let proxy = Proxy::start().await;
    let upstream = Upstream::start(r#"{"ok":true}"#).await;
    let config = assigned(&proxy.url, true);
    let url = format!("{}/v1/usage", upstream.url);

    let routed = transport_in(&config, "claude:team").await.unwrap();
    assert!(routed.is_proxied());
    let body = routed.client().get(&url).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, r#"{"ok":true}"#);
    assert!(proxy.saw(&format!("GET {url} ")), "proxy saw {:?}", proxy.seen());
    assert_eq!(upstream.count(), 1);

    let other = transport_in(&config, "claude:solo").await.unwrap();
    assert!(!other.is_proxied());
    let unrelated = transport_in(&config, "openai:work").await.unwrap();
    assert!(!unrelated.is_proxied());
    let direct = reqwest::Client::builder().no_proxy().build().unwrap();
    other.client_or(&direct).get(&url).send().await.unwrap();
    assert_eq!(proxy.seen().len(), 1, "direct request must not touch the proxy");
    assert_eq!(upstream.count(), 2);
}

#[tokio::test]
async fn exact_account_wins_over_the_bare_provider_entry() {
    let provider_proxy = Proxy::start().await;
    let account_proxy = Proxy::start().await;
    let config = config(&format!(
        r#"
[egress.wide]
url = "{}"
[egress.narrow]
url = "{}"

[[routing.accounts]]
id = "openai"
egress = "wide"

[[routing.accounts]]
id = "openai:work"
egress = "narrow"
"#,
        provider_proxy.url, account_proxy.url
    ));
    let upstream = Upstream::start("{}").await;
    let url = format!("{}/x", upstream.url);

    transport_in(&config, "openai:work").await.unwrap().client().get(&url).send().await.unwrap();
    assert_eq!(account_proxy.seen().len(), 1);
    assert!(provider_proxy.seen().is_empty());

    transport_in(&config, "openai:other").await.unwrap().client().get(&url).send().await.unwrap();
    transport_in(&config, "openai").await.unwrap().client().get(&url).send().await.unwrap();
    assert_eq!(provider_proxy.seen().len(), 2);
    assert_eq!(account_proxy.seen().len(), 1);
}

#[tokio::test]
async fn required_proxy_down_fails_closed_and_nothing_leaves() {
    let upstream = Upstream::start("{}").await;
    let config = assigned(&format!("http://127.0.0.1:{}", dead_port().await), true);

    let error = transport_in(&config, "claude:team").await.err().expect("fail closed");
    assert!(error.to_string().contains("'corp'"));
    assert_eq!(
        availability_in(&config, &account("claude:team")).await,
        Availability::EgressDown
    );
    assert_eq!(upstream.count(), 0);
    // The account that has no egress is unaffected.
    assert_eq!(
        availability_in(&config, &account("claude:solo")).await,
        Availability::Ready
    );
}

#[tokio::test]
async fn optional_proxy_down_goes_direct() {
    let config = assigned(&format!("http://127.0.0.1:{}", dead_port().await), false);
    let transport = transport_in(&config, "claude:team").await.unwrap();
    assert!(!transport.is_proxied());
    assert_eq!(
        availability_in(&config, &account("claude:team")).await,
        Availability::Ready
    );
}

#[tokio::test]
async fn unknown_or_invalid_egress_fails_closed() {
    let missing = config(
        r#"
[[routing.accounts]]
id = "claude:team"
egress = "nowhere"
"#,
    );
    assert_eq!(
        availability_in(&missing, &account("claude:team")).await,
        Availability::EgressDown
    );
    let invalid = assigned("ftp://secret-host:21", true);
    let error = transport_in(&invalid, "claude:team").await.err().unwrap();
    assert!(!error.to_string().contains("secret-host"));
}

#[tokio::test]
async fn http_connect_tunnel_reaches_the_target_through_the_proxy() {
    let proxy = Proxy::start().await;
    let upstream = Upstream::start("{}").await;
    let config = assigned(&proxy.url, true);
    let transport = transport_in(&config, "claude:team").await.unwrap();

    let mut stream = transport.connect_tcp("127.0.0.1", upstream.port).await.unwrap();
    stream
        .write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut reply = [0u8; 12];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"HTTP/1.1 200");
    assert!(proxy.saw(&format!("CONNECT 127.0.0.1:{} ", upstream.port)));
}

#[tokio::test]
async fn direct_transport_connects_without_a_proxy() {
    let upstream = Upstream::start("{}").await;
    let mut stream = Transport::default()
        .connect_tcp("127.0.0.1", upstream.port)
        .await
        .unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"HTTP");
}

/// A SOCKS5 server that records the requested target and tunnels to it.
async fn socks5_server(auth: Option<(&'static str, &'static str)>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut head = [0u8; 2];
                client.read_exact(&mut head).await.ok()?;
                let mut methods = vec![0u8; usize::from(head[1])];
                client.read_exact(&mut methods).await.ok()?;
                if let Some((user, password)) = auth {
                    client.write_all(&[5, 2]).await.ok()?;
                    let mut version = [0u8; 2];
                    client.read_exact(&mut version).await.ok()?;
                    let mut name = vec![0u8; usize::from(version[1])];
                    client.read_exact(&mut name).await.ok()?;
                    let mut len = [0u8; 1];
                    client.read_exact(&mut len).await.ok()?;
                    let mut pass = vec![0u8; usize::from(len[0])];
                    client.read_exact(&mut pass).await.ok()?;
                    let good = name == user.as_bytes() && pass == password.as_bytes();
                    client.write_all(&[1, u8::from(!good)]).await.ok()?;
                    if !good {
                        return None;
                    }
                } else {
                    client.write_all(&[5, 0]).await.ok()?;
                }
                let mut request = [0u8; 4];
                client.read_exact(&mut request).await.ok()?;
                let host = match request[3] {
                    1 => {
                        let mut ip = [0u8; 4];
                        client.read_exact(&mut ip).await.ok()?;
                        std::net::Ipv4Addr::from(ip).to_string()
                    }
                    3 => {
                        let mut len = [0u8; 1];
                        client.read_exact(&mut len).await.ok()?;
                        let mut name = vec![0u8; usize::from(len[0])];
                        client.read_exact(&mut name).await.ok()?;
                        String::from_utf8(name).ok()?
                    }
                    _ => return None,
                };
                let mut port = [0u8; 2];
                client.read_exact(&mut port).await.ok()?;
                let port = u16::from_be_bytes(port);
                log.lock().unwrap().push(format!("{host}:{port}"));
                let target = if host == "upstream.test" {
                    format!("127.0.0.1:{port}")
                } else {
                    format!("{host}:{port}")
                };
                let mut upstream = TcpStream::connect(target).await.ok()?;
                client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok()?;
                tokio::io::copy_bidirectional(&mut client, &mut upstream).await.ok()?;
                Some(())
            });
        }
    });
    (port, seen)
}

#[tokio::test]
async fn socks5_proxy_carries_http_clients_and_raw_tunnels() {
    let (port, seen) = socks5_server(None).await;
    let upstream = Upstream::start("{}").await;
    let config = assigned(&format!("socks5://127.0.0.1:{port}"), true);
    let transport = transport_in(&config, "claude:team").await.unwrap();

    transport
        .client()
        .get(format!("http://127.0.0.1:{}/via-socks", upstream.port))
        .send()
        .await
        .unwrap();
    assert_eq!(upstream.seen()[0].request_line, "GET /via-socks HTTP/1.1");
    assert_eq!(seen.lock().unwrap().len(), 1);

    let mut stream = transport.connect_tcp("upstream.test", upstream.port).await.unwrap();
    stream.write_all(b"GET /raw HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"HTTP");
    assert_eq!(seen.lock().unwrap().last().unwrap(), &format!("upstream.test:{}", upstream.port));
}

#[tokio::test]
async fn socks5_credentials_come_from_the_proxy_url() {
    let (port, _) = socks5_server(Some(("agent", "p@ss"))).await;
    let upstream = Upstream::start("{}").await;
    let good = assigned(&format!("socks5://agent:p%40ss@127.0.0.1:{port}"), true);
    let transport = transport_in(&good, "claude:team").await.unwrap();
    transport.connect_tcp("127.0.0.1", upstream.port).await.unwrap();

    let bad = assigned(&format!("socks5://agent:wrong@127.0.0.1:{port}"), true);
    let transport = transport_in(&bad, "claude:team").await.unwrap();
    assert!(transport.connect_tcp("127.0.0.1", upstream.port).await.is_err());
}

#[tokio::test]
async fn one_client_is_cached_per_proxy_url() {
    let proxy = Proxy::start().await;
    let first = Endpoint::for_url(&proxy.url).unwrap();
    let second = Endpoint::for_url(&format!("{}/", proxy.url)).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let other = Endpoint::for_url(&format!("http://127.0.0.1:{}", dead_port().await)).unwrap();
    assert!(!Arc::ptr_eq(&first, &other));
}

#[tokio::test]
async fn https_proxy_is_refused_for_raw_tunnels_instead_of_bypassed() {
    let proxy = Proxy::start().await;
    let config = assigned(&proxy.url.replace("http://", "https://"), true);
    let transport = transport_in(&config, "claude:team").await.unwrap();
    let error = transport.connect_tcp("127.0.0.1", 9).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(proxy.seen().is_empty() || !proxy.saw("CONNECT"));
}
