use super::*;
use chrono::{Duration as ChronoDuration, Utc};
use ratatui::layout::Position;
use std::io::Write;
use std::time::{Duration as StdDuration, SystemTime};

#[path = "session_picker_tests/resume_canvas.rs"]
mod resume_canvas;

fn write_session_file_with_mtime(
    path: impl AsRef<std::path::Path>,
    content: &str,
    modified_secs: u64,
) {
    let mut file = std::fs::File::create(path.as_ref()).expect("create session file");
    file.write_all(content.as_bytes())
        .expect("write session file");
    file.set_modified(SystemTime::UNIX_EPOCH + StdDuration::from_secs(modified_secs))
        .expect("set modified time");
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

pub(super) fn make_session_for_presence(id: &str) -> SessionInfo {
    make_session(id, id, false, SessionStatus::Closed)
}

fn make_session(id: &str, short_name: &str, is_debug: bool, status: SessionStatus) -> SessionInfo {
    make_session_with_flags(id, short_name, is_debug, false, status)
}

#[path = "session_picker_tests/identity_search.rs"]
mod identity_search;

fn make_session_with_flags(
    id: &str,
    short_name: &str,
    is_debug: bool,
    is_canary: bool,
    status: SessionStatus,
) -> SessionInfo {
    let now = Utc::now();
    let title = "Test session".to_string();
    let working_dir = Some("/tmp".to_string());
    let messages_preview = vec![
        PreviewMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        },
        PreviewMessage {
            role: "assistant".to_string(),
            content: "world".to_string(),
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        },
    ];
    let search_index = build_search_index(
        id,
        short_name,
        &title,
        working_dir.as_deref(),
        None,
        &messages_preview,
    );

    SessionInfo {
        id: id.to_string(),
        parent_id: None,
        short_name: short_name.to_string(),
        icon: "🧪".to_string(),
        title,
        message_count: 2,
        user_message_count: 1,
        assistant_message_count: 1,
        created_at: now - ChronoDuration::minutes(5),
        last_message_time: now - ChronoDuration::minutes(1),
        last_active_at: Some(now - ChronoDuration::minutes(1)),
        working_dir,
        model: None,
        provider_key: None,
        is_canary,
        is_debug,
        saved: false,
        save_label: None,
        status,
        needs_catchup: false,
        estimated_tokens: 200,
        first_user_prompt: messages_preview
            .iter()
            .find(|msg| msg.role == "user" && !msg.content.trim().is_empty())
            .map(|msg| msg.content.clone()),
        messages_preview,
        search_index,
        server_name: None,
        server_icon: None,
        source: SessionSource::Jcode,
        resume_target: ResumeTarget::JcodeSession {
            session_id: id.to_string(),
        },
        external_path: None,
        user_intention: None,
        todo_statuses: Vec::new(),
    }
}

#[test]
fn cloud_session_picker_renders_host_and_preserves_resume_target() {
    let mut session = make_session(
        "session_cloud_1770000000000",
        "cloud",
        false,
        SessionStatus::Closed,
    );
    session.server_name = Some("cloud: dev-cloud".into());
    session.server_icon = Some("☁".into());
    let target = session.resume_target.clone();
    let mut picker = SessionPicker::new_grouped(
        vec![ServerGroup {
            name: "cloud: dev-cloud".into(),
            icon: "☁".into(),
            version: String::new(),
            git_hash: String::new(),
            is_running: false,
            sessions: vec![session],
        }],
        vec![],
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| picker.render(frame)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.to_lowercase().contains("cloud: dev-cloud"));
    assert_eq!(picker.selection_or_current_targets(), vec![target]);
}

