//! Continuous voice remains client-side. Only gated requests become user turns.
mod dispatch;
mod history_control;
use super::jarvis_status::CaptureStatus;
use super::*;
use crate::tui::JarvisCaption;
use crate::tui::backend::RemoteConnection;
use jcode_voice::continuous::{ContinuousConfig, ContinuousEvent, ContinuousRecording, VoiceState};
use serde_json::json;
use std::collections::VecDeque;

/// Audible cue that speech reached Jarvis (gate or Fn tap). Fire and forget.
fn play_trigger_sound() -> Option<std::process::Child> {
    let sound = crate::config::config()
        .jarvis
        .listening
        .trigger_sound
        .clone();
    if sound.is_empty() || !std::path::Path::new(&sound).is_file() {
        return None;
    }
    for player in ["pw-play", "paplay"] {
        if let Ok(child) = std::process::Command::new(player)
            .arg(&sound)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            jcode_voice::timing::mark("trigger sound started");
            return Some(child);
        }
    }
    None
}

fn envelope_trigger(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        background_transcript: Vec<String>,
        trigger: String,
        #[serde(default)]
        #[allow(dead_code)]
        still_speaking: bool,
    }
    if text.len() > 128 * 1024 {
        return None;
    }
    // Ordinary chat text is not an envelope and is displayed unchanged.
    let Ok(e) = serde_json::from_str::<Envelope>(text) else {
        return None;
    };
    (e.background_transcript.len() <= 16
        && e.background_transcript
            .iter()
            .all(|s| s.chars().count() <= 1024)
        && !e.trigger.trim().is_empty()
        && !e.trigger.contains('\0')
        && e.trigger.len() <= 4096)
        .then_some(e.trigger)
}

fn recent_chat(messages: &[DisplayMessage]) -> Vec<(String, String)> {
    let mut chat: Vec<_> = messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant") && !m.content.trim().is_empty())
        .filter_map(|m| {
            let visible;
            let text = if m.role == "assistant" {
                visible = super::input::strip_reasoning_lines(&m.content);
                visible.as_str()
            } else {
                m.content.as_str()
            };
            if text.trim().is_empty() {
                return None;
            }
            let (text, _) = jcode_session_types::strip_transcription(text);
            let trigger = m.is_jarvis_user().then(|| m.user_row_text(false));
            Some((
                m.role.clone(),
                trigger
                    .as_deref()
                    .unwrap_or(&text)
                    .chars()
                    .take(1024)
                    .collect(),
            ))
        })
        .take(8)
        .collect();
    chat.reverse();
    chat
}

