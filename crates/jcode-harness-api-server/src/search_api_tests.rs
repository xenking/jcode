//! Round trip for `SearchSessions` through the harness API bridge.
//!
//! The index tests prove the query; these prove the wire. A client sends one
//! JSON frame and gets one reply frame back, with no daemon round trip and no
//! attachment: global search is the one request that answers from the stored
//! corpus alone, so a fresh connection must be able to ask it.

use crate::search_index::{SearchIndex, install_global_for_tests};
use crate::translate::{BridgeState, Outbound};
use jcode_harness_api::{ApiEvent, ApiRequest, ServerFrame, SessionSearchRole};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn test_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jcode-search-api-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("sessions")).expect("create test dir");
    path
}

fn write_session(dir: &PathBuf, id: &str, title: &str, prompt: &str, answer: &str) {
    let snapshot = json!({
        "id": id,
        "title": title,
        "updated_at": "2026-01-02T04:05:06.000Z",
        "messages": [
            {
                "id": format!("{id}-m1"),
                "role": "user",
                "content": [{"type": "text", "text": prompt}],
                "timestamp": "2026-01-02T03:04:06Z",
            },
            {
                "id": format!("{id}-m2"),
                "role": "assistant",
                "content": [{"type": "text", "text": answer}],
                "timestamp": "2026-01-02T03:05:06Z",
            },
        ],
    });
    fs::write(
        dir.join("sessions").join(format!("{id}.json")),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .expect("write session");
}

/// Install a corpus as the process-wide index. Serialized by the translate
/// test lock because the slot is global.
fn install_corpus(dir: &PathBuf) {
    let index =
        SearchIndex::open_at(&dir.join("search.db"), &dir.join("sessions")).expect("open index");
    install_global_for_tests(index);
}

fn request(state: &mut BridgeState, frame: serde_json::Value) -> Vec<Outbound> {
    state.api_request_to_legacy(&frame)
}

fn reply_frames(outbound: Vec<Outbound>) -> Vec<ServerFrame> {
    outbound
        .into_iter()
        .filter_map(|outbound| match outbound {
            Outbound::Reply(frame) => Some(frame),
            Outbound::Legacy(value) => panic!("search must not reach the daemon: {value}"),
        })
        .collect()
}

#[test]
fn search_request_round_trips_through_the_bridge() {
    let _guard = crate::translate::jcode_home_test_lock();
    let dir = test_dir("roundtrip");
    write_session(
        &dir,
        "session_alpha",
        "Walrus planning",
        "how do we migrate the walrus index",
        "incrementally, by mtime",
    );
    install_corpus(&dir);

    // The client frame is exactly what the SDK serializes.
    let client = jcode_harness_api::ClientFrame::new(
        11,
        ApiRequest::SearchSessions {
            query: "walrus".into(),
            limit: Some(10),
        },
    );
    let wire = serde_json::to_value(&client).unwrap();
    assert_eq!(wire["req"], "search_sessions");
    assert_eq!(wire["query"], "walrus");

    let mut state = BridgeState::with_crash_on_disconnect(false);
    let frames = reply_frames(request(&mut state, wire));
    assert_eq!(frames.len(), 1, "one reply, no streaming frames");
    assert_eq!(frames[0].reply_to, Some(11));
    let ApiEvent::SessionSearch {
        query,
        titles,
        messages,
        indexed_sessions,
        truncated,
    } = &frames[0].event
    else {
        panic!("expected a session_search reply, got {:?}", frames[0].event);
    };
    assert_eq!(query, "walrus");
    assert_eq!(*indexed_sessions, 1);
    assert!(!truncated);
    // The title match is its own section; the prompt carries the message hit.
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].session_id, "session_alpha");
    assert_eq!(titles[0].role, SessionSearchRole::Title);
    assert_eq!(titles[0].timestamp_ms, 1_767_326_706_000);
    assert!(!messages.is_empty());
    let prompt_hit = messages
        .iter()
        .find(|hit| hit.role == SessionSearchRole::User)
        .expect("the prompt matched");
    assert_eq!(prompt_hit.session_id, "session_alpha");
    assert_eq!(prompt_hit.title, "Walrus planning");
    assert_eq!(
        prompt_hit.message_id.as_deref(),
        Some("session_alpha-m1"),
        "the hit names the message so the client can scroll to it"
    );
    assert_eq!(prompt_hit.timestamp_ms, 1_767_323_046_000);
    assert!(
        !prompt_hit.highlights.is_empty(),
        "the snippet marks the matched word"
    );

    // The reply decodes as the event a client sees, and re-encodes identically.
    let encoded = serde_json::to_value(&frames[0]).unwrap();
    assert_eq!(encoded["ev"], "session_search");
    let decoded: ServerFrame = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.event, frames[0].event);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
}

#[test]
fn search_needs_no_attachment_and_rejects_an_empty_query() {
    let _guard = crate::translate::jcode_home_test_lock();
    let dir = test_dir("guard");
    write_session(&dir, "session_beta", "Beta", "walrus again", "sure");
    install_corpus(&dir);

    // A brand new connection: nothing attached, nothing subscribed.
    let mut state = BridgeState::with_crash_on_disconnect(false);
    let frames = reply_frames(request(
        &mut state,
        json!({"v": 1, "id": 3, "req": "search_sessions", "query": "walrus"}),
    ));
    assert!(matches!(frames[0].event, ApiEvent::SessionSearch { .. }));

    let frames = reply_frames(request(
        &mut state,
        json!({"v": 1, "id": 4, "req": "search_sessions", "query": "   "}),
    ));
    assert_eq!(frames[0].reply_to, Some(4));
    let ApiEvent::Error { code, .. } = &frames[0].event else {
        panic!("an empty query is a client error, not an empty result");
    };
    assert_eq!(*code, jcode_harness_api::ErrorCode::InvalidRequest);
}

#[test]
fn search_reports_an_empty_corpus_honestly() {
    let _guard = crate::translate::jcode_home_test_lock();
    let dir = test_dir("empty");
    install_corpus(&dir);
    let mut state = BridgeState::with_crash_on_disconnect(false);
    let frames = reply_frames(request(
        &mut state,
        json!({"v": 1, "id": 5, "req": "search_sessions", "query": "nothing at all"}),
    ));
    let ApiEvent::SessionSearch {
        titles,
        messages,
        indexed_sessions,
        ..
    } = &frames[0].event
    else {
        panic!("expected a session_search reply");
    };
    assert!(titles.is_empty());
    assert!(messages.is_empty());
    assert_eq!(*indexed_sessions, 0, "no sessions were indexed");
}
