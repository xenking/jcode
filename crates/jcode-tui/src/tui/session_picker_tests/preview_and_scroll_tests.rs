use super::*;

#[test]
pub(super) fn saved_placement_recent_bookmark_still_gets_header() {
    let mut saved = make_session(
        "recent_saved_placement",
        "saved",
        false,
        SessionStatus::Closed,
    );
    saved.saved = true;
    let picker = SessionPicker::new(vec![saved]);
    assert!(
        picker
            .items
            .iter()
            .any(|item| matches!(item, PickerItem::SavedHeader { session_count: 1 }))
    );
}

#[test]
pub(super) fn test_rebuild_items_prunes_selected_sessions_hidden_by_filter() {
    let mut saved = make_session("session_saved", "saved", false, SessionStatus::Closed);
    saved.saved = true;
    let normal = make_session("session_normal", "normal", false, SessionStatus::Closed);

    let mut picker = SessionPicker::new(vec![saved, normal]);
    picker
        .selected_session_ids
        .insert("session_saved".to_string());
    picker
        .selected_session_ids
        .insert("session_normal".to_string());

    picker.filter_mode = SessionFilterMode::Saved;
    picker.rebuild_items();

    assert_eq!(picker.selected_session_ids.len(), 1);
    assert!(picker.selected_session_ids.contains("session_saved"));
}

#[test]
pub(super) fn test_mouse_scroll_only_affects_hovered_pane_without_changing_focus() {
    let s1 = make_session("session_1", "one", false, SessionStatus::Closed);
    let s2 = make_session("session_2", "two", false, SessionStatus::Closed);
    let s3 = make_session("session_3", "three", false, SessionStatus::Closed);
    let mut picker = SessionPicker::new(vec![s1, s2, s3]);

    picker.focus = PaneFocus::Preview;
    picker.scroll_offset = 7;
    picker.last_list_area = Some(Rect::new(0, 0, 20, 10));
    picker.last_preview_area = Some(Rect::new(20, 0, 20, 10));

    picker.handle_overlay_mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::empty(),
    });

    assert_eq!(picker.focus, PaneFocus::Preview);
    assert_eq!(picker.scroll_offset, 0);
    assert_eq!(
        picker.selected_session().map(|s| s.id.as_str()),
        Some("session_2")
    );
}

#[test]
pub(super) fn test_keyboard_scroll_uses_sessions_focus_for_paging() {
    let s1 = make_session("session_1", "one", false, SessionStatus::Closed);
    let s2 = make_session("session_2", "two", false, SessionStatus::Closed);
    let s3 = make_session("session_3", "three", false, SessionStatus::Closed);
    let s4 = make_session("session_4", "four", false, SessionStatus::Closed);
    let mut picker = SessionPicker::new(vec![s1, s2, s3, s4]);

    picker.focus = PaneFocus::Sessions;
    picker.scroll_offset = 6;

    let result = picker.handle_overlay_key(KeyCode::PageDown, KeyModifiers::empty());

    assert!(matches!(result, Ok(OverlayAction::Continue)));
    assert_eq!(picker.focus, PaneFocus::Sessions);
    assert_eq!(picker.scroll_offset, 0);
    assert_eq!(
        picker.selected_session().map(|s| s.id.as_str()),
        Some("session_1")
    );
}

#[test]
pub(super) fn test_keyboard_scroll_uses_preview_focus_for_paging() {
    let s1 = make_session("session_1", "one", false, SessionStatus::Closed);
    let s2 = make_session("session_2", "two", false, SessionStatus::Closed);
    let mut picker = SessionPicker::new(vec![s1, s2]);

    picker.focus = PaneFocus::Preview;

    let result = picker.handle_overlay_key(KeyCode::PageDown, KeyModifiers::empty());

    assert!(matches!(result, Ok(OverlayAction::Continue)));
    assert_eq!(picker.focus, PaneFocus::Preview);
    assert_eq!(picker.scroll_offset, PREVIEW_PAGE_SCROLL);
    assert_eq!(
        picker.selected_session().map(|s| s.id.as_str()),
        Some("session_2")
    );
}

/// Build a session with many short user/assistant turns so the preview overflows
/// a small viewport (used to exercise the preview scrollbar + sticky header).
pub(super) fn make_session_with_many_turns(id: &str, turns: usize) -> SessionInfo {
    let mut session = make_session(id, id, false, SessionStatus::Closed);
    let mut preview = Vec::new();
    for i in 0..turns {
        preview.push(PreviewMessage {
            role: "user".to_string(),
            content: format!("user prompt number {i}"),
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        });
        preview.push(PreviewMessage {
            role: "assistant".to_string(),
            content: format!("assistant reply number {i}"),
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        });
    }
    session.first_user_prompt = preview.first().map(|m| m.content.clone());
    session.messages_preview = preview;
    session
}

