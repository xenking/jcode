use super::status_target_sessions;

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// `status` with `session_ids` used to ignore the list and report on the
/// caller's own session, so a coordinator asking about a worker saw itself.
#[test]
fn status_reports_on_session_ids_not_the_caller() {
    let workers = ids(&["session_horse", "session_koala"]);
    assert_eq!(
        status_target_sessions(None, Some(&workers), "session_coordinator"),
        workers
    );
}

#[test]
fn status_target_session_wins_and_current_is_the_default() {
    let workers = ids(&["session_horse"]);
    assert_eq!(
        status_target_sessions(Some("session_fox".into()), Some(&workers), "me"),
        ids(&["session_fox"])
    );
    assert_eq!(
        status_target_sessions(Some("current".into()), None, "me"),
        ids(&["me"])
    );
    assert_eq!(status_target_sessions(None, None, "me"), ids(&["me"]));
    assert_eq!(status_target_sessions(None, Some(&[]), "me"), ids(&["me"]));
}

#[test]
fn status_session_ids_are_deduplicated() {
    let workers = ids(&["a", "b", "a", "current"]);
    assert_eq!(
        status_target_sessions(None, Some(&workers), "me"),
        ids(&["a", "b", "me"])
    );
}
