use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::storage;
use jcode_gateway_types::{DeviceScope, PairedDevice, PairingCode};

// ---------------------------------------------------------------------------
// Device registry (persisted to ~/.jcode/devices.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeviceRegistry {
    pub devices: Vec<PairedDevice>,
    #[serde(default)]
    pub pending_codes: Vec<PairingCode>,
}

impl DeviceRegistry {
    /// Load from ~/.jcode/devices.json
    pub fn load() -> Self {
        let path = match storage::jcode_dir() {
            Ok(d) => d.join("devices.json"),
            Err(_) => return Self::default(),
        };
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to ~/.jcode/devices.json
    pub fn save(&self) -> Result<()> {
        let path = storage::jcode_dir()?.join("devices.json");
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, contents)?;
        Ok(())
    }

    /// Generate a 6-digit pairing code, valid for 5 minutes
    pub fn generate_pairing_code(&mut self) -> String {
        self.generate_pairing_code_scoped(DeviceScope::Phone)
    }

    pub fn generate_pairing_code_scoped(&mut self, scope: DeviceScope) -> String {
        use rand::Rng;
        let now = chrono::Utc::now();
        let expires = now + chrono::Duration::minutes(5);

        // Clean up expired codes
        let now_str = now.to_rfc3339();
        self.pending_codes.retain(|c| c.expires_at > now_str);

        let code = loop {
            let candidate = format!("{:06}", rand::rng().random_range(0..1_000_000u32));
            if self
                .pending_codes
                .iter()
                .all(|pending| pending.code != candidate)
            {
                break candidate;
            }
        };

        self.pending_codes.push(PairingCode {
            scope,
            code: code.clone(),
            created_at: now.to_rfc3339(),
            expires_at: expires.to_rfc3339(),
        });

        let _ = self.save();
        code
    }

    /// Validate a pairing code and consume it. Returns true if valid.
    pub fn validate_code(&mut self, code: &str) -> bool {
        self.validate_code_scoped(code, DeviceScope::Phone)
    }

    pub fn validate_code_scoped(&mut self, code: &str, scope: DeviceScope) -> bool {
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(idx) = self
            .pending_codes
            .iter()
            .position(|c| c.code == code && c.scope == scope && c.expires_at > now)
        {
            self.pending_codes.remove(idx);
            let _ = self.save();
            true
        } else {
            false
        }
    }

    /// Register a new paired device. Returns the auth token.
    pub fn pair_device(
        &mut self,
        device_id: String,
        device_name: String,
        apns_token: Option<String>,
    ) -> String {
        self.pair_device_scoped(device_id, device_name, apns_token, DeviceScope::Phone)
    }

    pub fn pair_device_scoped(
        &mut self,
        device_id: String,
        device_name: String,
        apns_token: Option<String>,
        scope: DeviceScope,
    ) -> String {
        use rand::Rng;
        // Generate a random auth token
        let token_bytes: [u8; 32] = rand::rng().random();
        let token = hex::encode(token_bytes);

        // Store hash of token, not the token itself
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let token_hash = format!("sha256:{}", hex::encode(hasher.finalize()));

        let now = chrono::Utc::now().to_rfc3339();

        // Remove existing device with same ID (re-pairing)
        self.devices.retain(|d| d.id != device_id);

        self.devices.push(PairedDevice {
            scope,
            id: device_id,
            name: device_name,
            apns_token,
            token_hash,
            paired_at: now.clone(),
            last_seen: now,
        });

        let _ = self.save();
        token
    }

    /// Validate an auth token. Returns the device if valid.
    pub fn validate_token(&self, token: &str) -> Option<&PairedDevice> {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let token_hash = format!("sha256:{}", hex::encode(hasher.finalize()));

        self.devices.iter().find(|d| d.token_hash == token_hash)
    }

    /// Update last_seen for a device
    pub fn touch_device(&mut self, token: &str) {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let token_hash = format!("sha256:{}", hex::encode(hasher.finalize()));
        let now = chrono::Utc::now().to_rfc3339();

        if let Some(device) = self.devices.iter_mut().find(|d| d.token_hash == token_hash) {
            device.last_seen = now;
            let _ = self.save();
        }
    }
}
