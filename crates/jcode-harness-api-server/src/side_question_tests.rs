use super::{
    tests::{ScopedJcodeHome, only_reply_event},
    *,
};

#[test]
fn side_question_fork_never_falls_back_to_an_ordinary_split() {
    let request = json!({"req": "fork_side_question", "id": 1, "session_id": "parent"});
    let mut old = BridgeState {
        session_id: Some("parent".into()),
        ..BridgeState::default()
    };
    assert!(matches!(
        only_reply_event(old.api_request_to_legacy(&request)),
        ApiEvent::Error {
            code: ErrorCode::UnknownRequest,
            ..
        }
    ));

    let mut current = BridgeState {
        session_id: Some("parent".into()),
        side_questions_supported: true,
        ..BridgeState::default()
    };
    // A capable backend must use the distinct durable side-question fork.
    let output = current.api_request_to_legacy(&request);
    assert!(
        matches!(&output[..], [Outbound::Legacy(frame)] if frame["type"] == "split_side_question")
    );
}

#[test]
fn side_question_catalog_keeps_direct_children_and_stable_order_after_promotion() {
    use jcode_base::{
        message::{Message, Role},
        session::{Session, load_side_question_history},
    };

    let _home = ScopedJcodeHome::new("side-question-lineage");
    let parent = "session_btw_parent";
    let mut ordinary =
        Session::create_with_id("session_ordinary".into(), Some(parent.into()), None);
    ordinary.save_prepared().unwrap();
    let mut older = Session::create_with_id("session_btw_older".into(), Some(parent.into()), None);
    older.set_created_at(older.created_at() - std::time::Duration::from_secs(3));
    older.set_model(Some("gpt-5-btw-child".into()));
    older.set_reasoning_effort(Some("high".into()));
    older.add_message(Role::User, Message::user("main topic context").content);
    older.save_new_side_question().unwrap();
    let mut newer = Session::create_with_id("session_btw_newer".into(), Some(parent.into()), None);
    newer.save_new_side_question().unwrap();
    let mut nested = Session::create_with_id(
        "session_btw_nested".into(),
        Some(older.id().to_owned()),
        None,
    );
    nested.save_new_side_question().unwrap();
    let mut legacy =
        Session::create_with_id("session_btw_legacy".into(), Some(parent.into()), None);
    legacy.set_created_at(legacy.created_at() - std::time::Duration::from_secs(10));
    legacy.hidden = true;
    legacy.save_prepared().unwrap();

    let list = || {
        Session::list_side_questions(parent)
            .unwrap()
            .into_iter()
            .map(|topic| (topic.session_id, topic.message_start))
            .collect::<Vec<_>>()
    };
    let expected = [
        (newer.id().to_owned(), Some(0)),
        (older.id().to_owned(), Some(1)),
        (legacy.id().to_owned(), None),
    ];
    assert_eq!(list(), expected);
    let topics = Session::list_side_questions(parent).unwrap();
    let context = topics[1].context.as_ref().unwrap();
    assert_eq!(context.model.as_deref(), Some("gpt-5-btw-child"));
    assert_eq!(context.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        Session::list_side_questions(older.id()).unwrap()[0].session_id,
        nested.id()
    );
    assert!(load_side_question_history(legacy.id()).is_err());
    assert!(load_side_question_history(ordinary.id()).is_err());
    assert!(load_side_question_history(older.id()).unwrap().is_empty());

    older.add_message(Role::User, Message::user("side topic question").content);
    older.add_message(Role::Assistant, Message::user("side topic answer").content);
    older.save_prepared().unwrap();
    let messages = load_side_question_history(older.id()).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["side topic question", "side topic answer"]
    );
    assert!(BridgeState::hidden_side_question_ids().contains(older.id()));
    Session::promote_side_question(older.id()).unwrap();
    Session::promote_side_question(older.id()).unwrap();
    assert!(!BridgeState::hidden_side_question_ids().contains(older.id()));
    assert_eq!(list(), expected);
}
