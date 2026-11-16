//! Native Soniox realtime streaming. No audio, transcript, credential, or
//! provider body is logged. Ported from Orca's `soniox-transcription-client`.
//!
//! Uses the same PCM shape as Nari: bounded mono 16 kHz PCM16 chunks from
//! [`crate::nari_pcm_channel`] (or [`soniox_pcm_channel`]), dropped senders
//! finalize the stream.
use super::nari::{NariEvent, cancelled, receive, send, tls_connector};
use super::{MAX_RECORDING_DURATION, MAX_RESPONSE_BYTES, VoiceError};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

/// Public Soniox realtime endpoint.
pub const SONIOX_URL: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
/// Realtime model requested in the session configuration.
pub const SONIOX_MODEL: &str = "stt-rt-v6";
/// Languages to hint per session. Soniox treats these as hints, not a filter:
/// other languages are still transcribed, and a short list recognises better
/// than a long one. This fork is used in Russian and English, often mixed in
/// one sentence, which the model handles without `language_hints_strict`.
pub const SONIOX_LANGUAGE_HINTS: &[&str] = &["ru", "en"];
/// Same chunk bound as Nari so capture code can feed either backend.
pub const SONIOX_PCM_CHUNK_SAMPLES: usize = super::nari::NARI_PCM_CHUNK_SAMPLES;
const SAMPLE_RATE: u32 = 16000;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const FINAL_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const MAX_KEY_BYTES: usize = 1024;
/// Soniox control tokens that mark endpoints/finalization, never spoken text.
const CONTROL_TOKENS: &[&str] = &["<end>", "<fin>"];

/// Full aggregate revisions, not deltas. Finished is emitted exactly once by run().
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SonioxEvent {
    Started,
    /// Final plus provisional text. Replaces the previous revision.
    Partial(String),
    /// Text Soniox has marked final. Grows monotonically.
    Final(String),
    Finished(Result<String, VoiceError>),
}

/// `SONIOX_API_KEY`, else `<jcode config dir>/soniox.env`.
pub fn soniox_api_key() -> Option<String> {
    jcode_provider_env::load_api_key_from_env_or_config("SONIOX_API_KEY", "soniox.env")
        .filter(|key| !key.trim().is_empty())
}

