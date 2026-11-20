use super::*;
use std::{io::Read, net::TcpListener, sync::mpsc};

fn options(url: reqwest::Url) -> Options {
    Options::new(url, "future-model", "custom-voice", None).unwrap()
}

fn server(response: Vec<u8>) -> (reqwest::Url, thread::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&format!("http://{}/speak", listener.local_addr().unwrap())).unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break bytes.len();
            }
        };
        let headers = String::from_utf8_lossy(&bytes);
        assert!(headers.starts_with("POST /speak HTTP/1.1"));
        let len: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .unwrap()
            .parse()
            .unwrap();
        bytes.resize(header_end + len, 0);
        stream.read_exact(&mut bytes[header_end..]).unwrap();
        let body = serde_json::from_slice(&bytes[header_end..]).unwrap();
        let _ = stream.write_all(&response);
        body
    });
    (url, handle)
}

#[test]
fn validates_endpoints() {
    for value in [
        "file:///tmp/a",
        "ftp://host/a",
        "http://user@host",
        "https://a/#x",
        "http://a:bad",
    ] {
        assert!(endpoint(value).is_err(), "{value}");
    }
    assert!(endpoint("https://explicit-remote.example/tts").is_ok());
}

#[test]
fn configured_service_receives_exact_openai_schema() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 12\r\nConnection: close\r\n\r\n".to_vec();
    response.extend_from_slice(b"RIFF\x04\0\0\0WAVE");
    let (url, server) = server(response);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(synthesize(
        &reqwest::Client::new(),
        &options(url),
        "Hello Jarvis",
    ));
    assert!(result.is_some());
    assert_eq!(
        server.join().unwrap(),
        serde_json::json!({"input":"Hello Jarvis", "model":"future-model", "voice":"custom-voice", "response_format":"wav"})
    );
}

#[test]
fn openai_options_validate_without_leaking_credentials() {
    let remote = endpoint("http://remote.example/tts").unwrap();
    assert!(Options::new(remote, "model", "voice", Some("secret".into())).is_err());
    let local = endpoint("http://127.0.0.1:8765/tts").unwrap();
    assert!(Options::new(local.clone(), "", "voice", None).is_err());
    assert!(Options::new(local.clone(), "model", "voice", Some("\r\nsecret".into())).is_err());
    assert!(Options::new(local, "model", "voice", Some("secret".into())).is_ok());
    assert!(
        !SpeechError::InvalidConfiguration
            .to_string()
            .contains("secret")
    );
}

#[test]
fn real_json_request_and_bounded_wav_response() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for (status, mime, body, declared, valid) in [
        (200, "audio/wav", b"RIFF\x04\0\0\0WAVE".as_slice(), 12, true),
        (
            500,
            "audio/wav",
            b"RIFF\x04\0\0\0WAVE".as_slice(),
            12,
            false,
        ),
        (
            302,
            "audio/wav",
            b"RIFF\x04\0\0\0WAVE".as_slice(),
            12,
            false,
        ),
        (200, "audio/wav", b"bad".as_slice(), 3, false),
        (
            200,
            "text/plain",
            b"RIFF\x04\0\0\0WAVE".as_slice(),
            12,
            false,
        ),
        (200, "audio/wav", b"".as_slice(), MAX_WAV + 1, false),
    ] {
        let mut response = format!("HTTP/1.1 {status} Test\r\nContent-Type: {mime}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n").into_bytes();
        response.extend_from_slice(body);
        let (url, handle) = server(response);
        assert_eq!(
            runtime
                .block_on(synthesize(&client, &options(url), "hello \"world\""))
                .is_some(),
            valid
        );
        assert_eq!(
            handle.join().unwrap(),
            serde_json::json!({"input": "hello \"world\"", "model": "future-model", "voice": "custom-voice", "response_format": "wav"})
        );
    }
}

#[test]
fn streaming_body_without_length_is_bounded() {
    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n".to_vec();
    response.extend_from_slice(b"RIFF\0\0\0\0WAVE");
    response.resize(response.len() + MAX_WAV, 0);
    let (url, server) = server(response);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(synthesize(
                &reqwest::Client::new(),
                &options(url),
                "bounded"
            ))
            .is_none()
    );
    server.join().unwrap();
}

