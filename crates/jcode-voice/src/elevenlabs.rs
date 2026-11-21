//! Scribe V2 Realtime over the public ElevenLabs WebSocket API.
use super::nari::{NariEvent, cancelled, send, tls_connector};
use super::{MAX_RECORDING_DURATION, MAX_RESPONSE_BYTES, NARI_PCM_CHUNK_SAMPLES, VoiceError};
use base64::{Engine, engine::general_purpose::STANDARD};
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
    tungstenite::{
        Error, Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

pub(super) const URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime?model_id=scribe_v2_realtime&audio_format=pcm_16000&commit_strategy=manual";
const RATE: usize = 16_000;
const SEGMENT_SAMPLES: usize = 20 * RATE;
const TIMEOUT: Duration = Duration::from_secs(10);
const FINAL_TIMEOUT: Duration = Duration::from_secs(15);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Streaming Scribe session authenticated with a caller-supplied ElevenLabs key.
/// It never uses external app credentials and only connects to ElevenLabs.
pub struct ElevenLabsSession {
    socket: Socket,
    cancel: Arc<AtomicBool>,
}

impl ElevenLabsSession {
    pub async fn connect(key: &str, cancel: Arc<AtomicBool>) -> Result<Self, VoiceError> {
        Self::connect_to(URL, key, cancel).await
    }

    async fn connect_to(url: &str, key: &str, cancel: Arc<AtomicBool>) -> Result<Self, VoiceError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        let key = key.trim();
        if key.is_empty() || key.len() > 1024 || !key.bytes().all(|b| (33..=126).contains(&b)) {
            return Err(VoiceError::ElevenLabsNotConfigured);
        }
        let mut header =
            HeaderValue::from_str(key).map_err(|_| VoiceError::ElevenLabsNotConfigured)?;
        header.set_sensitive(true);
        let mut request = url.into_client_request().map_err(|_| VoiceError::Network)?;
        request.headers_mut().insert("xi-api-key", header);
        let setup = async {
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            }
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
            .map_err(|error| match error {
                Error::Http(response) => match response.status().as_u16() {
                    401 | 403 => VoiceError::ElevenLabsNotConfigured,
                    status => VoiceError::Http(status),
                },
                _ => VoiceError::Network,
            })?;
            let start = receive(&mut socket).await?;
            match message_type(&start)? {
                "session_started" => Ok(socket),
                kind => Err(rejection(kind).unwrap_or(VoiceError::InvalidResponse)),
            }
        };
        let socket = tokio::select! {
            biased;
            _ = cancelled(&cancel) => return Err(VoiceError::Cancelled),
            result = timeout(TIMEOUT, setup) => result.map_err(|_| VoiceError::Timeout)??,
        };
        Ok(Self { socket, cancel })
    }

    /// Consumes mono 16 kHz PCM16. Each transcript replaces the previous
    /// complete revision. Close the PCM sender to commit and finish.
    pub async fn run(
        self,
        pcm: mpsc::Receiver<Vec<i16>>,
        mut event: impl FnMut(NariEvent),
    ) -> Result<String, VoiceError> {
        let cancel = self.cancel.clone();
        event(NariEvent::Started);
        let result = tokio::select! {
            biased;
            _ = cancelled(&cancel) => Err(VoiceError::Cancelled),
            result = self.run_inner(pcm, &mut event) => result,
        };
        event(NariEvent::Finished(result.clone()));
        result
    }

    async fn run_inner(
        self,
        mut pcm: mpsc::Receiver<Vec<i16>>,
        event: &mut impl FnMut(NariEvent),
    ) -> Result<String, VoiceError> {
        let (mut sink, mut source) = self.socket.split();
        let mut transcript = Transcript::default();
        let mut samples = 0;
        let mut uncommitted = 0;
        let mut pending_commits = 0usize;
        let mut stopping = false;
        let mut deadline = Instant::now() + MAX_RECORDING_DURATION + TIMEOUT;
        loop {
            if stopping && pending_commits == 0 {
                let _ = timeout(TIMEOUT, sink.close()).await;
                return Ok(transcript.committed);
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Err(VoiceError::Timeout),
                chunk = pcm.recv(), if !stopping => match chunk {
                    Some(chunk) => {
                        if chunk.is_empty() || chunk.len() > NARI_PCM_CHUNK_SAMPLES {
                            return Err(VoiceError::InvalidAudio);
                        }
                        samples += chunk.len();
                        if samples > RATE * MAX_RECORDING_DURATION.as_secs() as usize {
                            return Err(VoiceError::InvalidAudio);
                        }
                        uncommitted += chunk.len();
                        let commit = uncommitted >= SEGMENT_SAMPLES;
                        if commit {
                            pending_commits += 1;
                            uncommitted = 0;
                        }
                        send(&mut sink, audio_chunk(&chunk, commit)).await?;
                    }
                    None => {
                        stopping = true;
                        if uncommitted > 0 {
                            pending_commits += 1;
                            send(&mut sink, audio_chunk(&[], true)).await?;
                        }
                        deadline = Instant::now() + FINAL_TIMEOUT;
                    }
                },
                incoming = receive(&mut source) => {
                    let incoming = incoming?;
                    let kind = message_type(&incoming)?;
                    if kind == "committed_transcript" {
                        pending_commits = pending_commits.saturating_sub(1);
                    }
                    if transcript.apply(&incoming)? {
                        event(NariEvent::Transcript(transcript.text.clone()));
                    }
                }
            }
        }
    }
}

