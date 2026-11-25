//! Backend selection shared by terminal and desktop voice clients.
use super::{SONIOX_URL, nari, nari_api_key, soniox_api_key};

/// Realtime streaming provider used by `NariRecording::start_streaming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBackend {
    Nari,
    Soniox,
    ElevenLabs,
}

impl StreamingBackend {
    /// Public provider endpoint used when no `[dictation] base_url` is set.
    pub fn default_url(self) -> &'static str {
        match self {
            Self::Nari => nari::URL,
            Self::Soniox => SONIOX_URL,
            Self::ElevenLabs => super::elevenlabs::URL,
        }
    }

    /// API key from env (`NARI_API_KEY` / `SONIOX_API_KEY`), else
    /// `~/.config/jcode/{nari,soniox}.env`.
    pub fn api_key(self) -> Option<String> {
        match self {
            Self::Nari => nari_api_key(),
            Self::Soniox => soniox_api_key(),
            Self::ElevenLabs => {
                let (var, file) = self.key_location();
                jcode_provider_env::load_api_key_from_env_or_config(var, file)
            }
        }
    }

    /// Human name, as shown in Settings and in error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nari => "Nari",
            Self::Soniox => "Soniox",
            Self::ElevenLabs => "ElevenLabs Scribe",
        }
    }

    /// Model requested when `[dictation] model` is empty.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Nari => super::NARI_MODEL,
            Self::Soniox => super::SONIOX_MODEL,
            Self::ElevenLabs => super::elevenlabs::MODEL,
        }
    }

    /// `(env var, env file name)` holding this backend's key.
    pub fn key_location(self) -> (&'static str, &'static str) {
        match self {
            Self::Nari => ("NARI_API_KEY", "nari.env"),
            Self::Soniox => ("SONIOX_API_KEY", "soniox.env"),
            Self::ElevenLabs => ("ELEVENLABS_API_KEY", "elevenlabs.env"),
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

/// The provider a dictation start will use, resolved from `[dictation]`
/// without touching the microphone or the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingPlan {
    pub backend: StreamingBackend,
    pub key: String,
    /// Non-empty `[dictation] base_url`.
    pub url: Option<String>,
    /// Non-empty `[dictation] model`. `None` means the backend default.
    pub model: Option<String>,
    /// `backend = auto` picked this provider from the keys that exist.
    pub auto: bool,
}

impl StreamingPlan {
    /// Model that will actually be requested.
    pub fn effective_model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or_else(|| self.backend.default_model())
    }

    /// `Soniox (stt-rt-v6)`, for status lines and errors.
    pub fn label(&self) -> String {
        format!("{} ({})", self.backend.name(), self.effective_model())
    }
}

/// Resolve `[dictation]` to the streaming provider to start. The selected
/// backend and model always win: a missing key is an error naming that
/// backend, never a switch to another provider. Only `auto` chooses, Nari
/// first, then Soniox, and says so through [`StreamingPlan::auto`].
#[cfg(feature = "desktop-config")]
pub fn plan_streaming(
    dictation: &jcode_base::config::DictationConfig,
    key_for: impl Fn(StreamingBackend) -> Option<String>,
) -> Result<StreamingPlan, super::VoiceError> {
    use jcode_base::config::DictationBackend as B;
    use super::VoiceError;
    let url = Some(dictation.base_url.trim().to_owned()).filter(|u| !u.is_empty());
    let model = Some(dictation.model.trim().to_owned()).filter(|m| !m.is_empty());
    let (backend, key, auto) = match dictation.backend {
        B::Nari => (
            StreamingBackend::Nari,
            key_for(StreamingBackend::Nari).ok_or(VoiceError::NariNotConfigured)?,
            false,
        ),
        B::Soniox => (
            StreamingBackend::Soniox,
            key_for(StreamingBackend::Soniox).ok_or(VoiceError::SonioxNotConfigured)?,
            false,
        ),
        B::ElevenLabs => (
            StreamingBackend::ElevenLabs,
            key_for(StreamingBackend::ElevenLabs).ok_or(VoiceError::ElevenLabsNotConfigured)?,
            false,
        ),
        B::Auto => {
            if let Some(key) = key_for(StreamingBackend::Nari) {
                (StreamingBackend::Nari, key, true)
            } else if let Some(key) = key_for(StreamingBackend::Soniox) {
                (StreamingBackend::Soniox, key, true)
            } else {
                return Err(VoiceError::NariNotConfigured);
            }
        }
        B::Subscription => return Err(VoiceError::NotStreaming("Jcode subscription")),
        B::Command => return Err(VoiceError::NotStreaming("external command")),
    };
    Ok(StreamingPlan {
        backend,
        key,
        url,
        model,
        auto,
    })
}

