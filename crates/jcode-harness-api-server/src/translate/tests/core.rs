use super::*;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;

fn jcode_protocol_like_kv_miss() -> Value {
    json!({
        "type": "kv_cache_miss", "reason": "prefix_changed", "harness_caused": true,
        "missed_tokens": 46_000, "expected_tokens": 50_000, "read_tokens": 4_000,
        "message": "KV cache miss: ~46K tokens resent"
    })
}

pub(crate) struct ScopedJcodeHome {
    path: PathBuf,
    previous: Option<OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl ScopedJcodeHome {
    pub(crate) fn new(label: &str) -> Self {
        let guard = jcode_home_test_lock();
        let previous = std::env::var_os("JCODE_HOME");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "jcode-harness-api-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create isolated JCODE_HOME");
        // SAFETY: all tests in this module that mutate JCODE_HOME share `LOCK`,
        // and this guard restores the prior value before it is released.
        unsafe { std::env::set_var("JCODE_HOME", &path) };
        Self {
            path,
            previous,
            _guard: guard,
        }
    }
}

impl Drop for ScopedJcodeHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
            None => unsafe { std::env::remove_var("JCODE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_session_record(home: &Path, session_id: &str, working_dir: &Path) -> PathBuf {
    write_session_record_with_titles(home, session_id, working_dir, None, None)
}

fn write_session_record_with_titles(
    home: &Path,
    session_id: &str,
    working_dir: &Path,
    title: Option<&str>,
    custom_title: Option<&str>,
) -> PathBuf {
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions directory");
    let path = sessions.join(format!("{session_id}.json"));
    std::fs::write(
        &path,
        json!({
            "working_dir": working_dir,
            "title": title,
            "custom_title": custom_title,
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string(),
    )
    .expect("write session record");
    path
}

pub(super) fn only_reply_event(outbound: Vec<Outbound>) -> ApiEvent {
    assert_eq!(outbound.len(), 1, "expected exactly one reply");
    match outbound.into_iter().next().expect("one outbound") {
        Outbound::Reply(frame) => frame.event,
        other => panic!("expected API reply, got {other:?}"),
    }
}

fn state_with_session() -> BridgeState {
    BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    }
}

fn ids_state() -> BridgeState {
    BridgeState {
        session_id: Some("s1".into()),
        soft_interrupt_ids_supported: true,
        ..Default::default()
    }
}

fn legacy_out(out: &[Outbound]) -> &serde_json::Value {
    match out {
        [Outbound::Legacy(value)] => value,
        other => panic!("expected one legacy frame, got {other:?}"),
    }
}

// --- Capabilities added to close the API coverage gaps --------------------

#[path = "core/catalog_activity.rs"]
mod catalog_activity;

fn recovery_attach(state: &mut BridgeState, target: Option<&str>) -> (Value, Value) {
    let request = match target {
        Some(target) => json!({"req":"attach_session", "id":71, "session_id":target}),
        None => json!({"req":"create_session", "id":71}),
    };
    let out = state.api_request_to_legacy(&request);
    let Outbound::Legacy(subscribe) = &out[0] else {
        panic!("subscribe")
    };
    let Outbound::Legacy(snapshot) = &out[1] else {
        panic!("state")
    };
    (
        json!({"type":"history", "id":subscribe["id"], "session_id":"recover",
            "messages":[{"role":"user", "content":"finish task"}],
            "activity":{"is_processing":false}, "was_interrupted":true,
            "reload_recovery":{"continuation_message":"Continue the exact task", "reconnect_notice":"Recovered build"}}),
        json!({"type":"state", "id":snapshot["id"], "session_id":"recover", "is_processing":false}),
    )
}

fn markdown_panel() -> Value {
    json!({"focused_page_id":"notes","pages":[{"id":"notes","title":"Notes",
        "file_path":"/notes.md","format":"markdown","source":"managed",
        "content":"# Notes\n```mermaid\ngraph LR; A-->B\n```","updated_at_ms":42}]})
}

fn assert_panel(frames: &[ServerFrame], session: &str, snapshot: Value) {
    let panels: Vec<_> = frames
        .iter()
        .filter(|f| matches!(f.event, ApiEvent::SidePanelState { .. }))
        .collect();
    assert_eq!(panels.len(), 1);
    assert_eq!(
        panels[0],
        &ServerFrame::event(ApiEvent::SidePanelState {
            session_id: session.into(),
            snapshot: serde_json::from_value(snapshot).unwrap(),
        })
    );
}

#[path = "core/text_framing_tests.rs"]
mod text_framing_tests;

#[path = "core/tool_controls.rs"]
mod tool_controls;

#[path = "core/part1.rs"]
mod part1;

#[path = "core/part2.rs"]
mod part2;

#[path = "core/part3.rs"]
mod part3;

#[path = "core/part4.rs"]
mod part4;