pub(super) fn picker_buffer(picker: &mut SessionPicker, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| picker.render(frame))
        .expect("render picker");
    terminal.backend().buffer().clone()
}

pub(super) fn buffer_text(picker: &mut SessionPicker, w: u16, h: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| picker.render(frame))
        .expect("render picker");
    let buffer = terminal.backend().buffer().clone();
    buffer.content().iter().map(|cell| cell.symbol()).collect()
}

// ---------------------------------------------------------------------------
// Developer benchmarks: profile the operations exercised by the `/resume`
// overlay. These are `#[ignore]`d so they never run in CI; run them with:
//
//   cargo test -p jcode-tui --lib --release -- --ignored --nocapture benchmark_resume_op
//
// They print human-readable timing lines to stderr. They use synthetic
// sessions so they are deterministic and independent of the user's session
// store.
// ---------------------------------------------------------------------------

/// Build a synthetic preview message list that mimics a realistic conversation:
/// alternating user prompts and multi-paragraph markdown assistant replies. The
/// assistant content includes markdown (headers, lists, code) so it exercises
/// the same markdown render + wrap path as the real preview.
pub(super) fn bench_preview_messages(turns: usize, assistant_paragraphs: usize) -> Vec<PreviewMessage> {
    let mut preview = Vec::with_capacity(turns * 2);
    for turn in 0..turns {
        preview.push(PreviewMessage {
            role: "user".to_string(),
            content: format!(
                "Prompt {turn}: can you refactor the session picker so that the preview \
                 pane does not rebuild and re-wrap every line on every single frame?"
            ),
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        });

        let mut body = String::new();
        body.push_str(&format!("## Response {turn}\n\n"));
        for para in 0..assistant_paragraphs {
            body.push_str(&format!(
                "Here is paragraph {para} of a longer answer that wraps across several \
                 terminal columns and therefore costs real work to lay out. It mentions \
                 `render_preview`, `wrap_lines`, and the scroll offset so the markdown \
                 renderer has inline code spans to style.\n\n"
            ));
            body.push_str("- a bullet point that also needs wrapping and styling\n");
            body.push_str("- another bullet with `inline_code` to style\n\n");
        }
        body.push_str("```rust\nlet scroll = self.scroll_offset as usize; // cached?\n```\n");
        preview.push(PreviewMessage {
            role: "assistant".to_string(),
            content: body,
            tool_calls: Vec::new(),
            tool_data: None,
            duration_secs: None,
            title: None,
            timestamp: None,
        });
    }
    preview
}

/// A session whose preview is large enough to overflow the viewport and require
/// scrolling (the case the user reported as slow).
pub(super) fn bench_large_session(id: &str, turns: usize, assistant_paragraphs: usize) -> SessionInfo {
    let mut session = make_session(id, id, false, SessionStatus::Closed);
    let preview = bench_preview_messages(turns, assistant_paragraphs);
    session.first_user_prompt = preview.first().map(|m| m.content.clone());
    session.estimated_tokens = 4_000 * turns;
    session.message_count = turns * 2;
    session.user_message_count = turns;
    session.assistant_message_count = turns;
    session.messages_preview = preview;
    session
}

pub(super) fn bench_render_full(picker: &mut SessionPicker, w: u16, h: u16) -> std::time::Duration {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let start = std::time::Instant::now();
    terminal
        .draw(|frame| picker.render(frame))
        .expect("render picker");
    start.elapsed()
}

pub(super) fn bench_render_preview_only(picker: &mut SessionPicker, area: Rect) -> std::time::Duration {
    // Render into a backend sized exactly to the area, placing it at the origin
    // (the preview/list rendering only depends on width/height, not x/y).
    let area = Rect::new(0, 0, area.width, area.height);
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let start = std::time::Instant::now();
    terminal
        .draw(|frame| picker.render_preview(frame, area))
        .expect("render preview");
    start.elapsed()
}

pub(super) fn bench_render_list_only(picker: &mut SessionPicker, area: Rect) -> std::time::Duration {
    let area = Rect::new(0, 0, area.width, area.height);
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let start = std::time::Instant::now();
    terminal
        .draw(|frame| picker.render_session_list(frame, area))
        .expect("render list");
    start.elapsed()
}

