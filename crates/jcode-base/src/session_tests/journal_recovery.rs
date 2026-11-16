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
