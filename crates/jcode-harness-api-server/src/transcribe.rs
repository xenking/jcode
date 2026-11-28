//! `transcribe_audio`: the bridge answers it itself by running the backend
//! the host installed, instead of translating it for the daemon.
//!
//! The backend lives outside this crate because the voice crate sits above the
//! shared runtime that links this one. A binary that has a backend calls
//! [`install_audio_transcriber`] once at startup; without one the capability is
//! not advertised and the request is an unknown request.
use base64::{Engine, engine::general_purpose::STANDARD};
use jcode_harness_api::{
    ApiEvent, ErrorCode, MAX_TRANSCRIBE_WAV_BASE64_BYTES, MAX_TRANSCRIBE_WAV_BYTES, ServerFrame,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

/// Why a transcription failed, in terms the client can branch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeError {
    pub code: ErrorCode,
    /// One sentence for the user. Never carries keys, audio or provider bodies.
    pub message: String,
}

/// A dictation backend that can transcribe a finished recording.
pub trait AudioTranscriber: Send + Sync + 'static {
    /// Whether the configured backend can ever take uploaded audio. The
    /// `transcribe_v1` capability follows this; missing keys still surface as
    /// an error on the request.
    fn available(&self) -> bool;

    /// Transcribe one mono PCM16 WAV. Dropping the future cancels the work.
    fn transcribe(
        &self,
        wav: Vec<u8>,
        language: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, TranscribeError>> + Send>>;
}

static INSTALLED: OnceLock<Arc<dyn AudioTranscriber>> = OnceLock::new();

/// Install the process-wide backend. The first call wins.
pub fn install_audio_transcriber(transcriber: Arc<dyn AudioTranscriber>) {
    let _ = INSTALLED.set(transcriber);
}

pub(crate) fn installed() -> Option<Arc<dyn AudioTranscriber>> {
    INSTALLED.get().cloned()
}

/// A transcription in flight on one connection. Polled by the connection's own
/// loop, so closing the connection drops it and cancels the backend call.
pub(crate) type Transcription = Pin<Box<dyn Future<Output = ServerFrame> + Send>>;

/// Check the request and start the backend, or return the error reply to send.
/// Every limit is enforced here, before the backend can touch the network.
pub(crate) fn start(
    transcriber: Option<&Arc<dyn AudioTranscriber>>,
    busy: bool,
    request: Value,
) -> Result<(Transcription, usize), Box<ServerFrame>> {
    let id = request["id"].as_u64().unwrap_or(0);
    let fail = |code, message: &str| {
        Box::new(ServerFrame::reply(
            id,
            ApiEvent::Error {
                code,
                message: message.into(),
            },
        ))
    };
    let Some(transcriber) = transcriber else {
        return Err(fail(
            ErrorCode::UnknownRequest,
            "This runtime cannot transcribe audio.",
        ));
    };
    if busy {
        return Err(fail(
            ErrorCode::InvalidRequest,
            "A transcription is already running on this connection.",
        ));
    }
    let Some(encoded) = request["wav"].as_str() else {
        return Err(fail(
            ErrorCode::InvalidRequest,
            "`wav` must be a base64 string.",
        ));
    };
    let too_large = || {
        fail(
            ErrorCode::AudioTooLarge,
            "The recording is larger than 10 MiB. Record a shorter clip.",
        )
    };
    if encoded.len() > MAX_TRANSCRIBE_WAV_BASE64_BYTES {
        return Err(too_large());
    }
    let language = match &request["language"] {
        Value::Null => None,
        Value::String(language) => Some(language.clone()),
        _ => {
            return Err(fail(
                ErrorCode::InvalidRequest,
                "`language` must be a string.",
            ));
        }
    };
    let wav = STANDARD
        .decode(encoded)
        .map_err(|_| fail(ErrorCode::InvalidRequest, "`wav` is not valid base64."))?;
    if wav.len() > MAX_TRANSCRIBE_WAV_BYTES {
        return Err(too_large());
    }
    drop(request);
    let bytes = wav.len();
    let work = transcriber.transcribe(wav, language);
    Ok((
        Box::pin(async move {
            ServerFrame::reply(
                id,
                match work.await {
                    Ok(text) => ApiEvent::AudioTranscript { text },
                    Err(error) => ApiEvent::Error {
                        code: error.code,
                        message: error.message,
                    },
                },
            )
        }),
        bytes,
    ))
}