pub(super) fn bench_median(mut samples: Vec<std::time::Duration>) -> std::time::Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Profile the cost of a single preview-scroll frame. This is the operation the
/// user reported as slow: after scrolling, every frame rebuilds + re-wraps the
/// entire preview. We render once to warm any lazy state, then time repeated
/// scroll-and-render ticks and attribute time to the preview vs the (unchanged)
/// session list.
#[test]
#[ignore = "developer benchmark: profiles /resume preview scroll frame cost"]
pub(super) fn benchmark_resume_op_preview_scroll_frame_cost() {
    const W: u16 = 120;
    const H: u16 = 40;
    let main_area = Rect::new(0, 0, W, H);
    // Mirrors render(): list = 40%, preview = 60% of the width.
    let list_area = Rect::new(0, 0, (W as f32 * 0.40) as u16, H);
    let preview_area = Rect::new(list_area.width, 0, W - list_area.width, H);

    for &(turns, paras) in &[(20usize, 2usize), (80, 3), (200, 4)] {
        let session = bench_large_session("scroll_bench", turns, paras);
        let preview_len = session.messages_preview.len();
        let mut picker = SessionPicker::new(vec![session]);
        picker.focus = PaneFocus::Preview;

        // Warm render (auto-scrolls to bottom, builds wrap state once).
        let _ = bench_render_full(&mut picker, W, H);

        const ITERS: usize = 60;
        let mut full_samples = Vec::with_capacity(ITERS);
        let mut preview_samples = Vec::with_capacity(ITERS);
        let mut list_samples = Vec::with_capacity(ITERS);
        for i in 0..ITERS {
            // Alternate scroll direction so we exercise both bounds.
            if i % 2 == 0 {
                picker.scroll_preview_up(1);
            } else {
                picker.scroll_preview_down(1);
            }
            full_samples.push(bench_render_full(&mut picker, W, H));
            preview_samples.push(bench_render_preview_only(&mut picker, preview_area));
            list_samples.push(bench_render_list_only(&mut picker, list_area));
        }

        let full = bench_median(full_samples);
        let preview = bench_median(preview_samples);
        let list = bench_median(list_samples);
        eprintln!(
            "preview scroll frame: turns={turns} paras={paras} preview_msgs={preview_len} \
             area={}x{} | full_frame={:>6.0}us preview_only={:>6.0}us list_only={:>6.0}us \
             (preview is {:.0}% of frame)",
            main_area.width,
            main_area.height,
            full.as_nanos() as f64 / 1000.0,
            preview.as_nanos() as f64 / 1000.0,
            list.as_nanos() as f64 / 1000.0,
            preview.as_nanos() as f64 / full.as_nanos().max(1) as f64 * 100.0,
        );
    }
}

/// Profile how list rendering scales with the number of sessions. Because
/// `render_session_list` rebuilds a `ListItem` for *every* session each frame
/// (not just the visible window), this should grow ~linearly with N even though
/// only ~H rows are visible. Relevant to scroll because the list is redrawn on
/// every preview-scroll frame too.
#[test]
#[ignore = "developer benchmark: profiles /resume session-list render scaling vs N"]
pub(super) fn benchmark_resume_op_list_render_scaling() {
    const W: u16 = 48;
    const H: u16 = 40;
    let list_area = Rect::new(0, 0, W, H);

    for &n in &[50usize, 200, 1000, 3000] {
        let sessions: Vec<SessionInfo> = (0..n)
            .map(|i| {
                make_session(
                    &format!("scale_{i}"),
                    &format!("session {i}"),
                    false,
                    SessionStatus::Closed,
                )
            })
            .collect();
        let mut picker = SessionPicker::new(sessions);
        picker.focus = PaneFocus::Sessions;
        let _ = bench_render_list_only(&mut picker, list_area);

        const ITERS: usize = 30;
        let mut samples = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            samples.push(bench_render_list_only(&mut picker, list_area));
        }
        let m = bench_median(samples);
        eprintln!(
            "list render scaling: N={n:>5} visible_rows~={} | list_render={:>7.0}us \
             ({:.1}us/session)",
            H.saturating_sub(2),
            m.as_nanos() as f64 / 1000.0,
            m.as_nanos() as f64 / 1000.0 / n as f64,
        );
    }
}