fn turn_retry_after(turns: &VecDeque<Instant>, limit: u32, continuation: bool) -> Option<Duration> {
    if continuation || limit == 0 {
        return None;
    }
    let oldest_blocking = turns.get(turns.len().checked_sub(limit as usize)?)?;
    Some(Duration::from_secs(3600).saturating_sub(oldest_blocking.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_turn_limit_applies_only_to_new_requests() {
        let turns = VecDeque::from([Instant::now() - Duration::from_secs(3500)]);
        assert_eq!(turn_retry_after(&turns, 0, false), None);
        assert_eq!(turn_retry_after(&turns, 2, false), None);
        assert_eq!(turn_retry_after(&turns, 1, true), None);
        let delay = turn_retry_after(&turns, 1, false).unwrap();
        assert!((99..=100).contains(&delay.as_secs()));
    }

    #[test]
    fn spoken_cue_waits_for_final_and_yields_to_ready_reply() {
        let mut playback = DeferredPlayback::default();
        playback.trigger(1, r#"{"still_speaking":true,"trigger":"open"}"#);
        playback.prepare_spoken_cue();
        assert!(playback.speech.is_none());
        playback.finalize(1, "open it");
        playback.prepare_spoken_cue();
        assert_eq!(playback.speech.take().unwrap().0, "On it");
        playback.prepare_spoken_cue();
        assert!(playback.speech.is_none());
        playback.trigger(2, "finished request");
        playback.speech = Some(("Already done".into(), Instant::now()));
        playback.prepare_spoken_cue();
        assert_eq!(playback.speech.take().unwrap().0, "Already done");
        assert!(!playback.ding);
    }

    #[test]
    fn deferred_playback_typed_reply_survives_capture_failure() {
        let mut listener = AlwaysListening::default();
        listener.defer_speech("typed reply".into());
        listener.capture_failure(None);
        assert_eq!(listener.playback.speech.as_ref().unwrap().0, "typed reply");
    }

    #[test]
    fn deferred_playback_only_matching_final_releases_or_revises() {
        let mut listener = AlwaysListening::default();
        let partial = r#"{"trigger":"open it","still_speaking":true}"#;
        listener.playback.trigger(5, partial);
        assert!(!listener.playback.ding, "no cue while still speaking");
        listener.defer_speech("opening".into());
        listener.playback.finalize(4, "different turn");
        assert!(!listener.playback.ding);
        assert!(listener.playback.partial.is_some() && listener.playback.speech.is_some());
        listener.playback.finalize(5, "open it tomorrow");
        assert!(listener.playback.ding, "cue plays once speech ends");
        assert!(listener.playback.partial.is_none() && listener.playback.speech.is_none());
        listener.playback.trigger(6, partial);
        listener.defer_speech("opening now".into());
        listener.playback.finalize(6, "open it");
        assert!(listener.playback.partial.is_none() && listener.playback.speech.is_some());
        listener.detach();
        assert!(!listener.playback.ding && listener.playback.speech.is_none());
    }

    #[test]
    fn caption_is_bounded_terminal_safe_and_never_queued() {
        let mut listener = AlwaysListening::default();
        listener.update_caption(1, "\x1b[31mhello\x1b[0m\nworld\0\u{202e}");
        assert_eq!(
            listener.captions.front().map(|r| r.text.as_str()),
            Some("hello world")
        );
        listener.update_caption(2, &"界".repeat(500));
        assert_eq!(listener.captions.back().unwrap().text.chars().count(), 160);
        assert!(listener.dispatch.pop().is_none());
        listener.detach();
        assert!(listener.captions.is_empty());
    }

    #[test]
    fn final_caption_revises_only_matching_visible_row() {
        let mut listener = AlwaysListening::default();
        listener.update_caption(1, "turn");
        listener.score_caption(1, 0.98, true);
        listener.update_caption(2, "ambient");
        listener.update_caption(1, "turn off");
        listener.finalize_caption(9, "dropped");
        listener.finalize_caption(1, "turn on");
        listener.finalize_caption(1, "turn on");
        assert_eq!(listener.captions.len(), 2);
        assert_eq!(listener.captions[0].text, "turn on");
        assert_eq!(listener.captions[0].score, Some((0.98, true)));
        assert_eq!(listener.captions[1].text, "ambient");
    }

    #[test]
    fn caption_scores_follow_ids_not_repeated_text_or_position() {
        let mut listener = AlwaysListening::default();
        for id in 1..=5 {
            listener.update_caption(id, "again");
        }
        assert!(!listener.score_caption(1, 0.99, true));
        assert!(listener.score_caption(4, 0.99, true));
        assert!(listener.score_caption(2, 0.25, false));
        let scores: Vec<_> = listener.captions.iter().map(|r| r.score).collect();
        assert_eq!(
            scores,
            [Some((0.25, false)), None, Some((0.99, true)), None]
        );
    }

    #[test]
    fn context_only_includes_visible_dialogue_and_unwraps_voice() {
        let rows = [
            DisplayMessage::system("secret"),
            DisplayMessage::jarvis_user(
                r#"{"background_transcript":["private"],"trigger":"question"}"#,
            ),
            DisplayMessage::assistant(format!(
                "{}\nreply",
                jcode_tui_markdown::reasoning_line_markup("secret")
            )),
        ];
        assert_eq!(
            recent_chat(&rows),
            vec![
                ("user".into(), "question".into()),
                ("assistant".into(), "reply".into())
            ]
        );
    }

    #[test]
    fn hard_error_explains_deliberate_voice_retry_without_replaying_or_resetting_turns() {
        let mut app = super::super::tests::create_test_app();
        let listener = &mut app.jarvis_listener;
        listener.initialized = true;
        listener.turns.push_back(Instant::now());
        listener.update_caption(7, "private caption");
        listener.playback.trigger(7, "private caption");
        let notice = listener
            .capture_failure(Some(&jcode_voice::VoiceError::NariCreditsExhausted))
            .unwrap();
        assert!(notice.contains("Add credits") && notice.contains("/voice"));
        assert!(!notice.contains("private"));
        assert!(listener.failed && listener.initialized);
        assert!(listener.capture_failure(None).is_none()); // subsequent Stopped must not mask the cause
        app.toggle_always_listening();
        assert!(app.status_notice.as_ref().unwrap().0.contains("Retrying"));
        let listener = &app.jarvis_listener;
        assert!(!listener.initialized && !listener.failed);
        assert_eq!(listener.turns.len(), 1);
        assert!(listener.captions.is_empty() && !listener.playback.ding);
    }

    #[test]
    fn trigger_expires_without_overriding_pause_or_failure() {
        let now = Instant::now();
        let mut listener = AlwaysListening::default();
        listener.triggered_at = Some(now);
        listener.state = Some(VoiceState::Listening);
        assert_eq!(listener.indicator_state(false, true, now), "triggered");
        assert_eq!(listener.indicator_state(true, false, now), "paused");
        assert_eq!(
            listener.indicator_state(false, false, now + Duration::from_secs(3)),
            "listening"
        );
        listener.failed = true;
        assert_eq!(listener.indicator_state(true, false, now), "error");
    }
}

fn caption_text(text: &str) -> String {
    let plain = crate::message::strip_ansi_escape_sequences(text);
    let text: String = plain.chars().filter_map(|c| {
        if c.is_whitespace() { Some(' ') } else if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') { None } else { Some(c) }
    }).take(160).collect();
    text.trim().to_owned()
}

#[derive(Default)]
pub(super) struct AlwaysListening {
    playback: DeferredPlayback,
    history: history_control::HistoryControl,
    history_command: Option<String>,
    history_request: Option<u64>,
    initialized: bool,
    recording: Option<ContinuousRecording>,
    status: Option<CaptureStatus>,
    wake: Option<tokio::task::JoinHandle<()>>,
    session: Option<String>,
    dispatch: dispatch::Dispatch,
    turns: VecDeque<Instant>,
    state: Option<VoiceState>,
    failed: bool,
    captions: VecDeque<JarvisCaption>,
    chat_version: Option<u64>,
    triggered_at: Option<Instant>,
}
#[derive(Default)]
struct DeferredPlayback {
    partial: Option<(u64, String, Instant)>,
    cue_player: Option<(std::process::Child, Instant)>,
    latest: Option<u64>,
    speech: Option<(String, Instant)>,
    ding: bool,
}

impl DeferredPlayback {
    fn prepare_spoken_cue(&mut self) {
        if std::mem::take(&mut self.ding) && self.speech.is_none() {
            self.speech = Some(("On it".into(), Instant::now()));
        }
    }

    fn trigger(&mut self, id: u64, text: &str) {
        self.latest = Some(id);
        self.speech = None;
        self.partial = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(env) if env["still_speaking"].as_bool() == Some(true) => Some((
                id,
                env["trigger"].as_str().unwrap_or("").trim().to_owned(),
                Instant::now(),
            )),
            _ => None,
        };
        // No cue while the user is still talking: it plays on finalize.
        self.ding = self.partial.is_none();
    }

    fn finalize(&mut self, id: u64, text: &str) {
        let Some((_, partial, _)) = self.partial.take_if(|(old, _, _)| *old == id) else {
            return;
        };
        self.ding = true;
        if partial != text.trim() {
            self.speech = None;
        }
    }
}
fn stop_cue(child: &mut std::process::Child) {
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        crate::logging::warn(&format!("Could not stop voice cue: {error}"));
    }
}
impl Drop for DeferredPlayback {
    fn drop(&mut self) {
        if let Some((mut child, _)) = self.cue_player.take() {
            stop_cue(&mut child);
            if let Err(error) = child.wait() {
                crate::logging::warn(&format!("Could not reap voice cue: {error}"));
            }
        }
    }
}
impl Drop for AlwaysListening {
    fn drop(&mut self) {
        if let Some(task) = self.wake.take() {
            task.abort();
        }
    }
}
impl AlwaysListening {
    pub(super) fn queue_history(&mut self, cmd: &str) -> &'static str {
        if self.recording.is_none() || self.session.is_none() {
            return "Always-on Jarvis is not ready";
        }
        self.history_command = Some(
            if cmd.trim().is_empty() {
                "status"
            } else {
                cmd.trim()
            }
            .into(),
        );
        "Local audio history command queued"
    }
    fn playback_trace(&self) -> jcode_voice::speech::PlaybackTrace {
        jcode_voice::speech::PlaybackTrace {
            session: self.session.clone().unwrap_or_else(|| "none".into()),
            utterance: self.playback.latest.unwrap_or(0),
        }
    }
    pub(super) fn defer_speech(&mut self, text: String) {
        self.playback_trace().mark("deferred", 0);
        self.playback.speech = Some((text, Instant::now()));
        jcode_voice::timing::mark("speech callback deferred");
    }

    pub(super) fn clear_playback(&mut self) {
        self.playback = DeferredPlayback::default();
    }
    pub(super) fn captions(&self) -> &VecDeque<JarvisCaption> {
        &self.captions
    }

    fn update_caption(&mut self, id: u64, text: &str) {
        let text = caption_text(text);
        if text.is_empty() {
            return;
        }
        if let Some(row) = self.captions.iter_mut().find(|r| r.id == id) {
            // Streaming revision of the same utterance: grow in place.
            row.text = text;
            return;
        }
        if self.captions.len() == 4 {
            self.captions.pop_front();
        }
        self.captions.push_back(JarvisCaption {
            id,
            text,
            score: None,
        });
    }

    fn finalize_caption(&mut self, id: u64, text: &str) {
        // A late final must not resurrect an evicted or suppressed row.
        if self.captions.iter().any(|r| r.id == id) {
            self.update_caption(id, text);
        }
    }

    fn score_caption(&mut self, id: u64, probability: f64, accepted: bool) -> bool {
        let Some(row) = self.captions.iter_mut().find(|r| r.id == id) else {
            return false;
        };
        row.score = Some((probability, accepted));
        true
    }

    fn indicator_state(&self, paused: bool, speech_busy: bool, now: Instant) -> &'static str {
        if self.failed {
            "error"
        } else if paused {
            "paused"
        } else if self
            .triggered_at
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(3))
        {
            "triggered"
        } else if speech_busy {
            "speech suppressed"
        } else {
            match self.state {
                Some(VoiceState::Transcribing) => "transcribing",
                Some(VoiceState::Speaking) => "hearing speech",
                Some(VoiceState::Listening) => "listening",
                _ => "starting",
            }
        }
    }

    fn trace(&self, id: u64, stage: &str) {
        dispatch::trace(self.session.as_deref(), id, stage);
    }
    fn clear_dispatch(&mut self, reason: &str) {
        let session = self.session.as_deref();
        self.dispatch
            .clear(|id| dispatch::trace(session, id, reason));
    }
    fn capture_failure(&mut self, error: Option<&jcode_voice::VoiceError>) -> Option<String> {
        if self.failed {
            return None;
        }
        self.failed = true;
        if let Some(status) = &mut self.status {
            status.set_failure(error);
        }
        self.clear_dispatch("capture_failed");
        if self.playback.latest.is_some() {
            self.clear_playback();
        }
        self.captions.clear();
        self.triggered_at = None;
        let reason = error
            .map(ToString::to_string)
            .unwrap_or_else(|| "Capture stopped".into());
        Some(format!(
            "Always-on Jarvis stopped: {reason}. After resolving the issue, use /voice to retry."
        ))
    }

    fn request_retry(&mut self) -> &'static str {
        if self.recording.as_ref().is_some_and(|r| !r.is_finished()) {
            return "Microphone is still stopping. Use /voice again once stopped.";
        }
        self.detach();
        self.recording = None;
        self.status = None;
        if let Some(task) = self.wake.take() {
            task.abort();
        }
        self.state = None;
        self.failed = false;
        self.initialized = false;
        Bus::global().publish(BusEvent::VoiceInputWake);
        "Retrying always-on Jarvis. Previous speech will not be replayed."
    }

    fn initialize(&mut self) -> anyhow::Result<()> {
        self.initialized = true;
        self.status = Some(CaptureStatus::new()?);
        self.wake = Some(tokio::spawn(async {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tick.tick().await;
                Bus::global().publish(BusEvent::VoiceInputWake);
            }
        }));
        let cfg = &crate::config::config().jarvis.listening;
        anyhow::ensure!(
            cfg.max_agent_turns_per_hour <= 120
                && cfg.action_probability.is_finite()
                && (0.5..=1.0).contains(&cfg.action_probability),
            "Invalid Jarvis listening limits: agent turns 0 (unlimited) or 1..120/hour, probability 0.5..1"
        );
        // Always-on listening streams to Nari only. Another selected backend
        // is an error, not a silent switch to Nari.
        anyhow::ensure!(
            matches!(
                crate::config::config().dictation.backend,
                jcode_voice::DictationBackend::Auto | jcode_voice::DictationBackend::Nari
            ),
            "Always-on listening needs the Nari backend, but `[dictation] backend` is {}. \
             Select Nari (or auto) to use it.",
            jcode_voice::configured_label()
        );
        let key = jcode_voice::nari_api_key().ok_or(jcode_voice::VoiceError::NariNotConfigured)?;
        // Resolve before opening the microphone. A missing gate must not turn
        // continuous speech into unchecked agent requests.
        crate::jev::JevClient::for_voice()?;
        let recording = ContinuousRecording::start_auto_with_restoration(
            Arc::new(AtomicBool::new(false)),
            &key,
            &crate::config::config().dictation.recorder,
            ContinuousConfig {
                require_jev: false,
                action_probability: cfg.action_probability,
                live_action_probability: Some(|| {
                    crate::config::config().jarvis.listening.action_probability
                }),
                warm_nari: cfg.warm_nari,
            },
            || Box::pin(crate::network_retry::wait_for_connectivity_restoration()),
        )?;
        self.recording = Some(recording);
        Ok(())
    }

    pub(super) fn suppress_speech(&self) {
        if let Some(recording) = &self.recording {
            recording.set_suppressed(true);
        }
    }

    pub(super) fn detach(&mut self) {
        self.clear_playback();
        self.captions.clear();
        self.clear_dispatch("detached");
        self.chat_version = None;
        self.triggered_at = None;
        self.session = None;
        self.history = Default::default();
        self.history_command = None;
        self.history_request = None;
        if let Some(recording) = &self.recording {
            recording.set_suppressed(true);
            recording.set_paused(true);
            recording.reset_audio_history(Some(false));
        }
    }
}