/// What `[dictation]` selects, for status lines and errors that must name the
/// backend and model in use: `Soniox (stt-rt-v6)`, `external command`, or
/// `auto (Nari or Soniox, by key)`.
#[cfg(feature = "desktop-config")]
pub fn configured_label() -> String {
    use jcode_base::config::DictationBackend as B;
    let dictation = &jcode_base::config::config().dictation;
    let with_model = |backend: StreamingBackend| {
        let model = dictation.model.trim();
        let model = if model.is_empty() {
            backend.default_model()
        } else {
            model
        };
        format!("{} ({model})", backend.name())
    };
    match dictation.backend {
        B::Nari => with_model(StreamingBackend::Nari),
        B::Soniox => with_model(StreamingBackend::Soniox),
        B::ElevenLabs => with_model(StreamingBackend::ElevenLabs),
        B::Auto => "auto (Nari or Soniox, by key)".to_owned(),
        B::Subscription => "Jcode subscription".to_owned(),
        B::Command => "external command".to_owned(),
    }
}

/// How a dictation trigger (`/dictate`, the dictation key, Desktop's
/// `/dictation`) starts, so one setting decides it everywhere.
#[cfg(feature = "desktop-config")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerRoute {
    /// Run `[dictation] command`.
    Command,
    /// Live capture through the selected streaming backend.
    Streaming,
}

