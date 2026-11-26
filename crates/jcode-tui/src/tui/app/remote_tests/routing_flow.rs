//! Routing in a live remote session: decision events, the gate card, `/acct`.
use super::*;
use crate::protocol::{RoutingGateOption, ServerEvent};
use crossterm::event::{KeyCode, KeyModifiers};
use tokio::io::AsyncBufReadExt;

fn decision(account: &str, warnings: &[&str], pending: Option<&str>) -> ServerEvent {
    ServerEvent::RoutingDecision {
        account: account.into(),
        reason: "priority".into(),
        warnings: warnings.iter().map(|w| w.to_string()).collect(),
        pending: pending.map(str::to_string),
    }
}

fn persistent_header(app: &crate::tui::app::App, width: u16) -> Vec<ratatui::text::Line<'static>> {
    let (mut lines, secondary) = crate::tui::ui::header::build_header_sections(app, width);
    lines.extend(secondary);
    lines
}

fn header_text(app: &crate::tui::app::App, width: u16) -> String {
    persistent_header(app, width)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn decision_event_drives_the_header_chip_and_never_writes_the_chat() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let before = app.display_messages().len();

    handle_server_event(&mut app, decision("claude:team", &[], None), &mut remote);
    assert_eq!(
        app.display_messages().len(),
        before,
        "first decision is quiet"
    );
    let chip = crate::tui::TuiState::routing_chip(&app).unwrap();
    assert_eq!(chip.account, "claude:team");

    handle_server_event(
        &mut app,
        decision("claude:team", &["w1", "w2"], Some("claude:spare")),
        &mut remote,
    );
    assert_eq!(
        app.display_messages().len(),
        before,
        "same account, no announcement"
    );
    let chip = crate::tui::TuiState::routing_chip(&app).unwrap();
    assert_eq!(
        (chip.warnings, chip.pending.as_deref()),
        (2, Some("claude:spare"))
    );

    handle_server_event(&mut app, decision("claude:spare", &[], None), &mut remote);
    assert_eq!(
        app.display_messages().len(),
        before,
        "the daemon's one setting_change row announces a move, not a client line"
    );
    let chip = crate::tui::TuiState::routing_chip(&app).unwrap();
    assert_eq!(chip.account, "claude:spare");
}

#[test]
fn header_chip_follows_the_decision_at_narrow_and_wide_widths() {
    let _lock = crate::tui::ui::render_state_test_lock();
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.remote_provider_model = Some("claude-opus-4-5".into());
    handle_server_event(
        &mut app,
        decision("claude:team", &["near limit"], Some("claude:spare")),
        &mut remote,
    );
    let wide = header_text(&app, 140);
    assert!(wide.contains("claude:team"), "{wide}");
    assert!(
        wide.contains(&format!(
            "→ {} claude:spare",
            crate::routing_view::resolved_account_icon("claude:spare")
        )),
        "{wide}"
    );
    for width in [20u16, 32, 48, 64] {
        for line in persistent_header(&app, width) {
            assert!(line.width() < usize::from(width), "width={width}: {line:?}");
        }
    }
}

#[test]
fn gate_card_opens_answers_with_the_focused_switch_and_closes() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let peer = remote.take_dummy_peer().unwrap();
    let mut peer = tokio::io::BufReader::new(peer);

    handle_server_event(
        &mut app,
        ServerEvent::RoutingGate {
            gate_id: "gate-7".into(),
            reason: "claude:team hit its 5h limit".into(),
            options: vec![
                RoutingGateOption::WaitUntil {
                    resets_at: 1_900_000_000,
                },
                RoutingGateOption::Switch {
                    account: "claude:spare".into(),
                    detail: "40% used".into(),
                },
            ],
        },
        &mut remote,
    );
    assert!(app.routing_gate.is_some());
    let view = crate::tui::TuiState::inline_view_state(&app).expect("gate card is visible");
    assert!(
        view.lines
            .iter()
            .any(|l| l.contains("› 2. Switch to claude:spare"))
    );

    rt.block_on(app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote))
        .unwrap();
    assert!(app.routing_gate.is_none());
    assert!(crate::tui::TuiState::inline_view_state(&app).is_none());

    let mut line = String::new();
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), peer.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
    });
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["type"], "routing_gate_answer");
    assert_eq!(request["gate_id"], "gate-7");
    assert_eq!(request["choice"]["kind"], "switch");
    assert_eq!(request["choice"]["account"], "claude:spare");
}

