use super::bounded::page_records;

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "exercise the busy-agent history path"
)]
async fn journal_tail_pages_from_live_to_busy_with_snapshot_replay_events() {
    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::provider::Provider;
    use crate::tool::Registry;
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("JCODE_HOME");
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() {
                crate::env::set_var("JCODE_HOME", value);
            } else {
                crate::env::remove_var("JCODE_HOME");
            }
        }
    }
    let _restore = Restore(old_home);
    crate::env::set_var("JCODE_HOME", home.path());
    let id = "session_paging_busy_journal";
    let mut session = crate::session::Session::create_with_id(id.into(), None, None);
    let add = |s: &mut crate::session::Session, text: &str| {
        s.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
        )
    };
    add(&mut session, "old");
    session.record_replay_display_message("error", None, "historical failure");
    session.replay_events_mut_for_tests()[0].timestamp =
        chrono::Utc::now() - chrono::Duration::seconds(1);
    add(&mut session, "middle");
    session.save().unwrap();
    add(&mut session, "journal tail");
    session.save().unwrap();
    assert!(
        home.path()
            .join("sessions")
            .join(format!("{id}.journal.jsonl"))
            .exists()
    );
    struct TestProvider;
    #[async_trait::async_trait]
    impl Provider for TestProvider {
        async fn complete(
            &self,
            _: &[crate::message::Message],
            _: &[crate::message::ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::provider::EventStream> {
            anyhow::bail!("unexpected provider call")
        }
        fn name(&self) -> &str {
            "test"
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(Self)
        }
    }
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let agent = Arc::new(Mutex::new(Agent::new_with_session(
        provider,
        Registry::empty(),
        session,
        None,
    )));
    async fn request(
        agent: &Arc<Mutex<Agent>>,
        id: &str,
        before: Option<String>,
    ) -> crate::protocol::ServerEvent {
        let (stream, peer) = crate::transport::stream_pair().unwrap();
        let (_, write) = stream.into_split();
        let writer = Arc::new(Mutex::new(write));
        handle_history_page(1, id, agent, &writer, before, 1)
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(peer);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let crate::protocol::ServerEvent::HistoryPage {
        before: Some(cursor),
        ..
    } = request(&agent, id, None).await
    else {
        panic!("missing first page")
    };
    let guard = agent.lock().await;
    let event = request(&agent, id, Some(cursor)).await;
    drop(guard);
    let crate::protocol::ServerEvent::HistoryPage { fragments, .. } = event else {
        panic!("expected older page, got {event:?}")
    };
    assert!(fragments[0]["text"].as_str().unwrap().contains("middle"));
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "This test must keep its process-global home isolated across async operations"
)]
async fn initial_history_page_replays_context_occupancy_but_scrollback_does_not() {
    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::provider::Provider;
    use crate::tool::Registry;
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("JCODE_HOME");
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() {
                crate::env::set_var("JCODE_HOME", value);
            } else {
                crate::env::remove_var("JCODE_HOME");
            }
        }
    }
    let _restore = Restore(old_home);
    crate::env::set_var("JCODE_HOME", home.path());
    let id = "session_paging_context_occupancy";
    let mut session = crate::session::Session::create_with_id(id.into(), None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "how full is the context window?".into(),
            cache_control: None,
        }],
    );
    session.add_message_ext(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "the provider reported the prompt size".into(),
            cache_control: None,
        }],
        None,
        Some(crate::session::StoredTokenUsage {
            prompt_tokens: Some(91_000),
            input_tokens: 1_000,
            output_tokens: 20,
            cache_read_input_tokens: Some(90_000),
            cache_creation_input_tokens: None,
        }),
    );
    session.save().unwrap();
    struct TestProvider;
    #[async_trait::async_trait]
    impl Provider for TestProvider {
        async fn complete(
            &self,
            _: &[crate::message::Message],
            _: &[crate::message::ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::provider::EventStream> {
            anyhow::bail!("unexpected provider call")
        }
        fn name(&self) -> &str {
            "test"
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(Self)
        }
    }
    let agent = Arc::new(Mutex::new(Agent::new_with_session(
        Arc::new(TestProvider) as Arc<dyn Provider>,
        Registry::empty(),
        session,
        None,
    )));
    async fn request(
        agent: &Arc<Mutex<Agent>>,
        id: &str,
        before: Option<String>,
    ) -> Vec<crate::protocol::ServerEvent> {
        let (stream, peer) = crate::transport::stream_pair().unwrap();
        let (_, write) = stream.into_split();
        let writer = Arc::new(Mutex::new(write));
        handle_history_page(1, id, agent, &writer, before, 1)
            .await
            .unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(peer);
        let mut events = Vec::new();
        loop {
            let mut line = String::new();
            if tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .unwrap()
                == 0
            {
                break;
            }
            events.push(serde_json::from_str(&line).unwrap());
        }
        events
    }
    let initial = request(&agent, id, None).await;
    assert!(
        matches!(
            initial.first(),
            Some(crate::protocol::ServerEvent::HistoryPage { .. })
        ),
        "the page leads the attach payload: {initial:?}"
    );
    assert!(
        matches!(
            initial.get(1),
            Some(crate::protocol::ServerEvent::ContextOccupancy {
                prompt_tokens: 91_000,
                ..
            })
        ),
        "the initial page must replay the persisted prompt occupancy: {initial:?}"
    );
    let Some(crate::protocol::ServerEvent::HistoryPage {
        before: Some(cursor),
        ..
    }) = initial.first()
    else {
        panic!("missing first page cursor: {initial:?}")
    };
    let scrollback = request(&agent, id, Some(cursor.clone())).await;
    assert!(
        matches!(
            scrollback.as_slice(),
            [crate::protocol::ServerEvent::HistoryPage { .. }]
        ),
        "scrollback pages carry no attach state: {scrollback:?}"
    );
}

#[test]
fn generated_seventy_megabyte_history_is_bounded_and_lossless() {
    let entries: Vec<String> = (0..70).map(|i| serde_json::json!({"role":"user", "content":format!("{i}:{}", "\"\\\nЖ".repeat(170_000))}).to_string()).collect();
    assert!(entries.iter().map(String::len).sum::<usize>() > 64 * 1024 * 1024);
    let mut cursor = None;
    let mut assembled: Vec<String> = vec![String::new(); entries.len()];
    let mut calls = 0;
    loop {
        let page = page_records(&entries, cursor.as_deref(), 128).unwrap();
        let wire = serde_json::to_vec(&page).unwrap();
        assert!(
            wire.len() < 4 * 1024 * 1024,
            "frame is {} bytes",
            wire.len()
        );
        for part in page.fragments {
            assembled[part.index].insert_str(0, &part.text);
        }
        calls += 1;
        cursor = page.before;
        if cursor.is_none() {
            break;
        }
        assert!(calls < 1000);
    }
    assert_eq!(assembled, entries);
}

#[test]
fn cursor_survives_append_but_rejects_changed_boundary() {
    let mut rows = vec!["first".to_owned(), "second".to_owned()];
    let page = page_records(&rows, None, 1).unwrap();
    let cursor = page.before.unwrap();
    rows.push("new".into());
    assert_eq!(
        page_records(&rows, Some(&cursor), 1).unwrap().fragments[0].text,
        "first"
    );
    rows[0] = "rewritten".into();
    assert!(page_records(&rows, Some(&cursor), 1).is_err());
}

#[test]
fn cursor_survives_rows_inserted_before_boundary() {
    let live = vec!["old".to_owned(), "boundary".to_owned(), "new".to_owned()];
    let cursor = page_records(&live, None, 1).unwrap().before.unwrap();
    let persisted = vec![
        "inserted".to_owned(),
        "old".to_owned(),
        "boundary".to_owned(),
        "new".to_owned(),
    ];
    let page = page_records(&persisted, Some(&cursor), 1).unwrap();
    assert_eq!(page.fragments[0].text, "boundary");
}

#[test]
fn shifted_partial_row_keeps_fragment_identity() {
    let row = "x".repeat(1_200_000);
    let first = page_records(std::slice::from_ref(&row), None, 1).unwrap();
    let shifted = vec!["inserted".to_owned(), row];
    let second = page_records(&shifted, first.before.as_deref(), 1).unwrap();
    let third = page_records(&shifted, second.before.as_deref(), 1).unwrap();
    assert_eq!(first.fragments[0].index, second.fragments[0].index);
    assert_eq!(first.fragments[0].index, third.fragments[0].index);
    assert_eq!(third.fragments[0].start, 0);
}

#[test]
fn shifted_ambiguous_boundary_is_not_guessed() {
    let first = vec!["boundary".to_owned(), "tail".to_owned()];
    let cursor = page_records(&first, None, 1).unwrap().before.unwrap();
    let changed = vec![
        "other".to_owned(),
        "boundary".to_owned(),
        "boundary".to_owned(),
    ];
    assert_eq!(
        page_records(&changed, Some(&cursor), 1).unwrap_err(),
        "stale cursor"
    );
}

#[test]
fn bounded_history_images_use_stable_session_ids_even_for_duplicate_payloads() {
    use crate::message::{ContentBlock, Role};
    let mut session = crate::session::Session::create_with_id("image-page-test".into(), None, None);
    for _ in 0..2 {
        session.add_message(
            Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "same".into(),
            }],
        );
    }
    let ids = session.session_images();
    let records = super::bounded::records(&session).unwrap();
    let image_keys: Vec<String> = records
        .iter()
        .flat_map(|record| {
            let value: serde_json::Value = serde_json::from_str(record).unwrap();
            value["images"]
                .as_array()
                .unwrap()
                .iter()
                .map(|image| image["data"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        image_keys,
        ids.iter()
            .map(|image| format!("jcode-lazy:{}", image.image_id))
            .collect::<Vec<_>>()
    );
    assert_ne!(image_keys[0], image_keys[1]);
    let tail = page_records(&records, None, 1).unwrap();
    let first = page_records(&records, tail.before.as_deref(), 1).unwrap();
    for (page, expected) in [(first, &ids[0]), (tail, &ids[1])] {
        let record: serde_json::Value = serde_json::from_str(&page.fragments[0].text).unwrap();
        assert_eq!(
            record["images"][0]["session_image"],
            serde_json::json!({
                "image_id": expected.image_id, "ordinal": expected.ordinal,
            })
        );
    }
    assert_eq!(
        super::image_chunk(&session, &ids[0].image_id, 0).unwrap(),
        (4, "same".into())
    );
    assert_eq!(
        super::image_chunk(&session, &ids[1].image_id, 0).unwrap(),
        (4, "same".into())
    );
}

#[test]
fn prompt_offsets_count_answered_ask_results() {
    use crate::message::{ContentBlock, Role};
    let mut session = crate::session::Session::create_with_id("offset-test".into(), None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".into(),
            cache_control: None,
        }],
    );
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "ask-1".into(),
            name: "ask".into(),
            input: serde_json::json!({"questions": []}),
            thought_signature: None,
        }],
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "ask-1".into(),
            content: "User answers:\n1. Which? -> B\n".into(),
            is_error: None,
        }],
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "last".into(),
            cache_control: None,
        }],
    );
    let records = super::bounded::records(&session).unwrap();
    let last: serde_json::Value = serde_json::from_str(records.last().unwrap()).unwrap();
    assert_eq!(last["message"]["content"], "last");
    assert_eq!(last["prompt_offset"], 2);
}