/// `command` runs the command. A streaming backend runs live capture and never
/// the command. `auto` keeps the historical behaviour: the command when one is
/// set, otherwise live capture.
#[cfg(feature = "desktop-config")]
pub fn trigger_route(dictation: &jcode_base::config::DictationConfig) -> TriggerRoute {
    use jcode_base::config::DictationBackend as B;
    match dictation.backend {
        B::Command => TriggerRoute::Command,
        B::Auto if !dictation.command.trim().is_empty() => TriggerRoute::Command,
        _ => TriggerRoute::Streaming,
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

    #[cfg(feature = "desktop-config")]
    mod selection {
        use super::super::*;
        use crate::VoiceError;
        use jcode_base::config::{Config, DictationBackend as B, DictationConfig};

        fn parsed(toml: &str) -> DictationConfig {
            toml::from_str::<Config>(toml).unwrap().dictation
        }

        const ALL: [StreamingBackend; 3] = [
            StreamingBackend::Nari,
            StreamingBackend::Soniox,
            StreamingBackend::ElevenLabs,
        ];

        /// Keys exist for every backend, so a wrong pick could not hide behind a
        /// missing key. The backend named in the file is the one started, with
        /// the file's url and model.
        #[test]
        fn each_selected_backend_and_model_is_the_one_started() {
            for (slug, want) in [
                ("nari", StreamingBackend::Nari),
                ("soniox", StreamingBackend::Soniox),
                ("elevenlabs", StreamingBackend::ElevenLabs),
            ] {
                let cfg = parsed(&format!(
                    "[dictation]\nbackend = \"{slug}\"\nmodel = \" chosen-model \"\nbase_url = \"ws://127.0.0.1:1/x\"\n"
                ));
                let plan =
                    plan_streaming(&cfg, |b| Some(format!("key-for-{}", b.name()))).unwrap();
                assert_eq!(plan.backend, want, "{slug}");
                assert_eq!(plan.key, format!("key-for-{}", want.name()));
                assert_eq!(plan.model.as_deref(), Some("chosen-model"));
                assert_eq!(plan.url.as_deref(), Some("ws://127.0.0.1:1/x"));
                assert!(!plan.auto);
                assert_eq!(plan.label(), format!("{} (chosen-model)", want.name()));
            }
        }

        #[test]
        fn empty_model_means_the_backend_default_for_every_backend() {
            for backend in ALL {
                let plan = StreamingPlan {
                    backend,
                    key: "k".into(),
                    url: None,
                    model: None,
                    auto: false,
                };
                assert_eq!(plan.effective_model(), backend.default_model());
            }
            assert_eq!(StreamingBackend::Soniox.default_model(), "stt-rt-v6");
        }

        /// A missing key on the selected backend is an error naming it. It is
        /// never answered by another backend that does have a key.
        #[test]
        fn selected_backend_without_a_key_fails_instead_of_switching() {
            for (slug, own, other, err) in [
                ("nari", StreamingBackend::Nari, StreamingBackend::Soniox, VoiceError::NariNotConfigured),
                ("soniox", StreamingBackend::Soniox, StreamingBackend::Nari, VoiceError::SonioxNotConfigured),
                ("elevenlabs", StreamingBackend::ElevenLabs, StreamingBackend::Soniox, VoiceError::ElevenLabsNotConfigured),
            ] {
                let cfg = parsed(&format!("[dictation]\nbackend = \"{slug}\"\n"));
                let result = plan_streaming(&cfg, |b| (b == other).then(|| "other-key".to_owned()));
                assert_eq!(result, Err(err), "{slug} must not fall back to {}", other.name());
                assert!(plan_streaming(&cfg, |b| (b == own).then(|| "k".to_owned())).is_ok());
            }
        }

        #[test]
        fn auto_is_the_only_backend_that_chooses_and_it_says_so() {
            let cfg = parsed("[dictation]\n");
            assert_eq!(cfg.backend, B::Auto);
            let both = plan_streaming(&cfg, |b| Some(b.name().to_owned())).unwrap();
            assert_eq!((both.backend, both.auto), (StreamingBackend::Nari, true));
            let soniox_only =
                plan_streaming(&cfg, |b| (b == StreamingBackend::Soniox).then(|| "k".to_owned()))
                    .unwrap();
            assert_eq!((soniox_only.backend, soniox_only.auto), (StreamingBackend::Soniox, true));
            assert_eq!(plan_streaming(&cfg, |_| None), Err(VoiceError::NariNotConfigured));
        }

        #[test]
        fn non_streaming_backends_name_themselves() {
            for (slug, name) in [("subscription", "Jcode subscription"), ("command", "external command")] {
                let cfg = parsed(&format!("[dictation]\nbackend = \"{slug}\"\n"));
                assert_eq!(
                    plan_streaming(&cfg, |_| Some("k".into())),
                    Err(VoiceError::NotStreaming(name))
                );
            }
        }

        /// `/dictate`, the dictation key and Desktop's `/dictation` all follow
        /// the one setting. A streaming backend never runs the command.
        #[test]
        fn triggers_follow_the_selected_backend() {
            let route = |toml: &str| trigger_route(&parsed(toml));
            let command = "command = \"my-whisper\"\n";
            for slug in ["nari", "soniox", "elevenlabs", "subscription"] {
                assert_eq!(
                    route(&format!("[dictation]\n{command}backend = \"{slug}\"\n")),
                    TriggerRoute::Streaming,
                    "{slug} must not run the external command"
                );
            }
            assert_eq!(
                route(&format!("[dictation]\n{command}backend = \"command\"\n")),
                TriggerRoute::Command
            );
            assert_eq!(route(&format!("[dictation]\n{command}")), TriggerRoute::Command);
            assert_eq!(route("[dictation]\n"), TriggerRoute::Streaming);
        }
    }
}
