#![cfg(unix)]

use base64::{Engine, engine::general_purpose::STANDARD};
use jcode_harness_api::{
    API_VERSION_MAJOR, ApiEvent, ApiRequest, ClientFrame, ErrorCode, MAX_TRANSCRIBE_WAV_BYTES,
    ServerFrame, TRANSCRIBE_CAPABILITY, read_frame, write_frame,
};
use jcode_sdk::{ConnectOptions, ErrorKind, JcodeClient, Transport};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

struct PairTransport(UnixStream);
impl Transport for PairTransport {
    fn split(
        self: Box<Self>,
    ) -> jcode_sdk::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        let writer = self.0.try_clone().unwrap();
        Ok((Box::new(BufReader::new(self.0)), Box::new(writer)))
    }
}

/// A scripted bridge. It answers each transcription with the next event, and a
/// `Ping` ends it, handing back every request it saw before that.
fn bridge(
    capabilities: Vec<String>,
    answers: Vec<ApiEvent>,
) -> (JcodeClient, std::thread::JoinHandle<Vec<ApiRequest>>) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(theirs.try_clone().unwrap());
        let mut writer = theirs;
        let hello: ClientFrame = read_frame(&mut reader).unwrap();
        let ok = ApiEvent::HelloOk {
            runtime_identity: None,
            version: API_VERSION_MAJOR,
            server: "fake".into(),
            capabilities,
        };
        write_frame(&mut writer, &ServerFrame::reply(hello.id, ok)).unwrap();
        let mut seen = Vec::new();
        let mut answers = answers.into_iter();
        while let Ok(frame) = read_frame::<_, ClientFrame>(&mut reader) {
            if matches!(frame.request, ApiRequest::Ping) {
                write_frame(&mut writer, &ServerFrame::reply(frame.id, ApiEvent::Pong)).unwrap();
                break;
            }
            seen.push(frame.request);
            let answer = answers.next().expect("one answer per request");
            write_frame(&mut writer, &ServerFrame::reply(frame.id, answer)).unwrap();
        }
        seen
    });
    let client = JcodeClient::connect_with(
        Box::new(PairTransport(ours)),
        ConnectOptions {
            request_timeout: Some(Duration::from_secs(5)),
            ensure_runtime: false,
            ..Default::default()
        },
    )
    .unwrap();
    (client, server)
}

#[test]
fn an_old_daemon_is_refused_before_any_audio_is_sent() {
    let (client, server) = bridge(vec!["sessions".into()], vec![]);
    let error = client.transcribe_audio(vec![1; 64], None).unwrap_err();
    assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
    client.ping().unwrap();
    assert!(server.join().unwrap().is_empty());
}

#[test]
fn a_recording_goes_out_as_base64_and_the_transcript_comes_back() {
    let (client, server) = bridge(
        vec![TRANSCRIBE_CAPABILITY.into()],
        vec![
            ApiEvent::AudioTranscript {
                text: "hello there".into(),
            },
            ApiEvent::Error {
                code: ErrorCode::TranscriptionNotConfigured,
                message: "no key".into(),
            },
        ],
    );
    let text = client
        .transcribe_audio(vec![1, 2, 3, 4], Some("en"))
        .unwrap();
    assert_eq!(text, "hello there");
    let error = client.transcribe_audio(vec![5], None).unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Harness(ErrorCode::TranscriptionNotConfigured)
    );
    assert_eq!(error.code(), "transcription_not_configured");
    client.ping().unwrap();
    let seen = server.join().unwrap();
    let ApiRequest::TranscribeAudio { wav, language } = &seen[0] else {
        panic!("expected transcribe_audio, got {:?}", seen[0]);
    };
    assert_eq!(STANDARD.decode(wav).unwrap(), [1, 2, 3, 4]);
    assert_eq!(language.as_deref(), Some("en"));
}

#[test]
fn an_oversized_recording_never_leaves_the_client() {
    let (client, server) = bridge(vec![TRANSCRIBE_CAPABILITY.into()], vec![]);
    let error = client
        .transcribe_audio(vec![0; MAX_TRANSCRIBE_WAV_BYTES + 1], None)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Harness(ErrorCode::AudioTooLarge));
    client.ping().unwrap();
    assert!(server.join().unwrap().is_empty());
}