#[test]
fn esc_cancels_the_gate_and_an_interrupt_closes_it_silently() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let mut peer = tokio::io::BufReader::new(remote.take_dummy_peer().unwrap());
    let gate = || ServerEvent::RoutingGate {
        gate_id: "g".into(),
        reason: "limit".into(),
        options: vec![RoutingGateOption::Ack {
            account: "a".into(),
        }],
    };

    handle_server_event(&mut app, gate(), &mut remote);
    rt.block_on(app.handle_remote_key(KeyCode::Esc, KeyModifiers::empty(), &mut remote))
        .unwrap();
    let mut line = String::new();
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), peer.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
    });
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["choice"]["kind"], "cancel");

    handle_server_event(&mut app, gate(), &mut remote);
    handle_server_event(&mut app, ServerEvent::Interrupted, &mut remote);
    assert!(app.routing_gate.is_none());
}

#[test]
fn acct_command_sends_an_intent_and_shows_the_reply_transiently() {
    let mut app = create_test_app();
    app.is_remote = true;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();
    let mut peer = tokio::io::BufReader::new(remote.take_dummy_peer().unwrap());
    let before = app.display_messages().len();

    app.input = "/acct hold claude:team 2h".into();
    app.cursor_pos = app.input.len();
    rt.block_on(app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote))
        .unwrap();
    let mut line = String::new();
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), peer.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
    });
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["type"], "routing_intent");
    assert_eq!(request["action"]["action"], "hold");
    assert_eq!(request["action"]["account"], "claude:team");
    assert_eq!(request["action"]["minutes"], 120);
    assert!(!app.pending_turn, "/acct never becomes a conversation turn");
    let id = request["id"].as_u64().unwrap();

    handle_server_event(
        &mut app,
        ServerEvent::RoutingStatus {
            id,
            text: "holding claude:team for 2h".into(),
        },
        &mut remote,
    );
    assert_eq!(
        app.display_messages().len(),
        before,
        "an intent reply never lands in the transcript"
    );
    assert_eq!(
        app.status_notice.as_ref().map(|(text, _)| text.as_str()),
        Some("holding claude:team for 2h")
    );
}

#[test]
fn the_routing_table_opens_a_dismissable_overlay_not_a_chat_row() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let before = app.display_messages().len();
    let table =
        "routing: auto\nthis session: 🦊 claude:fox\n* 🦊 claude:fox  subscription  priority 1";

    handle_server_event(
        &mut app,
        ServerEvent::RoutingStatus {
            id: 1,
            text: table.into(),
        },
        &mut remote,
    );

    assert_eq!(app.display_messages().len(), before);
    let (scroll, content) = crate::tui::TuiState::model_status_overlay(&app).unwrap();
    assert_eq!((scroll, content), (0, table));
    assert_eq!(
        crate::tui::TuiState::model_status_overlay_title(&app),
        "/acct status"
    );
    app.handle_model_status_key(KeyCode::Esc).unwrap();
    assert!(crate::tui::TuiState::model_status_overlay(&app).is_none());
}

#[test]
fn acct_usage_errors_stay_local() {
    let mut app = create_test_app();
    app.is_remote = true;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();
    let next_id = remote.next_request_id_for_test();

    app.input = "/acct hold".into();
    app.cursor_pos = app.input.len();
    rt.block_on(app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote))
        .unwrap();
    assert_eq!(
        remote.next_request_id_for_test(),
        next_id,
        "nothing was sent"
    );
    assert!(
        app.display_messages()
            .last()
            .unwrap()
            .content
            .contains("Usage: /acct")
    );
}

#[test]
fn daemon_closing_a_gate_closes_only_the_matching_card() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    handle_server_event(
        &mut app,
        ServerEvent::RoutingGate {
            gate_id: "g1".into(),
            reason: "limit".into(),
            options: vec![RoutingGateOption::Ack {
                account: "a".into(),
            }],
        },
        &mut remote,
    );
    handle_server_event(
        &mut app,
        ServerEvent::RoutingGateClosed {
            gate_id: "other".into(),
        },
        &mut remote,
    );
    assert!(app.routing_gate.is_some());
    handle_server_event(
        &mut app,
        ServerEvent::RoutingGateClosed {
            gate_id: "g1".into(),
        },
        &mut remote,
    );
    assert!(app.routing_gate.is_none());
    assert!(crate::tui::TuiState::inline_view_state(&app).is_none());
}