/// Profile a search keystroke (`rebuild_items` + the cached search narrowing)
/// as the query grows, plus the cost of clearing the query (the non-prefix /
/// backspace path that cannot reuse the narrowing cache).
#[test]
#[ignore = "developer benchmark: profiles /resume search keystroke cost"]
pub(super) fn benchmark_resume_op_search_keystroke() {
    for &n in &[200usize, 1000, 3000] {
        let sessions: Vec<SessionInfo> = (0..n)
            .map(|i| {
                make_session(
                    &format!("search_{i}"),
                    &format!("session about topic {} number {i}", i % 17),
                    false,
                    SessionStatus::Closed,
                )
            })
            .collect();
        let mut picker = SessionPicker::new(sessions);

        // Progressive typing: each keystroke appends one char and rebuilds.
        let query = "session about topic 3";
        let mut typed = String::new();
        let mut keystroke_samples = Vec::new();
        for ch in query.chars() {
            typed.push(ch);
            picker.search_query = typed.clone();
            picker.search_active = true;
            let start = std::time::Instant::now();
            picker.rebuild_items();
            keystroke_samples.push(start.elapsed());
        }
        let typed_median = bench_median(keystroke_samples.clone());
        let typed_worst = keystroke_samples.iter().copied().max().unwrap();

        // Clearing the search (full re-scan, no narrowing cache reuse).
        picker.search_query.clear();
        picker.search_active = false;
        let clear_start = std::time::Instant::now();
        picker.rebuild_items();
        let clear_elapsed = clear_start.elapsed();

        eprintln!(
            "search keystroke: N={n:>5} | per_keystroke_median={:>6.0}us worst={:>6.0}us \
             clear_query={:>6.0}us",
            typed_median.as_nanos() as f64 / 1000.0,
            typed_worst.as_nanos() as f64 / 1000.0,
            clear_elapsed.as_nanos() as f64 / 1000.0,
        );
    }
}

/// Profile navigating the session list (next/previous) followed by a re-render,
/// across list sizes. Navigation resets preview scroll and triggers a full
/// re-render of both panes.
#[test]
#[ignore = "developer benchmark: profiles /resume list navigation frame cost"]
pub(super) fn benchmark_resume_op_nav_frame_cost() {
    const W: u16 = 120;
    const H: u16 = 40;

    for &n in &[50usize, 500, 2000] {
        let sessions: Vec<SessionInfo> = (0..n)
            .map(|i| bench_large_session(&format!("nav_{i}"), 6, 2))
            .collect();
        let mut picker = SessionPicker::new(sessions);
        picker.focus = PaneFocus::Sessions;
        let _ = bench_render_full(&mut picker, W, H);

        const ITERS: usize = 40;
        let mut samples = Vec::with_capacity(ITERS);
        for i in 0..ITERS {
            if i % 2 == 0 {
                picker.next();
            } else {
                picker.previous();
            }
            samples.push(bench_render_full(&mut picker, W, H));
        }
        let m = bench_median(samples);
        eprintln!(
            "nav frame: N={n:>5} | nav+full_render_median={:>7.0}us",
            m.as_nanos() as f64 / 1000.0,
        );
    }
}

/// Profile constructing the picker (`new`) and the initial `rebuild_items`
/// across list sizes, isolating the non-IO construction cost that runs
/// synchronously when `/resume` opens.
#[test]
#[ignore = "developer benchmark: profiles /resume picker construction cost vs N"]
pub(super) fn benchmark_resume_op_construction_cost() {
    for &n in &[200usize, 1000, 5000] {
        let sessions: Vec<SessionInfo> = (0..n)
            .map(|i| {
                make_session(
                    &format!("ctor_{i}"),
                    &format!("session {i}"),
                    false,
                    SessionStatus::Closed,
                )
            })
            .collect();

        const ITERS: usize = 20;
        let mut samples = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let clone = sessions.clone();
            let start = std::time::Instant::now();
            let _picker = SessionPicker::new(clone);
            samples.push(start.elapsed());
        }
        let m = bench_median(samples);
        eprintln!(
            "construction: N={n:>5} | new()+rebuild_items_median={:>7.0}us ({:.2}us/session)",
            m.as_nanos() as f64 / 1000.0,
            m.as_nanos() as f64 / 1000.0 / n as f64,
        );
    }
}

/// Any of the native scrollbar thumb glyphs (see `render_native_scrollbar`).
pub(super) fn contains_scrollbar_glyph(text: &str) -> bool {
    text.contains('•') || text.contains('╷') || text.contains('╵') || text.contains('│')
}

