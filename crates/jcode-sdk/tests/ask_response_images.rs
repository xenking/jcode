#![cfg(unix)]

use jcode_harness_api::{
    API_VERSION_MAJOR, ApiEvent, ApiRequest, AskAnswer, AskResponse, ClientFrame, ServerFrame,
    read_frame, write_frame,
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
fn ask_images_require_server_capability_before_any_socket_write() {
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
                        version: API_VERSION_MAJOR,
                        server: "fake".into(),
                        capabilities: if supported {
                            vec!["ask_response_images".into()]
                        } else {
                            vec![]
                        },
                    },
                ),
            )
            .unwrap();
            let request: ClientFrame = read_frame(&mut reader).unwrap();
            let ApiRequest::AskResponse { response, .. } = request.request else {
                panic!("expected ask response")
            };
            let answer = &response.answers.unwrap()[0];
            if supported {
                assert_eq!(answer.other_images[0], ("image/png".into(), "YWJj".into()));
                assert_eq!(
                    answer.comment_images["B"][0],
                    ("image/jpeg".into(), "ZGVm".into())
                );
            } else {
                assert!(
                    answer.other_images.is_empty() && answer.comment_images.is_empty(),
                    "image response was sent to old server"
                );
                assert_eq!(answer.other.as_deref(), Some("text only"));
            }
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
        let image_answer = AskResponse {
            answers: Some(vec![AskAnswer {
                selected: vec!["B".into()],
                other_images: vec![("image/png".into(), "YWJj".into())],
                comment_images: [("B".into(), vec![("image/jpeg".into(), "ZGVm".into())])]
                    .into_iter()
                    .collect(),
                ..Default::default()
            }]),
        };
        if supported {
            client.respond_to_ask("s", "r", image_answer).unwrap();
        } else {
            let error = client
                .respond_to_ask("s", "r", image_answer.clone())
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
            let comment_only = AskResponse {
                answers: Some(vec![AskAnswer {
                    comment_images: [("B".into(), vec![("image/jpeg".into(), "ZGVm".into())])]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                }]),
            };
            assert_eq!(
                client
                    .respond_to_ask("s", "r", comment_only)
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsupportedCapability
            );
            let error = client
                .request(ApiRequest::AskResponse {
                    session_id: "s".into(),
                    request_id: "r".into(),
                    response: image_answer,
                })
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
            client
                .respond_to_ask(
                    "s",
                    "r",
                    AskResponse {
                        answers: Some(vec![AskAnswer {
                            other: Some("text only".into()),
                            ..Default::default()
                        }]),
                    },
                )
                .unwrap();
        }
        server.join().unwrap();
    }
}