fn audio_chunk(samples: &[i16], commit: bool) -> Value {
    let bytes: Vec<_> = samples.iter().flat_map(|n| n.to_le_bytes()).collect();
    json!({"message_type":"input_audio_chunk", "audio_base_64":STANDARD.encode(bytes), "sample_rate":RATE, "commit":commit})
}

async fn receive(
    source: &mut (impl futures::Stream<Item = Result<Message, Error>> + Unpin),
) -> Result<Value, VoiceError> {
    loop {
        match source.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|_| VoiceError::InvalidResponse);
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return Err(VoiceError::Network),
        }
    }
}

fn message_type(message: &Value) -> Result<&str, VoiceError> {
    message["message_type"]
        .as_str()
        .ok_or(VoiceError::InvalidResponse)
}

fn rejection(kind: &str) -> Option<VoiceError> {
    match kind {
        "auth_error" => Some(VoiceError::ElevenLabsNotConfigured),
        "rate_limited" | "commit_throttled" => Some(VoiceError::Http(429)),
        "error"
        | "quota_exceeded"
        | "unaccepted_terms"
        | "queue_overflow"
        | "resource_exhausted"
        | "session_time_limit_exceeded"
        | "input_error"
        | "invalid_request"
        | "chunk_size_exceeded"
        | "transcriber_error"
        | "insufficient_audio_activity" => Some(VoiceError::ElevenLabsRejected),
        _ => None,
    }
}

#[derive(Default)]
struct Transcript {
    committed: String,
    text: String,
}

impl Transcript {
    fn apply(&mut self, message: &Value) -> Result<bool, VoiceError> {
        let kind = message_type(message)?;
        let next = match kind {
            "partial_transcript" => {
                let tail = message["text"]
                    .as_str()
                    .ok_or(VoiceError::InvalidResponse)?;
                let mut next = self.committed.clone();
                append_segment(&mut next, tail);
                next
            }
            "committed_transcript" => {
                let segment = message["text"]
                    .as_str()
                    .ok_or(VoiceError::InvalidResponse)?;
                append_segment(&mut self.committed, segment);
                self.committed.clone()
            }
            _ => return rejection(kind).map_or(Ok(false), Err),
        };
        if next.len() > MAX_RESPONSE_BYTES {
            return Err(VoiceError::ResponseTooLarge);
        }
        if next == self.text {
            return Ok(false);
        }
        self.text = next;
        Ok(true)
    }
}