#[path = "session_picker_tests/loading_and_benchmarks.rs"]
mod loading_and_benchmarks;
#[test]
fn test_format_estimated_tokens_uses_compact_units() {
    assert_eq!(SessionPicker::format_estimated_tokens(0), "~0 tok");
    assert_eq!(SessionPicker::format_estimated_tokens(999), "~999 tok");
    assert_eq!(SessionPicker::format_estimated_tokens(1_000), "~1k tok");
    assert_eq!(SessionPicker::format_estimated_tokens(1_234), "~1.2k tok");
    assert_eq!(SessionPicker::format_estimated_tokens(12_345), "~12k tok");
    assert_eq!(SessionPicker::format_estimated_tokens(999_500), "~1M tok");
    assert_eq!(
        SessionPicker::format_estimated_tokens(1_234_567),
        "~1.2M tok"
    );
    assert_eq!(
        SessionPicker::format_estimated_tokens(1_234_567_890),
        "~1.2B tok"
    );
    assert_eq!(
        SessionPicker::format_estimated_tokens(1_234_567_890_123),
        "~1.2T tok"
    );
}

#[test]
fn test_session_item_uses_single_primary_title_line() {
    let mut session = make_session(
        "session_primary_title",
        "rhino",
        false,
        SessionStatus::Closed,
    );
    session.title = "Generated release planning".to_string();
    session.estimated_tokens = 1_234_567;
    let picker = SessionPicker::new(vec![session.clone()]);

    let rows = picker.render_session_item_lines(&session, false);
    let text_rows: Vec<String> = rows.iter().map(line_text).collect();

    // The title must appear on exactly one row (the primary line); other rows
    // (compact activity/prompt context) must not repeat it.
    assert_eq!(
        text_rows
            .iter()
            .filter(|row| row.contains("Generated release planning"))
            .count(),
        1,
        "title should render on exactly one row: {text_rows:?}"
    );
    assert!(text_rows[0].contains("Generated release planning"));
    assert!(
        text_rows[1..]
            .iter()
            .all(|row| !row.contains("Generated release planning")),
        "title should only be rendered on the primary row: {text_rows:?}"
    );
    assert!(
        text_rows.iter().all(|row| !row.contains("rhino")),
        "memorable short name should remain searchable but not take display space: {text_rows:?}"
    );
    assert_eq!(
        text_rows.len(),
        2,
        "normal entries should use only two rows"
    );
}

#[test]
fn test_status_inference() {
    // Load sessions and ensure status display works
    let sessions = load_sessions().unwrap();
    for session in &sessions {
        let _ = session.status.display();
    }
}

#[test]
fn test_collect_recent_session_stems_skips_empty_recent_sessions() {
    let dir = tempfile::TempDir::new().expect("tempdir");

    write_session_file_with_mtime(
        dir.path().join("session_alpha_1000.json"),
        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        1000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_beta_2000.json"),
        r#"{"messages":[]}"#,
        2000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_gamma_3000.json"),
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
        3000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_delta_4000.json"),
        r#"{"messages":[]}"#,
        4000,
    );

    let stems = collect_recent_session_stems(dir.path(), 2).expect("collect stems");
    assert_eq!(stems, vec!["session_gamma_3000", "session_alpha_1000"]);
}

#[test]
fn test_collect_recent_session_stems_skips_system_context_only_sessions() {
    let dir = tempfile::TempDir::new().expect("tempdir");

    write_session_file_with_mtime(
        dir.path().join("session_empty_context_9000.json"),
        r##"{"messages":[{"role":"user","display_role":"system","content":[{"type":"text","text":"<system-reminder>\n# Session Context\n</system-reminder>"}]}]}"##,
        9000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_real_1000.json"),
        r#"{"messages":[{"role":"user","content":"real prompt"}]}"#,
        1000,
    );

    let stems = collect_recent_session_stems(dir.path(), 1).expect("collect stems");
    assert_eq!(stems, vec!["session_real_1000"]);
}

