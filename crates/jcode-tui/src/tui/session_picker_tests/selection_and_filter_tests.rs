use super::*;

#[test]
pub(super) fn test_collect_recent_session_stems_prefers_recently_modified_long_running_session() {
    let dir = tempfile::TempDir::new().expect("tempdir");

    for idx in 0..120 {
        write_session_file_with_mtime(
            dir.path().join(format!(
                "session_newer_created_{:013}.json",
                2_000_000 + idx
            )),
            r#"{"messages":[{"role":"user","content":"short newer-created session"}]}"#,
            1000 + idx,
        );
    }

    let target = "session_long_running_0000000000500";
    write_session_file_with_mtime(
        dir.path().join(format!("{target}.json")),
        r#"{"messages":[{"role":"user","content":"old creation time, recently active"}]}"#,
        10_000,
    );

    let stems = collect_recent_session_stems(dir.path(), 100).expect("collect stems");
    assert_eq!(stems.first().map(String::as_str), Some(target));
    assert!(stems.iter().any(|stem| stem == target));
}

#[test]
pub(super) fn test_toggle_test_sessions_rebuilds_visibility() {
    let normal = make_session("session_normal", "normal", false, SessionStatus::Closed);
    let debug = make_session("session_debug", "debug", true, SessionStatus::Closed);

    let mut picker = SessionPicker::new(vec![normal.clone(), debug.clone()]);

    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(!picker.show_test_sessions);
    assert_eq!(picker.hidden_test_count, 1);

    picker.toggle_test_sessions();
    assert!(picker.show_test_sessions);
    assert_eq!(picker.visible_sessions.len(), 2);
    assert_eq!(picker.hidden_test_count, 0);

    picker.toggle_test_sessions();
    assert!(!picker.show_test_sessions);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert_eq!(picker.hidden_test_count, 1);
}

#[test]
pub(super) fn test_new_grouped_hides_debug_by_default() {
    let normal = make_session("session_normal", "normal", false, SessionStatus::Closed);
    let debug = make_session("session_debug", "debug", true, SessionStatus::Closed);
    let canary = make_session_with_flags(
        "session_canary",
        "canary",
        false,
        true,
        SessionStatus::Closed,
    );
    let orphan_normal = make_session(
        "orphan_normal",
        "orphan-normal",
        false,
        SessionStatus::Closed,
    );
    let orphan_debug = make_session("orphan_debug", "orphan-debug", true, SessionStatus::Closed);

    let groups = vec![ServerGroup {
        name: "main".to_string(),
        icon: "🛰".to_string(),
        version: "v0.1.0".to_string(),
        git_hash: "abc1234".to_string(),
        is_running: true,
        sessions: vec![normal.clone(), debug.clone(), canary.clone()],
    }];

    let mut picker = SessionPicker::new_grouped(groups, vec![orphan_normal, orphan_debug]);

    assert!(!picker.show_test_sessions);
    // Canary sessions are now visible by default, only debug sessions are hidden
    assert_eq!(picker.visible_sessions.len(), 3); // normal + canary + orphan_normal
    assert!(picker.visible_session_iter().all(|s| !s.is_debug));
    assert_eq!(picker.hidden_test_count, 2); // debug + orphan_debug

    picker.toggle_test_sessions();
    assert!(picker.show_test_sessions);
    assert_eq!(picker.visible_sessions.len(), 5);
    assert_eq!(picker.hidden_test_count, 0);
    assert!(picker.visible_session_iter().any(|s| s.is_debug));
    assert!(picker.visible_session_iter().any(|s| s.is_canary));
}

#[test]
pub(super) fn test_new_grouped_without_servers_shows_orphan_sessions() {
    let normal = make_session("session_normal", "normal", false, SessionStatus::Closed);
    let debug = make_session("session_debug", "debug", true, SessionStatus::Closed);

    let mut picker = SessionPicker::new_grouped(Vec::new(), vec![normal, debug]);

    assert!(!picker.show_test_sessions);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(picker.visible_session_iter().all(|s| !s.is_debug));
    assert_eq!(picker.hidden_test_count, 1);
    assert_eq!(picker.items.len(), 1);
    assert_eq!(picker.list_state.selected(), Some(0));

    picker.toggle_test_sessions();
    assert!(picker.show_test_sessions);
    assert_eq!(picker.visible_sessions.len(), 2);
    assert_eq!(picker.hidden_test_count, 0);
    assert_eq!(picker.items.len(), 2);
    assert!(picker.visible_session_iter().any(|s| s.is_debug));
}