/// Bounded mono 16 kHz PCM16 source. Identical shape to `nari_pcm_channel`.
pub fn soniox_pcm_channel() -> (mpsc::Sender<Vec<i16>>, mpsc::Receiver<Vec<i16>>) {
    super::nari::nari_pcm_channel()
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct SonioxSession {
    socket: Socket,
    cancel: Arc<AtomicBool>,
}

impl SonioxSession {
    /// Connect to the public endpoint. Resolves once the socket is open and
    /// configured. A server-side key rejection arrives during `run`.
    pub async fn connect(key: &str, cancel: Arc<AtomicBool>) -> Result<Self, VoiceError> {
        Self::connect_to(SONIOX_URL, key, cancel).await
    }

    /// Connect to an override endpoint (`[dictation] base_url`). Must be
    /// `wss://` without credentials or fragment. Plain `ws://` is accepted
    /// only for literal loopback IPs (local proxies and tests).
    pub async fn connect_to(
        url: &str,
        key: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, VoiceError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        validate_url(url)?;
        let key = key.trim();
        if key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || !key.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(VoiceError::SonioxNotConfigured);
        }
        let setup = async {
            if rustls::crypto::CryptoProvider::get_default().is_none()
                && rustls::crypto::aws_lc_rs::default_provider()
                    .install_default()
                    .is_err()
                && rustls::crypto::CryptoProvider::get_default().is_none()
            {
                return Err(VoiceError::Network);
            }
            let mut request = url.into_client_request().map_err(|_| VoiceError::Network)?;
            // The key travels only in the upgrade header, never in JSON.
            let mut auth = format!("Bearer {key}")
                .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .map_err(|_| VoiceError::SonioxNotConfigured)?;
            auth.set_sensitive(true);
            request.headers_mut().insert("Authorization", auth);
            let config = WebSocketConfig {
                max_message_size: Some(MAX_RESPONSE_BYTES),
                max_frame_size: Some(MAX_RESPONSE_BYTES),
                ..Default::default()
            };
            let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
                request,
                Some(config),
                false,
                Some(tls_connector()),
            )
            .await
            .map_err(handshake_error)?;
            super::timing::mark("soniox websocket connected");
            send(&mut socket, session_config()).await?;
            Ok(socket)
        };
        let socket = tokio::select! {
            biased;
            _ = cancelled(&cancel) => return Err(VoiceError::Cancelled),
            result = timeout(IO_TIMEOUT, setup) => result.map_err(|_| VoiceError::Timeout)??,
        };
        Ok(Self { socket, cancel })
    }

    /// Stream bounded PCM. EOF sends the end-of-audio frame and waits for
    /// `finished`. Cancellation interrupts reads and writes.
    pub async fn run(
        self,
        pcm: mpsc::Receiver<Vec<i16>>,
        mut event: impl FnMut(SonioxEvent),
    ) -> Result<String, VoiceError> {
        let cancel = self.cancel.clone();
        event(SonioxEvent::Started);
        let result = tokio::select! {
            biased;
            _ = cancelled(&cancel) => Err(VoiceError::Cancelled),
            result = self.run_inner(pcm, &mut event) => result,
        };
        event(SonioxEvent::Finished(result.clone()));
        result
    }

    /// Like `run`, but reports `NariEvent`s so callers built for Nari can swap
    /// backends without changes. `Partial` maps to `Transcript`.
    pub async fn run_nari_events(
        self,
        pcm: mpsc::Receiver<Vec<i16>>,
        mut event: impl FnMut(NariEvent),
    ) -> Result<String, VoiceError> {
        self.run(pcm, |e| match e {
            SonioxEvent::Started => event(NariEvent::Started),
            SonioxEvent::Partial(text) => event(NariEvent::Transcript(text)),
            SonioxEvent::Final(_) => {}
            SonioxEvent::Finished(result) => event(NariEvent::Finished(result)),
        })
        .await
    }

    async fn run_inner(
        self,
        mut pcm: mpsc::Receiver<Vec<i16>>,
        event: &mut impl FnMut(SonioxEvent),
    ) -> Result<String, VoiceError> {
        let (mut sink, mut source) = self.socket.split();
        let mut state = TranscriptState::default();
        let mut samples = 0usize;
        let mut sent_audio = false;
        let mut stopping = false;
        let mut keepalive =
            tokio::time::interval_at(Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
        let mut deadline = Instant::now() + MAX_RECORDING_DURATION + IO_TIMEOUT;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Err(VoiceError::Timeout),
                _ = keepalive.tick(), if !stopping => {
                    send(&mut sink, json!({"type":"keepalive"})).await?;
                }
                chunk = pcm.recv(), if !stopping => match chunk {
                    Some(chunk) => {
                        if chunk.is_empty() || chunk.len() > SONIOX_PCM_CHUNK_SAMPLES {
                            return Err(VoiceError::InvalidAudio);
                        }
                        samples += chunk.len();
                        if samples > SAMPLE_RATE as usize * MAX_RECORDING_DURATION.as_secs() as usize {
                            return Err(VoiceError::InvalidAudio);
                        }
                        if !sent_audio {
                            super::timing::mark("first audio chunk sent to soniox");
                        }
                        sent_audio = true;
                        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                        send_message(&mut sink, Message::Binary(bytes)).await?;
                    }
                    None => {
                        stopping = true;
                        if !sent_audio {
                            let _ = sink.close().await;
                            return Ok(String::new());
                        }
                        // An empty text frame ends the stream; an empty binary frame does not.
                        send_message(&mut sink, Message::Text(String::new())).await?;
                        deadline = Instant::now() + FINAL_TIMEOUT;
                    }
                },
                incoming = receive(&mut source) => {
                    let update = state.apply(&incoming?)?;
                    if update.final_changed {
                        event(SonioxEvent::Final(state.final_text()));
                    }
                    if update.text_changed {
                        event(SonioxEvent::Partial(state.text()));
                    }
                    if update.finished {
                        if !stopping {
                            return Err(VoiceError::InvalidResponse);
                        }
                        let _ = sink.close().await;
                        return Ok(state.text());
                    }
                }
            }
        }
    }
}

fn session_config() -> Value {
    session_config_with(SONIOX_LANGUAGE_HINTS, &super::nari::recognition_terms())
}

/// The start message for a realtime session. `language_hints` steers mixed
/// Russian/English speech and `context.terms` carries the built-in names plus
/// `[dictation] vocabulary`, which is what Soniox uses to spell unusual words
/// the way the user writes them. Both are omitted when empty, so an
/// unconfigured install sends exactly the audio fields.
fn session_config_with(hints: &[&str], terms: &[String]) -> Value {
    let mut config = json!({
        "model": SONIOX_MODEL,
        "audio_format": "pcm_s16le",
        "sample_rate": SAMPLE_RATE,
        "num_channels": 1,
    });
    if !hints.is_empty() {
        // Soniox rejects duplicate hints rather than deduplicating them.
        let mut unique: Vec<&str> = Vec::with_capacity(hints.len());
        for hint in hints {
            if !unique.contains(hint) {
                unique.push(hint);
            }
        }
        config["language_hints"] = json!(unique);
    }
    if !terms.is_empty() {
        config["context"] = json!({ "terms": terms });
    }
    config
}