#[test]
fn test_collect_recent_session_stems_keeps_system_context_with_visible_journal_turn() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let stem = "session_context_then_journal_9000";

    write_session_file_with_mtime(
        dir.path().join(format!("{stem}.json")),
        r##"{"messages":[{"role":"user","display_role":"system","content":[{"type":"text","text":"<system-reminder>\n# Session Context\n</system-reminder>"}]}]}"##,
        1000,
    );
    write_session_file_with_mtime(
        dir.path().join(format!("{stem}.journal.jsonl")),
        r#"{"meta":{"updated_at":"2026-05-01T00:00:00Z"},"append_messages":[{"role":"user","content":"real prompt from journal"}]}"#,
        9000,
    );

    let stems = collect_recent_session_stems(dir.path(), 1).expect("collect stems");
    assert_eq!(stems, vec![stem]);
}

#[test]
fn test_collect_recent_session_stems_uses_timestamp_as_mtime_tiebreaker() {
    let dir = tempfile::TempDir::new().expect("tempdir");

    write_session_file_with_mtime(
        dir.path().join("session_old_1111.json"),
        r#"{"messages":[{"role":"user","content":"old"}]}"#,
        1000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_mid_2222.json"),
        r#"{"messages":[{"role":"user","content":"mid"}]}"#,
        1000,
    );
    write_session_file_with_mtime(
        dir.path().join("session_new_3333.json"),
        r#"{"messages":[{"role":"user","content":"new"}]}"#,
        1000,
    );

    let stems = collect_recent_session_stems(dir.path(), 3).expect("collect stems");
    assert_eq!(
        stems,
        vec!["session_new_3333", "session_mid_2222", "session_old_1111"]
    );
}

#[path = "session_picker_tests/selection_and_filter_tests.rs"]
mod selection_and_filter_tests;
#[allow(unused_imports)]
pub(crate) use selection_and_filter_tests::*;


#[test]
fn saved_placement_expiry_keeps_bookmarks_in_saved_filter() {
    let mut saved = make_session(
        "expired_saved_placement",
        "saved",
        false,
        SessionStatus::Closed,
    );
    saved.saved = true;
    saved.last_message_time = Utc::now() - ChronoDuration::days(8);
    let normal = make_session(
        "recent_normal_placement",
        "normal",
        false,
        SessionStatus::Closed,
    );
    let mut picker = SessionPicker::new(vec![saved, normal]);
    assert!(
        !picker
            .items
            .iter()
            .any(|item| matches!(item, PickerItem::SavedHeader { .. }))
    );
    let ids: Vec<_> = picker
        .visible_session_iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["recent_normal_placement", "expired_saved_placement"]
    );
    picker.filter_mode = SessionFilterMode::Saved;
    picker.rebuild_items();
    assert_eq!(picker.visible_sessions.len(), 1);
    assert_eq!(
        picker.selected_session().unwrap().id,
        "expired_saved_placement"
    );
    assert!(picker.selected_session().unwrap().saved);
}

#[path = "session_picker_tests/preview_and_scroll_tests.rs"]
mod preview_and_scroll_tests;
#[allow(unused_imports)]
pub(crate) use preview_and_scroll_tests::*;


#[test]
fn test_reseed_grouped_keeps_selection_when_list_changes() {
    // The highlighted session must follow its id even when the refreshed list has
    // a different order / additional sessions (the realistic refresh case).
    let initial: Vec<SessionInfo> = (0..4)
        .map(|i| {
            make_session(
                &format!("session_keep_{i}"),
                &format!("keep{i}"),
                false,
                SessionStatus::Closed,
            )
        })
        .collect();
    let mut picker = SessionPicker::new(initial.clone());
    picker.next(); // select session_keep_1
    let target = picker
        .selected_session()
        .map(|s| s.id.clone())
        .expect("selection");

    // Refreshed list: prepend a brand-new session and keep the rest, changing
    // indices so a naive index-based selection would drift.
    let mut refreshed = vec![make_session(
        "session_keep_new",
        "keepnew",
        false,
        SessionStatus::Closed,
    )];
    refreshed.extend(initial);

    picker.reseed_grouped(Vec::new(), refreshed);

    assert_eq!(
        picker.selected_session().map(|s| s.id.clone()),
        Some(target),
        "selection should follow the session id across a reordered refresh"
    );
}