fn append_segment(text: &mut String, segment: &str) {
    let segment = segment.trim();
    if !segment.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn committed_segments_remain_while_the_current_partial_is_replaced() {
        let mut transcript = Transcript::default();
        for (kind, text, changed, visible) in [
            ("partial_transcript", "У синей", true, "У синей"),
            ("partial_transcript", "У синего", true, "У синего"),
            (
                "committed_transcript",
                "У синей реки.",
                true,
                "У синей реки.",
            ),
            ("partial_transcript", "Деревя", true, "У синей реки. Деревя"),
            (
                "partial_transcript",
                "Деревя",
                false,
                "У синей реки. Деревя",
            ),
            (
                "partial_transcript",
                "Деревянный мост",
                true,
                "У синей реки. Деревянный мост",
            ),
            (
                "committed_transcript",
                "Деревянный мост.",
                true,
                "У синей реки. Деревянный мост.",
            ),
            (
                "partial_transcript",
                "",
                false,
                "У синей реки. Деревянный мост.",
            ),
        ] {
            assert_eq!(
                transcript.apply(&json!({"message_type": kind, "text": text})),
                Ok(changed)
            );
            assert_eq!(transcript.text, visible);
        }
    }

    #[tokio::test]
    async fn streams_revisions_before_stop_and_commits_the_final_segment() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                socket,
                |request: &Request, response: Response| {
                    assert_eq!(request.headers()["xi-api-key"], "fixture-key");
                    Ok(response)
                },
            )
            .await
            .unwrap();
            send(&mut socket, json!({"message_type":"session_started"}))
                .await
                .unwrap();
            let frame = receive(&mut socket).await.unwrap();
            assert_eq!(frame["message_type"], "input_audio_chunk");
            assert_eq!(frame["sample_rate"], 16_000);
            assert_eq!(frame["commit"], false);
            assert_eq!(
                STANDARD
                    .decode(frame["audio_base_64"].as_str().unwrap())
                    .unwrap(),
                [0x34, 0x12, 0xfe, 0xff],
            );
            for text in ["Синий", "Синяя река"] {
                send(
                    &mut socket,
                    json!({"message_type":"partial_transcript", "text":text}),
                )
                .await
                .unwrap();
            }
            let commit = receive(&mut socket).await.unwrap();
            assert_eq!(commit["commit"], true);
            assert_eq!(commit["audio_base_64"], "");
            send(
                &mut socket,
                json!({"message_type":"committed_transcript", "text":"Синяя река."}),
            )
            .await
            .unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        });
        timeout(Duration::from_secs(5), async {
            let session = ElevenLabsSession::connect_to(&url, "fixture-key", Arc::default())
                .await
                .unwrap();
            let (pcm_tx, pcm_rx) = mpsc::channel(1);
            let (events_tx, mut events_rx) = mpsc::unbounded_channel();
            let client = tokio::spawn(session.run(pcm_rx, move |event| {
                events_tx.send(event).unwrap();
            }));
            pcm_tx.send(vec![0x1234, -2]).await.unwrap();
            assert!(matches!(events_rx.recv().await, Some(NariEvent::Started)));
            for expected in ["Синий", "Синяя река"] {
                assert!(matches!(events_rx.recv().await, Some(NariEvent::Transcript(text)) if text == expected));
            }
            drop(pcm_tx);
            assert!(matches!(events_rx.recv().await, Some(NariEvent::Transcript(text)) if text == "Синяя река."));
            assert!(matches!(events_rx.recv().await, Some(NariEvent::Finished(Ok(text))) if text == "Синяя река."));
            assert_eq!(client.await.unwrap(), Ok("Синяя река.".to_owned()));
            assert!(events_rx.recv().await.is_none());
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn malformed_transcript_is_rejected_and_ancillary_messages_are_ignored() {
        let mut transcript = Transcript::default();
        assert_eq!(
            transcript.apply(&json!({"message_type":"partial_transcript","text":123})),
            Err(VoiceError::InvalidResponse)
        );
        assert_eq!(
            transcript.apply(&json!({"message_type":"warning", "warning":"provider text"})),
            Ok(false)
        );
        assert!(transcript.text.is_empty());
    }
}
