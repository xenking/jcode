//! Explicitly configured external synthesis. No redirects or implicit service discovery.
#![allow(clippy::collapsible_if)]
#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;

use super::*;
use std::io::Write;

const MAX_WAV: usize = 16 * 1024 * 1024;
type Shared = Arc<(Mutex<State>, Condvar)>;

fn endpoint(value: &str) -> Result<reqwest::Url, SpeechError> {
    let url = reqwest::Url::parse(value).map_err(|_| SpeechError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(SpeechError::InvalidEndpoint);
    }
    Ok(url)
}

pub(super) fn create(
    value: &str,
    model: &str,
    voice: &str,
    api_key: Option<String>,
) -> Result<SpeechPlayer, SpeechError> {
    create_player(Options::new(endpoint(value)?, model, voice, api_key)?)
}

fn create_player(options: Options) -> Result<SpeechPlayer, SpeechError> {
    let executable = ["pw-play", "aplay"]
        .into_iter()
        .find(|name| probe(name))
        .ok_or(SpeechError::PlayerUnavailable)?;
    create_with_player(
        options,
        Arc::new(move |_| {
            Command::new(executable)
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        }),
    )
}

struct Options {
    url: reqwest::Url,
    model: String,
    voice: String,
    authorization: Option<reqwest::header::HeaderValue>,
}

impl Options {
    fn new(
        url: reqwest::Url,
        model: &str,
        voice: &str,
        api_key: Option<String>,
    ) -> Result<Self, SpeechError> {
        if [model, voice]
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(SpeechError::InvalidConfiguration);
        }
        let authorization = match api_key {
            Some(key) if !key.trim().is_empty() => {
                // Provider credentials must not travel over remote plain HTTP.
                let loopback = url.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
                if url.scheme() != "https" && !loopback {
                    return Err(SpeechError::InvalidConfiguration);
                }
                let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| SpeechError::InvalidConfiguration)?;
                header.set_sensitive(true);
                Some(header)
            }
            Some(_) => return Err(SpeechError::InvalidConfiguration),
            None => None,
        };
        Ok(Self {
            url,
            model: model.into(),
            voice: voice.into(),
            authorization,
        })
    }
}

fn create_with_player(options: Options, backend: Backend) -> Result<SpeechPlayer, SpeechError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|_| SpeechError::WorkerUnavailable)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| SpeechError::WorkerUnavailable)?;
    let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
    let state = shared.clone();
    let worker = thread::Builder::new()
        .name("jcode-http-speech".into())
        .spawn(move || {
            run_http(state, backend, client, options, runtime);
        })
        .map_err(|_| SpeechError::WorkerUnavailable)?;
    Ok(SpeechPlayer {
        shared,
        worker: Some(worker),
    })
}

async fn synthesize(client: &reqwest::Client, options: &Options, text: &str) -> Option<Vec<u8>> {
    let body = serde_json::json!({"model": options.model, "voice": options.voice, "input": text, "response_format": "wav"});
    let mut request = client.post(options.url.clone()).json(&body);
    if let Some(authorization) = &options.authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization.clone());
    }
    let mut response = request.send().await.ok()?;
    if response.status() != reqwest::StatusCode::OK
        || response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .trim()
            != "audio/wav"
        || response
            .content_length()
            .is_some_and(|n| n > MAX_WAV as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_WAV - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    Some(bytes)
}

fn run_http(
    shared: Shared,
    backend: Backend,
    client: reqwest::Client,
    options: Options,
    runtime: tokio::runtime::Runtime,
) {
    let (lock, wake) = &*shared;
    loop {
        let (text, generation) = {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                if let Some((child, start)) = state.active.as_mut() {
                    match child.try_wait() {
                        Ok(None) if start.elapsed() < MAX_PLAYBACK => {}
                        Ok(Some(status)) if status.success() => state.stop_child(),
                        _ => {
                            state.last_error =
                                Some("Local speech audio playback failed or timed out");
                            state.stop_child();
                        }
                    }
                }
                if state.active.is_none() {
                    if let Some(text) = state.queue.pop_front() {
                        state.synthesizing = true;
                        break (text, state.generation);
                    }
                }
                state = wake
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        let wav = runtime.block_on(async {
            tokio::select! {
                result = synthesize(&client, &options, &text) => result,
                _ = async {
                    loop {
                        tokio::time::sleep(POLL).await;
                        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if state.stopped || state.generation != generation { break; }
                    }
                } => None,
            }
        });
        let Some(wav) = wav else {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.synthesizing = false;
            if !state.stopped && state.generation == generation {
                state.last_error = Some("Speech API request failed or returned invalid WAV audio");
            }
            continue;
        };
        let stdin = {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.synthesizing = false;
            if state.stopped || state.generation != generation {
                continue;
            }
            // Spawn is serialized with cancel, but network and pipe writes never hold the lock.
            let mut child = match backend(&text) {
                Ok(child) => child,
                Err(_) => {
                    state.last_error = Some("Could not start the local speech audio player");
                    continue;
                }
            };
            let stdin = child.stdin.take();
            state.active = Some((child, Instant::now()));
            stdin
        };
        if let Some(mut stdin) = stdin {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                // A stalled player must not trap the worker in a full pipe.
                let fd = stdin.as_raw_fd();
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags < 0
                    || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
                {
                    lock.lock().unwrap_or_else(|e| e.into_inner()).stop_child();
                    continue;
                }
            }
            let mut remaining = wav.as_slice();
            while !remaining.is_empty() {
                {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stopped || state.generation != generation {
                        break;
                    }
                    if state
                        .active
                        .as_ref()
                        .is_some_and(|(_, start)| start.elapsed() >= MAX_PLAYBACK)
                    {
                        state.stop_child();
                        break;
                    }
                }
                match stdin.write(remaining) {
                    Ok(0) => break,
                    Ok(n) => remaining = &remaining[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(POLL),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
    }
}
