//! Dedicated-thread streaming capture and the nonblocking push-to-talk handle.
//! Native (cpal) capture needs the `voice-capture` feature. External recorder
//! capture (`super::command_capture`) works in every build.
#[cfg(feature = "voice-capture")]
use super::resample::Resampler;
use super::*;
#[cfg(feature = "voice-capture")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread,
};

/// Mono 16k PCM capture. Stop is a signal, EOF follows buffered chunks.
/// Dropping an unfinished handle cancels, never blocks the UI on native teardown.
pub struct PcmRecording {
    pub(super) level: Arc<AtomicU32>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) worker: Option<thread::JoinHandle<Result<(), VoiceError>>>,
}
#[cfg(feature = "voice-capture")]
impl MicrophoneRecording {
    /// Blocking setup, call off the UI thread and only after explicit user consent.
    pub fn start_pcm_cancellable(
        cancel: Arc<AtomicBool>,
    ) -> Result<(PcmRecording, tokio::sync::mpsc::Receiver<Vec<i16>>), VoiceError> {
        PcmRecording::start(cancel)
    }
}
impl PcmRecording {
    #[cfg(feature = "voice-capture")]
    fn start(
        cancel: Arc<AtomicBool>,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<Vec<i16>>), VoiceError> {
        Self::start_worker(cancel, capture)
    }
    /// Shared setup: `body` owns the device on a dedicated thread, sends PCM on
    /// the channel, reports readiness once, and returns after stop or cancel.
    pub(super) fn start_worker(
        cancel: Arc<AtomicBool>,
        body: impl FnOnce(
            tokio::sync::mpsc::Sender<Vec<i16>>,
            &mpsc::SyncSender<Result<(), VoiceError>>,
            Arc<AtomicBool>,
            Arc<AtomicBool>,
            Arc<AtomicU32>,
        ) -> Result<(), VoiceError>
        + Send
        + 'static,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<Vec<i16>>), VoiceError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        let (tx, rx) = super::nari::capture_pcm_channel();
        let (ready, started) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let worker_level = level.clone();
        let (c, s) = (cancel.clone(), stop.clone());
        let worker = thread::Builder::new()
            .name("voice-pcm".into())
            .spawn(move || {
                let result = body(tx, &ready, c, s, worker_level);
                if let Err(e) = &result
                    && ready.try_send(Err(e.clone())).is_err()
                {
                    eprintln!("Voice setup error could not reach the startup receiver: {e}");
                }
                result
            })
            .map_err(|_| VoiceError::CaptureFailed)?;
        let started = wait_started(&started, &cancel);
        super::timing::mark("pcm start returned");
        match started {
            Ok(Ok(())) => Ok((
                Self {
                    level,
                    stop,
                    cancel,
                    worker: Some(worker),
                },
                rx,
            )),
            Ok(Err(e)) => Err(e),
            _ => Err(VoiceError::CaptureFailed),
        }
    }
    pub fn stop(&self) {
        self.stop_handle().stop();
    }
    /// Cloneable release signal that wakes the capture thread immediately.
    fn stop_handle(&self) -> StopHandle {
        StopHandle {
            flag: self.stop.clone(),
            thread: self.worker.as_ref().map(|w| w.thread().clone()),
        }
    }
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.is_finished())
    }
    /// Join off the UI thread to observe device/backpressure failures after EOF.
    pub fn finish(mut self) -> Result<(), VoiceError> {
        self.stop();
        self.worker
            .take()
            .ok_or(VoiceError::CaptureFailed)?
            .join()
            .map_err(|_| VoiceError::CaptureFailed)?
    }
}
impl Drop for PcmRecording {
    fn drop(&mut self) {
        if self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.stop();
    }
}
#[derive(Clone)]
struct StopHandle {
    flag: Arc<AtomicBool>,
    thread: Option<thread::Thread>,
}
impl StopHandle {
    fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // The capture loop parks between checks. Waking it here makes release
        // take effect now rather than on the next poll.
        if let Some(thread) = &self.thread {
            thread.unpark();
        }
    }
}
fn wait_started(
    ready: &mpsc::Receiver<Result<(), VoiceError>>,
    cancel: &AtomicBool,
) -> Result<Result<(), VoiceError>, mpsc::RecvError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(Err(VoiceError::Cancelled));
        }
        if std::time::Instant::now() >= deadline {
            cancel.store(true, Ordering::SeqCst);
            return Ok(Err(VoiceError::Timeout));
        }
        match ready.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => return Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(mpsc::RecvError),
        }
    }
}