#[test]
pub(super) fn test_crash_reason_line_for_crashed_sessions() {
    let crashed = make_session(
        "session_crash",
        "crash",
        false,
        SessionStatus::Crashed {
            message: Some("Terminal or window closed (SIGHUP)".to_string()),
        },
    );
    let line = SessionPicker::crash_reason_line(&crashed).expect("crash reason should render");
    let text: String = line
        .spans
        .into_iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(text.contains("reason:"));
    assert!(text.contains("SIGHUP"));
}

#[test]
pub(super) fn test_batch_restore_detection_excludes_already_recovered_parent_sessions() {
    let crashed = make_session(
        "session_crash_source",
        "crash-source",
        false,
        SessionStatus::Crashed {
            message: Some("boom".to_string()),
        },
    );

    let mut recovered = make_session(
        "session_recovery_rec123",
        "recovered",
        false,
        SessionStatus::Closed,
    );
    recovered.parent_id = Some(crashed.id.clone());

    let picker = SessionPicker::new(vec![crashed, recovered]);

    assert!(picker.crashed_sessions.is_none());
    assert!(picker.crashed_session_ids.is_empty());
}

#[test]
pub(super) fn test_grouped_batch_restore_uses_last_active_at_and_includes_debug_sessions() {
    let now = Utc::now();

    let mut recent_normal = make_session(
        "session_recent_normal",
        "recent-normal",
        false,
        SessionStatus::Crashed {
            message: Some("recent crash".to_string()),
        },
    );
    recent_normal.last_message_time = now - ChronoDuration::minutes(10);
    recent_normal.last_active_at = Some(now - ChronoDuration::seconds(10));

    let mut recent_debug = make_session(
        "session_recent_debug",
        "recent-debug",
        true,
        SessionStatus::Crashed {
            message: Some("debug crash".to_string()),
        },
    );
    recent_debug.last_message_time = now - ChronoDuration::minutes(9);
    recent_debug.last_active_at = Some(now - ChronoDuration::seconds(20));

    let mut stale_crash = make_session(
        "session_stale_crash",
        "stale-crash",
        false,
        SessionStatus::Crashed {
            message: Some("old crash".to_string()),
        },
    );
    stale_crash.last_message_time = now - ChronoDuration::seconds(30);
    stale_crash.last_active_at = Some(now - ChronoDuration::minutes(3));

    let picker = SessionPicker::new_grouped(
        vec![ServerGroup {
            name: "main".to_string(),
            icon: "🛰".to_string(),
            version: "v0.1.0".to_string(),
            git_hash: "abc1234".to_string(),
            is_running: true,
            sessions: vec![recent_normal.clone(), recent_debug.clone(), stale_crash],
        }],
        Vec::new(),
    );

    let crashed = picker
        .crashed_sessions
        .as_ref()
        .expect("expected eligible crashed sessions");

    assert_eq!(crashed.session_ids.len(), 2);
    assert_eq!(crashed.omitted_crashed_count, 1);
    assert!(crashed.session_ids.contains(&recent_normal.id));
    assert!(crashed.session_ids.contains(&recent_debug.id));
    assert!(
        !crashed
            .session_ids
            .iter()
            .any(|id| id == "session_stale_crash")
    );

    let mut picker = picker;
    let action = picker
        .handle_overlay_key(KeyCode::Char('R'), KeyModifiers::empty())
        .expect("restore group key should be handled");
    let OverlayAction::Selected(PickerResult::RestoreCrashedGroup(ids)) = action else {
        panic!("expected restore group action");
    };
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&recent_normal.id));
    assert!(ids.contains(&recent_debug.id));
    assert!(!ids.iter().any(|id| id == "session_stale_crash"));
}

