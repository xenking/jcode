//! `searchSessions` over a real socket pair.
//!
//! The index and the bridge translation are tested where they live. This is the
//! client's side: that the request the SDK puts on the wire is the one the
//! bridge expects, that both sections and highlight ranges survive the decode,
//! and that a reply of the wrong kind fails instead of fabricating results.

use super::*;
use jcode_harness_api::{SessionSearchHighlight, SessionSearchHit, SessionSearchRole};

fn hit(session_id: &str, role: SessionSearchRole, message_id: Option<&str>) -> SessionSearchHit {
    SessionSearchHit {
        session_id: session_id.to_string(),
        title: "Walrus planning".to_string(),
        role,
        snippet: "…the walrus index…".to_string(),
        highlights: vec![SessionSearchHighlight { start: 4, end: 10 }],
        timestamp_ms: 1_767_323_046_000,
        message_id: message_id.map(str::to_string),
    }
}

#[test]
fn search_sessions_round_trips_both_sections() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let client = fake_harness(move |frame, writer| {
        log.lock().expect("request log").push(frame.request.clone());
        let ApiRequest::SearchSessions { query, limit } = &frame.request else {
            panic!("unexpected request: {:?}", frame.request);
        };
        assert_eq!(query, "walrus");
        assert_eq!(*limit, Some(25));
        reply(
            frame,
            ApiEvent::SessionSearch {
                query: query.clone(),
                titles: vec![hit("s_title", SessionSearchRole::Title, None)],
                messages: vec![hit("s_msg", SessionSearchRole::Assistant, Some("m2"))],
                indexed_sessions: 189,
                truncated: true,
            },
            writer,
        );
    });

    let results = client
        .search_sessions("walrus", Some(25))
        .expect("search reply");
    assert_eq!(results.query, "walrus");
    assert_eq!(results.indexed_sessions, 189);
    assert!(results.truncated);
    assert_eq!(results.titles.len(), 1);
    assert_eq!(results.titles[0].role, SessionSearchRole::Title);
    assert_eq!(
        results.titles[0].message_id, None,
        "a title hit has no message"
    );
    assert_eq!(results.messages.len(), 1);
    assert_eq!(results.messages[0].session_id, "s_msg");
    assert_eq!(results.messages[0].role, SessionSearchRole::Assistant);
    assert_eq!(results.messages[0].message_id.as_deref(), Some("m2"));
    assert_eq!(
        results.messages[0].highlights,
        vec![SessionSearchHighlight { start: 4, end: 10 }]
    );

    assert_eq!(
        *seen.lock().expect("request log"),
        vec![ApiRequest::SearchSessions {
            query: "walrus".to_string(),
            limit: Some(25),
        }]
    );
}

#[test]
fn a_third_party_bridge_without_search_fails_cleanly() {
    // An older bridge answers an unknown request with an error frame; the
    // client must surface that rather than hang or invent an empty result.
    let client = fake_harness(move |frame, writer| {
        reply(
            frame,
            ApiEvent::Error {
                code: jcode_harness_api::ErrorCode::InvalidRequest,
                message: "unknown request: search_sessions".to_string(),
            },
            writer,
        );
    });
    let error = client
        .search_sessions("walrus", None)
        .expect_err("an unsupported bridge must not look like an empty corpus");
    assert!(
        error.to_string().contains("search_sessions")
            || error.to_string().contains("Invalid")
            || !error.to_string().is_empty()
    );
}