#[cfg(feature = "voice-capture")]
struct Chunker {
    level: Arc<AtomicU32>,
    resampler: Resampler,
    chunk: Vec<i16>,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    failed: bool,
    samples: usize,
    first_callback: bool,
    first_voice: bool,
}
#[cfg(feature = "voice-capture")]
impl Chunker {
    fn push<T>(&mut self, data: &[T], channels: usize)
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        if self.failed {
            return;
        }
        if !self.first_callback {
            self.first_callback = true;
            super::timing::mark("first microphone callback");
        }
        let mut energy = 0.0f64;
        let mut frames = 0usize;
        for frame in data.chunks_exact(channels) {
            if self.samples >= 16000 * MAX_STREAMING_DURATION.as_secs() as usize {
                break;
            }
            let mono = frame
                .iter()
                .map(|s| {
                    let sample = <f32 as cpal::FromSample<T>>::from_sample_(*s);
                    if sample.is_finite() {
                        sample.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    }
                })
                .sum::<f32>()
                / channels as f32;
            energy += f64::from(mono).powi(2);
            frames += 1;
            let before = self.chunk.len();
            self.resampler.push(mono, &mut self.chunk);
            // Upsampling can emit two output samples for one native frame.
            // Clamp this final frame as well as the filter tail.
            let remaining = 16000 * MAX_STREAMING_DURATION.as_secs() as usize - self.samples;
            self.chunk.truncate(before + remaining);
            self.samples += self.chunk.len() - before;
            if self.chunk.len() >= 1600 {
                self.flush();
                if self.failed {
                    break;
                }
            }
        }
        let rms = if frames == 0 {
            0.0
        } else {
            (energy / frames as f64).sqrt() as f32
        };
        if !self.first_voice && rms > 0.02 {
            self.first_voice = true;
            super::timing::mark("first audible input (rms > 0.02)");
        }
        self.level
            .store(rms.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    fn finish(&mut self, limit: usize) {
        let remaining = limit.saturating_sub(self.samples);
        let before = self.chunk.len();
        self.resampler.finish(&mut self.chunk);
        self.chunk.truncate(before + remaining);
        self.flush();
    }
    fn flush(&mut self) {
        if self.chunk.is_empty() {
            return;
        }
        let chunk = std::mem::replace(&mut self.chunk, Vec::with_capacity(1602));
        // Never block an audio callback. Fail closed rather than silently lose audio.
        if self.tx.try_send(chunk).is_err() {
            self.failed = true;
        }
    }
}
#[cfg(feature = "voice-capture")]
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Mutex<Chunker>>,
) -> Result<cpal::Stream, VoiceError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let errors = state.clone();
    let channels = config.channels as usize;
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if let Ok(mut state) = state.lock() {
                    state.push(data, channels);
                }
            },
            move |_| {
                if let Ok(mut state) = errors.lock() {
                    state.failed = true;
                }
            },
            None,
        )
        .map_err(|_| VoiceError::MicrophoneUnavailable)
}
#[cfg(feature = "voice-capture")]
fn capture(
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ready: &mpsc::SyncSender<Result<(), VoiceError>>,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
) -> Result<(), VoiceError> {
    // Declared before the stream so teardown completes before the final reset.
    struct ResetLevel(Arc<AtomicU32>);
    impl Drop for ResetLevel {
        fn drop(&mut self) {
            self.0.store(0, Ordering::Relaxed);
        }
    }
    let _reset_level = ResetLevel(level.clone());
    if cancel.load(Ordering::SeqCst) {
        return Err(VoiceError::Cancelled);
    }
    super::timing::mark("capture thread started");
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(VoiceError::MicrophoneUnavailable)?;
    let supported = device
        .default_input_config()
        .map_err(|_| VoiceError::MicrophoneUnavailable)?;
    super::timing::mark("microphone device opened");
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    if config.channels == 0 {
        return Err(VoiceError::MicrophoneUnavailable);
    }
    let state = Arc::new(Mutex::new(Chunker {
        level,
        resampler: Resampler::new(config.sample_rate.0)?,
        chunk: Vec::with_capacity(1602),
        tx,
        failed: false,
        samples: 0,
        first_callback: false,
        first_voice: false,
    }));
    macro_rules! build {
        ($t:ty) => {
            build::<$t>(&device, &config, state.clone())?
        };
    }
    let stream = match format {
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        _ => return Err(VoiceError::MicrophoneUnavailable),
    };
    if cancel.load(Ordering::SeqCst) {
        return Err(VoiceError::Cancelled);
    }
    super::timing::mark("input stream built");
    stream
        .play()
        .map_err(|_| VoiceError::MicrophoneUnavailable)?;
    super::timing::mark("input stream playing");
    // PipeWire can take hundreds of ms to deliver the first buffer after a cold
    // start. Report ready only once audio flows, so the recording indicator never
    // precedes capture. Bounded so a quiet device still starts.
    let first_audio_deadline = std::time::Instant::now() + Duration::from_millis(1500);
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        let state = state.lock().map_err(|_| VoiceError::CaptureFailed)?;
        if state.failed {
            return Err(VoiceError::CaptureFailed);
        }
        if state.first_callback || std::time::Instant::now() >= first_audio_deadline {
            break;
        }
        drop(state);
        thread::sleep(Duration::from_millis(2));
    }
    super::timing::mark("capture signalled ready");
    ready.send(Ok(())).map_err(|_| VoiceError::Cancelled)?;
    let deadline = std::time::Instant::now() + MAX_STREAMING_DURATION;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        if stop.load(Ordering::SeqCst) || std::time::Instant::now() >= deadline {
            break;
        }
        {
            let state = state.lock().map_err(|_| VoiceError::CaptureFailed)?;
            if state.failed {
                return Err(VoiceError::CaptureFailed);
            }
            if state.samples >= 16000 * MAX_STREAMING_DURATION.as_secs() as usize {
                break;
            }
        }
        // Parked, not slept: stop() unparks so release is seen immediately.
        thread::park_timeout(Duration::from_millis(10));
    }
    super::timing::mark("capture saw stop");
    drop(stream);
    super::timing::mark("input stream dropped");
    let mut state = state.lock().map_err(|_| VoiceError::CaptureFailed)?;
    state.finish(16000 * MAX_STREAMING_DURATION.as_secs() as usize);
    super::timing::mark("capture flushed tail");
    if state.failed {
        Err(VoiceError::CaptureFailed)
    } else {
        Ok(())
    }
}

