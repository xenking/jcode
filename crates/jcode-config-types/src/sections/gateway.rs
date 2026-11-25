use serde::{Deserialize, Serialize};

/// WebSocket gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Enable the WebSocket gateway (default: false)
    pub enabled: bool,
    /// Enable paired Harness clients on a loopback-only gateway.
    pub harness_enabled: bool,
    /// TCP port to listen on (default: 7643)
    pub port: u16,
    /// Bind address (default: 0.0.0.0)
    pub bind_addr: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            harness_enabled: false,
            port: 7643,
            bind_addr: "0.0.0.0".to_string(),
        }
    }
}