#[test]
pub(super) fn test_preview_transcript_omits_picker_metadata() {
    let _render_state = crate::tui::ui::render_state_test_lock();
    let _changelog =
        crate::tui::ui::header::scoped_unseen_changelog_entries_override_for_tests(vec![
            "Deterministic preview changelog fixture".to_string(),
        ]);
    let mut session = make_session("metadata", "metadata-monkey", false, SessionStatus::Active);
    session.title = "metadata-video-title".to_string();
    session.working_dir = Some("/metadata-only-directory".to_string());
    let mut picker = SessionPicker::new(vec![session]);
    picker.auto_scroll_preview = false;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).expect("test terminal");
    terminal
        .draw(|frame| picker.render_preview(frame, frame.area()))
        .unwrap();
    let cache = picker.preview_cache.as_ref().expect("rendered transcript");
    let text = cache
        .wrapped_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("1› hello"), "{text}");
    assert!(text.contains("Updates"), "{text}");
    assert!(
        text.contains("Deterministic preview changelog fixture"),
        "{text}"
    );
    assert!(
        text.contains("client: Metadata-monkey"),
        "the real startup header belongs to the scrolling transcript"
    );
    for metadata in ["metadata-video-title", "▶ Active"] {
        assert!(
            !text.contains(metadata),
            "unexpected picker metadata: {text}"
        );
    }
    // Startup Updates boxes legitimately contain horizontal borders. Only the
    // standalone picker separator is metadata, not every run of border glyphs.
    assert!(
        !text.lines().any(|line| {
            let line = line.trim();
            line.starts_with("────") && line.chars().all(|ch| ch == '─')
        }),
        "unexpected standalone picker separator: {text}"
    );
}

#[test]
pub(super) fn preview_transcript_hash_ignores_picker_metadata() {
    let session = make_session("metadata", "metadata-monkey", false, SessionStatus::Active);
    let picker = SessionPicker::new(vec![session.clone()]);
    let mode = crate::config::DiffDisplayMode::Off;
    let original = picker.preview_content_hash(&session, mode);
    let mut changed = session;
    changed.title = "renamed".into();
    changed.status = SessionStatus::Closed;
    changed.saved = true;
    changed.working_dir = Some("/another-directory".into());
    assert_eq!(original, picker.preview_content_hash(&changed, mode));
    changed.messages_preview[1].content.push_str(" updated");
    assert_ne!(original, picker.preview_content_hash(&changed, mode));
}

#[test]
pub(super) fn test_preview_is_left_aligned_independently_of_chat_markdown_context() {
    let _env_lock = crate::storage::lock_test_env();
    // Renders share process-global layout and markdown caches.
    let _render_state = crate::tui::ui::render_state_test_lock();
    for width in [60, 100] {
        let mut reference = None;
        for centered in [false, true] {
            markdown::with_center_code_blocks(centered, || {
                let mut session =
                    make_session("alignment", "alignment", false, SessionStatus::Closed);
                session.messages_preview[1].content =
                    "world\n\n- list item\n\n```text\ncode sample\n```".to_string();
                session.messages_preview[1].tool_calls = vec!["read".to_string()];
                let mut picker = SessionPicker::new(vec![session]);
                picker.auto_scroll_preview = false;
                let backend = ratatui::backend::TestBackend::new(width, 40);
                let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
                // Exercise both the initial render and a cached redraw.
                for _ in 0..2 {
                    terminal
                        .draw(|frame| picker.render_preview(frame, frame.area()))
                        .expect("render preview");
                    assert_eq!(markdown::center_code_blocks(), centered);
                    let buffer = terminal.backend().buffer();
                    let rows: Vec<String> = (1..39)
                        .map(|y| (1..width - 1).map(|x| buffer[(x, y)].symbol()).collect())
                        .collect();
                    for text in ["1› hello", "world"] {
                        let row = rows.iter().find(|row| row.contains(text)).expect(text);
                        assert!(
                            row.starts_with(&format!(" {text}")),
                            "preview must use the live one-cell gutter: {row:?}"
                        );
                    }
                    assert!(
                        rows.iter().all(|row| !row.contains("tool:")),
                        "preview must not show a tool-name summary line: {rows:?}"
                    );
                    for text in ["list item", "code sample"] {
                        let row = rows.iter().find(|row| row.contains(text)).expect(text);
                        assert!(
                            row.chars().take_while(|c| *c == ' ').count() <= 2,
                            "structured content must not inherit centering padding: {row:?}"
                        );
                    }
                    if let Some(reference) = &reference {
                        assert_eq!(&rows, reference, "chat centering must not affect preview");
                    } else {
                        reference = Some(rows);
                    }
                }
            });
        }
    }
}