#[path = "session_picker_tests/ctrl_layout.rs"]
mod ctrl_layout;

#[test]
fn test_search_mode_ctrl_j_k_navigate_session_list() {
    let mut newer = make_session("session_newer", "newer", false, SessionStatus::Closed);
    let mut older = make_session("session_older", "older", false, SessionStatus::Closed);
    newer.last_message_time = Utc::now();
    older.last_message_time = Utc::now() - ChronoDuration::minutes(1);
    let mut picker = SessionPicker::new(vec![older, newer]);

    // Enter search mode (both visible sessions still match the empty query).
    picker
        .handle_overlay_key(KeyCode::Char('/'), KeyModifiers::empty())
        .unwrap();
    assert!(picker.search_active);
    let first = picker
        .selected_session()
        .map(|s| s.id.clone())
        .expect("a session is selected on entering search");

    // Ctrl+J moves down the list without typing 'j' into the query.
    picker
        .handle_overlay_key(KeyCode::Char('j'), KeyModifiers::CONTROL)
        .unwrap();
    assert!(
        picker.search_query.is_empty(),
        "Ctrl+J must not type into search"
    );
    let second = picker
        .selected_session()
        .map(|s| s.id.clone())
        .expect("a session is selected after Ctrl+J");
    assert_ne!(first, second, "Ctrl+J should move the selection down");

    // Ctrl+K moves back up to the original selection.
    picker
        .handle_overlay_key(KeyCode::Char('k'), KeyModifiers::CONTROL)
        .unwrap();
    assert!(
        picker.search_query.is_empty(),
        "Ctrl+K must not type into search"
    );
    assert_eq!(
        picker.selected_session().map(|s| s.id.clone()),
        Some(first),
        "Ctrl+K should move the selection back up"
    );
}

#[test]
fn test_search_mode_ctrl_backspace_deletes_word() {
    let mut picker = SessionPicker::new(vec![make_session(
        "session_a",
        "a",
        false,
        SessionStatus::Closed,
    )]);
    picker
        .handle_overlay_key(KeyCode::Char('/'), KeyModifiers::empty())
        .unwrap();
    for c in "hello world".chars() {
        picker
            .handle_overlay_key(KeyCode::Char(c), KeyModifiers::empty())
            .unwrap();
    }
    assert_eq!(picker.search_query, "hello world");

    // Ctrl+Backspace deletes the trailing word.
    picker
        .handle_overlay_key(KeyCode::Backspace, KeyModifiers::CONTROL)
        .unwrap();
    assert_eq!(picker.search_query, "hello ");

    // The \u{8} alias some terminals send for Ctrl+Backspace also deletes a word.
    picker
        .handle_overlay_key(KeyCode::Char('\u{8}'), KeyModifiers::empty())
        .unwrap();
    assert_eq!(picker.search_query, "");

    // Plain Backspace still deletes a single character.
    for c in "abc".chars() {
        picker
            .handle_overlay_key(KeyCode::Char(c), KeyModifiers::empty())
            .unwrap();
    }
    picker
        .handle_overlay_key(KeyCode::Backspace, KeyModifiers::empty())
        .unwrap();
    assert_eq!(picker.search_query, "ab");
}

#[test]
fn test_search_mode_ctrl_u_clears_query() {
    let mut picker = SessionPicker::new(vec![make_session(
        "session_a",
        "a",
        false,
        SessionStatus::Closed,
    )]);
    picker
        .handle_overlay_key(KeyCode::Char('/'), KeyModifiers::empty())
        .unwrap();
    for c in "needle".chars() {
        picker
            .handle_overlay_key(KeyCode::Char(c), KeyModifiers::empty())
            .unwrap();
    }
    assert_eq!(picker.search_query, "needle");
    picker
        .handle_overlay_key(KeyCode::Char('u'), KeyModifiers::CONTROL)
        .unwrap();
    assert_eq!(picker.search_query, "");
    assert!(
        picker.search_active,
        "Ctrl+U clears text but stays in search"
    );
}

