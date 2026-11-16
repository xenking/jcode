use super::*;
use serde_json::json;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "jcode-swarm-metadata-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, value: serde_json::Value) {
        std::fs::write(self.0.join(name), value.to_string()).unwrap();
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn session(id: &str) -> SessionInfo {
    serde_json::from_value(json!({"session_id": id, "status": "idle"})).unwrap()
}

#[test]
fn old_session_wire_shape_defaults_and_omits_swarm_fields() {
    let session = session("legacy");
    assert_eq!(session.parent_session_id, None);
    assert_eq!(session.agent_label, None);
    assert_eq!(session.swarm_status, None);
    let value = serde_json::to_value(session).unwrap();
    for field in ["parent_session_id", "agent_label", "swarm_status"] {
        assert!(value.get(field).is_none());
    }
}

#[test]
fn new_session_wire_shape_roundtrips_unknown_status() {
    let value = json!({"session_id": "worker", "status": "idle", "parent_session_id": "root",
        "agent_label": "API reviewer", "swarm_status": "future_status"});
    let session: SessionInfo = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(session).unwrap(), value);
}

#[test]
fn enrichment_maps_only_swarm_ownership_and_preserves_title() {
    let dir = TempDir::new();
    dir.write("swarm.json", json!({"updated_at_unix_ms": 1, "members": [
        {"session_id": "root", "status": "running"},
        {"session_id": "worker", "report_back_to_session_id": "root", "task_label": "review", "status": "ready"},
        {"session_id": "grandchild", "report_back_to_session_id": "worker", "status": "completed"}
    ]}));
    let mut sessions = vec![
        session("root"),
        session("worker"),
        session("grandchild"),
        serde_json::from_value(
            json!({"session_id": "fork", "status": "idle", "parent_id": "root"}),
        )
        .unwrap(),
    ];
    sessions[1].title = Some("Custom title".into());
    enrich_sessions_from_swarm_state(&mut sessions, &dir.0);
    assert_eq!(sessions[0].parent_session_id, None);
    assert_eq!(sessions[0].swarm_status.as_deref(), Some("running"));
    assert_eq!(sessions[1].parent_session_id.as_deref(), Some("root"));
    assert_eq!(sessions[1].agent_label.as_deref(), Some("review"));
    assert_eq!(sessions[1].swarm_status.as_deref(), Some("ready"));
    assert_eq!(sessions[1].title.as_deref(), Some("Custom title"));
    assert_eq!(sessions[1].status, "idle");
    assert_eq!(sessions[2].parent_session_id.as_deref(), Some("worker"));
    assert_eq!(sessions[3].parent_session_id, None);
    assert_eq!(sessions[3].swarm_status, None);
}

#[test]
fn recent_watchdog_stall_is_visible_but_stale_or_finished_is_not() {
    let dir = TempDir::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    dir.write(
        "current.json",
        json!({"updated_at_unix_ms": now, "members": [
            {"session_id":"stalled", "status":"running", "stalled_secs": 601},
            {"session_id":"finished", "status":"ready", "stalled_secs": 601},
            {"session_id":"healthy", "status":"running"}
        ]}),
    );
    dir.write(
        "old.json",
        json!({"updated_at_unix_ms": now - 180_000, "members": [
            {"session_id":"stale", "status":"running", "stalled_secs": 601}
        ]}),
    );
    let mut sessions = vec![
        session("stalled"),
        session("finished"),
        session("healthy"),
        session("stale"),
    ];
    enrich_sessions_from_swarm_state(&mut sessions, &dir.0);
    let statuses: Vec<_> = sessions.iter().map(|s| s.swarm_status.as_deref()).collect();
    assert_eq!(
        statuses,
        vec![
            Some("stalled"),
            Some("ready"),
            Some("running"),
            Some("running")
        ]
    );
}

