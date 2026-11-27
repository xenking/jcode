use serde::{Deserialize, Serialize};

/// WebSocket gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Enable the WebSocket gateway (default: false)
    pub enabled: bool,
    /// Enable paired Harness clients on a loopback-only gateway.
    pub harness_enabled: bool,
    /// Concurrent paired Harness WebSockets, one per open remote session
    /// (default: 16, clamped to 1..=256). The 17th client gets HTTP 503.
    pub max_harness_connections: usize,
    /// TCP port to listen on (default: 7643)
    pub port: u16,
    /// Bind address (default: 0.0.0.0)
    pub bind_addr: String,
}

/// Default for [`GatewayConfig::max_harness_connections`].
pub const DEFAULT_MAX_HARNESS_CONNECTIONS: usize = 16;

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            harness_enabled: false,
            max_harness_connections: DEFAULT_MAX_HARNESS_CONNECTIONS,
            port: 7643,
            bind_addr: "0.0.0.0".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_harness_connection_limit_is_a_gateway_setting() {
        assert_eq!(
            GatewayConfig::default().max_harness_connections,
            DEFAULT_MAX_HARNESS_CONNECTIONS
        );
        let parsed: GatewayConfig = toml::from_str("max_harness_connections = 64").unwrap();
        assert_eq!(parsed.max_harness_connections, 64);
    }
}