enum Session {
    Nari(NariSession),
    Soniox(super::SonioxSession),
    ElevenLabs(ElevenLabsSession),
}

/// Everything needed to open one more provider session.
#[derive(Clone)]
struct Target {
    backend: super::StreamingBackend,
    url: String,
    key: String,
    model: Option<String>,
}
impl Target {
    async fn connect(&self, cancel: &Arc<AtomicBool>) -> Result<Session, VoiceError> {
        let (url, key, model, c) = (&self.url, &self.key, self.model.as_deref(), cancel.clone());
        Ok(match self.backend {
            super::StreamingBackend::Nari => {
                Session::Nari(NariSession::connect_with_model(url, key, model, c).await?)
            }
            super::StreamingBackend::Soniox => Session::Soniox(
                super::SonioxSession::connect_with_model(url, key, model, c).await?,
            ),
            super::StreamingBackend::ElevenLabs => {
                Session::ElevenLabs(ElevenLabsSession::connect_with_model(key, model, c).await?)
            }
        })
    }
    async fn run_segment(
        session: Session,
        pcm: &mut tokio::sync::mpsc::Receiver<Vec<i16>>,
        limit_samples: usize,
        event: impl FnMut(NariEvent),
    ) -> Result<super::soniox::Segment, VoiceError> {
        match session {
            Session::Nari(s) => s.run_segment(pcm, limit_samples, event).await,
            Session::Soniox(s) => s.run_segment(pcm, limit_samples, event).await,
            Session::ElevenLabs(s) => s.run_segment(pcm, limit_samples, event).await,
        }
    }
}

