//! Opt-in live check against the real Soniox realtime API.
//!
//! Needs `SONIOX_API_KEY` or `<jcode config dir>/soniox.env`, plus a 16 kHz mono
//! PCM16 WAV at `SONIOX_LIVE_WAV`. Run with:
//! `SONIOX_LIVE_WAV=/path/t.wav justrust test -p jcode-voice --test soniox_live -- --ignored`
use jcode_voice::{SONIOX_MODEL, SonioxSession, soniox_api_key, soniox_pcm_channel};
use std::sync::{Arc, atomic::AtomicBool};

#[tokio::test]
#[ignore = "paid live Soniox call"]
async fn live_realtime_session_transcribes_wav() {
    let key = soniox_api_key().expect("SONIOX_API_KEY or <jcode config dir>/soniox.env");
    let path = std::env::var("SONIOX_LIVE_WAV").expect("SONIOX_LIVE_WAV");
    let bytes = std::fs::read(path).unwrap();
    // Find the `data` chunk (macOS afconvert inserts an FLLR padding chunk).
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("WAV data chunk")
        + 8;
    let samples: Vec<i16> = bytes[data..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let session = SonioxSession::connect(&key, Arc::new(AtomicBool::new(false)))
        .await
        .expect("connect");
    let (tx, rx) = soniox_pcm_channel();
    tokio::spawn(async move {
        for chunk in samples.chunks(1600) {
            if tx.send(chunk.to_vec()).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    let text = session.run(rx, |_| {}).await.expect("transcript");
    eprintln!("model={SONIOX_MODEL} text={text:?}");
    assert!(text.to_lowercase().contains("hello"), "got {text:?}");
}