#[test]
fn complete_rows_are_returned_before_a_large_older_row() {
    let rows = vec!["large".repeat(200_000), "recent".into()];
    let page = page_records(&rows, None, 128).unwrap();
    assert_eq!(page.fragments.len(), 1);
    assert_eq!(page.fragments[0].text, "recent");
    let older = page_records(&rows, page.before.as_deref(), 128).unwrap();
    assert_eq!(older.fragments.len(), 1);
    assert!(older.fragments[0].start > 0);
}

#[test]
#[ignore = "creates a synthetic >64 MiB session in JCODE_HOME for isolated runtime testing"]
fn generate_large_runtime_fixture() {
    use crate::message::{ContentBlock, Role};
    use crate::session::{Session, StoredCompactionState};
    let home = std::env::var("JCODE_HOME").expect("isolated JCODE_HOME required");
    assert!(home.contains("hmb2"), "refuse non-scratch JCODE_HOME");
    let mut session = Session::create_with_id(
        "session_bounded_fixture".into(),
        None,
        Some("Synthetic bounded history".into()),
    );
    for i in 0..100 {
        let text = if i == 0 {
            format!("large synthetic {}", "x".repeat(70 * 1024 * 1024))
        } else {
            format!("synthetic user prompt {i}")
        };
        let mut blocks = vec![ContentBlock::Text {
            text,
            cache_control: None,
        }];
        if i == 95 || i == 30 {
            blocks.push(ContentBlock::Image { media_type: "image/png".into(), data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jxFoAAAAASUVORK5CYII=".into() });
        }
        session.add_message(Role::User, blocks);
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: format!("synthetic answer {i}"),
                cache_control: None,
            }],
        );
        if i == 97 {
            session.add_message(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "synthetic-swarm".into(),
                    name: "functions.swarm".into(),
                    input: serde_json::json!({"action":"spawn","label":"test"}),
                    thought_signature: None,
                }],
            );
            session.add_message(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "synthetic-swarm".into(),
                    content: "spawned".into(),
                    is_error: None,
                }],
            );
        }
    }
    session.set_compaction(Some(std::sync::Arc::new(StoredCompactionState {
        summary_text: "synthetic summary".into(),
        plan_context: None,
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 80,
        original_turn_count: 100,
        compacted_count: 160,
    })));
    let dir = std::path::Path::new(&home).join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session_bounded_fixture.json");
    let file = std::fs::File::create(&path).unwrap();
    serde_json::to_writer(std::io::BufWriter::new(file), &session).unwrap();
    assert!(std::fs::metadata(path).unwrap().len() > 64 * 1024 * 1024);
}