#[test]
pub(super) fn test_filter_matches_recent_message_content() {
    let mut picker = SessionPicker::new(vec![make_session(
        "session_content",
        "content",
        false,
        SessionStatus::Closed,
    )]);

    picker.search_query = "world".to_string();
    picker.rebuild_items();
    assert_eq!(picker.visible_sessions.len(), 1);

    picker.search_query = "not-in-preview".to_string();
    picker.rebuild_items();
    wait_for_picker_search(&mut picker);
    assert!(picker.visible_sessions.is_empty());
}

#[test]
pub(super) fn resume_search_recovers_matches_as_typo_query_grows() {
    let mut session = make_session("typo_target", "target", false, SessionStatus::Closed);
    session.search_index = "config deploy".into();
    let mut picker = SessionPicker::new(vec![session]);
    for query in ["c", "co", "coo", "coon", "coonf", "coonfig"] {
        picker.search_query = query.into();
        picker.rebuild_items();
    }
    wait_for_picker_search(&mut picker);
    assert_eq!(picker.visible_sessions.len(), 1);
    picker.search_query = "deply conifg".into();
    picker.rebuild_items();
    wait_for_picker_search(&mut picker);
    assert_eq!(picker.visible_sessions.len(), 1);
}

pub(super) fn wait_for_picker_search(picker: &mut SessionPicker) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while picker.transcript_search.pending() {
        assert!(std::time::Instant::now() < deadline, "search timed out");
        picker.poll_transcript_search();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
pub(super) fn resume_search_ranks_literal_matches_before_newer_typo_matches() {
    let mut exact = make_session("literal", "literal", false, SessionStatus::Closed);
    exact.search_index = "config deploy".into();
    let mut typo = make_session("typo", "typo", false, SessionStatus::Closed);
    typo.search_index = "conifg deploy".into();
    typo.last_message_time = exact.last_message_time + chrono::Duration::seconds(10);
    let mut picker = SessionPicker::new(vec![typo, exact]);
    picker.search_query = "config".into();
    picker.rebuild_items();
    wait_for_picker_search(&mut picker);
    assert_eq!(picker.visible_sessions.len(), 2);
    assert_eq!(
        picker
            .session_by_ref(picker.visible_sessions[0])
            .unwrap()
            .id,
        "literal"
    );
}

#[test]
pub(super) fn resume_search_battery_prompt_outranks_incidental_tool_mentions() {
    let mut whale = make_session("whale", "power-triage", false, SessionStatus::Closed);
    whale.first_user_prompt = Some("Can you help me optimize my BATTERY life?".into());
    whale.search_index = "power-triage can you help me optimize my battery life?".into();
    let mut sessions = vec![whale.clone()];
    for i in 0..97 {
        let mut incidental = make_session(
            &format!("unrelated-{i}"),
            "Health check",
            false,
            SessionStatus::Closed,
        );
        incidental.first_user_prompt = Some("check my niri".into());
        incidental.search_index =
            "health check check my niri tool output battery-notify.desktop".into();
        incidental.last_message_time = whale.last_message_time + chrono::Duration::seconds(i + 1);
        sessions.push(incidental);
    }
    let mut picker = SessionPicker::new(sessions);
    picker.search_query = "battery".into();
    picker.rebuild_items();
    wait_for_picker_search(&mut picker);
    assert_eq!(picker.visible_sessions.len(), 98);
    assert_eq!(
        picker
            .session_by_ref(picker.visible_sessions[0])
            .unwrap()
            .id,
        "whale"
    );
    picker.search_query.clear();
    picker.rebuild_items();
    assert_eq!(
        picker
            .session_by_ref(picker.visible_sessions[0])
            .unwrap()
            .id,
        "unrelated-96"
    );
}

#[test]
pub(super) fn test_loading_preview_refreshes_search_index_for_picker_filtering() {
    let _env_lock = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("temp dir");
    let previous_home = std::env::var("JCODE_HOME").ok();
    crate::env::set_var("JCODE_HOME", temp.path());

    let mut session = Session::create_with_id(
        "session_preview_search".to_string(),
        Some("/tmp/preview-search".to_string()),
        Some("Preview Search".to_string()),
    );
    session.append_stored_message(crate::session::StoredMessage {
        codemode: None,
        attachment_data: None,
        id: "msg1".to_string(),
        role: crate::message::Role::User,
        content: vec![crate::message::ContentBlock::Text {
            text: "needle hidden outside the initial picker summary".to_string(),
            cache_control: None,
        }]
        .into(),
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.save().expect("save session");

    let sessions = load_sessions().expect("load sessions");
    let mut picker = SessionPicker::new(sessions);

    picker.ensure_selected_preview_loaded();

    let selected_after = picker
        .selected_session()
        .expect("selected session after preview");
    assert!(selected_after.search_index.contains("needle hidden"));

    picker.search_query = "needle hidden".to_string();
    picker.rebuild_items();
    assert_eq!(picker.visible_sessions.len(), 1);

    if let Some(previous_home) = previous_home {
        crate::env::set_var("JCODE_HOME", previous_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[test]
pub(super) fn benchmark_resume_search_reports_incremental_timings() {
    let sessions = (0..500)
        .map(|idx| {
            let mut session = make_session(
                &format!("session_bench_{idx:03}"),
                &format!("bench-{idx:03}"),
                false,
                SessionStatus::Closed,
            );
            session.messages_preview = vec![PreviewMessage {
                role: "user".to_string(),
                content: format!("benchmark transcript content alpha beta zebra-token-{idx:03}"),
                tool_calls: Vec::new(),
                tool_data: None,
                duration_secs: None,
                title: None,
                timestamp: None,
            }];
            session.search_index = build_search_index(
                &session.id,
                &session.short_name,
                &session.title,
                session.working_dir.as_deref(),
                None,
                &session.messages_preview,
            );
            session
        })
        .collect::<Vec<_>>();

    let mut picker = SessionPicker::new(sessions);

    let first_start = std::time::Instant::now();
    picker.search_query = "z".to_string();
    picker.rebuild_items();
    let first_ms = first_start.elapsed().as_secs_f64() * 1000.0;

    let second_start = std::time::Instant::now();
    picker.search_query = "ze".to_string();
    picker.rebuild_items();
    let second_ms = second_start.elapsed().as_secs_f64() * 1000.0;

    let third_start = std::time::Instant::now();
    picker.search_query = "zebra-token-499".to_string();
    picker.rebuild_items();
    let third_ms = third_start.elapsed().as_secs_f64() * 1000.0;

    wait_for_picker_search(&mut picker);
    assert_eq!(picker.visible_sessions.len(), 1);
    eprintln!(
        "resume search bench: first_char={:.3}ms second_char={:.3}ms full_query={:.3}ms sessions=500",
        first_ms, second_ms, third_ms
    );
}

#[test]
pub(super) fn test_filter_mode_cycles_through_requested_session_sources() {
    let mut saved = make_session("session_saved", "saved", false, SessionStatus::Closed);
    saved.saved = true;
    saved.needs_catchup = true;

    let mut claude_code = make_session("claude:demo", "claude-code", false, SessionStatus::Closed);
    claude_code.source = SessionSource::ClaudeCode;
    claude_code.resume_target = ResumeTarget::ClaudeCodeSession {
        session_id: "claude-session-demo".to_string(),
        session_path: "/tmp/claude-session-demo.jsonl".to_string(),
    };

    let mut codex = make_session("session_codex", "codex", false, SessionStatus::Closed);
    codex.model = Some("gpt-5.3-codex".to_string());
    codex.source = SessionSource::Codex;

    let mut pi = make_session("session_pi", "pi", false, SessionStatus::Closed);
    pi.provider_key = Some("pi".to_string());
    pi.source = SessionSource::Pi;

    let mut opencode = make_session("session_opencode", "opencode", false, SessionStatus::Closed);
    opencode.provider_key = Some("opencode".to_string());
    opencode.source = SessionSource::OpenCode;

    let mut cursor = make_session("session_cursor", "cursor", false, SessionStatus::Closed);
    cursor.provider_key = Some("cursor".to_string());
    cursor.source = SessionSource::Cursor;

    let mut picker = SessionPicker::new(vec![saved, claude_code, codex, pi, opencode, cursor]);
    picker.all_sessions[0].working_dir = Some("/work/project".to_string());
    picker.set_current_dir(Some("/work/project/".to_string()));
    picker.rebuild_items();

    assert_eq!(picker.filter_mode, SessionFilterMode::All);
    assert_eq!(picker.visible_sessions.len(), 6);

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Cloud);
    assert!(picker.visible_sessions.is_empty());

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::CurrentDir);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(|session| picker.session_in_current_dir(session))
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::CatchUp);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(|session| session.needs_catchup)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Saved);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(picker.visible_session_iter().all(|session| session.saved));
    assert_eq!(picker.items.len(), picker.visible_sessions.len());

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Active);
    // No live processes own these synthetic sessions, so the Active view is
    // empty in tests.
    assert_eq!(picker.visible_sessions.len(), 0);

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::ClaudeCode);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(SessionPicker::session_is_claude_code)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Codex);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(SessionPicker::session_is_codex)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Pi);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(SessionPicker::session_is_pi)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::OpenCode);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(SessionPicker::session_is_open_code)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::Cursor);
    assert_eq!(picker.visible_sessions.len(), 1);
    assert!(
        picker
            .visible_session_iter()
            .all(SessionPicker::session_is_cursor)
    );

    picker.cycle_filter_mode();
    assert_eq!(picker.filter_mode, SessionFilterMode::All);
    assert_eq!(picker.visible_sessions.len(), 6);
}