#[test]
pub(super) fn test_preview_structured_messages_stay_left_aligned() {
    // The todo tool card is only shown inline when todos are not pinned.
    let _env_lock = crate::storage::lock_test_env();
    crate::env::set_var("JCODE_PIN_TODOS", "0");
    crate::config::invalidate_config_cache();
    struct RestorePin;
    impl Drop for RestorePin {
        fn drop(&mut self) {
            crate::env::remove_var("JCODE_PIN_TODOS");
            crate::config::invalidate_config_cache();
        }
    }
    let _restore = RestorePin;
    let todos = serde_json::json!([{
        // Finished lists intentionally collapse to status dots. Keep an active
        // item so this test exercises alignment of the structured card body.
        "id": "alignment", "content": "Verify structured preview alignment",
        "status": "in_progress", "priority": "high", "confidence": "verified"
    }]);
    let mut session = make_session(
        "structured_alignment",
        "alignment",
        false,
        SessionStatus::Closed,
    );
    session.messages_preview[1].tool_calls = vec!["todo".to_string()];
    for (role, content, tool) in [
        ("tool", todos.to_string(), Some("todo")),
        ("system", "🔍 Reviewing the weak points of this turn for you...".to_string(), None),
        ("tool", "Command completed successfully (no output)".to_string(), Some("bash")),
        ("background_task", "**Background task** `alignment-task` · `selfdev test` (`selfdev-test`) · ✓ completed · 18.5s · exit 0\n\n```text\nAll alignment checks passed\n```".to_string(), None),
    ] {
        session.messages_preview.push(PreviewMessage {
            role: role.to_string(), content, tool_calls: Vec::new(), duration_secs: None, title: None, timestamp: None,
            tool_data: tool.map(|name| crate::message::ToolCall {
                id: format!("alignment-{name}"), name: name.to_string(),
                input: serde_json::json!({"command": "echo alignment"}),
                intent: Some("Check structured preview alignment".to_string()),
                thought_signature: None,
            }),
        });
    }
    for width in [100, 200, 320] {
        markdown::with_center_code_blocks(true, || {
            let mut picker = SessionPicker::new(vec![session.clone()]);
            picker.auto_scroll_preview = false;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 60)).unwrap();
            terminal
                .draw(|frame| picker.render_preview(frame, frame.area()))
                .unwrap();
            assert!(
                markdown::center_code_blocks(),
                "preview must restore chat context"
            );
            let cache = picker.preview_cache.as_ref().unwrap();
            assert!(
                cache
                    .wrapped_lines
                    .iter()
                    .all(|line| !line_text(line).contains("tool:")),
                "preview must not show a tool-name summary line"
            );
            for needle in [
                "Verify structured preview alignment",
                "Reviewing the weak points",
                "Check structured preview alignment",
                // Successful background notices render as one compact line.
                "Background task alignment-task",
            ] {
                let line = cache
                    .wrapped_lines
                    .iter()
                    .find(|line| line_text(line).contains(needle))
                    .unwrap_or_else(|| panic!("missing {needle}"));
                let text = line_text(line);
                assert_eq!(line.alignment, Some(Alignment::Left), "{needle}: {line:?}");
                assert!(
                    text.chars().take_while(|c| *c == ' ').count() <= 4,
                    "unwanted centering at width {width}: {text:?}"
                );
            }
        });
    }
}

#[test]
pub(super) fn test_preview_pane_shows_scrollbar_when_overflowing() {
    let session = make_session_with_many_turns("preview_scroll", 60);
    let mut picker = SessionPicker::new(vec![session]);
    picker.focus = PaneFocus::Preview;

    // Small height so the long preview overflows and needs a scrollbar.
    let text = buffer_text(&mut picker, 100, 16);
    assert!(
        contains_scrollbar_glyph(&text),
        "preview scrollbar glyph should render when content overflows:\n{text}"
    );
}

#[test]
pub(super) fn test_session_list_shows_scrollbar_when_overflowing() {
    // Many sessions so the left list overflows a short viewport.
    let sessions: Vec<SessionInfo> = (0..40)
        .map(|i| {
            make_session(
                &format!("list_scroll_{i}"),
                &format!("s{i}"),
                false,
                SessionStatus::Closed,
            )
        })
        .collect();
    let mut picker = SessionPicker::new(sessions);
    picker.focus = PaneFocus::Sessions;

    let text = buffer_text(&mut picker, 100, 16);
    assert!(
        contains_scrollbar_glyph(&text),
        "session list scrollbar glyph should render when list overflows:\n{text}"
    );
}

#[test]
pub(super) fn test_preview_sticky_prompt_header_appears_after_scrolling() {
    let session = make_session_with_many_turns("sticky_header", 60);
    let mut picker = SessionPicker::new(vec![session]);
    picker.focus = PaneFocus::Preview;

    // First render auto-scrolls to the bottom; the topmost prompts are off-screen,
    // so a dimmed "N› ..." sticky header should pin a prior prompt at the top of
    // the preview's content area.
    let w = 100u16;
    let h = 16u16;
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| picker.render(frame))
        .expect("render picker");
    let buffer = terminal.backend().buffer().clone();

    // Startup chrome scrolls away. The sticky previous prompt is the first row.
    let preview = picker.last_preview_area.expect("preview pane");
    let preview_inner_x = preview.x;
    let header_y = preview.y;
    let header_row: String = (preview_inner_x..preview.right().saturating_sub(1))
        .map(|x| buffer[(x, header_y)].symbol())
        .collect();
    assert!(
        header_row.contains('›'),
        "sticky prompt header should pin a numbered prompt at the top of the preview:\n\
         row={header_row:?}"
    );
    // The header marker is a prompt number followed by the chevron.
    assert!(
        header_row
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        "sticky header should begin flush left with a prompt number:\nrow={header_row:?}"
    );
}

