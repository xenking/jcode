//! Backend selection shared by terminal and desktop voice clients.
use super::{SONIOX_URL, nari, nari_api_key, soniox_api_key};

/// Realtime streaming provider used by `NariRecording::start_streaming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBackend {
    Nari,
    Soniox,
}

impl StreamingBackend {
    /// Public provider endpoint used when no `[dictation] base_url` is set.
    pub fn default_url(self) -> &'static str {
        match self {
            Self::Nari => nari::URL,
            Self::Soniox => SONIOX_URL,
        }
    }

    /// API key from env (`NARI_API_KEY` / `SONIOX_API_KEY`), else
    /// `~/.config/jcode/{nari,soniox}.env`.
    pub fn api_key(self) -> Option<String> {
        match self {
            Self::Nari => nari_api_key(),
            Self::Soniox => soniox_api_key(),
        }
    }

    /// `(env var, env file name)` holding this backend's key.
    pub fn key_location(self) -> (&'static str, &'static str) {
        match self {
            Self::Nari => ("NARI_API_KEY", "nari.env"),
            Self::Soniox => ("SONIOX_API_KEY", "soniox.env"),
        }
    }

    /// Persist the key to `~/.config/jcode/<file>` as a secret (0600) file.
    /// File-only: never sets the process environment.
    pub fn save_api_key(self, key: &str) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() || key.len() > 1024 || !key.bytes().all(|b| (33..=126).contains(&b)) {
            return Err("invalid API key".into());
        }
        let (var, file) = self.key_location();
        jcode_provider_env::save_named_api_key(file, var, key).map_err(|e| e.to_string())
    }
}

/// Effective `[dictation] backend` and non-empty `base_url` from the jcode config.
#[cfg(feature = "desktop-config")]
pub fn configured_backend() -> (jcode_base::config::DictationBackend, Option<String>) {
    let dictation = &jcode_base::config::config().dictation;
    let url = Some(dictation.base_url.trim().to_owned()).filter(|u| !u.is_empty());
    (dictation.backend, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_metadata() {
        assert_eq!(StreamingBackend::Soniox.default_url(), SONIOX_URL);
        assert!(StreamingBackend::Nari.default_url().contains("narilabs"));
        assert_eq!(
            StreamingBackend::Soniox.key_location(),
            ("SONIOX_API_KEY", "soniox.env")
        );
        assert!(StreamingBackend::Soniox.save_api_key("bad key").is_err());
        assert!(StreamingBackend::Soniox.save_api_key(" ").is_err());
    }
}