#[test]
pub(super) fn test_filter_mode_keyboard_shortcuts_cycle_both_directions() {
    let mut picker = SessionPicker::new(vec![make_session(
        "session_saved",
        "saved",
        false,
        SessionStatus::Closed,
    )]);
    picker
        .handle_overlay_key(KeyCode::Char('s'), KeyModifiers::empty())
        .unwrap();
    assert_eq!(picker.filter_mode, SessionFilterMode::Cloud);

    picker
        .handle_overlay_key(KeyCode::Char('S'), KeyModifiers::empty())
        .unwrap();
    assert_eq!(picker.filter_mode, SessionFilterMode::All);

    // Alt+S cycles even while typing a search query, without inserting text.
    picker
        .handle_overlay_key(KeyCode::Char('/'), KeyModifiers::empty())
        .unwrap();
    picker
        .handle_overlay_key(KeyCode::Char('s'), KeyModifiers::ALT)
        .unwrap();
    assert_eq!(picker.filter_mode, SessionFilterMode::Cloud);
    assert!(picker.search_query.is_empty());
    picker
        .handle_overlay_key(KeyCode::Char('S'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        .unwrap();
    assert_eq!(picker.filter_mode, SessionFilterMode::All);
}

include!("live_presence.rs");

pub(super) fn make_claude_session(session_id: &str) -> SessionInfo {
    let mut session = make_session(
        &format!("claude:{session_id}"),
        "claude live",
        false,
        SessionStatus::Closed,
    );
    session.source = SessionSource::ClaudeCode;
    session.provider_key = Some("claude-code".to_string());
    session.resume_target = ResumeTarget::ClaudeCodeSession {
        session_id: session_id.to_string(),
        session_path: format!("/tmp/{session_id}.jsonl"),
    };
    session
}

#[test]
pub(super) fn live_claude_takeover_requires_explicit_key_and_confirmation() {
    let session = make_claude_session("claude-live-id");
    let target = session.resume_target.clone();
    let mut picker = SessionPicker::new(vec![session]);
    picker.set_live_presence_for_test(vec![live_presence("claude:claude-live-id", false)]);

    // Ordinary Enter remains an ordinary resume and never implies takeover.
    let ordinary = picker
        .handle_overlay_key(KeyCode::Enter, KeyModifiers::empty())
        .unwrap();
    assert!(matches!(
        ordinary,
        OverlayAction::Selected(PickerResult::SelectedInCurrentTerminal(_))
            | OverlayAction::Selected(PickerResult::SelectedInNewTerminal(_))
    ));
    assert!(!picker.claude_takeover_confirmation_active_for_test());

    assert!(matches!(
        picker
            .handle_overlay_key(KeyCode::Char('T'), KeyModifiers::empty())
            .unwrap(),
        OverlayAction::Continue
    ));
    assert!(picker.claude_takeover_confirmation_active_for_test());

    // Cancellation does not emit an action.
    assert!(matches!(
        picker
            .handle_overlay_key(KeyCode::Esc, KeyModifiers::empty())
            .unwrap(),
        OverlayAction::Continue
    ));
    assert!(!picker.claude_takeover_confirmation_active_for_test());

    picker
        .handle_overlay_key(KeyCode::Char('T'), KeyModifiers::empty())
        .unwrap();
    let confirmed = picker
        .handle_overlay_key(KeyCode::Enter, KeyModifiers::empty())
        .unwrap();
    assert!(matches!(
        confirmed,
        OverlayAction::Selected(PickerResult::TakeOverClaude(actual)) if actual == target
    ));
}

#[test]
pub(super) fn live_claude_row_has_age_only_and_closed_claude_cannot_take_over() {
    let mut live = make_claude_session("live-row");
    let mut closed = make_claude_session("closed-row");
    live.last_message_time = Utc::now();
    closed.last_message_time = Utc::now() - ChronoDuration::minutes(1);
    let mut picker = SessionPicker::new(vec![live.clone(), closed]);
    picker.set_live_presence_for_test(vec![live_presence("claude:live-row", false)]);

    let rendered = picker
        .render_session_item_lines(&live, false)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !rendered.contains("live Claude"),
        "rendered row: {rendered}"
    );
    assert!(rendered.contains("ago"), "rendered row: {rendered}");

    picker
        .handle_overlay_key(KeyCode::Down, KeyModifiers::empty())
        .unwrap();
    picker
        .handle_overlay_key(KeyCode::Char('T'), KeyModifiers::empty())
        .unwrap();
    assert!(!picker.claude_takeover_confirmation_active_for_test());
}

#[test]
pub(super) fn test_space_selects_multiple_sessions_and_enter_returns_them() {
    let mut newer = make_session("session_newer", "newer", false, SessionStatus::Closed);
    let mut older = make_session("session_older", "older", false, SessionStatus::Closed);
    newer.last_message_time = Utc::now();
    older.last_message_time = Utc::now() - ChronoDuration::minutes(1);

    let mut picker = SessionPicker::new(vec![older, newer]);

    picker
        .handle_overlay_key(KeyCode::Char(' '), KeyModifiers::empty())
        .unwrap();
    picker
        .handle_overlay_key(KeyCode::Down, KeyModifiers::empty())
        .unwrap();
    picker
        .handle_overlay_key(KeyCode::Char(' '), KeyModifiers::empty())
        .unwrap();

    let action = picker
        .handle_overlay_key(KeyCode::Enter, KeyModifiers::empty())
        .unwrap();

    match action {
        OverlayAction::Selected(PickerResult::SelectedInCurrentTerminal(ids)) => {
            assert_eq!(
                ids,
                vec![
                    ResumeTarget::JcodeSession {
                        session_id: "session_newer".to_string(),
                    },
                    ResumeTarget::JcodeSession {
                        session_id: "session_older".to_string(),
                    }
                ]
            );
        }
        other => panic!("expected selected sessions, got {other:?}"),
    }

    let alternate_action = picker
        .handle_overlay_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    match alternate_action {
        OverlayAction::Selected(PickerResult::SelectedInNewTerminal(ids)) => {
            assert_eq!(
                ids,
                vec![
                    ResumeTarget::JcodeSession {
                        session_id: "session_newer".to_string(),
                    },
                    ResumeTarget::JcodeSession {
                        session_id: "session_older".to_string(),
                    }
                ]
            );
        }
        other => panic!("expected alternate selected sessions, got {other:?}"),
    }
}