#[test]
fn enrichment_prefers_latest_snapshot_and_existing_api_values() {
    let dir = TempDir::new();
    dir.write("a-new.json", json!({"updated_at_unix_ms": 20, "members": [
        {"session_id": "worker", "report_back_to_session_id": "new-root", "task_label": "new", "status": "completed"}
    ]}));
    dir.write("z-old.json", json!({"updated_at_unix_ms": 10, "members": [
        {"session_id": "worker", "report_back_to_session_id": "old-root", "task_label": "old", "status": "running"}
    ]}));
    let mut sessions = vec![session("worker")];
    enrich_sessions_from_swarm_state(&mut sessions, &dir.0);
    assert_eq!(sessions[0].parent_session_id.as_deref(), Some("new-root"));
    assert_eq!(sessions[0].agent_label.as_deref(), Some("new"));
    assert_eq!(sessions[0].swarm_status.as_deref(), Some("completed"));
    sessions[0].parent_session_id = Some("api-root".into());
    sessions[0].agent_label = Some("api-label".into());
    sessions[0].swarm_status = Some("running".into());
    let before = sessions.clone();
    enrich_sessions_from_swarm_state(&mut sessions, &dir.0);
    assert_eq!(sessions, before);
}

#[test]
fn malformed_missing_and_legacy_snapshots_degrade_per_record() {
    let dir = TempDir::new();
    std::fs::write(dir.0.join("broken.json"), "{").unwrap();
    dir.write(
        "ignored.json.bak",
        json!({"members": [{"session_id":"ignored", "status":"running"}]}),
    );
    dir.write("legacy.json", json!({"members": [
        {"session_id": 123},
        {"session_id": "legacy", "report_back_to_session_id": "root"},
        {"session_id": "self", "report_back_to_session_id": "self", "task_label": "  ", "status": ""}
    ]}));
    let mut sessions = vec![session("legacy"), session("self"), session("ignored")];
    enrich_sessions_from_swarm_state(&mut sessions, dir.0.join("missing"));
    assert_eq!(sessions[0].parent_session_id, None);
    enrich_sessions_from_swarm_state(&mut sessions, &dir.0);
    assert_eq!(sessions[0].parent_session_id.as_deref(), Some("root"));
    assert_eq!(sessions[0].agent_label, None);
    assert_eq!(sessions[0].swarm_status, None);
    assert_eq!(sessions[1], session("self"));
    assert_eq!(sessions[2], session("ignored"));
}

/// Regression from live acceptance: Desktop starts the bridge with
/// `JCODE_RUNTIME_DIR=$TMPDIR` (to reach the shared daemon socket) while the
/// daemon persists swarm snapshots to `~/.jcode/state/swarm`. Enrichment only
/// read `$JCODE_RUNTIME_DIR/durable-state/swarm`, so 0 of 124 listed sessions
/// carried `parent_session_id` and the coordinator showed no workers.
/// Fixture: the coordinator's real snapshot, `detail` redacted.
#[test]
fn live_coordinator_snapshot_under_jcode_home_nests_workers_despite_runtime_dir() {
    let runtime = TempDir::new();
    let home = TempDir::new();
    let swarm = home.0.join("state/swarm");
    std::fs::create_dir_all(&swarm).unwrap();
    std::fs::write(
        swarm.join("session_session_microbe_1794670947943_2648f3bbec7c7e5e.json"),
        include_str!("fixtures/live_coordinator_swarm.json"),
    )
    .unwrap();
    let coordinator = "session_microbe_1794670947943_2648f3bbec7c7e5e";
    let mut sessions = vec![
        session(coordinator),
        session("session_hamster_1794718698462_fa6bdd5a9eec4f73"),
        session("session_fish_1794718668083_01dd7767afb8aa44"),
        session("session_fox_1794718668160_87674d9271df588d"),
    ];
    for dir in swarm_state_dirs(Some(runtime.0.clone()), Some(home.0.clone())) {
        enrich_sessions_from_swarm_state(&mut sessions, dir);
    }
    assert_eq!(sessions[0].parent_session_id, None);
    for worker in &sessions[1..] {
        assert_eq!(
            worker.parent_session_id.as_deref(),
            Some(coordinator),
            "{}",
            worker.session_id
        );
        assert!(worker.swarm_status.is_some());
    }
    assert_eq!(sessions[2].agent_label.as_deref(), Some("desktop-compact"));
    // Before the fix only the runtime dir was read: nothing nests.
    let mut before = vec![session("session_fish_1794718668083_01dd7767afb8aa44")];
    enrich_sessions_from_swarm_state(&mut before, runtime.0.join("durable-state/swarm"));
    assert_eq!(before[0].parent_session_id, None);
}
