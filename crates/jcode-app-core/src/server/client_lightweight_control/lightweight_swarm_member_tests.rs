use super::*;

struct HomeGuard {
    prev: Option<std::ffi::OsString>,
    _temp: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => crate::env::set_var("JCODE_HOME", prev),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn isolated_home() -> HomeGuard {
    let lock = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    HomeGuard {
        prev,
        _temp: temp,
        _lock: lock,
    }
}

type Members = Arc<RwLock<HashMap<String, SwarmMember>>>;
type Swarms = Arc<RwLock<HashMap<String, HashSet<String>>>>;

fn empty_state() -> (SessionAgents, Members, Swarms) {
    (
        Arc::new(crate::server::session_manager::SessionManager::new()),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
    )
}

/// Issue #1748 bug 2: a headless `jcode run` session persisted on disk but
/// never subscribed must become a swarm member on its first Comm* request,
/// with the caller's own working directory.
#[tokio::test]
async fn persisted_headless_session_is_registered_with_its_working_dir() {
    let _home = isolated_home();
    let session_id = "session_headless_run_1748";
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.working_dir = Some("/tmp/project-a".to_string());
    session.save_prepared().expect("save session");

    let (sessions, members, swarms) = empty_state();
    ensure_lightweight_swarm_member(session_id, &sessions, &members, &swarms).await;

    let members = members.read().await;
    let member = members.get(session_id).expect("member registered");
    assert!(member.is_headless);
    assert_eq!(
        member.working_dir.as_deref(),
        Some(std::path::Path::new("/tmp/project-a"))
    );
    let swarm_id = member.swarm_id.clone().expect("swarm id");
    assert!(
        swarms
            .read()
            .await
            .get(&swarm_id)
            .is_some_and(|ids| ids.contains(session_id))
    );
}

/// Unknown ids keep answering "Not in a swarm" rather than minting a
/// phantom member for any string a client sends.
#[tokio::test]
async fn unknown_session_is_not_registered() {
    let _home = isolated_home();
    let (sessions, members, swarms) = empty_state();
    ensure_lightweight_swarm_member("no-such-session", &sessions, &members, &swarms).await;
    assert!(members.read().await.is_empty());
    assert!(swarms.read().await.is_empty());
}
