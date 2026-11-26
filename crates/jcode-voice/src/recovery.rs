//! Keeps a dictation's microphone audio on disk while it streams, so a provider
//! failure never costs the user what was already said. The file is a valid
//! mono 16 kHz WAV at every moment, is deleted when the dictation succeeds, and
//! stays (named in the error) when it does not.
use super::VoiceError;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const SAMPLE_RATE: u32 = 16_000;
const HEADER_BYTES: u64 = 44;
/// Recordings kept after a failure are removed this long after their last write.
const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Where kept recordings live: `<jcode dir>/voice-recovery`.
pub(crate) fn default_dir() -> Option<PathBuf> {
    jcode_base::storage::jcode_dir()
        .ok()
        .map(|dir| dir.join("voice-recovery"))
}

type Pcm = tokio::sync::mpsc::Receiver<Vec<i16>>;

/// The microphone audio being written to disk.
pub(crate) struct Spill {
    path: PathBuf,
    audio_bytes: Arc<AtomicU64>,
    writer: Option<tokio::task::JoinHandle<()>>,
}

/// Copy every chunk of `pcm` to a new WAV in `dir` and forward it unchanged.
/// The copy continues after the consumer is gone (a provider that failed for
/// good) until the microphone stops, so audio spoken after a failure is kept
/// too. When the file cannot be created the receiver comes back untouched and
/// dictation runs without a safety net.
pub(crate) fn tee(mut pcm: Pcm, dir: &Path) -> Result<(Pcm, Spill), Pcm> {
    let Some((path, mut file)) = create(dir) else {
        return Err(pcm);
    };
    let (tx, rx) = super::nari::capture_pcm_channel();
    let audio_bytes = Arc::new(AtomicU64::new(0));
    let written = audio_bytes.clone();
    let writer = tokio::spawn(async move {
        let mut consumer_alive = true;
        let mut spilling = true;
        while let Some(chunk) = pcm.recv().await {
            if spilling {
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                let total = written.load(Ordering::Relaxed) + bytes.len() as u64;
                // A full disk stops the copy, never the dictation.
                spilling = file.write_all(&bytes).is_ok() && patch_header(&mut file, total).is_ok();
                if spilling {
                    written.store(total, Ordering::Relaxed);
                }
            }
            if consumer_alive && tx.send(chunk).await.is_err() {
                consumer_alive = false;
            }
        }
    });
    Ok((
        rx,
        Spill {
            path,
            audio_bytes,
            writer: Some(writer),
        },
    ))
}

impl Spill {
    /// The error, naming the kept recording when there is audio worth keeping.
    pub(crate) fn name_in(&self, error: VoiceError) -> VoiceError {
        if matches!(error, VoiceError::Cancelled) || self.audio_bytes.load(Ordering::Relaxed) == 0 {
            return error;
        }
        VoiceError::AudioKept {
            cause: Box::new(error),
            path: self.path.clone(),
        }
    }

    /// Stop writing. A successful dictation deletes its recording.
    pub(crate) async fn finish(mut self, keep: bool) {
        if let Some(writer) = self.writer.take() {
            // The microphone is already stopped, so this ends within a chunk.
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        }
        if !keep || self.audio_bytes.load(Ordering::Relaxed) == 0 {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create(dir: &Path) -> Option<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir).ok()?;
    prune(dir);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!("dictation-{stamp}.wav"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()?;
    patch_header(&mut file, 0).ok()?;
    Some((path, file))
}

/// Drop recordings nobody recovered within [`KEEP_FOR`].
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > KEEP_FOR);
        if old && entry.path().extension().is_some_and(|e| e == "wav") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Rewrite the 44-byte header for `data` bytes of audio, leaving the cursor at the end.
fn patch_header(file: &mut std::fs::File, data: u64) -> std::io::Result<()> {
    let data = u32::try_from(data).unwrap_or(u32::MAX);
    let mut header = Vec::with_capacity(HEADER_BYTES as usize);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&data.saturating_add(36).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&1u16.to_le_bytes()); // mono
    header.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.seek(SeekFrom::Start(HEADER_BYTES + u64::from(data)))?;
    Ok(())
}
