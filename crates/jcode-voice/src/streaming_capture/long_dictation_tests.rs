//! A fake Soniox server that ends every session after a fixed amount of audio,
//! the way a provider session limit does. A 3-minute dictation must still
//! arrive complete, in order, with no repeats.
use super::*;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
use tokio_tungstenite::tungstenite::Message;

/// 0.4 s of audio per chunk, so 450 chunks are 3 minutes.
const CHUNK: usize = NARI_PCM_CHUNK_SAMPLES;
const CHUNKS_3_MIN: usize = 450;

#[derive(Clone, Copy)]
enum Limit {
    /// Close the socket without a word after N chunks, like a dropped link.
    Close,
    /// Send a provider error frame after N chunks.
    ErrorFrame,
}

struct Fake {
    url: String,
    sessions: Arc<AtomicUsize>,
    /// Chunks the server has fully answered, across sessions.
    acked: Arc<AtomicUsize>,
    models: Arc<Mutex<Vec<String>>>,
}

/// Each chunk carries its index in sample 0. The server answers every chunk
/// with the final token `w<index> `, and ends the session after `limit` chunks.
async fn fake_server(limit: usize, how: Limit) -> Fake {
    fake_server_for(limit, how, usize::MAX).await
}

/// Like `fake_server`, but it stops listening after `max_sessions`, as if the
/// provider had gone away for good.
async fn fake_server_for(limit: usize, how: Limit, max_sessions: usize) -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/transcribe-websocket", listener.local_addr().unwrap());
    let sessions = Arc::new(AtomicUsize::new(0));
    let acked = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(Mutex::new(Vec::new()));
    let (s, a, m) = (sessions.clone(), acked.clone(), models.clone());
    tokio::spawn(async move {
        for _ in 0..max_sessions {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (s, a, m) = (s.clone(), a.clone(), m.clone());
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                s.fetch_add(1, Ordering::SeqCst);
                let Some(Ok(Message::Text(config))) = ws.next().await else {
                    return;
                };
                let config: Value = serde_json::from_str(&config).unwrap();
                m.lock()
                    .unwrap()
                    .push(config["model"].as_str().unwrap_or_default().to_owned());
                let mut received = 0;
                while let Some(Ok(message)) = ws.next().await {
                    match message {
                        Message::Binary(bytes) => {
                            let index = i16::from_le_bytes([bytes[0], bytes[1]]);
                            let token = json!({"tokens":[{"text":format!("w{index} "),"is_final":true}]});
                            ws.send(Message::Text(token.to_string())).await.unwrap();
                            received += 1;
                            if received == limit {
                                match how {
                                    Limit::Close => {
                                        let _ = ws.close(None).await;
                                    }
                                    Limit::ErrorFrame => {
                                        let frame = json!({"error_code":408,"error_message":"limit"});
                                        let _ = ws.send(Message::Text(frame.to_string())).await;
                                        let _ = ws.close(None).await;
                                    }
                                }
                                a.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            a.fetch_add(1, Ordering::SeqCst);
                        }
                        Message::Text(text) if text.is_empty() => {
                            let _ = ws
                                .send(Message::Text(json!({"tokens":[],"finished":true}).to_string()))
                                .await;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    Fake { url, sessions, acked, models }
}

/// Feeds `chunks` chunks, one per server acknowledgement, then stops. The wait
/// keeps the test lossless: it models real time, where a chunk is never sent
/// before the previous one was answered.
fn feeding_capture(
    cancel: Arc<AtomicBool>,
    acked: Arc<AtomicUsize>,
    chunks: usize,
    boundary: usize,
    answerable: usize,
) -> (PcmRecording, tokio::sync::mpsc::Receiver<Vec<i16>>) {
    let (tx, rx) = nari_pcm_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let (s, c) = (stop.clone(), cancel.clone());
    let worker = thread::spawn(move || {
        for index in 0..chunks {
            let started = std::time::Instant::now();
            while acked.load(Ordering::SeqCst) < index.min(answerable) {
                if started.elapsed() > Duration::from_secs(20) || c.load(Ordering::SeqCst) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(1));
            }
            if boundary != 0 && index != 0 && index % boundary == 0 {
                // Let the session notice a close that was sent with the last answer.
                thread::sleep(Duration::from_millis(60));
            }
            let mut chunk = vec![1i16; CHUNK];
            chunk[0] = index as i16;
            if tx.blocking_send(chunk).is_err() {
                // Like the real microphone: nobody is reading it any more.
                return Err(VoiceError::CaptureFailed);
            }
        }
        while !s.load(Ordering::SeqCst) && !c.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        drop(tx);
        Ok(())
    });
    (
        PcmRecording {
            level: Arc::new(AtomicU32::new(0)),
            stop,
            cancel,
            worker: Some(worker),
        },
        rx,
    )
}

fn expected(chunks: usize) -> String {
    (0..chunks).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ")
}

struct Outcome {
    text: Result<String, VoiceError>,
    interrupted: Vec<VoiceError>,
    last_live: String,
}

async fn dictate(fake: &Fake, chunks: usize, boundary: usize) -> Outcome {
    run_dictation(fake, chunks, chunks, boundary).await
}

/// `answered` chunks get answered, then `fed` chunks in total were spoken.
async fn dictate_with_extra_audio(fake: &Fake, answered: usize, fed: usize) -> Outcome {
    run_dictation(fake, answered, fed, 0).await
}

async fn run_dictation(fake: &Fake, answered: usize, fed: usize, boundary: usize) -> Outcome {
    run_dictation_keeping(fake, answered, fed, boundary, None).await
}

async fn run_dictation_keeping(
    fake: &Fake,
    answered: usize,
    fed: usize,
    boundary: usize,
    recovery_dir: Option<std::path::PathBuf>,
) -> Outcome {
    let chunks = fed;
    let acked = fake.acked.clone();
    let url = fake.url.clone();
    let recording = tokio::task::spawn_blocking(move || {
        NariRecording::start_backend_with(
            Arc::new(AtomicBool::new(false)),
            StreamingBackend::Soniox,
            "sk-test",
            &url,
            None,
            StreamTuning {
                recovery_dir,
                ..StreamTuning::default()
            },
            move |cancel| Ok(feeding_capture(cancel, acked, chunks, boundary, answered)),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let wait_for_audio = tokio::time::timeout(Duration::from_secs(30), async {
        while fake.acked.load(Ordering::SeqCst) < answered {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    // Let a chunk that follows the last answer reach the queue.
    tokio::time::sleep(Duration::from_millis(100)).await;
    wait_for_audio.expect("every chunk was answered");
    recording.stop();
    let mut interrupted = Vec::new();
    let mut last_live = String::new();
    let text = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match recording.try_event() {
                Some(NariEvent::Finished(result)) => return result,
                Some(NariEvent::Transcript(text)) => last_live = text,
                Some(NariEvent::Interrupted(error)) => interrupted.push(error),
                Some(NariEvent::Started) => {}
                None => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
    })
    .await
    .expect("recording finishes");
    Outcome {
        text: text.map(|t| t.trim().to_owned()),
        interrupted,
        last_live: last_live.trim().to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_minutes_survive_a_ninety_second_session_limit() {
    // 90 s of audio per session is 225 chunks. Without renewal the third
    // minute never reaches the provider.
    let fake = fake_server(225, Limit::Close).await;
    let outcome = dictate(&fake, CHUNKS_3_MIN, 225).await;
    assert_eq!(outcome.text, Ok(expected(CHUNKS_3_MIN)));
    assert_eq!(outcome.last_live, expected(CHUNKS_3_MIN), "live preview joins up too");
    assert!(outcome.interrupted.is_empty(), "renewal is silent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_dropped_mid_dictation_reconnects_and_keeps_every_word() {
    // A limit far below the renewal point: the provider ends the session first.
    let fake = fake_server(40, Limit::Close).await;
    let outcome = dictate(&fake, 130, 40).await;
    assert_eq!(outcome.text, Ok(expected(130)));
    assert!(fake.sessions.load(Ordering::SeqCst) >= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_error_frame_mid_dictation_does_not_drop_earlier_text() {
    let fake = fake_server(40, Limit::ErrorFrame).await;
    let outcome = dictate(&fake, 130, 40).await;
    assert_eq!(outcome.text, Ok(expected(130)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_gone_for_good_still_delivers_the_text_so_far_with_a_named_reason() {
    // One session, then the provider is unreachable. The 20 chunks it
    // answered must come back, with the reason, not vanish.
    let fake = fake_server_for(20, Limit::Close, 1).await;
    let outcome = dictate_with_extra_audio(&fake, 20, 40).await;
    assert_eq!(outcome.text, Ok(expected(20)));
    // The microphone failing afterwards, because nothing reads it, is an effect
    // of the provider being gone and must not replace the reason.
    assert_eq!(outcome.interrupted, [VoiceError::Network]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chosen_model_is_what_the_session_requests() {
    let fake = fake_server(usize::MAX, Limit::Close).await;
    let acked = fake.acked.clone();
    let url = fake.url.clone();
    let recording = tokio::task::spawn_blocking(move || {
        NariRecording::start_backend_with(
            Arc::new(AtomicBool::new(false)),
            StreamingBackend::Soniox,
            "sk-test",
            &url,
            Some("stt-rt-chosen"),
            StreamTuning::default(),
            move |cancel| Ok(feeding_capture(cancel, acked, 3, 0, usize::MAX)),
        )
    })
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while fake.acked.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    recording.stop();
    drop(recording);
    assert_eq!(*fake.models.lock().unwrap(), ["stt-rt-chosen"]);
}

fn kept_wavs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_dictation_keeps_all_its_audio_and_names_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_server_for(20, Limit::Close, 1).await;
    let outcome = run_dictation_keeping(&fake, 20, 40, 0, Some(dir.path().join("recovery"))).await;
    assert_eq!(
        outcome.text,
        Ok(expected(20)),
        "recognised text is still delivered"
    );
    let wavs = kept_wavs(&dir.path().join("recovery"));
    assert_eq!(wavs.len(), 1);
    // The error names the file, and the cause is still the provider.
    let [VoiceError::AudioKept { cause, path }] = &outcome.interrupted[..] else {
        panic!("expected a named recording, got {:?}", outcome.interrupted);
    };
    assert_eq!(**cause, VoiceError::Network);
    assert_eq!(path, &wavs[0]);
    assert!(
        outcome.interrupted[0]
            .to_string()
            .contains(&path.display().to_string())
    );
    // Everything spoken is in the WAV, including the 20 chunks nobody transcribed.
    let wav = std::fs::read(path).unwrap();
    let audio = 40 * CHUNK * 2;
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(wav.len(), 44 + audio);
    assert_eq!(
        u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize,
        audio
    );
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dictation_that_succeeds_leaves_no_audio_behind() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_server(usize::MAX, Limit::Close).await;
    let outcome = run_dictation_keeping(&fake, 5, 5, 0, Some(dir.path().to_owned())).await;
    assert_eq!(outcome.text, Ok(expected(5)));
    assert!(kept_wavs(dir.path()).is_empty());
}

/// A finished buffer goes through the selected provider and comes back in
/// order, with no recovery file. A provider that dies after speaking is an
/// error, not a silently shortened transcript.
#[tokio::test]
async fn a_finished_buffer_is_transcribed_through_the_selected_provider() {
    let fake = fake_server(usize::MAX, Limit::Close).await;
    let plan = crate::StreamingPlan {
        backend: crate::StreamingBackend::Soniox,
        key: "key".into(),
        url: Some(fake.url.clone()),
        model: Some("chosen".into()),
        auto: false,
    };
    let mut pcm = vec![1i16; CHUNK * 3 + 10];
    for (index, chunk) in pcm.chunks_mut(CHUNK).enumerate() {
        chunk[0] = index as i16;
    }
    let text = transcribe_buffered(&plan, &pcm, &Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    assert_eq!(text, "w0 w1 w2 w3");
    assert_eq!(*fake.models.lock().unwrap(), ["chosen"]);

    // Only one provider session ever opens, so the cut is a failure.
    let one = fake_server_for(2, Limit::ErrorFrame, 1).await;
    let plan = crate::StreamingPlan { url: Some(one.url.clone()), ..plan };
    let error = transcribe_buffered(&plan, &pcm, &Arc::new(AtomicBool::new(false)))
        .await
        .unwrap_err();
    assert_ne!(error, VoiceError::Cancelled);
}