async fn send_message<S>(socket: &mut S, message: Message) -> Result<(), VoiceError>
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    timeout(IO_TIMEOUT, socket.send(message))
        .await
        .map_err(|_| VoiceError::Timeout)?
        .map_err(|_| VoiceError::Network)
}

fn validate_url(url: &str) -> Result<(), VoiceError> {
    use tokio_tungstenite::tungstenite::http::Uri;
    if url.bytes().any(|b| b <= 32 || b >= 127) || url.contains(['@', '#', '\\']) {
        return Err(VoiceError::Network);
    }
    let uri: Uri = url.parse().map_err(|_| VoiceError::Network)?;
    let host = uri.host().ok_or(VoiceError::Network)?;
    match uri.scheme_str() {
        Some("wss") => Ok(()),
        Some("ws")
            if host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()) =>
        {
            Ok(())
        }
        _ => Err(VoiceError::Network),
    }
}

fn handshake_error(error: tokio_tungstenite::tungstenite::Error) -> VoiceError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error {
        status_error(response.status().as_u16())
    } else {
        VoiceError::Network
    }
}

fn status_error(status: u16) -> VoiceError {
    match status {
        401 | 403 => VoiceError::SonioxNotConfigured,
        402 => VoiceError::SonioxRejected,
        status => VoiceError::Http(status),
    }
}

#[derive(Default)]
struct TranscriptState {
    final_text: String,
    provisional: String,
    last: String,
}

#[derive(Default)]
struct Update {
    text_changed: bool,
    final_changed: bool,
    finished: bool,
}