#[test]
pub(super) fn test_preview_sticky_prompt_header_survives_async_preview_load() {
    // Regression: when the selected session's transcript is still loading on a
    // background thread, the first render only shows a "Loading…" placeholder
    // (max_scroll == 0). The auto-scroll flag must NOT be consumed on that
    // placeholder frame; otherwise the populated transcript stays pinned at the
    // top and the sticky "previous prompt" header never appears (it only renders
    // when scrolled past a prompt). This reproduces the intermittent "/resume
    // sometimes doesn't show your last prompt at the top" bug.
    let mut session = make_session_with_many_turns("async_sticky", 60);
    let full_preview = std::mem::take(&mut session.messages_preview);
    session.first_user_prompt = full_preview.first().map(|m| m.content.clone());

    let mut picker = SessionPicker::new(vec![session.clone()]);
    picker.focus = PaneFocus::Preview;

    // Simulate an in-flight background load for the selected session: empty
    // preview + a pending load whose id matches. Keep the sender alive so the
    // receiver reports `Empty` (still loading) rather than `Disconnected`.
    let (tx, rx) = std::sync::mpsc::channel::<Option<LoadedSessionPreview>>();
    picker.pending_preview_load = Some(PendingSessionPreviewLoad {
        session_id: "async_sticky".to_string(),
        receiver: rx,
    });

    let w = 100u16;
    let h = 16u16;
    let render = |picker: &mut SessionPicker| {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame))
            .expect("render picker");
        terminal.backend().buffer().clone()
    };

    // First frame: transcript still loading -> placeholder shown, auto-scroll
    // must remain armed.
    let _ = render(&mut picker);
    assert!(
        picker.auto_scroll_preview,
        "auto-scroll should stay armed while the preview is still loading"
    );

    // The initial session-list refresh can arrive while the transcript is
    // loading. It must preserve the pending bottom scroll for this selection.
    picker.reseed_grouped(Vec::new(), vec![session]);
    assert!(
        picker.auto_scroll_preview,
        "refresh must preserve the pending jump to the transcript bottom"
    );

    // The background load completes: deliver the real transcript exactly the way
    // `poll_preview_load` would (drop the channel + populate the preview).
    drop(tx);
    picker.pending_preview_load = None;
    picker.apply_session_preview("async_sticky", full_preview);

    // Second frame: now that content is present we snap to the bottom and the
    // top prompts scroll off-screen, so the sticky header should pin a prompt.
    let buffer = render(&mut picker);
    assert!(
        picker.scroll_offset > 0,
        "preview should auto-scroll to the bottom once content loads, got {}",
        picker.scroll_offset
    );

    assert!(picker.preview_max_scroll > 0);
    assert_eq!(
        picker.scroll_offset, picker.preview_max_scroll,
        "loaded transcript must open at the exact bottom"
    );
    assert!(!picker.auto_scroll_preview);

    // Subsequent refreshes must not pull a user who scrolled back to the bottom.
    assert!(picker.scroll_preview_up(3));
    let user_offset = picker.scroll_offset;
    let loaded_session = picker.selected_session().unwrap().clone();
    picker.reseed_grouped(Vec::new(), vec![loaded_session]);
    let _ = render(&mut picker);
    assert_eq!(picker.scroll_offset, user_offset);
    assert!(!picker.auto_scroll_preview);

    let preview = picker.last_preview_area.expect("preview pane");
    let preview_inner_x = preview.x;
    let header_y = preview.y;
    let header_row: String = (preview_inner_x..preview.right().saturating_sub(1))
        .map(|x| buffer[(x, header_y)].symbol())
        .collect();
    assert!(
        header_row.contains('›'),
        "sticky prompt header should appear after an async preview load:\n\
         row={header_row:?}"
    );
    assert!(
        header_row
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit()),
        "sticky header should begin with a prompt number:\nrow={header_row:?}"
    );
}

