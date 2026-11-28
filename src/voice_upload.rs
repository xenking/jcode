//! Lets a paired device (a phone over the Harness gateway) use this machine's
//! dictation backend. The harness bridge cannot link the voice crate, so the
//! executable installs this adapter at startup.
use jcode_harness_api::ErrorCode;
use jcode_harness_api_server::{AudioTranscriber, TranscribeError, install_audio_transcriber};
use jcode_voice::VoiceError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct ConfiguredDictation;

impl AudioTranscriber for ConfiguredDictation {
    fn available(&self) -> bool {
        jcode_voice::upload_supported()
    }

    fn transcribe(
        &self,
        wav: Vec<u8>,
        language: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, TranscribeError>> + Send>> {
        Box::pin(async move {
            jcode_voice::transcribe_upload(wav, language.as_deref())
                .await
                .map_err(refusal)
        })
    }
}

pub(crate) fn install() {
    install_audio_transcriber(Arc::new(ConfiguredDictation));
}

/// The wire code a client branches on. The text is `VoiceError`'s own, which
/// never carries keys, URLs, provider bodies or audio.
fn refusal(error: VoiceError) -> TranscribeError {
    let code = match &error {
        VoiceError::NariNotConfigured
        | VoiceError::SonioxNotConfigured
        | VoiceError::ElevenLabsNotConfigured
        | VoiceError::NotConfigured
        | VoiceError::UploadUnsupported(_) => ErrorCode::TranscriptionNotConfigured,
        VoiceError::InvalidAudio | VoiceError::InvalidLanguage => ErrorCode::InvalidRequest,
        VoiceError::Timeout => ErrorCode::TranscriptionTimeout,
        _ => ErrorCode::TranscriptionUnavailable,
    };
    let message = match error {
        VoiceError::InvalidAudio => "Audio must be a nonempty mono PCM16 WAV of at most 5 minutes and 10 MiB. Nari, Soniox and ElevenLabs need 16 kHz.".to_owned(),
        error => error.to_string(),
    };
    TranscribeError { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_size_limit_is_the_voice_crate_limit() {
        assert_eq!(
            jcode_harness_api::MAX_TRANSCRIBE_WAV_BYTES,
            jcode_voice::MAX_AUDIO_BYTES
        );
    }

    #[test]
    fn failures_map_to_the_code_a_client_branches_on() {
        for (error, code) in [
            (
                VoiceError::SonioxNotConfigured,
                ErrorCode::TranscriptionNotConfigured,
            ),
            (
                VoiceError::UploadUnsupported("external command"),
                ErrorCode::TranscriptionNotConfigured,
            ),
            (VoiceError::InvalidAudio, ErrorCode::InvalidRequest),
            (VoiceError::Timeout, ErrorCode::TranscriptionTimeout),
            (VoiceError::Network, ErrorCode::TranscriptionUnavailable),
            (VoiceError::Http(503), ErrorCode::TranscriptionUnavailable),
            (
                VoiceError::SonioxRejected,
                ErrorCode::TranscriptionUnavailable,
            ),
        ] {
            assert_eq!(refusal(error.clone()).code, code, "{error:?}");
        }
    }
}