/// What the always-on listener needs from the app that hosts it.
pub(super) trait ListenerHost {
    fn notice(&mut self, text: String);
    fn system_message(&mut self, text: String);
    fn error_message(&mut self, text: String);
    fn speech_player(&self) -> Option<&jcode_voice::speech::SpeechPlayer>;
    fn external_voice(&self) -> Option<&(crate::protocol::VoiceActivity, Instant)>;
    fn voice_input_active(&self) -> bool;
    fn chat_version(&self) -> u64;
    fn recent_chat(&self) -> Vec<(String, String)>;
}

impl ListenerHost for App {
    fn notice(&mut self, text: String) {
        self.set_status_notice(text);
    }

    fn system_message(&mut self, text: String) {
        self.push_display_message(DisplayMessage::system(text));
    }

    fn error_message(&mut self, text: String) {
        self.push_display_message(DisplayMessage::error(text));
    }

    fn speech_player(&self) -> Option<&jcode_voice::speech::SpeechPlayer> {
        self.jarvis_speech.as_ref()
    }

    fn external_voice(&self) -> Option<&(crate::protocol::VoiceActivity, Instant)> {
        self.external_voice.as_ref()
    }

    fn voice_input_active(&self) -> bool {
        self.voice_input.is_some()
    }

    fn chat_version(&self) -> u64 {
        self.display_messages_version
    }

