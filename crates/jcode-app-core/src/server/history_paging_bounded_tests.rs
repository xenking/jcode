use super::bounded::page_records;

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
fn prompt_offsets_count_answered_ask_results() {
    use crate::message::{ContentBlock, Role};
    let mut session = crate::session::Session::create_with_id("offset-test".into(), None, None);
    session.add_message(Role::User, vec![ContentBlock::Text { text: "first".into(), cache_control: None }]);
    session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
        id: "ask-1".into(), name: "ask".into(), input: serde_json::json!({"questions": []}), thought_signature: None,
    }]);
    session.add_message(Role::User, vec![ContentBlock::ToolResult {
        tool_use_id: "ask-1".into(), content: "User answers:\n1. Which? -> B\n".into(), is_error: None,
    }]);
    session.add_message(Role::User, vec![ContentBlock::Text { text: "last".into(), cache_control: None }]);
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
    session.compaction = Some(std::sync::Arc::new(StoredCompactionState {
        summary_text: "synthetic summary".into(),
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 80,
        original_turn_count: 100,
        compacted_count: 160,
    }));
    let dir = std::path::Path::new(&home).join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session_bounded_fixture.json");
    let file = std::fs::File::create(&path).unwrap();
    serde_json::to_writer(std::io::BufWriter::new(file), &session).unwrap();
    assert!(std::fs::metadata(path).unwrap().len() > 64 * 1024 * 1024);
}