#[test]
pub(super) fn preview_render_cache_is_reused_across_scroll_and_rebuilt_on_selection_change() {
    // The preview pane caches its fully-wrapped content keyed by a content hash
    // and pane geometry, so scrolling reuses the cache instead of re-rendering
    // and re-wrapping every line. Navigating to another session must invalidate
    // it (different content hash).
    let a = make_session_with_many_turns("cache_a", 60);
    let b = make_session_with_many_turns("cache_b", 60);
    let mut picker = SessionPicker::new(vec![a, b]);
    picker.focus = PaneFocus::Preview;

    let w = 100u16;
    let h = 16u16;
    let render = |picker: &mut SessionPicker| {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame))
            .expect("render picker");
    };

    // First render builds the cache for the selected session.
    render(&mut picker);
    let key_after_build = picker
        .preview_cache
        .as_ref()
        .map(|c| c.key.clone())
        .expect("preview cache built on first render");
    let wrapped_len = picker
        .preview_cache
        .as_ref()
        .map(|c| c.wrapped_lines.len())
        .unwrap();
    assert!(wrapped_len > h as usize, "preview should overflow viewport");

    // Scrolling several times must not change the cache key (content unchanged):
    // the cache is reused and only the scroll offset + visible slice move.
    for _ in 0..5 {
        picker.scroll_preview_up(1);
        render(&mut picker);
        let key_now = picker
            .preview_cache
            .as_ref()
            .map(|c| c.key.clone())
            .unwrap();
        assert!(
            key_now == key_after_build,
            "scrolling must reuse the cached wrapped preview"
        );
    }

    // Navigating to a different session changes the content hash, so the cache
    // is rebuilt for the new selection.
    picker.next();
    render(&mut picker);
    let key_after_nav = picker
        .preview_cache
        .as_ref()
        .map(|c| c.key.clone())
        .unwrap();
    assert!(
        key_after_nav != key_after_build,
        "selecting a different session must invalidate the preview cache"
    );
}

#[test]
pub(super) fn preview_visible_slice_matches_scroll_position() {
    // The renderer materializes only the visible window of wrapped lines. Confirm
    // that scrolling actually changes what is drawn (i.e. the slice tracks the
    // scroll offset rather than always showing the bottom).
    let session = make_session_with_many_turns("slice", 60);
    let mut picker = SessionPicker::new(vec![session]);
    picker.focus = PaneFocus::Preview;

    let w = 100u16;
    let h = 16u16;
    let render_text = |picker: &mut SessionPicker| -> String { buffer_text(picker, w, h) };

    // First render auto-scrolls to the bottom.
    let bottom = render_text(&mut picker);
    let bottom_scroll = picker.scroll_offset;
    assert!(bottom_scroll > 0, "long preview should be scrolled down");

    // Scroll to the very top; the rendered content must differ from the bottom.
    picker.scroll_preview_up(bottom_scroll);
    let top = render_text(&mut picker);
    assert_eq!(picker.scroll_offset, 0);
    assert_ne!(
        top, bottom,
        "scrolling to the top should render different content than the bottom"
    );
}

#[test]
pub(super) fn test_reseed_grouped_preserves_selection_and_search() {
    // Build a picker with several sessions, then simulate the user navigating to
    // a specific session and typing a search. A background refresh that reseeds
    // the same data must keep the highlighted session and the active search.
    let sessions: Vec<SessionInfo> = (0..6)
        .map(|i| {
            make_session(
                &format!("session_reseed_{i}"),
                &format!("reseed{i}"),
                false,
                SessionStatus::Closed,
            )
        })
        .collect();
    let mut picker = SessionPicker::new(sessions.clone());

    // Move selection to the third visible session.
    picker.next();
    picker.next();
    let selected_before = picker
        .selected_session()
        .map(|s| s.id.clone())
        .expect("a session should be selected");

    // Activate a search that matches only one session id.
    picker.search_query = "reseed4".to_string();
    picker.search_active = true;
    picker.focus = PaneFocus::Preview;
    picker.rebuild_items();
    wait_for_picker_search(&mut picker);
    let search_selected = picker
        .selected_session()
        .map(|s| s.id.clone())
        .expect("search should leave a selection");

    // Reseed with the same data (as the async refresh would).
    picker.reseed_grouped(Vec::new(), sessions);

    wait_for_picker_search(&mut picker);
    // Search query, search mode, focus, and the matched selection survive.
    assert_eq!(picker.search_query, "reseed4");
    assert!(picker.search_active);
    assert_eq!(picker.focus, PaneFocus::Preview);
    assert_eq!(
        picker.selected_session().map(|s| s.id.clone()),
        Some(search_selected)
    );

    // Clearing the search restores the full list; the originally highlighted
    // session is still resolvable in the reseeded data.
    picker.search_query.clear();
    picker.search_active = false;
    picker.rebuild_items();
    assert!(
        picker
            .visible_session_iter_for_test()
            .any(|s| s.id == selected_before),
        "previously selected session should still be present after reseed"
    );
}