#[test]
#[cfg(unix)]
fn speech_beginning_during_synthesis_blocks_prepared_audio_until_authorized() {
    for revise in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = endpoint(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (synth, started) = mpsc::channel();
        let (finish, ready) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            assert!(socket.read(&mut buf).unwrap() > 0);
            synth.send(()).unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 12\r\nConnection: close\r\n\r\nRIFF\x04\0\0\0WAVE").unwrap();
        });
        let (spawned, played) = mpsc::channel();
        let player = create_with_player(
            options(url),
            Arc::new(move |_| {
                spawned.send(()).unwrap();
                Command::new("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
            }),
        )
        .unwrap();
        player
            .enqueue_gated(
                "test",
                super::super::PlaybackTrace {
                    session: "test".into(),
                    utterance: 1,
                },
            )
            .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        // It was quiet when synthesis started. Audio isn't ready, so no
        // mic claim can occur. The user starts speaking before WAV arrives.
        assert!(!player.authorize_ready(|| panic!("suppressed mic during synthesis")));
        assert!(!player.owns_audio_output());
        finish.send(()).unwrap();
        server.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !player.shared.0.lock().unwrap().awaiting_quiet {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        assert!(!player.authorize_ready(|| false));
        assert!(!player.owns_audio_output());
        assert!(played.try_recv().is_err());
        if revise {
            player.cancel();
            drop(player);
            assert!(played.try_recv().is_err());
        } else {
            assert!(player.authorize_ready(|| true));
            played.recv_timeout(Duration::from_secs(3)).unwrap();
        }
    }
}

#[test]
fn blocked_http_does_not_block_enqueue_cancel_or_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (tx, rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buf = [0; 4096];
        assert!(socket.read(&mut buf).unwrap() > 0);
        tx.send(()).unwrap();
        // Wait for cancellation to close the request, not for a 30-second timeout.
        loop {
            if socket.read(&mut buf).unwrap() == 0 {
                break;
            }
        }
    });
    let player =
        create_with_player(options(url), Arc::new(|_| panic!("cancelled audio played"))).unwrap();
    player.enqueue("blocked").unwrap();
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    for _ in 0..MAX_QUEUED_UTTERANCES {
        player.enqueue("pending").unwrap();
    }
    assert_eq!(player.enqueue("overflow"), Err(SpeechError::QueueFull));
    player.cancel();
    drop(player);
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
fn asynchronous_failure_is_reported_without_secret_or_url() {
    let response = b"HTTP/1.1 401 Denied\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (url, server) = server(response);
    let player =
        create_with_player(options(url), Arc::new(|_| panic!("failed request played"))).unwrap();
    player.enqueue("test").unwrap();
    let start = Instant::now();
    while !player.is_idle() {
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(POLL);
    }
    assert_eq!(
        player.take_error(),
        Some("Speech API request failed or returned invalid WAV audio")
    );
    assert_eq!(player.take_error(), None);
    server.join().unwrap();
}

/// Explicit local-only acceptance check. Requires a running Piper service
/// and audio output, so it is never run by the hosted suite.
#[test]
#[ignore = "requires local Piper service and audible output"]
fn real_local_piper_api_playback() {
    let endpoint = std::env::var("JCODE_TEST_SPEECH_ENDPOINT").expect("set test speech endpoint");
    let player = SpeechPlayer::with_http(&endpoint, "piper-jarvis", "jarvis", None).unwrap();
    player
        .enqueue("Jarvis is online. This reply is generated locally through Jcode's speech API.")
        .unwrap();
    let start = Instant::now();
    while !player.is_idle() {
        assert!(start.elapsed() < Duration::from_secs(30));
        thread::sleep(POLL);
    }
    assert_eq!(player.take_error(), None);
    println!(
        "Real Piper speech API and local playback completed in {:?}",
        start.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn http_audio_reaches_fake_player_without_sound() {
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 12\r\n\r\nRIFF\x04\0\0\0WAVE".to_vec();
    let (url, server) = server(response);
    let output = tempfile::NamedTempFile::new().unwrap();
    let file = output.reopen().unwrap();
    let player = create_with_player(
        options(url),
        Arc::new(move |_| {
            Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(file.try_clone().unwrap())
                .spawn()
        }),
    )
    .unwrap();
    player.enqueue("hello").unwrap();
    let start = Instant::now();
    while std::fs::metadata(output.path()).unwrap().len() != 12 {
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(POLL);
    }
    assert_eq!(std::fs::read(output.path()).unwrap(), b"RIFF\x04\0\0\0WAVE");
    drop(player);
    server.join().unwrap();
}