    fn recent_chat(&self) -> Vec<(String, String)> {
        recent_chat(&self.display_messages)
    }
}

/// What one capture pass learned that the rest of the tick depends on.
struct Tick {
    changed: bool,
    paused: bool,
    speech_busy: bool,
    external_capture: bool,
}

impl AlwaysListening {
    /// Flip the microphone pause, or ask a failed capture to retry. Returns the
    /// status notice, or the error to show when the toggle failed.
    fn toggle_pause(&mut self) -> Result<&'static str, String> {
        if self.failed {
            return Ok(self.request_retry());
        }
        let Some(status) = &self.status else {
            return Ok("Always-on voice is not ready. Check the startup error.");
        };
        let paused = !status.paused();
        match status.set_paused(paused) {
            Ok(()) => Ok(if paused {
                "Jarvis microphone paused"
            } else {
                "Jarvis microphone resuming"
            }),
            Err(error) => Err(format!("Could not toggle Jarvis microphone: {error}")),
        }
    }

    /// Retry a failed start, follow the session, drain capture events and
    /// publish the indicator.
    fn pump_capture(&mut self, host: &mut impl ListenerHost, session: Option<String>) -> Tick {
        let mut changed = false;
        if self.failed && self.status.as_ref().is_some_and(CaptureStatus::take_retry) {
            host.notice(self.request_retry().into());
            changed = true;
        }
        if !self.initialized {
            match self.initialize() {
                Ok(()) => host.system_message(
                    "Always-on Jarvis: local speech detection, Nari transcription, and Jev request gating. Recent speech and visible chat are sent to Jev. Only accepted requests and their preceding speech are sent to the agent. Use /voice or click the Waybar microphone to pause. Speech audio is sent to Nari; request candidates are sent to Jev.".into()),
                Err(error) => {
                    self.failed = true;
                    if let Some(status) = &mut self.status {
                        status.set_failure(error.downcast_ref::<jcode_voice::VoiceError>());
                    }
                    host.error_message(format!("Always-on Jarvis did not start: {error}. After resolving the issue, use /voice to retry."));
                }
            }
            changed = true;
        }
        if self.session != session {
            self.detach();
            self.session = session;
            #[cfg(unix)]
            if let Some(status) = &self.status {
                self.history.attach(status.private_directory());
            }
        }
        let paused = self.status.as_ref().is_some_and(CaptureStatus::paused);
        let cue_busy = self.playback.cue_player.as_mut().is_some_and(|(child, at)| {
            if at.elapsed() > Duration::from_secs(3) {
                stop_cue(child);
            }
            matches!(child.try_wait(), Ok(None))
        });
        if !cue_busy {
            self.playback.cue_player = None;
        }
        let mut speech_busy = cue_busy
            || host
                .speech_player()
                .is_some_and(|player| player.owns_audio_output());
        let external_capture = host.external_voice().is_some_and(|(activity, at)| {
            matches!(activity, crate::protocol::VoiceActivity::Recording { .. })
                && at.elapsed() <= Duration::from_secs(3)
        }) || self
            .status
            .as_ref()
            .is_some_and(CaptureStatus::external_dictation_active);
        let mut indicator_failed = false;
        let recording = self.recording.take();
        if self.failed
            && recording.is_none()
            && let Some(status) = &mut self.status
        {
            // Setup can fail before a worker exists. Still heartbeat the error indicator
            // so its owner can explicitly retry without opening a microphone on its own.
            if status.publish(false, "error", 0.0, json!({})).is_err() {
                indicator_failed = true;
            }
        }
        if let Some(recording) = &recording {
            if let Some(cmd) = self
                .history_command
                .take()
                .or_else(|| self.history.incoming())
            {
                let blocked = speech_busy
                    || external_capture
                    || self.failed
                    || self.playback.partial.is_some()
                    || host.voice_input_active();
                let notice = self
                    .history
                    .run(recording, &cmd, self.history_request, blocked);
                host.notice(notice);
            }
            speech_busy |= self.history.busy();
            recording.set_paused(paused || self.failed);
            // Fn tap (companion): act on what was just heard.
            if self.status.as_ref().is_some_and(CaptureStatus::take_wake) {
                if paused || self.failed {
                    // Say why the tap did nothing instead of dropping it.
                    host.notice("Jarvis is paused; Fn tap ignored".into());
                } else if recording.manual_trigger() {
                    host.notice("Jarvis: sending what it just heard".into());
                } else {
                    host.notice("Jarvis hasn't heard anything recently".into());
                }
                changed = true;
            }
            if !paused && !self.failed && self.chat_version != Some(host.chat_version()) {
                recording.set_chat_context(host.recent_chat());
                self.chat_version = Some(host.chat_version());
            }
            recording.set_suppressed(speech_busy || external_capture || paused || self.failed);
            for _ in 0..32 {
                let Some(event) = recording.try_event() else {
                    break;
                };
                match event {
                    ContinuousEvent::State(state) => {
                        self.state = Some(state);
                    }
                    ContinuousEvent::Level(_) => {}
                    ContinuousEvent::Caption { id, text } => {
                        if !paused && !self.failed && !speech_busy && !external_capture {
                            self.update_caption(id, &text);
                            changed = true;
                        }
                    }
                    ContinuousEvent::CaptionScore {
                        id,
                        probability,
                        accepted,
                    } => {
                        changed |= self.score_caption(id, probability, accepted);
                    }
                    ContinuousEvent::Recovering { error, delay } => {
                        self.clear_dispatch("capture_recovering");
                        if self.playback.latest.is_some() {
                            self.clear_playback();
                            if let Some(player) = host.speech_player() {
                                player.cancel();
                            }
                        }
                        host.notice(format!("{error}. Microphone released for {}s. Repeat the request when listening resumes.", delay.as_secs()));
                        changed = true;
                    }
                    ContinuousEvent::Rejected(_) => {} // Never save ambient speech in chat or logs.
                    ContinuousEvent::Transcript { id, text }
                        if !paused && !self.failed && !speech_busy && !external_capture =>
                    {
                        self.triggered_at = Some(Instant::now());
                        let outcome = self.dispatch.enqueue(id, text.clone());
                        if outcome == "queued" {
                            if let Some(player) = host.speech_player() {
                                player.cancel();
                            }
                            self.history_request = Some(id);
                            self.playback.trigger(id, &text);
                        }
                        self.trace(id, outcome);
                        changed = true;
                    }
                    ContinuousEvent::Transcript { id, .. } => self.trace(id, "suppressed"),
                    ContinuousEvent::Continuation { id, text }
                        if !paused && !self.failed && !external_capture =>
                    {
                        self.finalize_caption(id, &text);
                        self.playback.finalize(id, &text);
                        let outcome = self.dispatch.finalize(id, text);
                        self.trace(id, outcome);
                        changed = true;
                    }
                    ContinuousEvent::Continuation { id, .. } => self.trace(id, "final_suppressed"),
                    ContinuousEvent::Error(error) => {
                        let voice_reply = self.playback.latest.is_some();
                        if let Some(notice) = self.capture_failure(Some(&error)) {
                            if voice_reply && let Some(player) = host.speech_player() {
                                player.cancel();
                            }
                            host.error_message(notice);
                        }
                        changed = true;
                    }
                    ContinuousEvent::Stopped => {
                        let voice_reply = self.playback.latest.is_some();
                        if let Some(notice) = self.capture_failure(None) {
                            if voice_reply && let Some(player) = host.speech_player() {
                                player.cancel();
                            }
                            host.error_message(notice);
                            changed = true;
                        }
                    }
                }
            }
            let state = self.indicator_state(paused, speech_busy, Instant::now());
            // Companion push-to-talk (Fn or Copilot) owns the microphone for
            // this session while Jarvis's own capture is suppressed. Show its
            // live level so the Waybar waveform keeps tracking the voice.
            let push_to_talk_level = external_capture
                .then(|| match host.external_voice() {
                    Some((crate::protocol::VoiceActivity::Recording { levels, .. }, _)) => {
                        Some(levels.last().copied().unwrap_or(0.0))
                    }
                    _ => None,
                })
                .flatten();
            let state = if push_to_talk_level.is_some() && !self.failed {
                "push to talk"
            } else {
                state
            };
            if let Some(status) = &mut self.status {
                let capturing = recording.is_capturing() || push_to_talk_level.is_some();
                if let Err(error) = status.publish(
                    capturing,
                    state,
                    recording
                        .audio_level()
                        .max(push_to_talk_level.unwrap_or(0.0)),
                    json!({"agent_turns_last_hour":self.turns.len(),"audio_history":self.history.info(recording)}),
                ) {
                    recording.stop();
                    self.failed = true;
                    indicator_failed = true;
                    host.error_message(format!(
                        "Capture indicator failed; microphone paused: {error}. Use /voice to retry."
                    ));
                    changed = true;
                }
            }
        }
        if paused || self.failed {
            changed |= !self.captions.is_empty();
            self.captions.clear();
            self.clear_dispatch("paused_or_failed");
            self.chat_version = None;
        }
        self.recording = recording;
        if indicator_failed {
            self.status = None;
        }
        let sid = self.session.as_deref();
        self.dispatch
            .expire(|id| dispatch::trace(sid, id, "expired"));
        while self
            .turns
            .front()
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(3600))
        {
            self.turns.pop_front();
        }
        Tick {
            changed,
            paused,
            speech_busy,
            external_capture,
        }
    }

    /// The next gated voice request, unless speech, a pause or a failed capture
    /// holds it back.
    fn next_request(&mut self, tick: &Tick) -> Option<dispatch::Request> {
        if tick.speech_busy || tick.paused || self.failed {
            return None;
        }
        self.dispatch.pop()
    }

    /// The hourly agent-turn cap and the seconds until a turn frees up, when
    /// `request` would exceed it.
    fn hourly_cap_wait(&self, request: &dispatch::Request) -> Option<(u32, u64)> {
        let limit = crate::config::config()
            .jarvis
            .listening
            .max_agent_turns_per_hour;
        let delay = turn_retry_after(&self.turns, limit, request.continuation)?;
        self.trace(request.id, "hourly_limit");
        Some((limit, delay.as_secs_f64().ceil() as u64))
    }

    /// Record how sending `request` went (`sent_as` is the wire request id).
    fn finish_dispatch(&mut self, request: &dispatch::Request, sent_as: Option<u64>, interleave: bool) {
        let stage = if sent_as.is_none() {
            "send_failed"
        } else {
            if !request.continuation {
                self.turns.push_back(Instant::now());
            }
            self.dispatch.sent(request);
            if interleave {
                "interleave_sent"
            } else {
                "sent"
            }
        };
        crate::logging::info(&format!(
            "JARVIS_DISPATCH session={} utterance={} stage={} monotonic_ms={} request_id={sent_as:?} queue_ms={} provisional={} final_update={}",
            self.session.as_deref().unwrap_or("none"),
            request.id,
            stage,
            jcode_voice::timing::monotonic_ms(),
            request.at.elapsed().as_millis(),
            request.provisional,
            request.continuation
        ));
    }

    /// Release or start spoken replies and the trigger cue once the microphone
    /// is quiet. Returns whether the screen changed.
    fn drive_playback(&mut self, host: &mut impl ListenerHost, tick: &Tick) -> bool {
        let mut changed = false;
        let orphaned_partial = self
            .playback
            .partial
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() >= Duration::from_secs(180));
        let disabled = tick.paused || self.failed;
        if (disabled && self.playback.latest.is_some())
            || tick.external_capture
            || host.voice_input_active()
            || orphaned_partial
        {
            if let Some(player) = host.speech_player() {
                player.cancel();
            }
            self.clear_playback();
        } else if !tick.speech_busy && self.playback.partial.is_none() {
            if self.playback.ding && crate::tui::jarvis::handterm_spoken_cue() {
                self.playback.prepare_spoken_cue();
            }
            let claim = || {
                if disabled {
                    return !self
                        .recording
                        .as_ref()
                        .is_some_and(ContinuousRecording::is_capturing);
                }
                self.state == Some(VoiceState::Listening)
                    && self
                        .recording
                        .as_ref()
                        .is_some_and(ContinuousRecording::suppress_if_quiet)
            };
            if let Some((text, at)) = self
                .playback
                .speech
                .take()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(30))
            {
                if let Some(player) = host.speech_player() {
                    player.cancel(); // replace any synthesized but not yet authorized reply
                    self.playback.ding = false;
                    let trace = self.playback_trace();
                    trace.mark("enqueue", at.elapsed().as_millis());
                    if let Err(error) = player.enqueue_gated(&text, trace) {
                        host.error_message(error.to_string());
                        changed = true;
                    }
                }
            } else if host
                .speech_player()
                .is_some_and(|p| p.authorize_ready(claim))
            {
                self.playback_trace().mark("quiet_authorized", 0);
            } else if self.playback.ding
                && host.speech_player().is_none_or(|p| p.is_idle())
                && claim()
            {
                self.playback.ding = false;
                self.playback.cue_player = play_trigger_sound().map(|c| {
                    self.playback_trace().mark("ding_started", 0);
                    (c, Instant::now())
                });
            }
        }
        changed
    }
}