#[test]
fn cloud_session_directory_uses_cloud_icon() {
    let mut session = make_session("cloud_repo", "cloud_repo", false, SessionStatus::Closed);
    session.working_dir = Some("/home/jeremy/justrust-clean-room".into());
    let picker = SessionPicker::new(vec![session.clone()]);
    let local_rows = picker.render_session_item_lines(&session, false);
    assert!(line_text(&local_rows[1]).contains("📁"));

    session.server_icon = Some("☁".into());
    session.server_name = Some("cloud: jcode-cloud".into());
    let cloud_rows = picker.render_session_item_lines(&session, false);
    let directory = line_text(&cloud_rows[1]);
    assert!(directory.contains("☁"));
    assert!(!directory.contains("📁"));
    assert!(directory.contains("justrust-clean-room"));
}

#[test]
fn test_current_dir_highlight_marks_matching_sessions() {
    let mut same = make_session("same_dir", "same", false, SessionStatus::Closed);
    same.working_dir = Some("/home/jeremy/project".to_string());
    let mut other = make_session("other_dir", "other", false, SessionStatus::Closed);
    other.working_dir = Some("/home/jeremy/elsewhere".to_string());

    let mut picker = SessionPicker::new(vec![same.clone(), other.clone()]);
    // Trailing slash on the current dir should still match (normalization).
    picker.set_current_dir(Some("/home/jeremy/project/".to_string()));

    assert!(picker.session_in_current_dir(&same));
    assert!(!picker.session_in_current_dir(&other));

    let rows = picker.render_session_item_lines(&same, false);
    let text: String = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(
        text.contains("here"),
        "matching session should show the `here` marker: {text}"
    );

    // The marker and directory line should be styled with the same-dir accent
    // green so the highlight is visually distinct, not just present as text.
    let same_dir_color = rgb(120, 200, 140);
    let marker_styled_green = rows.iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.content.contains("here") && span.style.fg == Some(same_dir_color))
    });
    assert!(
        marker_styled_green,
        "`here` marker should be rendered in the same-dir accent color"
    );

    let other_rows = picker.render_session_item_lines(&other, false);
    let other_text: String = other_rows
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !other_text.contains("▸ here"),
        "non-matching session should not show the marker: {other_text}"
    );
}

#[test]
fn test_current_dir_highlight_absent_without_current_dir() {
    let mut session = make_session("s", "s", false, SessionStatus::Closed);
    session.working_dir = Some("/home/jeremy/project".to_string());
    let picker = SessionPicker::new(vec![session.clone()]);
    // No current_dir set: nothing is highlighted.
    assert!(!picker.session_in_current_dir(&session));
}

#[test]
fn highlight_spans_marks_query_occurrences() {
    let base = Style::default().fg(Color::White);
    let tokens = vec!["resume".to_string()];
    let spans = SessionPicker::highlight_spans("Fix the Resume bug", &tokens, base);
    let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(combined, "Fix the Resume bug");

    let highlighted: Vec<&str> = spans
        .iter()
        .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        highlighted,
        vec!["Resume"],
        "match should be highlighted case-insensitively"
    );
}

#[test]
fn highlight_spans_marks_each_token_independently() {
    // Multi-word queries highlight every token (order independent), matching the
    // AND-token filter semantics.
    let base = Style::default().fg(Color::White);
    let tokens = vec!["resume".to_string(), "bug".to_string()];
    let spans = SessionPicker::highlight_spans("Fix the Resume bug now", &tokens, base);
    let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(combined, "Fix the Resume bug now");
    let highlighted: Vec<&str> = spans
        .iter()
        .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        highlighted,
        vec!["Resume", "bug"],
        "every token should be highlighted"
    );
}

