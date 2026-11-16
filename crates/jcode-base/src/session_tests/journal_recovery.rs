use super::*;
use anyhow::Result;

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.into(),
        cache_control: None,
    }
}

#[test]
fn journal_binary_corruption_keeps_valid_entries_and_heals() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    let id = "session_binary_journal";
    let mut session = Session::create_with_id(id.into(), None, None);
    session.add_message(Role::User, vec![text("before corruption")]);
    session.save()?;
    session.add_message(Role::Assistant, vec![text("valid journal prefix")]);
    session.save()?;
    let path = session_journal_path(id)?;
    let prefix = std::fs::read(&path)?;
    session.model = Some("tail-model".into());
    session.add_message(Role::User, vec![text("valid journal tail")]);
    session.save()?;
    let journal = std::fs::read(&path)?;
    let mut corrupt = prefix.clone();
    corrupt.extend_from_slice(b"\xff\xfe\n");
    corrupt.extend_from_slice(&journal[prefix.len()..]);
    std::fs::write(&path, &corrupt)?;

    let mut loaded = Session::load(id)?;
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[2].content_preview(), "valid journal tail");
    let remote = Session::load_for_remote_startup(id)?;
    assert_eq!(remote.messages.len(), 3);
    assert_eq!(remote.messages[2].content_preview(), "valid journal tail");
    assert_eq!(
        Session::load_startup_stub(id)?.model.as_deref(),
        Some("tail-model")
    );
    loaded.save()?;
    assert!(!path.exists());
    assert_eq!(
        std::fs::read(path.with_extension("corrupt.jsonl"))?,
        corrupt
    );
    assert_eq!(Session::load(id)?.messages.len(), 3);
    Ok(())
}

#[test]
fn startup_stub_does_not_revert_route_after_interrupted_checkpoint() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    let id = "session_stale_route_journal";
    let mut session = Session::create_with_id(id.into(), None, None);
    session.provider_key = Some("openai".into());
    session.model = Some("old-model".into());
    session.add_message(Role::User, vec![text("first turn")]);
    session.save()?;
    session.add_message(Role::Assistant, vec![text("old route response")]);
    session.save()?;
    let journal_path = session_journal_path(id)?;
    let stale_journal = std::fs::read(&journal_path)?;

    session.provider_key = Some("anthropic".into());
    session.model = Some("new-model".into());
    session.save_snapshot_to(&session_path(id)?)?;
    std::fs::write(&journal_path, stale_journal)?;

    assert_eq!(
        Session::load(id)?.provider_key.as_deref(),
        Some("anthropic")
    );
    let stub = Session::load_startup_stub(id)?;
    assert_eq!(stub.provider_key.as_deref(), Some("anthropic"));
    assert_eq!(stub.model.as_deref(), Some("new-model"));
    Ok(())
}

#[test]
fn startup_route_matches_full_replay_when_journal_clock_does_not_advance() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    for (id, rollback) in [
        ("session_equal_clock", false),
        ("session_rollback_clock", true),
    ] {
        let mut session = Session::create_with_id(id.into(), None, None);
        session.provider_key = Some("old-provider".into());
        session.add_message(Role::User, vec![text("snapshot turn")]);
        session.save()?;
        let snapshot_time = session.updated_at;
        session.add_message(Role::Assistant, vec![text("journal turn")]);
        session.save()?;
        let path = session_journal_path(id)?;
        let mut entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        entry["meta"]["provider_key"] = "new-provider".into();
        entry["meta"]["model"] = "new-tail-model".into();
        entry["meta"]["updated_at"] = serde_json::to_value(if rollback {
            snapshot_time - chrono::Duration::days(1)
        } else {
            snapshot_time
        })?;
        std::fs::write(&path, serde_json::to_vec(&entry)?)?;

        let full = Session::load(id)?;
        let remote = Session::load_for_remote_startup(id)?;
        let stub = Session::load_startup_stub(id)?;
        assert_eq!(full.model.as_deref(), Some("new-tail-model"));
        assert_eq!(stub.model, full.model, "{id}");
        assert_eq!(stub.provider_key, full.provider_key, "{id}");
        assert_eq!(remote.model, full.model, "{id}");
        assert!(stub.messages.is_empty());
    }
    Ok(())
}

fn metadata_only_route_change(id: &str) -> Result<(Session, chrono::DateTime<chrono::Utc>)> {
    let mut session = Session::create_with_id(id.into(), None, None);
    session.provider_key = Some("old-provider".into());
    session.model = Some("old-model".into());
    session.add_message(Role::User, vec![text(&"x".repeat(1024 * 1024 + 1))]);
    session.save()?;
    let snapshot_time = session.updated_at;
    session.provider_key = Some("new-provider".into());
    session.model = Some("new-model".into());
    session.save()?;
    let entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session_journal_path(id)?)?)?;
    assert!(entry.get("append_messages").is_none());
    assert_eq!(Session::load(id)?.model.as_deref(), Some("new-model"));
    Ok((session, snapshot_time))
}

#[test]
fn metadata_only_equal_clock_keeps_latest_route_in_all_loaders() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    let id = "session_metadata_clock_collision";
    let (_, snapshot_time) = metadata_only_route_change(id)?;
    let path = session_journal_path(id)?;
    let mut entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    entry["meta"]["updated_at"] = serde_json::to_value(snapshot_time)?;
    std::fs::write(&path, serde_json::to_vec(&entry)?)?;
    for loaded in [
        Session::load(id)?,
        Session::load_for_remote_startup(id)?,
        Session::load_startup_stub(id)?,
    ] {
        assert_eq!(loaded.provider_key.as_deref(), Some("new-provider"));
        assert_eq!(loaded.model.as_deref(), Some("new-model"));
    }
    Ok(())
}

#[test]
#[ignore = "needs journal generation + lock, see docs/session-journal-recovery-audit.md"]
fn metadata_only_stale_checkpoint_with_rollback_does_not_revert_snapshot() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    let id = "session_metadata_stale_checkpoint";
    let (mut session, snapshot_time) = metadata_only_route_change(id)?;
    let path = session_journal_path(id)?;
    let stale = std::fs::read(&path)?;
    session.provider_key = Some("checkpoint-provider".into());
    session.model = Some("checkpoint-model".into());
    session.updated_at = snapshot_time - chrono::Duration::days(1);
    session.save_snapshot_to(&session_path(id)?)?;
    std::fs::write(&path, stale)?;
    for loaded in [Session::load(id)?, Session::load_startup_stub(id)?] {
        assert_eq!(loaded.model.as_deref(), Some("checkpoint-model"));
    }
    Ok(())
}
