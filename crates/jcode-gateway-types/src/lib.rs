use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceScope {
    #[default]
    Phone,
    Harness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDevice {
    #[serde(default)]
    pub scope: DeviceScope,
    pub id: String,
    pub name: String,
    pub token_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apns_token: Option<String>,
    pub paired_at: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCode {
    #[serde(default)]
    pub scope: DeviceScope,
    pub code: String,
    pub created_at: String,
    pub expires_at: String,
}
