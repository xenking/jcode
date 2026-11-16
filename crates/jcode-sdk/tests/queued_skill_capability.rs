#![cfg(unix)]

use jcode_harness_api::{
    API_VERSION_MAJOR, ApiEvent, ApiRequest, ClientFrame, QueuedPrompt, QueuedPromptMode,
    ServerFrame, read_frame, write_frame,
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

#[test]
fn queued_skills_require_capability_before_any_socket_write() {
    for supported in [false, true] {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().unwrap());
            let mut writer = theirs;
            let hello: ClientFrame = read_frame(&mut reader).unwrap();
            write_frame(
                &mut writer,
                &ServerFrame::reply(
                    hello.id,
                    ApiEvent::HelloOk {
                        runtime_identity: None,
                        version: API_VERSION_MAJOR,
                        server: "fake".into(),
                        capabilities: if supported {
                            vec!["queued_prompt_active_skill".into()]
                        } else {
                            vec![]
                        },
                    },
                ),
            )
            .unwrap();
            let request: ClientFrame = read_frame(&mut reader).unwrap();
            let ApiRequest::QueuePrompt { active_skill, .. } = request.request else {
                panic!("expected queue prompt")
            };
            assert_eq!(
                active_skill.as_deref(),
                if supported {
                    Some("verification")
                } else {
                    None
                }
            );
            write_frame(&mut writer, &ServerFrame::reply(request.id, ApiEvent::Ok)).unwrap();
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
        let prompt = QueuedPrompt {
            client_id: "q".into(),
            content: "hello".into(),
            images: vec![],
            mode: QueuedPromptMode::Steer,
            active_skill: None,
        };
        if supported {
            client
                .queue_prompt_with_skill("s", prompt, Some("verification"))
                .unwrap();
        } else {
            let error = client
                .queue_prompt_with_skill("s", prompt.clone(), Some("verification"))
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
            let error = client
                .request(ApiRequest::QueuePrompt {
                    session_id: "s".into(),
                    client_id: "q".into(),
                    content: "hello".into(),
                    images: vec![],
                    mode: QueuedPromptMode::Steer,
                    system_reminder: None,
                    active_skill: Some("verification".into()),
                })
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
            client.queue_prompt("s", prompt).unwrap();
        }
        server.join().unwrap();
    }
}
