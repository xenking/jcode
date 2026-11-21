use super::dispatchable_lanes;
use crate::todo::TodoItem;

fn item(id: &str, stage: u32, lane: &str, owner: Option<&str>) -> TodoItem {
    TodoItem {
        id: id.to_string(),
        content: format!("task {id}"),
        status: "pending".to_string(),
        priority: "high".to_string(),
        stage: Some(stage),
        group: Some(lane.to_string()),
        lane_owner: owner.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn self_owned_lanes_never_dispatch() {
    let todos = vec![item("a", 1, "api", None), item("b", 1, "docs", None)];
    assert!(dispatchable_lanes(&todos).is_empty());
}

#[test]
fn ready_worker_lanes_in_one_phase_start_together() {
    let todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "docs", Some("worker")),
        item("c", 1, "mine", None),
    ];
    let lanes = dispatchable_lanes(&todos);
    assert_eq!(lanes.len(), 2, "{lanes:?}");
}

#[test]
fn later_phase_waits_then_unlocks_on_completion() {
    let mut todos = vec![
        item("a", 1, "api", None),
        item("b", 2, "client", Some("worker")),
    ];
    assert!(dispatchable_lanes(&todos).is_empty());
    todos[0].status = "completed".to_string();
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
}

#[test]
fn explicit_dependency_unlocks_lane_early() {
    let mut consumer = item("c", 2, "client", Some("worker"));
    consumer.lane_after = Some(vec!["a".into()]);
    let mut todos = vec![
        item("a", 1, "api", None),
        item("b", 1, "api", None),
        consumer,
    ];
    assert!(dispatchable_lanes(&todos).is_empty());
    todos[0].status = "completed".to_string();
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
}

#[test]
fn owned_lane_is_not_dispatched_again() {
    let mut todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "api", Some("worker")),
    ];
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
    todos[0].assigned_to = Some("worker-1".into());
    todos[1].assigned_to = Some("worker-1".into());
    // Plan edits and reloads re-evaluate the saved list; the persisted owner
    // keeps the lane from getting a second worker.
    todos.push({
        let mut extra = item("x", 1, "api", Some("worker"));
        extra.assigned_to = Some("worker-1".into());
        extra
    });
    assert!(dispatchable_lanes(&todos).is_empty());
}

#[test]
fn completed_lane_is_not_dispatched() {
    let mut done = item("a", 1, "api", Some("worker"));
    done.status = "completed".into();
    assert!(dispatchable_lanes(&[done]).is_empty());
}

#[test]
fn normalize_makes_owner_lane_wide() {
    let mut todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "api", None),
        item("c", 1, "docs", Some("self")),
    ];
    crate::todo::stages::normalize_lane_owners(&mut todos);
    assert_eq!(todos[1].lane_owner.as_deref(), Some("worker"));
    assert_eq!(todos[2].lane_owner, None);
}

fn member(session_id: &str, status: &str, report_back: Option<&str>) -> super::SwarmMember {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    super::SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: Default::default(),
        working_dir: None,
        swarm_id: Some("swarm".to_string()),
        swarm_enabled: true,
        status: status.to_string(),
        detail: None,
        task_label: None,
        friendly_name: None,
        report_back_to_session_id: report_back.map(str::to_string),
        latest_completion_report: None,
        role: "agent".to_string(),
        joined_at: std::time::Instant::now(),
        last_status_change: std::time::Instant::now(),
        is_headless: true,
        output_tail: None,
        todo_progress: None,
        todo_items: Vec::new(),
        runtime: crate::protocol::SwarmMemberRuntime::default(),
    }
}

#[test]
fn only_working_members_spawned_by_the_session_count() {
    let members: std::collections::HashMap<_, _> = [
        member("coord", "ready", None),
        member("w-running", "running", Some("coord")),
        member("w-queued", "queued", Some("coord")),
        member("w-done", "completed", Some("coord")),
        member("w-ready", "ready", Some("coord")),
        member("other-running", "running", Some("someone-else")),
    ]
    .into_iter()
    .map(|m| (m.session_id.clone(), m))
    .collect();
    assert_eq!(
        super::working_members_spawned_by(&members, "coord"),
        vec!["w-queued".to_string(), "w-running".to_string()]
    );
    assert!(super::working_members_spawned_by(&members, "w-done").is_empty());
}
