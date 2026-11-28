//! Transcribe a finished recording made on another device (a phone over the
//! harness API) with the backend `[dictation]` selects. Same selection rule as
//! live dictation: the selected backend runs, and a missing key or a backend
//! that cannot take uploaded audio is an error naming it, never a switch.
use super::{
    StreamingBackend, StreamingPlan, TRANSCRIPTION_TIMEOUT, VoiceError, plan_streaming,
    transcribe_wav, validate_wav,
};
use jcode_base::config::{DictationBackend as B, DictationConfig};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

/// Longest wait for one upload, whatever `[dictation] timeout_secs` says.
const MAX_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Rate the streaming providers take. The subscription backend resamples.
const STREAMING_RATE: u32 = 16_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadRoute {
    /// Jcode subscription: one HTTP upload of the WAV.
    Subscription,
    /// Nari, Soniox or ElevenLabs: the PCM is streamed to the provider.
    Streaming(StreamingPlan),
}

/// Resolve `[dictation]` to the backend an upload uses, without the network.
/// `command` records by itself, so it cannot take audio: that is an error.
pub fn upload_route(
    dictation: &DictationConfig,
    key_for: impl Fn(StreamingBackend) -> Option<String>,
) -> Result<UploadRoute, VoiceError> {
    match dictation.backend {
        B::Subscription => Ok(UploadRoute::Subscription),
        _ if crate::trigger_route(dictation) == crate::TriggerRoute::Command => {
            Err(VoiceError::UploadUnsupported("external command"))
        }
        _ => plan_streaming(dictation, key_for).map(UploadRoute::Streaming),
    }
}

/// Whether the configured backend can ever take uploaded audio. Says nothing
/// about keys or the network.
pub fn upload_supported() -> bool {
    let dictation = &jcode_base::config::config().dictation;
    upload_route(dictation, |_| Some(String::new())).is_ok()
}

/// How long one upload may run: `[dictation] timeout_secs`, `0` meaning the
/// provider default, never more than [`MAX_UPLOAD_TIMEOUT`].
fn upload_timeout(timeout_secs: u64) -> Duration {
    match timeout_secs {
        0 => TRANSCRIPTION_TIMEOUT,
        secs => Duration::from_secs(secs).min(MAX_UPLOAD_TIMEOUT),
    }
}

/// Transcribe an in-memory mono PCM16 WAV (at most [`MAX_AUDIO_BYTES`] and five
/// minutes) with the configured backend. Audio and transcript are never logged
/// or written to disk. `language` is a lowercase ISO 639-1 hint used by the
/// subscription backend only. Dropping the future cancels the request.
pub async fn transcribe_upload(wav: Vec<u8>, language: Option<&str>) -> Result<String, VoiceError> {
    let dictation = jcode_base::config::config().dictation.clone();
    let route = upload_route(&dictation, StreamingBackend::api_key)?;
    let limit = upload_timeout(dictation.timeout_secs);
    tokio::time::timeout(limit, run_route(route, wav, language))
        .await
        .unwrap_or(Err(VoiceError::Timeout))
}

async fn run_route(
    route: UploadRoute,
    wav: Vec<u8>,
    language: Option<&str>,
) -> Result<String, VoiceError> {
    // Everything cheap fails here, before any key is read or socket opened.
    let (rate, data) = validate_wav(&wav)?;
    if language.is_some_and(|l| l.len() != 2 || !l.bytes().all(|b| b.is_ascii_lowercase())) {
        return Err(VoiceError::InvalidLanguage);
    }
    match route {
        UploadRoute::Subscription => transcribe_wav(wav, language).await,
        UploadRoute::Streaming(plan) => {
            if rate != STREAMING_RATE {
                return Err(VoiceError::InvalidAudio);
            }
            let pcm: Vec<i16> = wav[data]
                .chunks_exact(2)
                .map(|s| i16::from_le_bytes([s[0], s[1]]))
                .collect();
            drop(wav);
            super::streaming_capture::transcribe_buffered(
                &plan,
                &pcm,
                &Arc::new(AtomicBool::new(false)),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(toml: &str) -> DictationConfig {
        toml::from_str::<jcode_base::config::Config>(toml)
            .unwrap()
            .dictation
    }

    fn keys(_: StreamingBackend) -> Option<String> {
        Some("key".into())
    }

    fn wav(rate: u32, samples: usize) -> Vec<u8> {
        let data = samples * 2;
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(36 + data as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 1, 0]);
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&[2, 0, 16, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data as u32).to_le_bytes());
        wav.resize(wav.len() + data, 1);
        wav
    }

    /// The backend named in the file is the one used. Keys exist for every
    /// backend, so a wrong pick could not hide behind a missing key.
    #[test]
    fn the_selected_backend_is_the_one_an_upload_uses() {
        for (slug, want) in [
            ("nari", StreamingBackend::Nari),
            ("soniox", StreamingBackend::Soniox),
            ("elevenlabs", StreamingBackend::ElevenLabs),
        ] {
            let cfg = parsed(&format!("[dictation]\nbackend = \"{slug}\"\n"));
            let Ok(UploadRoute::Streaming(plan)) = upload_route(&cfg, keys) else {
                panic!("{slug} must stream");
            };
            assert_eq!(plan.backend, want);
        }
        let cfg = parsed("[dictation]\nbackend = \"subscription\"\n");
        assert_eq!(upload_route(&cfg, keys), Ok(UploadRoute::Subscription));
    }

    /// A missing key names the selected backend instead of trying another one.
    #[test]
    fn a_missing_key_is_an_error_not_a_switch() {
        let cfg = parsed("[dictation]\nbackend = \"soniox\"\n");
        let only_nari = |b| (b == StreamingBackend::Nari).then(|| "key".to_owned());
        assert_eq!(
            upload_route(&cfg, only_nari),
            Err(VoiceError::SonioxNotConfigured)
        );
    }

    /// A command records by itself: explicit `command`, or `auto` with a command.
    #[test]
    fn a_command_backend_cannot_take_uploaded_audio() {
        for toml in [
            "[dictation]\nbackend = \"command\"\n",
            "[dictation]\nbackend = \"auto\"\ncommand = \"rec\"\n",
        ] {
            assert_eq!(
                upload_route(&parsed(toml), keys),
                Err(VoiceError::UploadUnsupported("external command")),
                "{toml}"
            );
        }
    }

    #[test]
    fn the_wait_follows_the_config_within_bounds() {
        assert_eq!(upload_timeout(0), TRANSCRIPTION_TIMEOUT);
        assert_eq!(upload_timeout(45), Duration::from_secs(45));
        assert_eq!(upload_timeout(86_400), MAX_UPLOAD_TIMEOUT);
    }

    /// Bad audio and bad language fail before any key is read or socket opened:
    /// the plan's key and URL are never used.
    #[tokio::test]
    async fn bad_uploads_fail_before_any_network_call() {
        let plan = StreamingPlan {
            backend: StreamingBackend::Soniox,
            key: "k".into(),
            url: Some("ws://127.0.0.1:1/unreachable".into()),
            model: None,
            auto: false,
        };
        let route = UploadRoute::Streaming(plan);
        for (wav, language, want) in [
            (b"not a wav".to_vec(), None, VoiceError::InvalidAudio),
            (wav(8000, 800), None, VoiceError::InvalidAudio),
            (wav(16000, 1600), Some("EN"), VoiceError::InvalidLanguage),
        ] {
            assert_eq!(run_route(route.clone(), wav, language).await, Err(want));
        }
    }
}
