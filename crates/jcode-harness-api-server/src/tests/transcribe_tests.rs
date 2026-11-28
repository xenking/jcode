//! `transcribe_audio` through the real stream entry point, with a stub backend:
//! no network, no keys, no microphone.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use jcode_harness_api::{
    MAX_TRANSCRIBE_WAV_BASE64_BYTES, MAX_TRANSCRIBE_WAV_BYTES, TRANSCRIBE_CAPABILITY,
};
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

/// Answers every recording with the byte count it received. A `hold` request
/// (language `hold`) waits until `release` is notified.
struct Stub {
    available: bool,
    calls: AtomicUsize,
    release: tokio::sync::Notify,
}

impl Stub {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available,
            calls: AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
        })
    }
}

struct Shared(Arc<Stub>);

impl AudioTranscriber for Shared {
    fn available(&self) -> bool {
        self.0.available
    }
    fn transcribe(
        &self,
        wav: Vec<u8>,
        language: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, TranscribeError>> + Send>> {
        let stub = self.0.clone();
        stub.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match language.as_deref() {
                Some("hold") => stub.release.notified().await,
                Some("nokey") => {
                    return Err(TranscribeError {
                        code: ErrorCode::TranscriptionNotConfigured,
                        message: "no key".into(),
                    });
                }
                _ => {}
            }
            Ok(format!("{} bytes", wav.len()))
        })
    }
}

struct Bridge {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    hello: Value,
    task: tokio::task::JoinHandle<Result<()>>,
    _root: tempfile::TempDir,
}

async fn connect(transcriber: Option<Arc<dyn AudioTranscriber>>) -> Bridge {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (client, bridge) = tokio::io::duplex(1 << 20);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let task = tokio::spawn(run_bridge_stream_inner(
        bridge_read,
        bridge_write,
        socket,
        None,
        transcriber,
    ));
    let (read, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(read);
    write_json_line(
        &mut writer,
        &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1}),
    )
    .await
    .unwrap();
    let (_daemon, _) = listener.accept().await.unwrap();
    let (mut probe, _) = listener.accept().await.unwrap();
    write_json_line(&mut probe, &json!({"type":"pong","id":0}))
        .await
        .unwrap();
    let mut line = String::new();
    read_frame(&mut reader, &mut line).await.unwrap();
    // The daemon sockets close with the listener at the end of this scope, so
    // park them until the bridge task ends.
    tokio::spawn(async move {
        let _keep = (_daemon, probe, listener);
        std::future::pending::<()>().await
    });
    Bridge {
        reader,
        writer,
        hello: serde_json::from_str(&line).unwrap(),
        task,
        _root: root,
    }
}

impl Bridge {
    async fn send(&mut self, value: Value) {
        write_json_line(&mut self.writer, &value).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame(&mut self.reader, &mut line),
        )
        .await
        .expect("reply in time")
        .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn transcribe(&mut self, id: u64, wav: &str, language: Option<&str>) -> Value {
        self.send(json!({"v":1,"id":id,"req":"transcribe_audio","wav":wav,"language":language}))
            .await;
        self.next().await
    }

    async fn finish(mut self) {
        self.writer.shutdown().await.unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn audio(bytes: usize) -> String {
    STANDARD.encode(vec![1u8; bytes])
}

fn shared(stub: &Arc<Stub>) -> Option<Arc<dyn AudioTranscriber>> {
    Some(Arc::new(Shared(stub.clone())))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_capability_is_advertised_only_when_a_backend_can_take_audio() {
    for (transcriber, advertised) in [
        (shared(&Stub::new(true)), true),
        (shared(&Stub::new(false)), false),
        (None, false),
    ] {
        let bridge = connect(transcriber).await;
        let caps = bridge.hello["capabilities"].as_array().unwrap();
        assert_eq!(caps.contains(&json!(TRANSCRIBE_CAPABILITY)), advertised);
        bridge.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_is_transcribed_and_backend_errors_keep_their_code() {
    let stub = Stub::new(true);
    let mut bridge = connect(shared(&stub)).await;
    let reply = bridge.transcribe(2, &audio(1000), Some("en")).await;
    assert_eq!(reply["ev"], "audio_transcript");
    assert_eq!(reply["reply_to"], 2);
    assert_eq!(reply["text"], "1000 bytes");
    let reply = bridge.transcribe(3, &audio(10), Some("nokey")).await;
    assert_eq!(reply["ev"], "error");
    assert_eq!(reply["code"], "transcription_not_configured");
    assert_eq!(reply["reply_to"], 3);
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    bridge.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_refused_before_the_backend_is_called() {
    let stub = Stub::new(true);
    let mut bridge = connect(shared(&stub)).await;
    let oversize = "A".repeat(MAX_TRANSCRIBE_WAV_BASE64_BYTES + 4);
    let reply = bridge.transcribe(2, &oversize, None).await;
    assert_eq!(reply["code"], "audio_too_large");
    // Under the base64 bound but over the decoded bound.
    let slack = audio(MAX_TRANSCRIBE_WAV_BYTES + 1);
    if slack.len() <= MAX_TRANSCRIBE_WAV_BASE64_BYTES {
        assert_eq!(
            bridge.transcribe(3, &slack, None).await["code"],
            "audio_too_large"
        );
    }
    assert_eq!(
        bridge.transcribe(4, "not base64!", None).await["code"],
        "invalid_request"
    );
    bridge
        .send(json!({"v":1,"id":5,"req":"transcribe_audio"}))
        .await;
    assert_eq!(bridge.next().await["code"], "invalid_request");
    bridge
        .send(json!({"v":1,"id":6,"req":"transcribe_audio","wav":"AA==","language":7}))
        .await;
    assert_eq!(bridge.next().await["code"], "invalid_request");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    bridge.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_without_a_backend_says_it_cannot_transcribe() {
    let mut bridge = connect(None).await;
    let reply = bridge.transcribe(2, &audio(10), None).await;
    assert_eq!(reply["code"], "unknown_request");
    bridge.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_transcription_runs_per_connection_and_the_connection_stays_live() {
    let stub = Stub::new(true);
    let mut bridge = connect(shared(&stub)).await;
    bridge
        .send(json!({"v":1,"id":2,"req":"transcribe_audio","wav":audio(8),"language":"hold"}))
        .await;
    let busy = bridge.transcribe(3, &audio(8), None).await;
    assert_eq!(busy["code"], "invalid_request");
    assert_eq!(busy["reply_to"], 3);
    // The bridge keeps answering other requests while the backend works.
    bridge.send(json!({"v":1,"id":4,"req":"ping"})).await;
    assert_eq!(bridge.next().await["ev"], "pong");
    stub.release.notify_one();
    let reply = bridge.next().await;
    assert_eq!(
        (reply["reply_to"].as_u64(), reply["text"].as_str()),
        (Some(2), Some("8 bytes"))
    );
    bridge.finish().await;
}