impl App {
    pub(super) fn toggle_always_listening(&mut self) {
        match self.jarvis_listener.toggle_pause() {
            Ok(notice) => self.set_status_notice(notice),
            Err(message) => self.push_display_message(DisplayMessage::error(message)),
        }
    }

    pub(super) async fn poll_jarvis_listening(&mut self, remote: &mut RemoteConnection) -> bool {
        if !crate::tui::jarvis::always_on() {
            return false;
        }
        if !remote.jarvis_tools_configured
            || !remote.has_loaded_history()
            || self.remote_session_id.is_none()
        {
            self.jarvis_listener.detach();
            return false;
        }
        let mut listener = std::mem::take(&mut self.jarvis_listener);
        let session = self.remote_session_id.clone();
        let tick = listener.pump_capture(self, session);
        let mut changed = tick.changed;
        if self.pending_login.is_none()
            && !self.cloud_handoff_blocks_work()
            && let Some(req) = listener.next_request(&tick)
        {
            if let Some((limit, wait_secs)) = listener.hourly_cap_wait(&req) {
                self.set_status_notice(format!(
                    "Jarvis voice request not sent: {limit}-turn hourly cap reached. Try again in {wait_secs}s."
                ));
            } else {
                let interleave = self.is_processing;
                let result = dispatch::submit(self, remote, &req.text, interleave).await;
                // The finished utterance must not wait for the turn's next safe
                // point: cancel the provisional turn. The server keeps the queued
                // interrupt and runs it as the next turn (Esc-redirect path).
                let result = match result {
                    Ok(id) if interleave && req.continuation => remote
                        .cancel_with_reason("jarvis_final_transcript")
                        .await
                        .map(|_| {
                            self.remote_interrupt_ack_deadline =
                                Some(Instant::now() + Duration::from_secs(3));
                            id
                        }),
                    other => other,
                };
                let sent_as = match result {
                    Err(error) => {
                        self.push_display_message(DisplayMessage::error(format!(
                            "Voice request was not sent: {error}"
                        )));
                        None
                    }
                    Ok(id) => Some(id),
                };
                listener.finish_dispatch(&req, sent_as, interleave);
            }
            changed = true;
        }
        changed |= listener.drive_playback(self, &tick);
        self.jarvis_listener = listener;
        changed
    }
}
