use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::Write;

use crate::storage;
use jcode_gateway_types::{DeviceScope, PairedDevice, PairingCode};

fn scoped_hash(scope: DeviceScope, value: &str) -> String {
    let prefix = match scope {
        DeviceScope::Phone => "sha256:",
        DeviceScope::Harness => "harness-sha256:",
    };
    format!("{prefix}{}", hex::encode(Sha256::digest(value.as_bytes())))
}

fn stored_code(scope: DeviceScope, code: &str) -> String {
    match scope {
        DeviceScope::Phone => code.to_owned(),
        DeviceScope::Harness => scoped_hash(scope, code),
    }
}

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
    pub fn lock_for_update() -> Result<std::fs::File> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(storage::jcode_dir()?.join("devices.lock"))?;
        lock.lock()?;
        Ok(lock)
    }

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
        let dir = storage::jcode_dir()?;
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.flush()?;
        file.persist(dir.join("devices.json"))?;
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
            let candidate = match scope {
                DeviceScope::Phone => format!("{:06}", rand::rng().random_range(0..1_000_000u32)),
                DeviceScope::Harness => hex::encode(rand::rng().random::<[u8; 16]>()),
            };
            let stored = stored_code(scope, &candidate);
            if self
                .pending_codes
                .iter()
                .all(|pending| pending.code != stored)
            {
                break candidate;
            }
        };

        self.pending_codes.push(PairingCode {
            scope,
            code: stored_code(scope, &code),
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
        let stored = stored_code(scope, code);
        if let Some(idx) = self
            .pending_codes
            .iter()
            .position(|c| c.code == stored && c.scope == scope && c.expires_at > now)
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
        let token_hash = scoped_hash(scope, &token);

        let now = chrono::Utc::now().to_rfc3339();

        // Remove existing device with same ID (re-pairing)
        self.devices
            .retain(|d| d.id != device_id || d.scope != scope);

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
        let phone = scoped_hash(DeviceScope::Phone, token);
        let harness = scoped_hash(DeviceScope::Harness, token);
        self.devices.iter().find(|d| match d.scope {
            DeviceScope::Phone => d.token_hash == phone,
            DeviceScope::Harness => d.token_hash == harness,
        })
    }

    /// Update last_seen for a device
    pub fn touch_device(&mut self, token: &str) {
        let Some(hash) = self
            .validate_token(token)
            .map(|device| device.token_hash.clone())
        else {
            return;
        };
        if let Some(device) = self.devices.iter_mut().find(|d| d.token_hash == hash) {
            device.last_seen = chrono::Utc::now().to_rfc3339();
            let _ = self.save();
        }
    }
}