/// How live dictation keeps a provider session alive for as long as the user
/// talks. Providers end sessions after a limit the client cannot see, so the
/// session is renewed on a schedule well inside any plausible limit, and also
/// reopened when it drops on its own.
#[derive(Clone, Copy)]
pub(super) struct StreamTuning {
    /// Audio sent on one session before it is closed cleanly and replaced.
    pub renew_after_samples: usize,
    /// Consecutive sessions that fail before producing any text. Reaching it
    /// ends the dictation with the text captured so far.
    pub max_failed_sessions: u32,
    /// Wait before reopening after a failure, multiplied by the failure count.
    pub backoff: Duration,
}
impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            renew_after_samples: 16000 * 60,
            max_failed_sessions: 3,
            backoff: Duration::from_millis(400),
        }
    }
}

/// What a dictation produced. `error` is why the stream stopped early; the
/// text is everything transcribed before that.
struct Streamed {
    text: String,
    error: Option<VoiceError>,
}

fn join_text(head: &str, tail: &str) -> String {
    let (head, tail) = (head.trim(), tail.trim());
    match (head.is_empty(), tail.is_empty()) {
        (true, _) => tail.to_owned(),
        (_, true) => head.to_owned(),
        _ => format!("{head} {tail}"),
    }
}

/// Drive provider sessions over one PCM source until the source ends.
///
/// Each session takes at most `renew_after_samples` of audio, then finalizes
/// cleanly, so its text is complete before the next session opens and the
/// transcripts concatenate without gaps or repeats. Audio that arrives while
/// the next session connects stays queued in `pcm`. A session that dies on its
/// own keeps whatever text it had, and the stream continues on a new one. The
/// only error returned is `Cancelled`: every other failure comes back inside
/// [`Streamed`] together with all the text captured so far.
async fn stream_resilient(
    first: Session,
    target: &Target,
    cancel: &Arc<AtomicBool>,
    mut pcm: tokio::sync::mpsc::Receiver<Vec<i16>>,
    tuning: StreamTuning,
    sink: &mut impl FnMut(NariEvent),
) -> Result<Streamed, VoiceError> {
    let mut committed = String::new();
    let mut next = Some(first);
    let mut failed_in_a_row = 0u32;
    loop {
        let session = match next.take() {
            Some(session) => session,
            None => match target.connect(cancel).await {
                Ok(session) => session,
                Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
                Err(error) => {
                    failed_in_a_row += 1;
                    if !retryable(&error, false)
                        || failed_in_a_row >= tuning.max_failed_sessions
                        || audio_finished(&pcm)
                    {
                        return Ok(Streamed { text: committed, error: Some(error) });
                    }
                    tokio::time::sleep(tuning.backoff * failed_in_a_row).await;
                    continue;
                }
            },
        };
        let mut live = String::new();
        let segment = Target::run_segment(session, &mut pcm, tuning.renew_after_samples, |event| {
            if let NariEvent::Transcript(text) = event {
                sink(NariEvent::Transcript(join_text(&committed, &text)));
                live = text;
            }
        })
        .await;
        match segment {
            Ok(segment) => {
                failed_in_a_row = 0;
                committed = join_text(&committed, &segment.text);
                sink(NariEvent::Transcript(committed.clone()));
                // Nothing more to say: do not open a session just to close it.
                if !segment.more || audio_finished(&pcm) {
                    return Ok(Streamed { text: committed, error: None });
                }
            }
            Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
            Err(error) => {
                // The session died. Its latest revision is the best text there is.
                if !live.trim().is_empty() {
                    committed = join_text(&committed, &live);
                    sink(NariEvent::Transcript(committed.clone()));
                    failed_in_a_row = 0;
                } else {
                    failed_in_a_row += 1;
                }
                if !retryable(&error, !live.trim().is_empty() || !committed.is_empty())
                    || failed_in_a_row >= tuning.max_failed_sessions
                    || audio_finished(&pcm)
                {
                    return Ok(Streamed { text: committed, error: Some(error) });
                }
                tokio::time::sleep(tuning.backoff).await;
            }
        }
    }
}