#[test]
fn highlight_spans_without_query_returns_single_span() {
    let base = Style::default().fg(Color::White);
    let spans = SessionPicker::highlight_spans("hello world", &[], base);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.as_ref(), "hello world");
}

#[test]
fn search_highlights_matching_title_in_rendered_rows() {
    let session = make_session("abc", "deploy pipeline", false, SessionStatus::Closed);
    let mut picker = SessionPicker::new(vec![session]);
    // make_session sets title = "Test session"; search a substring of the title.
    picker.search_query = "sess".to_string();
    let rows = picker.render_session_item_lines(picker.all_sessions.first().unwrap(), false);
    let has_highlight = rows[0]
        .spans
        .iter()
        .any(|s| s.content.as_ref() == "sess" && s.style.add_modifier.contains(Modifier::BOLD));
    assert!(
        has_highlight,
        "query substring in title should be highlighted"
    );
}

#[test]
fn search_highlights_match_in_preview_and_scrolls_to_it() {
    // A long transcript where a distinctive term ("flibbertigibbet") appears only
    // in an early message. Searching for it should both highlight the match in the
    // preview pane and scroll the preview to the match rather than to the bottom.
    let mut session = make_session_with_many_turns("long", 60);
    // Inject the unique term near the top of the transcript.
    session.messages_preview[4].content = "the magic flibbertigibbet token".to_string();
    let mut picker = SessionPicker::new(vec![session]);
    picker.focus = PaneFocus::Preview;

    let w = 100u16;
    let h = 16u16;

    // Baseline: no search -> auto-scrolls to bottom.
    let _ = buffer_text(&mut picker, w, h);
    let bottom_scroll = picker.scroll_offset;
    assert!(
        bottom_scroll > 0,
        "long preview should scroll to bottom by default"
    );

    // Now search for the unique early term. Reset auto-scroll like a keystroke would.
    picker.search_query = "flibbertigibbet".to_string();
    picker.auto_scroll_preview = true;
    let text = buffer_text(&mut picker, w, h);

    // The preview should have scrolled to the match (near the top), not the bottom.
    assert!(
        picker.scroll_offset < bottom_scroll,
        "preview should scroll up to the match (got {}, bottom was {})",
        picker.scroll_offset,
        bottom_scroll
    );
    assert!(
        text.contains("flibbertigibbet"),
        "matched term should be visible in the preview after scrolling"
    );

    // The match should be highlighted (bold) where the renderer drew it.
    let buffer = picker_buffer(&mut picker, w, h);
    let highlighted = (0..h).any(|y| {
        let row: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
        row.find("flibbertigibbet").is_some_and(|byte| {
            let x = row[..byte].chars().count() as u16;
            buffer[(x, y)].modifier.contains(Modifier::BOLD)
        })
    });
    assert!(
        highlighted,
        "matched term in preview body should be highlighted"
    );
}

#[test]
fn preview_without_search_has_no_highlight_and_scrolls_to_bottom() {
    let session = make_session_with_many_turns("nosrch", 60);
    let mut picker = SessionPicker::new(vec![session]);
    picker.focus = PaneFocus::Preview;
    let _ = buffer_text(&mut picker, 100, 16);
    assert!(
        picker.scroll_offset > 0,
        "should scroll to bottom without search"
    );
    let any_highlight = picker
        .preview_cache
        .as_ref()
        .expect("preview cache built")
        .wrapped_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|s| {
            s.style.add_modifier.contains(Modifier::BOLD) && s.style.fg == Some(rgb(255, 214, 90))
        });
    assert!(
        !any_highlight,
        "no search means no highlight color in preview"
    );
}

#[path = "session_picker_scope_tests.rs"]
mod project_scope;

#[path = "session_picker_tests/onboarding.rs"]
mod onboarding;

#[path = "session_picker_tests_additional_tests.rs"]
mod additional_copy_session_identifier_reports_success_and_failure;