impl TranscriptState {
    fn text(&self) -> String {
        format!("{}{}", self.final_text, self.provisional)
            .trim()
            .to_owned()
    }
    fn final_text(&self) -> String {
        self.final_text.trim().to_owned()
    }
    fn apply(&mut self, response: &Value) -> Result<Update, VoiceError> {
        // Provider error text can echo request details; keep only the code.
        if let Some(code) = response.get("error_code") {
            return Err(code
                .as_u64()
                .and_then(|c| u16::try_from(c).ok())
                .map_or(VoiceError::SonioxRejected, status_error));
        }
        let mut update = Update::default();
        if let Some(tokens) = response.get("tokens") {
            let tokens = tokens.as_array().ok_or(VoiceError::InvalidResponse)?;
            let mut provisional = String::new();
            let before = self.final_text.len();
            for token in tokens {
                let Some(text) = token["text"].as_str() else {
                    continue;
                };
                if CONTROL_TOKENS.contains(&text) {
                    continue;
                }
                if token["is_final"] == true {
                    self.final_text.push_str(text);
                } else {
                    provisional.push_str(text);
                }
            }
            if self.final_text.len() + provisional.len() > MAX_RESPONSE_BYTES {
                return Err(VoiceError::ResponseTooLarge);
            }
            // Each response carries the complete provisional tail.
            self.provisional = provisional;
            update.final_changed = self.final_text.len() != before;
        }
        let text = self.text();
        if text != self.last {
            self.last = text;
            update.text_changed = true;
        }
        update.finished = response["finished"] == true;
        Ok(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Mock Soniox server. `script` responses are sent after the first audio
    /// frame; `finished` is sent after the empty end frame.
    #[allow(
        clippy::result_large_err,
        reason = "WebSocket handshake callback uses tungstenite error type"
    )]
    async fn server(
        key: &'static str,
        script: Vec<Value>,
        on_end: Vec<Value>,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/transcribe-websocket",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    assert_eq!(request.headers()["authorization"], format!("Bearer {key}"));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let config = receive(&mut ws).await.unwrap();
            assert_eq!(config, session_config());
            assert!(config.get("api_key").is_none());
            let mut audio = Vec::new();
            let mut scripted = false;
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Binary(bytes) => {
                        audio.extend(bytes);
                        if !scripted {
                            scripted = true;
                            for value in &script {
                                send(&mut ws, value.clone()).await.unwrap();
                            }
                        }
                    }
                    Message::Text(text) if text.is_empty() => {
                        for value in &on_end {
                            send(&mut ws, value.clone()).await.unwrap();
                        }
                        send(&mut ws, json!({"tokens":[], "finished":true}))
                            .await
                            .unwrap();
                    }
                    Message::Text(text) => {
                        assert_eq!(text, json!({"type":"keepalive"}).to_string());
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            audio
        });
        (url, task)
    }

    #[tokio::test]
    async fn streams_pcm_and_emits_partial_and_final_text() {
        let (url, task) = server(
            "sk-test",
            vec![
                json!({"tokens":[{"text":"Hel","is_final":false}]}),
                json!({"tokens":[{"text":"Hello","is_final":true},{"text":" wor","is_final":false}]}),
            ],
            vec![json!({"tokens":[{"text":" world","is_final":true},{"text":"<fin>","is_final":true}]})],
        )
        .await;
        let session = SonioxSession::connect_to(&url, "sk-test", cancel())
            .await
            .unwrap();
        let (tx, rx) = soniox_pcm_channel();
        tx.send(vec![1, -1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(tx);
        let mut events = Vec::new();
        let text = session.run(rx, |e| events.push(e)).await.unwrap();
        assert_eq!(text, "Hello world");
        assert_eq!(task.await.unwrap(), vec![1, 0, 255, 255]);
        assert_eq!(events.first(), Some(&SonioxEvent::Started));
        assert!(events.contains(&SonioxEvent::Partial("Hel".into())));
        assert!(events.contains(&SonioxEvent::Partial("Hello wor".into())));
        assert!(events.contains(&SonioxEvent::Final("Hello".into())));
        assert!(events.contains(&SonioxEvent::Final("Hello world".into())));
        assert_eq!(
            events.last(),
            Some(&SonioxEvent::Finished(Ok("Hello world".into())))
        );
    }

    #[tokio::test]
    async fn nari_event_adapter_maps_partials_to_transcripts() {
        let (url, task) = server(
            "k",
            vec![json!({"tokens":[{"text":"Hi","is_final":true}]})],
            vec![],
        )
        .await;
        let session = SonioxSession::connect_to(&url, "k", cancel())
            .await
            .unwrap();
        let (tx, rx) = crate::nari_pcm_channel();
        tx.send(vec![7]).await.unwrap();
        drop(tx);
        let mut events = Vec::new();
        assert_eq!(
            session
                .run_nari_events(rx, |e| events.push(e))
                .await
                .unwrap(),
            "Hi"
        );
        task.await.unwrap();
        assert!(matches!(events.first(), Some(NariEvent::Started)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NariEvent::Transcript(t) if t == "Hi"))
        );
        assert!(matches!(events.last(), Some(NariEvent::Finished(Ok(t))) if t == "Hi"));
    }

    #[tokio::test]
    async fn no_audio_finishes_empty_without_end_frame() {
        let (url, task) = server("k", vec![], vec![]).await;
        let session = SonioxSession::connect_to(&url, "k", cancel())
            .await
            .unwrap();
        let (tx, rx) = soniox_pcm_channel();
        drop(tx);
        assert_eq!(session.run(rx, |_| {}).await.unwrap(), "");
        assert!(task.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_error_code_maps_without_message() {
        let (url, _task) = server(
            "k",
            vec![json!({"error_code":401, "error_message":"secret detail"})],
            vec![],
        )
        .await;
        let session = SonioxSession::connect_to(&url, "k", cancel())
            .await
            .unwrap();
        let (tx, rx) = soniox_pcm_channel();
        tx.send(vec![1]).await.unwrap();
        let mut events = Vec::new();
        assert_eq!(
            session.run(rx, |e| events.push(e)).await,
            Err(VoiceError::SonioxNotConfigured)
        );
        drop(tx);
        assert_eq!(
            events.last(),
            Some(&SonioxEvent::Finished(Err(VoiceError::SonioxNotConfigured)))
        );
    }

    #[tokio::test]
    async fn unexpected_finish_before_eof_is_rejected() {
        let (url, _task) = server("k", vec![json!({"finished":true})], vec![]).await;
        let session = SonioxSession::connect_to(&url, "k", cancel())
            .await
            .unwrap();
        let (tx, rx) = soniox_pcm_channel();
        tx.send(vec![1]).await.unwrap();
        assert_eq!(
            session.run(rx, |_| {}).await,
            Err(VoiceError::InvalidResponse)
        );
        drop(tx);
    }

    #[tokio::test]
    async fn invalid_chunks_and_cancellation() {
        let (url, _task) = server("k", vec![], vec![]).await;
        let session = SonioxSession::connect_to(&url, "k", cancel())
            .await
            .unwrap();
        let (tx, rx) = soniox_pcm_channel();
        tx.send(vec![0; SONIOX_PCM_CHUNK_SAMPLES + 1])
            .await
            .unwrap();
        assert_eq!(session.run(rx, |_| {}).await, Err(VoiceError::InvalidAudio));

        let (url, _task) = server("k", vec![], vec![]).await;
        let flag = cancel();
        let session = SonioxSession::connect_to(&url, "k", flag.clone())
            .await
            .unwrap();
        let (_tx, rx) = soniox_pcm_channel();
        flag.store(true, Ordering::SeqCst);
        assert_eq!(session.run(rx, |_| {}).await, Err(VoiceError::Cancelled));
    }

    #[tokio::test]
    async fn rejects_bad_keys_and_urls_before_network() {
        for key in ["", "  ", "bad key", "bad\r\nkey"] {
            assert_eq!(
                SonioxSession::connect_to("wss://soniox.invalid/x", key, cancel())
                    .await
                    .err(),
                Some(VoiceError::SonioxNotConfigured)
            );
        }
        assert_eq!(
            SonioxSession::connect(&"x".repeat(MAX_KEY_BYTES + 1), cancel())
                .await
                .err(),
            Some(VoiceError::SonioxNotConfigured)
        );
        for url in [
            "https://stt.example/x",
            "ws://stt.example/x",
            "wss://user:pw@stt.example/x",
            "wss://stt.example/x#f",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
        assert!(validate_url(SONIOX_URL).is_ok());
        assert!(validate_url("ws://127.0.0.1:9/x").is_ok());
        assert!(validate_url("ws://[::1]:9/x").is_ok());
    }

    #[tokio::test]
    async fn http_rejection_maps_to_not_configured() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/x", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut tcp, _) = listener.accept().await.unwrap();
            tcp.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        assert_eq!(
            SonioxSession::connect_to(&url, "k", cancel()).await.err(),
            Some(VoiceError::SonioxNotConfigured)
        );
    }

    #[test]
    fn control_tokens_are_dropped_and_provisional_replaced() {
        let mut state = TranscriptState::default();
        state
            .apply(&json!({"tokens":[{"text":"a","is_final":false}]}))
            .unwrap();
        state
            .apply(
                &json!({"tokens":[{"text":"<end>","is_final":true},{"text":"b","is_final":false}]}),
            )
            .unwrap();
        assert_eq!(state.text(), "b");
        assert!(state.apply(&json!({"tokens":"x"})).is_err());
    }

    /// The start message must carry the audio fields Soniox requires plus the
    /// two accuracy levers: mixed-language hints and vocabulary terms.
    #[test]
    fn session_config_carries_language_hints_and_vocabulary_terms() {
        let terms = vec!["Jcode".to_string(), "Kubernetes".to_string()];
        let config = session_config_with(SONIOX_LANGUAGE_HINTS, &terms);
        assert_eq!(config["model"], SONIOX_MODEL);
        assert_eq!(config["model"], "stt-rt-v6");
        assert_eq!(config["audio_format"], "pcm_s16le");
        assert_eq!(config["sample_rate"], 16_000);
        assert_eq!(config["num_channels"], 1);
        assert_eq!(config["language_hints"], json!(["ru", "en"]));
        assert_eq!(config["context"]["terms"], json!(["Jcode", "Kubernetes"]));
        assert!(
            config.get("api_key").is_none(),
            "the key stays in the header"
        );
        assert!(
            config.get("language_hints_strict").is_none(),
            "hints must not reject a language outside the list"
        );
    }

    /// Every field is optional: an install with no vocabulary or hints sends
    /// exactly the audio configuration and nothing empty.
    #[test]
    fn session_config_omits_empty_optional_context() {
        let bare = session_config_with(&[], &[]);
        assert!(bare.get("language_hints").is_none(), "{bare}");
        assert!(bare.get("context").is_none(), "{bare}");
        assert_eq!(bare["model"], SONIOX_MODEL);
    }

    /// Soniox rejects the request when hints repeat, so they are deduplicated
    /// before they reach the wire.
    #[test]
    fn session_config_deduplicates_language_hints() {
        let config = session_config_with(&["ru", "en", "ru"], &[]);
        assert_eq!(config["language_hints"], json!(["ru", "en"]));
    }

    /// The live config reads `[dictation] vocabulary` on top of the built-in
    /// names every install recognizes.
    #[test]
    fn live_session_config_sends_builtin_terms_and_hints() {
        let config = session_config();
        let terms = config["context"]["terms"]
            .as_array()
            .expect("recognition terms");
        assert!(terms.iter().any(|term| term == "Jcode"), "{terms:?}");
        assert!(terms.iter().any(|term| term == "Cargo"), "{terms:?}");
        assert_eq!(config["language_hints"], json!(SONIOX_LANGUAGE_HINTS));
    }
}