/// Whether opening another session can help. Network trouble and server-side
/// limits can pass. A bad key, exhausted credits, a refused configuration or
/// invalid audio will fail the same way again. A provider "rejected" reply
/// after the stream already produced text is a mid-session limit, which a new
/// session gets past, but a rejection at the very start is final.
fn retryable(error: &VoiceError, produced_text: bool) -> bool {
    match error {
        VoiceError::Network | VoiceError::Timeout | VoiceError::InvalidResponse => true,
        VoiceError::Http(status) => matches!(status, 408 | 425 | 429 | 500..=599),
        VoiceError::NariRejected
        | VoiceError::SonioxRejected
        | VoiceError::ElevenLabsRejected => produced_text,
        _ => false,
    }
}

/// The microphone is released and every queued chunk was consumed.
fn audio_finished(pcm: &tokio::sync::mpsc::Receiver<Vec<i16>>) -> bool {
    pcm.is_closed() && pcm.is_empty()
}

#[derive(Default)]
struct Events {
    queue: VecDeque<NariEvent>,
    finished: bool,
    ready: Arc<tokio::sync::Notify>,
}
impl Events {
    fn lock(events: &Mutex<Self>) -> std::sync::MutexGuard<'_, Self> {
        events.lock().unwrap_or_else(|poisoned| {
            eprintln!("Voice event state was poisoned, recovering pending events and wakeup");
            // Queue and Notify remain structurally valid after interrupted mutation.
            events.clear_poison();
            poisoned.into_inner()
        })
    }

    fn push(&mut self, event: NariEvent) {
        // Stores a permit when nobody waits, so a wakeup is never lost.
        self.ready.notify_one();
        if matches!(event, NariEvent::Finished(_)) {
            self.finished = true;
        }
        // A slow UI needs only the latest full revision. Started and Finished survive.
        if matches!(event, NariEvent::Transcript(_)) {
            self.queue
                .retain(|e| !matches!(e, NariEvent::Transcript(_)));
        }
        self.queue.push_back(event);
    }
}
/// Unified network + microphone operation. Construction, every handle method
/// and Drop are nonblocking, so a push-to-talk UI can show recording at the
/// press. The microphone opens and the provider handshake runs concurrently in
/// the background. PCM is buffered until session.configured, so speech during
/// setup is kept. Setup failures arrive as `NariEvent::Finished(Err(_))`.
pub struct NariRecording {
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: Arc<Mutex<Events>>,
    worker: thread::JoinHandle<()>,
    // Set once the native stream exists. Reads zero until then.
    level: Arc<std::sync::OnceLock<Arc<AtomicU32>>>,
    // Microphone release signal once capture exists. The mutex orders it
    // against stop() so a release during setup is never lost.
    mic_stop: Arc<Mutex<Option<StopHandle>>>,
}
impl NariRecording {
    /// Native microphone capture. Requires the `voice-capture` feature.
    #[cfg(feature = "voice-capture")]
    pub fn start_cancellable(cancel: Arc<AtomicBool>, key: &str) -> Result<Self, VoiceError> {
        Self::start_with(
            cancel,
            key,
            super::nari::URL,
            MicrophoneRecording::start_pcm_cancellable,
        )
    }
    /// Capture through external recorder processes, tried in order until one
    /// delivers audio. Works without the `voice-capture` feature.
    pub fn start_with_recorders(
        cancel: Arc<AtomicBool>,
        key: &str,
        recorders: Vec<super::RecorderCommand>,
    ) -> Result<Self, VoiceError> {
        if recorders.is_empty() {
            return Err(VoiceError::MicrophoneUnavailable);
        }
        Self::start_with(cancel, key, super::nari::URL, move |c| {
            super::command_capture::start_pcm(c, recorders)
        })
    }
    /// Push-to-talk for terminal clients. A configured `recorder` command wins.
    /// Otherwise native capture is used when compiled in, else the first
    /// working external recorder found on `PATH`.
    pub fn start_auto(
        cancel: Arc<AtomicBool>,
        key: &str,
        recorder: &str,
    ) -> Result<Self, VoiceError> {
        let custom = recorder.trim();
        #[cfg(feature = "voice-capture")]
        if custom.is_empty() {
            return Self::start_cancellable(cancel, key);
        }
        Self::start_with_recorders(cancel, key, super::detect_recorders(custom))
    }
    /// Push-to-talk for the configured `[dictation] backend` / `base_url` /
    /// `recorder`. `Auto` uses a Nari key if present, else a Soniox key, else
    /// `NariNotConfigured`. A missing key returns `NariNotConfigured` or
    /// `SonioxNotConfigured`. ElevenLabs must be explicitly selected and needs
    /// its own API key. `Subscription`/`Command` are not streaming
    /// backends here and return `Unavailable`.
    pub fn start_configured(cancel: Arc<AtomicBool>) -> Result<Self, VoiceError> {
        Self::start_configured_plan(cancel).map(|(recording, _)| recording)
    }
    /// [`Self::start_configured`], also returning the provider and model that
    /// were started so the UI can show them.
    pub fn start_configured_plan(
        cancel: Arc<AtomicBool>,
    ) -> Result<(Self, super::StreamingPlan), VoiceError> {
        let dictation = jcode_base::config::config().dictation.clone();
        let plan = super::plan_streaming(&dictation, super::StreamingBackend::api_key)?;
        let recording = Self::start_streaming(
            cancel,
            plan.backend,
            &plan.key,
            plan.url.as_deref(),
            plan.model.as_deref(),
            &dictation.recorder,
        )?;
        Ok((recording, plan))
    }
    /// Backend-selectable push-to-talk. Same handle, events and semantics as
    /// `start_auto`; `url` overrides the backend's public endpoint when set.
    /// Soniox `Partial` revisions arrive as `NariEvent::Transcript`.
    pub fn start_streaming(
        cancel: Arc<AtomicBool>,
        backend: super::StreamingBackend,
        key: &str,
        url: Option<&str>,
        model: Option<&str>,
        recorder: &str,
    ) -> Result<Self, VoiceError> {
        let url = url
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(backend.default_url())
            .to_owned();
        let custom = recorder.trim().to_owned();
        #[cfg(feature = "voice-capture")]
        if custom.is_empty() {
            return Self::start_backend_with(
                cancel,
                backend,
                key,
                &url,
                model,
                StreamTuning::default(),
                MicrophoneRecording::start_pcm_cancellable,
            );
        }
        let recorders = super::detect_recorders(&custom);
        if recorders.is_empty() {
            return Err(VoiceError::MicrophoneUnavailable);
        }
        Self::start_backend_with(
            cancel,
            backend,
            key,
            &url,
            model,
            StreamTuning::default(),
            move |c| super::command_capture::start_pcm(c, recorders),
        )
    }
    fn start_with(
        cancel: Arc<AtomicBool>,
        key: &str,
        url: &str,
        factory: impl FnOnce(
            Arc<AtomicBool>,
        ) -> Result<
            (PcmRecording, tokio::sync::mpsc::Receiver<Vec<i16>>),
            VoiceError,
        > + Send
        + 'static,
    ) -> Result<Self, VoiceError> {
        Self::start_backend(cancel, super::StreamingBackend::Nari, key, url, factory)
    }
    pub(super) fn start_backend(
        cancel: Arc<AtomicBool>,
        backend: super::StreamingBackend,
        key: &str,
        url: &str,
        factory: impl FnOnce(
            Arc<AtomicBool>,
        ) -> Result<
            (PcmRecording, tokio::sync::mpsc::Receiver<Vec<i16>>),
            VoiceError,
        > + Send
        + 'static,
    ) -> Result<Self, VoiceError> {
        Self::start_backend_with(
            cancel,
            backend,
            key,
            url,
            None,
            StreamTuning::default(),
            factory,
        )
    }
    pub(super) fn start_backend_with(
        cancel: Arc<AtomicBool>,
        backend: super::StreamingBackend,
        key: &str,
        url: &str,
        model: Option<&str>,
        tuning: StreamTuning,
        factory: impl FnOnce(
            Arc<AtomicBool>,
        ) -> Result<
            (PcmRecording, tokio::sync::mpsc::Receiver<Vec<i16>>),
            VoiceError,
        > + Send
        + 'static,
    ) -> Result<Self, VoiceError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(VoiceError::Cancelled);
        }
        let target = Target {
            backend,
            url: url.to_owned(),
            key: key.to_owned(),
            model: model.map(str::to_owned),
        };
        let events = Arc::new(Mutex::new(Events::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(std::sync::OnceLock::new());
        let mic_stop = Arc::new(Mutex::new(None::<StopHandle>));
        let (c, s, e, l) = (cancel.clone(), stop.clone(), events.clone(), level.clone());
        let m = mic_stop.clone();
        let worker = thread::Builder::new()
            .name("voice-nari".into())
            .spawn(move || {
                let result = (|| {
                    // Two workers so the provider handshake (native root loading
                    // and TLS setup are CPU-bound) never delays mic readiness.
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .thread_name("voice-nari-rt")
                        .enable_all()
                        .build()
                        .map_err(|_| VoiceError::CaptureFailed)?;
                    let result = runtime.block_on(async {
                        super::timing::mark("nari worker started");
                        // The press is the consent to record. Open the microphone
                        // while the provider handshake is in flight and buffer PCM
                        // until it is configured, so the first words are never lost.
                        let connect = tokio::spawn({
                            let (c, target) = (c.clone(), target.clone());
                            async move { target.connect(&c).await }
                        });
                        let setup_cancel = Arc::new(AtomicBool::new(false));
                        let factory_cancel = setup_cancel.clone();
                        let (mic, pcm) = tokio::select! {
                            biased;
                            _ = super::nari::cancelled(&c) => { setup_cancel.store(true, Ordering::SeqCst); return Err(VoiceError::Cancelled); },
                            result = tokio::task::spawn_blocking(move || factory(factory_cancel)) => result.map_err(|_|VoiceError::CaptureFailed)??,
                        };
                        l.set(mic.level.clone()).map_err(|_| VoiceError::CaptureFailed)?;
                        super::timing::mark("microphone capturing");
                        // Release reaches the microphone directly, even while still
                        // connecting. Buffered audio then drains and commits once
                        // the session is up.
                        {
                            let mut slot = m.lock().map_err(|_| VoiceError::CaptureFailed)?;
                            let handle = mic.stop_handle();
                            if s.load(Ordering::SeqCst) {
                                handle.stop();
                            }
                            *slot = Some(handle);
                        }
                        // Dropping `mic` on failure cancels the unfinished capture.
                        let session = connect.await.map_err(|_| VoiceError::CaptureFailed)??;
                        let mut sink = |event: NariEvent| {
                            if !matches!(event, NariEvent::Finished(_)) {
                                Events::lock(&e).push(event);
                            }
                        };
                        sink(NariEvent::Started);
                        let streamed =
                            stream_resilient(session, &target, &c, pcm, tuning, &mut sink).await;
                        // Words already captured outlive any provider failure.
                        let mut reported = false;
                        let result = match streamed {
                            Err(error) => Err(error),
                            Ok(Streamed { text, error: None }) => Ok(text),
                            Ok(Streamed { text, error: Some(error) }) if text.trim().is_empty() => {
                                Err(error)
                            }
                            Ok(Streamed { text, error: Some(error) }) => {
                                sink(NariEvent::Interrupted(error));
                                reported = true;
                                Ok(text)
                            }
                        };
                        super::timing::mark("nari stream finished");
                        mic.stop();
                        // Native stream is owned by its capture thread, never the UI.
                        let capture = mic.finish();
                        super::timing::mark("capture joined");
                        match (result, capture) {
                            (Err(err), _) => Err(err),
                            // A microphone fault after speech must not discard the words.
                            (Ok(text), Err(err)) if !text.trim().is_empty() => {
                                // The provider failure came first and is the cause: the
                                // capture only failed because nothing was reading it.
                                if !reported {
                                    sink(NariEvent::Interrupted(err));
                                }
                                Ok(text)
                            }
                            (Ok(_), Err(err)) => Err(err),
                            (Ok(text), Ok(())) => Ok(text),
                        }
                    });
                    runtime.shutdown_background();
                    super::timing::mark("runtime shut down");
                    result
                })();
                Events::lock(&e).push(NariEvent::Finished(result));
                super::timing::mark("finished event published");
            })
            .map_err(|_| VoiceError::CaptureFailed)?;
        Ok(Self {
            cancel,
            stop,
            events,
            worker,
            level,
            mic_stop,
        })
    }
    pub fn stop(&self) {
        let slot = self.mic_stop.lock();
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(slot) = slot
            && let Some(mic) = slot.as_ref()
        {
            mic.stop();
        }
    }
    /// Resolves when an event may be available (or immediately if one arrived
    /// since the last wait). Lets a UI react to the final transcript at once
    /// instead of on its next poll. Executor-agnostic and `'static`.
    pub fn event_ready(&self) -> impl std::future::Future<Output = ()> + Send + 'static + use<> {
        let ready = self.event_notify();
        async move { ready.notified().await }
    }
    /// Shared wakeup behind [`Self::event_ready`], for a long-lived waker task
    /// that does not hold the recording itself. Stores a permit when nobody
    /// waits, so an event is never missed.
    pub fn event_notify(&self) -> Arc<tokio::sync::Notify> {
        Events::lock(&self.events).ready.clone()
    }
    /// Latest microphone callback's linear RMS of the downmixed mono signal,
    /// before resampling, normalized to `0.0..=1.0` (not decibels).
    /// Silence is zero. No smoothing or artificial animation is applied.
    /// Returns zero after stop, cancellation, or completion. Polling is lock-free
    /// and does not consume events or audio. Requires the `voice-capture` feature.
    pub fn audio_level(&self) -> f32 {
        if self.stop.load(Ordering::SeqCst)
            || self.cancel.load(Ordering::SeqCst)
            || self.is_finished()
        {
            return 0.0;
        }
        self.level
            .get()
            .map_or(0.0, |level| f32::from_bits(level.load(Ordering::Relaxed)))
    }
    pub fn try_event(&self) -> Option<NariEvent> {
        Events::lock(&self.events).queue.pop_front()
    }
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }
}
impl Drop for NariRecording {
    fn drop(&mut self) {
        if !self.events.lock().is_ok_and(|events| events.finished) && !self.worker.is_finished() {
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.stop();
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod long_dictation_tests;
