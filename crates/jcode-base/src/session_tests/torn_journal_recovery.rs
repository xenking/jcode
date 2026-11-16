use super::*;
use anyhow::Result;

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.into(),
        cache_control: None,
    }
}

fn message_ids(session: &Session) -> Vec<&str> {
    session.messages.iter().map(|m| m.id.as_str()).collect()
}

#[test]
fn repeated_torn_appends_recover_every_complete_group_and_heal() -> Result<()> {
    let _lock = lock_env();
    for separator in [b"{\"meta\":{\"updated_at\":\"".as_slice(), b"".as_slice()] {
        let home = tempfile::tempdir()?;
        let _home = EnvVarGuard::set("JCODE_HOME", home.path());
        let id = "session_repeated_torn_appends";
        let mut session = Session::create_with_id(id.into(), None, None);
        session.add_message(Role::User, vec![text("snapshot")]);
        session.save()?;
        for (role, content) in [
            (Role::Assistant, "first complete record"),
            (Role::User, "middle complete record"),
            (Role::Assistant, "final complete record"),
        ] {
            session.model = Some(content.into());
            session.add_message(role, vec![text(content)]);
            session.save()?;
        }
        let path = session_journal_path(id)?;
        let journal = std::fs::read(&path)?;
        let records: Vec<_> = journal
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(records.len(), 3);
        let mut corrupt = records[0].to_vec();
        corrupt.extend_from_slice(separator);
        corrupt.extend_from_slice(records[1]);
        corrupt.extend_from_slice(b"\xff\xfe");
        corrupt.extend_from_slice(records[2]);
        std::fs::write(&path, &corrupt)?;

        let mut full = Session::load(id)?;
        assert_eq!(message_ids(&full), message_ids(&session));
        assert_eq!(full.model, session.model);
        let remote = Session::load_for_remote_startup(id)?;
        assert_eq!(message_ids(&remote), message_ids(&session));
        assert_eq!(remote.model, session.model);
        assert_eq!(Session::load_startup_stub(id)?.model, session.model);

        full.save()?;
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(path.with_extension("corrupt.jsonl"))?,
            corrupt
        );
        assert_eq!(message_ids(&Session::load(id)?), message_ids(&session));
    }
    Ok(())
}

#[test]
fn salvage_does_not_replay_nested_meta_from_complete_records() -> Result<()> {
    let _lock = lock_env();
    let home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", home.path());
    let id = "session_nested_journal_meta";
    let mut session = Session::create_with_id(id.into(), None, None);
    session.add_message(Role::User, vec![text("snapshot")]);
    session.save()?;
    session.add_message(Role::Assistant, vec![text("journal seed")]);
    session.save()?;
    let path = session_journal_path(id)?;
    let entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let mut nested_meta = entry["meta"].clone();
    nested_meta["model"] = serde_json::json!("nested-fake-model");
    session.model = Some("outer-model".into());
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call_nested_meta".into(),
            name: "sample".into(),
            input: serde_json::json!({"meta": nested_meta}),
            thought_signature: None,
        }],
    );
    session.save()?;
    let mut corrupt = std::fs::read(&path)?;
    assert_eq!(corrupt.pop(), Some(b'\n'));
    corrupt.push(0xff);
    std::fs::write(&path, corrupt)?;

    let full = Session::load(id)?;
    assert_eq!(message_ids(&full), message_ids(&session));
    assert_eq!(full.model, session.model);
    let remote = Session::load_for_remote_startup(id)?;
    assert_eq!(message_ids(&remote), message_ids(&session));
    assert_eq!(remote.model, session.model);
    assert_eq!(Session::load_startup_stub(id)?.model, session.model);
    Ok(())
}
