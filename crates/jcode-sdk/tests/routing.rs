//! Routing controls over the public SDK transport.
#![cfg(unix)]

use jcode_harness_api::{
    ApiEvent, ApiRequest, ClientFrame, ErrorCode, RoutingGateChoice, RoutingIntentAction,
    RoutingView, ServerFrame, read_frame, write_frame,
};
use jcode_sdk::{ConnectOptions, ErrorKind, JcodeClient, ROUTING_CAPABILITY, Transport};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

struct PairTransport(UnixStream);
impl Transport for PairTransport {
    fn split(
        self: Box<Self>,
    ) -> jcode_sdk::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        Ok((
            Box::new(BufReader::new(self.0.try_clone().unwrap())),
            Box::new(self.0),
        ))
    }
}

fn connect(
    capabilities: Vec<String>,
    serve: impl FnOnce(UnixStream) + Send + 'static,
) -> (JcodeClient, std::thread::JoinHandle<()>) {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_socket.try_clone().unwrap());
        let mut writer = server_socket.try_clone().unwrap();
        let hello: ClientFrame = read_frame(&mut reader).unwrap();
        write_frame(
            &mut writer,
            &ServerFrame::reply(
                hello.id,
                ApiEvent::HelloOk {
                    runtime_identity: None,
                    version: 1,
                    server: "routing-test".into(),
                    capabilities,
                },
            ),
        )
        .unwrap();
        serve(server_socket);
    });
    let client = JcodeClient::connect_with(
        Box::new(PairTransport(client_socket)),
        ConnectOptions {
            request_timeout: Some(Duration::from_secs(5)),
            ensure_runtime: false,
            ..Default::default()
        },
    )
    .unwrap();
    (client, server)
}

fn next(reader: &mut BufReader<UnixStream>) -> ClientFrame {
    read_frame(reader).unwrap()
}

#[test]
fn gate_answer_intent_and_view_use_their_requests_and_replies() {
    let (client, server) = connect(vec![ROUTING_CAPABILITY.into()], |socket| {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut writer = socket;

        let frame = next(&mut reader);
        assert_eq!(
            frame.request,
            ApiRequest::RoutingGateAnswer {
                session_id: "s1".into(),
                gate_id: "g1".into(),
                choice: RoutingGateChoice::Switch {
                    account: "claude:spare".into()
                },
            }
        );
        write_frame(&mut writer, &ServerFrame::reply(frame.id, ApiEvent::Ok)).unwrap();

        let frame = next(&mut reader);
        assert_eq!(
            frame.request,
            ApiRequest::RoutingIntent {
                session_id: "s1".into(),
                action: RoutingIntentAction::Pin {
                    account: Some("claude:team".into())
                },
            }
        );
        write_frame(
            &mut writer,
            &ServerFrame::reply(
                frame.id,
                ApiEvent::RoutingStatus {
                    text: "Pinned".into(),
                },
            ),
        )
        .unwrap();

        let frame = next(&mut reader);
        assert_eq!(frame.request, ApiRequest::GetRoutingView);
        write_frame(
            &mut writer,
            &ServerFrame::reply(
                frame.id,
                ApiEvent::RoutingView {
                    view: RoutingView {
                        mode: "auto".into(),
                        paid: "off".into(),
                        ..RoutingView::default()
                    },
                },
            ),
        )
        .unwrap();

        // A gate somebody else already answered.
        let frame = next(&mut reader);
        write_frame(
            &mut writer,
            &ServerFrame::reply(
                frame.id,
                ApiEvent::Error {
                    code: ErrorCode::Internal,
                    message: "No routing gate g1 is waiting for this session".into(),
                },
            ),
        )
        .unwrap();
    });

    client
        .answer_routing_gate(
            "s1",
            "g1",
            RoutingGateChoice::Switch {
                account: "claude:spare".into(),
            },
        )
        .unwrap();
    let text = client
        .routing_intent(
            "s1",
            RoutingIntentAction::Pin {
                account: Some("claude:team".into()),
            },
        )
        .unwrap();
    assert_eq!(text, "Pinned");
    let view = client.routing_view().unwrap();
    assert_eq!((view.mode.as_str(), view.paid.as_str()), ("auto", "off"));

    let error = client
        .answer_routing_gate("s1", "g1", RoutingGateChoice::Cancel)
        .unwrap_err();
    assert!(matches!(
        error.kind,
        ErrorKind::Harness(ErrorCode::Internal)
    ));
    assert_eq!(
        error.message,
        "No routing gate g1 is waiting for this session"
    );
    drop(client);
    server.join().unwrap();
}

#[test]
fn a_runtime_without_routing_is_refused_before_anything_is_sent() {
    let (client, server) = connect(Vec::new(), |_socket| {});
    for error in [
        client
            .answer_routing_gate("s1", "g1", RoutingGateChoice::Wait)
            .unwrap_err(),
        client
            .routing_intent("s1", RoutingIntentAction::Status)
            .unwrap_err(),
        client.routing_view().map(drop).unwrap_err(),
    ] {
        assert!(
            matches!(error.kind, ErrorKind::UnsupportedCapability),
            "{error:?}"
        );
    }
    drop(client);
    server.join().unwrap();
}
