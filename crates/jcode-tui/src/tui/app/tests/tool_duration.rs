use super::*;

#[test]
fn native_search_duration_includes_execution_and_preserves_late_query() {
    use crate::protocol::ServerEvent;
    use crate::tui::backend::RemoteEventState as _;
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    for event in [
        ServerEvent::ToolStart {
            id: "ws_timed".into(),
            name: "web_search".into(),
        },
        ServerEvent::ToolInput {
            id: Some("ws_timed".into()),
            delta: r#"{"query":""}"#.into(),
        },
        ServerEvent::ToolExec {
            id: "ws_timed".into(),
            name: "web_search".into(),
        },
    ] {
        app.handle_server_event(event, &mut remote);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    app.handle_server_event(
        ServerEvent::ToolInput {
            id: Some("ws_timed".into()),
            delta: r#"{"query":"jcode timing"}"#.into(),
        },
        &mut remote,
    );
    assert_eq!(
        remote.tool_events().tool_input_json("ws_timed")["query"],
        "jcode timing"
    );
    assert_eq!(app.streaming_tool_calls[0].input["query"], "jcode timing");
    app.handle_server_event(
        ServerEvent::ToolDone {
            codemode: None,
            ask: None,
            id: "ws_timed".into(),
            name: "web_search".into(),
            output: "Searched: jcode timing".into(),
            error: None,
        },
        &mut remote,
    );
    let message = app.display_messages().last().unwrap();
    assert!(message.duration_secs.unwrap() >= 0.02);
    assert_eq!(
        message.tool_data.as_ref().unwrap().input["query"],
        "jcode timing"
    );
}

#[test]
fn tool_duration_remote_completion_and_local_update() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    for (id, output, expected) in [
        ("header", "[tool timing: duration=2.4s] done", Some(2.4)),
        ("unknown", "done", None),
    ] {
        app.handle_server_event(
            crate::protocol::ServerEvent::ToolDone {
                codemode: None,
                ask: None,
                id: id.into(),
                name: "bash".into(),
                output: output.into(),
                error: None,
            },
            &mut remote,
        );
        assert_eq!(
            app.display_messages().last().unwrap().duration_secs,
            expected
        );
    }
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolExec {
            id: "observed".into(),
            name: "bash".into(),
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            codemode: None,
            ask: None,
            id: "observed".into(),
            name: "bash".into(),
            output: "failed".into(),
            error: Some("failed".into()),
        },
        &mut remote,
    );
    assert!(
        app.display_messages()
            .last()
            .unwrap()
            .duration_secs
            .is_some()
    );
    app.set_latest_tool_duration("observed", 1.234);
    app.replace_latest_tool_display_message("observed", None, "updated".into());
    assert_eq!(
        app.display_messages().last().unwrap().duration_secs,
        Some(1.234)
    );
}

#[test]
fn tool_duration_replay_does_not_measure_playback_clock() {
    let mut app = create_test_app();
    let mut remote = crate::tui::backend::ReplayRemoteState::default();
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolExec {
            id: "replay".into(),
            name: "bash".into(),
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            codemode: None,
            ask: None,
            id: "replay".into(),
            name: "bash".into(),
            output: "done".into(),
            error: None,
        },
        &mut remote,
    );
    assert_eq!(app.display_messages().last().unwrap().duration_secs, None);
}

#[test]
fn tool_completion_live_and_replay_agree_on_protocol_timing_and_error_output() {
    use crate::protocol::ServerEvent;
    use crate::tui::backend::{RemoteConnection, RemoteEventState, ReplayRemoteState};

    fn complete(mut remote: impl RemoteEventState) -> (String, Option<f32>) {
        let mut app = create_test_app();
        app.handle_server_event(
            ServerEvent::ToolExec {
                id: "comparison".into(),
                name: "bash".into(),
            },
            &mut remote,
        );
        app.handle_server_event(
            ServerEvent::ToolDone {
                codemode: None,
                ask: None,
                id: "comparison".into(),
                name: "bash".into(),
                output: "[tool timing: duration=2.4s] failed".into(),
                error: Some("failed".into()),
            },
            &mut remote,
        );
        let row = app.display_messages().last().unwrap();
        (row.content.clone(), row.duration_secs)
    }

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let live = complete(RemoteConnection::dummy());
    let replay = complete(ReplayRemoteState::default());
    assert_eq!(live, replay);
    assert_eq!(live.1, Some(2.4));
    assert!(live.0.starts_with("Error:"), "{}", live.0);
}

#[test]
fn codemode_receipt_from_tool_done_reaches_the_transcript_row() {
    use jcode_session_types::{CodemodeCall, CodemodeCallStatus, CodemodeStatus, CodemodeSummary};
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let summary = CodemodeSummary {
        status: CodemodeStatus::Success,
        calls: vec![CodemodeCall {
            tool: "mcp__srv__list_files".into(),
            action: None,
            actor: None,
            target: None,
            direction: None,
            external: false,
            status: CodemodeCallStatus::Success,
            duration_ms: 12,
            preview: None,
            diagnostic: None,
        }],
        calls_omitted: 0,
        report_body_bytes: None,
    };
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            codemode: Some(Box::new(summary.clone())),
            ask: None,
            id: "cm1".into(),
            name: "codemode".into(),
            output: "ok".into(),
            error: None,
        },
        &mut remote,
    );
    let row = app.display_messages().last().unwrap();
    assert_eq!(row.codemode.as_deref(), Some(&summary));
}
