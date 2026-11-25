use super::*;

#[test]
fn expanded_diff_rows_fit_narrow_widths_and_expand_tabs() {
    let parsed = ParsedDiffLine {
        kind: DiffLineKind::Del,
        prefix: "123- ".into(),
        content: "\t界\tvalue".into(),
        file_path: None,
    };
    for width in 0..60 {
        let rows = expanded_diff_row(&parsed, None, 3, width);
        assert!(
            rows.iter().all(|row| row.width() <= width),
            "{width}: {rows:?}"
        );
    }
    let rows = expanded_diff_row(&parsed, None, 3, 60);
    assert_eq!(extract_line_text(&rows[0]), "│ 123 -     界  value");
    assert_eq!(rows[0].spans[1].style.fg, Some(diff_del_color()));
    let blank = ParsedDiffLine {
        content: String::new(),
        ..parsed
    };
    assert_eq!(expanded_diff_row(&blank, None, 3, 60).len(), 1);
}

#[test]
fn inline_diff_header_compacts_directories_before_filename() {
    let path = "/home/user/project/crates/very_long_directory/src/important_file.rs";
    let line = inline_diff_header("╭─", Some((23, 4)), Some(path), 48);
    let text = extract_line_text(&line);
    assert!(text.starts_with("╭─ diff · +23 -4 · "), "{text}");
    assert!(text.ends_with("important_file.rs"), "{text}");
    assert!(text.contains('…'), "{text}");
    assert!(line.width() <= 48);
    assert!(
        line.spans
            .iter()
            .any(|span| span.content == "+23" && span.style.fg == Some(diff_add_color()))
    );
}

#[test]
fn inline_diff_headers_fit_all_cell_budgets() {
    for path in [
        "/home/user/a/very/long/path/to/source.rs",
        "C:\\Users\\user\\project\\very_long_directory\\source.rs",
        "/目录/很长的目录/另一个目录/主文件.rs",
        "/very/long/path/a_filename_too_long_for_the_remaining_space.rs",
    ] {
        for width in 0..100 {
            for (prefix, counts) in [("╭─", Some((123, 45))), ("├─", None)] {
                let line = inline_diff_header(prefix, counts, Some(path), width);
                assert!(line.width() <= width, "width={width}, line={line:?}");
                if width >= 64 {
                    assert!(extract_line_text(&line).ends_with(".rs"), "{line:?}");
                }
            }
        }
    }
}

#[test]
fn render_tool_message_compacts_every_multi_file_diff_header() {
    let first = "/home/user/project/very_long_directory/nested/first.rs";
    let second = "/home/user/project/another_very_long_directory/nested/second.rs";
    let msg = DisplayMessage {
        role: "tool".into(),
        content: "Updated 2 files".into(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "compact_multi_file_headers".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({"patch_text": format!(
                "*** Begin Patch\n*** Update File: {first}\n@@\n-old\n+new\n*** Update File: {second}\n@@\n-old\n+new\n*** End Patch\n"
            )}),
            intent: None,
            thought_signature: None,
        }),
        codemode: None,
    };
    for width in [40, 72, 120] {
        for mode in [
            crate::config::DiffDisplayMode::Inline,
            crate::config::DiffDisplayMode::FullInline,
        ] {
            let lines = render_tool_message(&msg, width, mode);
            let headers: Vec<_> = lines
                .iter()
                .filter(|line| {
                    let text = extract_line_text(line);
                    text.trim_start().starts_with("╭─ diff")
                        || text.trim_start().starts_with("├─ diff")
                })
                .collect();
            assert_eq!(headers.len(), 2, "{lines:?}");
            for (header, filename) in headers.iter().zip(["first.rs", "second.rs"]) {
                assert!(header.width() <= width as usize, "{header:?}");
                assert!(extract_line_text(header).ends_with(filename), "{header:?}");
            }
        }
    }
}

#[test]
fn oauth_login_progress_renderer_keeps_short_link_and_hides_full_details() {
    let url =
        "https://example.invalid/authorize?state=private-state&code_challenge=private-challenge";
    let summary = format!(
        "✓ Sign-in link ready\n◌ Waiting for browser approval\n[Open OpenAI sign-in]({url})\nPaste callback URL here · /cancel"
    );
    let mut msg = DisplayMessage::system(jcode_tui_messages::encode_collapsible_swarm_content(
        "Sign-in details",
        &format!(
            "{summary}\n\n<!-- oauth-details -->\n\nFull sign-in URL:\n{url}\n\nManual fallback details"
        ),
    ));
    msg.title = Some("OpenAI login".to_string());
    for width in [40, 80, 120] {
        let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
        let text = lines
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Sign-in link ready"));
        assert!(text.contains("Waiting for browser approval"));
        assert!(text.contains("Open OpenAI sign-in"));
        assert!(text.contains(SWARM_EXPAND_BADGE));
        assert!(!text.contains("private-state"));
        assert!(!text.contains("Manual fallback details"));
        assert!(lines.iter().all(|line| line.width() <= width as usize));
    }
    msg.content = jcode_tui_messages::toggle_collapsible_swarm_content(&msg.content).unwrap();
    let lines = render_system_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let text = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("private-state"));
    assert!(text.contains("Manual fallback details"));
    assert!(text.contains(SWARM_COLLAPSE_BADGE));
}

fn extract_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

#[test]
fn usage_card_fits_content_on_wide_terminals() {
    let msg =
        DisplayMessage::system("# Updated\n+ OpenAI - available\n  Banked resets: 12 available");
    let render = |width| render_usage_message(&msg, width, crate::config::DiffDisplayMode::Off);
    let lines = render(240);
    assert_eq!(lines, render(120));
    assert_eq!(lines, render(160));
    assert_eq!(
        lines[0].width(),
        "  Banked resets: 12 available".width() + 4
    );
    assert!(lines.iter().all(|line| line.width() == lines[0].width()));
    assert!(extract_line_text(&lines[0]).contains("Usage"));
    assert!(extract_line_text(lines.last().unwrap()).starts_with('╰'));
}

#[test]
fn usage_card_compacts_padded_labels_in_narrow_terminals() {
    let msg = DisplayMessage::system(
        "  7-day window              : ████████░░░░░░  59% · resets in 5d 4h",
    );
    let lines = render_usage_message(&msg, 36, crate::config::DiffDisplayMode::Off);
    let rows: Vec<_> = lines.iter().map(extract_line_text).collect();
    assert!(
        rows.iter().any(|row| row.contains("7-day window")
            && row.contains(if crate::config::config().display.usage_display_used() {
                "59% used"
            } else {
                "41% left"
            })),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|row| row.contains("resets in 5d 4h")));
    assert!(lines.iter().all(|line| line.width() <= 36));
}

#[test]
fn usage_card_never_overflows_even_tiny_terminals() {
    let mut msg = DisplayMessage::system(
        "# Usage updated\n+ 工作账户 e\u{301} - available\n  Weekly: █████░░░░░ 50% · resets in 2h 5m\n  Session resets: 1 available\n\n! Claude - error\n  error: unavailable",
    );
    msg.title = Some("使用情况 with a very long title".into());
    for width in 0..=120 {
        let lines = render_usage_message(&msg, width, crate::config::DiffDisplayMode::Off);
        assert_eq!(lines.is_empty(), width == 0, "width {width}");
        for line in &lines {
            assert!(
                line.width() <= usize::from(width),
                "width {width}: {line:?}"
            );
        }
        if width >= 10 {
            assert!(lines.iter().all(|line| line.width() == lines[0].width()));
        }
    }
}

#[test]
fn usage_card_preserves_unicode_and_wrapped_metadata() {
    let msg = DisplayMessage::system(
        "+ 工作账户 e\u{301} - available\n  Weekly: █████░░░░░ 50% · resets in 2h 5m\n  Session resets: 1 available",
    );
    for width in [10, 18, 20, 32, 40, 60, 120] {
        let lines = render_usage_message(&msg, width, crate::config::DiffDisplayMode::Off);
        let body: String = lines[1..lines.len() - 1]
            .iter()
            .map(|line| {
                let text = extract_line_text(line);
                text.strip_prefix("│ ")
                    .unwrap()
                    .strip_suffix(" │")
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let body = without_whitespace(&body);
        for expected in [
            "工作账户e\u{301}",
            "available",
            "50%",
            "resetsin2h5m",
            "Sessionresets:1available",
        ] {
            assert!(body.contains(expected), "width {width}: {body}");
        }
    }
    assert_eq!(
        msg.content.matches(" - available").count(),
        1,
        "report protocol is unchanged"
    );
}

#[test]
fn usage_card_styles_heading_and_account_statuses() {
    use ratatui::style::Modifier;
    let msg = DisplayMessage::system(
        "# Updated\n+ OpenAI - available\n~ Work - 70% used\n! Claude - hard limit\n! Other - error\nProvider - no data",
    );
    let lines = render_usage_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
    for name in ["Updated", "OpenAI", "Work", "Claude", "Other"] {
        let span = spans.iter().find(|span| span.content == name).unwrap();
        assert_eq!(span.style.fg, Some(Color::White));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }
    for (status, color) in [
        (" · available", rgb(100, 200, 100)),
        (" · 70% used", rgb(255, 200, 100)),
        (" · hard limit", rgb(255, 100, 100)),
        (" · error", rgb(255, 100, 100)),
    ] {
        assert!(
            spans
                .iter()
                .any(|span| span.content == status && span.style.fg == Some(color))
        );
    }
    assert!(
        spans
            .iter()
            .any(|span| span.content == "Provider - no data" && span.style.fg == Some(dim_color()))
    );
}

#[test]
fn usage_card_keeps_narrow_gauges_and_words_together() {
    let msg = DisplayMessage::system(
        "+ OpenAI - available\n  5-hour window: ███████░░░░░░░ 50% · resets in 2h 5m\n\n~ Claude - 70% used",
    );
    for width in [20, 40, 60] {
        let lines = render_usage_message(&msg, width, crate::config::DiffDisplayMode::Off);
        let texts: Vec<_> = lines.iter().map(extract_line_text).collect();
        let gauges: Vec<_> = texts
            .iter()
            .filter(|text| text.contains('▰') || text.contains('▱'))
            .collect();
        assert_eq!(gauges.len(), 1, "width {width}: {texts:?}");
        assert!(gauges[0].contains("50%"), "width {width}: {texts:?}");
        assert!(texts.iter().any(|text| text.contains("available")));
        assert!(texts.iter().any(|text| text.contains("resets")));
        assert!(texts.iter().any(|text| text.contains("──────")));
        if width == 40 {
            assert!(!gauges[0].contains("resets"));
        }
    }
}

#[test]
fn usage_card_compacts_long_summary_without_mutating_report() {
    let report =
        "# Showing cached usage while refreshing all connected accounts\n+ OpenAI - available";
    let msg = DisplayMessage::system(report);
    let lines = render_usage_message(&msg, 20, crate::config::DiffDisplayMode::Off);
    assert!(extract_line_text(&lines[1]).contains('…'));
    assert!(extract_line_text(&lines[2]).contains("OpenAI"));
    assert_eq!(msg.content, report);
}

#[test]
fn usage_card_colors_utilization_and_dims_empty_bars_and_countdowns() {
    for percent in [0, 49, 50, 79, 80, 100] {
        let raw = format!("  Weekly: ██░░ {percent}% · resets in 2h");
        let actual = super::usage::usage_report_lines(&raw, 50);
        let expected = crate::tui::info_widget::LimitBar {
            label: "Weekly",
            label_column: 8,
            used_pct: percent,
            left_pct: 100 - percent,
            reset_time: Some("resets in 2h"),
            width: 50,
            display_used: crate::config::config().display.usage_display_used(),
            columns: None,
        }
        .render();
        assert_eq!(actual, vec![expected]);
    }
}

#[test]
fn usage_card_highlights_reset_counts_and_handles_empty_reports() {
    use ratatui::style::Modifier;
    for label in ["Banked", "Session"] {
        for count in [0_u64, 1, 12, u64::MAX] {
            let msg =
                DisplayMessage::system(format!("  {label} resets: {count} available · next in 2d"));
            let lines = render_usage_message(&msg, 120, crate::config::DiffDisplayMode::Off);
            let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
            let number = spans
                .iter()
                .find(|span| span.content == count.to_string())
                .unwrap();
            assert!(number.style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(
                number.style.fg,
                Some(if count == 0 {
                    dim_color()
                } else {
                    rgb(100, 200, 100)
                })
            );
            assert!(
                spans.iter().any(|span| span.content.contains("next in 2d")
                    && span.style.fg == Some(dim_color()))
            );
        }
    }
    for content in ["", "\n", "x"] {
        let mut msg = DisplayMessage::system(content);
        msg.title = Some("U".into());
        let lines = render_usage_message(&msg, 120, crate::config::DiffDisplayMode::Off);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| line.width() == lines[0].width()));
    }
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn leading_spaces(text: &str) -> usize {
    text.chars().take_while(|c| *c == ' ').count()
}

#[test]
fn successful_reload_notice_hides_internal_directive() {
    for content in [
        "[auto] Reload succeeded (old → new-dirty). Continue immediately from where you left off.",
        "[auto] Reload complete. Resuming your task.\n\nContinue the user's latest request.\nBuild: old → new.",
        "Reload complete. Resuming your task.\nBuild: old → new.",
    ] {
        let msg = DisplayMessage::system(content);
        for width in [1, 10, 30, 80, 120] {
            let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].width() <= width as usize);
            if width >= 80 {
                assert_eq!(
                    extract_line_text(&lines[0]).trim(),
                    "Reload complete. Resuming your task."
                );
            }
        }
        assert_eq!(msg.content, content);
    }
}

#[test]
fn transient_system_notices_are_single_line_without_changing_content() {
    let notices = [
        "[auto] Your session was interrupted by a server reload while a tool was running. The\ntool was aborted and results may be incomplete. Continue exactly where you left off.",
        "Reload complete - continuing because a recovery directive was pending.",
        "Stream read timeout: no data received for 180 seconds",
        "📡 Network appears offline - waiting to retry automatically. listening for Linux\nnetlink changes via `ip monitor`; also verifying with reconnect probes",
        "✓ Retrying continuation... (system message)",
        "An ordinary status notice that should never wrap even in a narrow terminal.",
        "[auto] Continue the task.\nKeep the todo up to date.",
    ];
    for content in notices {
        let msg = DisplayMessage::system(content);
        for width in [1, 10, 30, 80, 120] {
            let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
            assert_eq!(lines.len(), 1, "{content:?} at width {width}");
            assert!(lines[0].width() <= width as usize);
            assert!(!extract_line_text(&lines[0]).contains('\n'));
        }
        assert_eq!(
            msg.content, content,
            "display must not rewrite agent instructions"
        );
    }
    let msg = DisplayMessage::system(notices[0]);
    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    assert_eq!(
        extract_line_text(&lines[0]).trim(),
        "Server reloaded; resuming interrupted work."
    );
}

#[test]
fn ordinary_multiline_and_markdown_notices_are_one_terminal_row() {
    let saved = markdown::center_code_blocks();
    for centered in [false, true] {
        markdown::set_center_code_blocks(centered);
        for content in [
            "Header line\n  indented detail\n\nNext block",
            "**Status updated**\n- 工作账户 e\u{301} reconnected\n- More detail that must not wrap",
            "Using `some/very/long/path.rs` with **formatting** and a long status message",
            "\u{1b}[35mUpdated\u{1b}[0m\n\tMore details",
        ] {
            let msg = DisplayMessage::system(content);
            for width in 0..=120 {
                let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
                assert_eq!(
                    lines.len(),
                    usize::from(width != 0),
                    "{content:?} at {width}"
                );
                for line in &lines {
                    assert!(line.width() <= width as usize, "{line:?} at {width}");
                    assert!(!extract_line_text(line).contains(['\n', '\t', '\u{1b}']));
                }
                assert_eq!(
                    msg.content, content,
                    "display must not rewrite stored content"
                );
            }
        }
    }
    markdown::set_center_code_blocks(saved);
}

#[test]
fn file_activity_is_neutral_and_one_row_including_legacy_titles() {
    let saved = markdown::center_code_blocks();
    let content = jcode_tui_messages::encode_collapsible_swarm_content(
        "…/src/工作账户/e\u{301}.rs · Edited lines 323-348 (1 occurrence)",
        "Intent: fix the parser\n\n```text\n323- old line\n323+ new line\n```",
    );
    for centered in [false, true] {
        markdown::set_center_code_blocks(centered);
        for title in ["File activity · moss", "File activity"] {
            let msg = DisplayMessage::swarm(title, &content);
            for width in 0..=120 {
                let lines = render_swarm_message(&msg, width, crate::config::DiffDisplayMode::Off);
                assert_eq!(lines.len(), usize::from(width != 0), "{title} at {width}");
                for line in &lines {
                    assert!(line.width() <= width as usize, "{line:?} at {width}");
                    let text = extract_line_text(line);
                    assert!(!text.contains("Intent:"));
                    assert!(!text.contains("old line"));
                    for span in &line.spans {
                        if !span.content.trim().is_empty() {
                            assert_eq!(span.style.fg, Some(dim_color()), "{span:?}");
                        }
                    }
                    if width >= 80 {
                        assert!(text.contains(SWARM_DIFF_EXPAND_BADGE));
                        assert!(text.trim_end().ends_with(SWARM_DIFF_EXPAND_BADGE));
                    }
                }
            }
        }
    }
    markdown::set_center_code_blocks(saved);
}

#[test]
fn file_activity_explicit_expansion_preserves_diff_details() {
    let content = jcode_tui_messages::encode_collapsible_swarm_content(
        "src/parser.rs · Edited lines 323-348",
        "Intent: fix the parser\n\n```text\n323- old line\n323+ new line\n```",
    );
    let expanded = jcode_tui_messages::toggle_collapsible_swarm_content(&content).unwrap();
    let msg = DisplayMessage::swarm("File activity · moss", expanded);
    let lines = render_swarm_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let text = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.len() > 1);
    assert!(text.contains(SWARM_DIFF_COLLAPSE_BADGE));
    assert!(text.contains("Intent: fix the parser"));
    assert!(text.contains("323- old line"));
    assert!(text.contains("323+ new line"));
}

#[test]
fn raw_file_activity_system_notice_is_neutral_without_rewriting_content() {
    let content = "⚠️ File activity: /home/jeremy/jcode/src/parser.rs - moss previously read this file: read lines 1-1000\nMore details";
    let msg = DisplayMessage::system(content);
    let lines = render_system_message(&msg, 60, crate::config::DiffDisplayMode::Off);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].width() <= 60);
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|span| span.style.fg == Some(dim_color()))
    );
    assert_eq!(msg.content, content);
}

#[test]
fn interactive_login_prompt_keeps_instructions_visible() {
    let msg = DisplayMessage::system(
        "Azure OpenAI Login\n\nEnter your endpoint below.\nType /cancel to abort.",
    );
    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let text = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.len() > 1);
    assert!(text.contains("Enter your endpoint below."));
    assert!(text.contains("/cancel"));
}

#[test]
fn render_system_message_forces_system_color_on_all_spans() {
    let msg = DisplayMessage::system("**Reload complete** - continuing.");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);

    assert!(!lines.is_empty(), "expected rendered system message lines");
    for line in lines {
        for span in line.spans {
            assert_eq!(span.style.fg, Some(system_message_color()));
        }
    }
}

#[test]
fn render_system_message_drops_emoji_variation_selectors() {
    let msg = DisplayMessage::system(
        "⚠️ KV cache miss [harness: prefix changed] turn 13: ~336K tokens resent (an earlier message was modified). See KV_CACHE_USAGE in logs.",
    );

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let text: String = lines.iter().map(extract_line_text).collect();

    assert!(
        text.contains("336K KV cache miss · harness: prefix changed"),
        "got: {text}"
    );
    assert!(!text.contains('\u{FE0F}'), "VS16 must not reach the buffer");
}

#[test]
fn render_cold_cache_warning_is_always_one_width_bounded_line() {
    let saved = crate::tui::markdown::center_code_blocks();
    let msg = DisplayMessage::system(
        "🧊 Prompt cache went cold · next turn may resend ~96K tok · /cache extends",
    );

    for centered in [false, true] {
        crate::tui::markdown::set_center_code_blocks(centered);
        for width in [80_u16, 50, 30] {
            let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
            assert_eq!(
                lines.len(),
                1,
                "cold-cache notice wrapped at width {width} (centered={centered}): {lines:?}"
            );
            let text = extract_line_text(&lines[0]);
            assert!(
                !text.contains('\n'),
                "cold-cache notice contains a newline: {text:?}"
            );
            assert!(
                lines[0].width() <= width as usize,
                "cold-cache notice width {} exceeds {width}: {text:?}",
                lines[0].width()
            );
            assert!(
                text.contains("Prompt cache went cold"),
                "cold-cache identity was truncated away: {text:?}"
            );
            if width < 80 {
                assert!(
                    text.ends_with('…'),
                    "narrow cold-cache notice should end in an ellipsis: {text:?}"
                );
            }
            for span in &lines[0].spans {
                assert_eq!(span.style.fg, Some(system_message_color()));
            }
        }
    }

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
fn render_launch_hotkeys_keeps_both_shortcuts_visible() {
    let saved = crate::tui::markdown::center_code_blocks();
    let content = "Hotkeys: Super+; → jcode · Super+' → home";
    let msg = DisplayMessage::system(content).with_title("Launch hotkeys");

    for centered in [false, true] {
        crate::tui::markdown::set_center_code_blocks(centered);
        for width in [80_u16, 50] {
            let lines = render_system_message(&msg, width, crate::config::DiffDisplayMode::Off);
            assert_eq!(lines.len(), 1);
            assert_eq!(extract_line_text(&lines[0]).trim(), content);
            assert!(lines[0].width() <= width as usize);
        }
    }

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
fn render_compact_launch_and_divergence_notices_as_one_line() {
    let saved = crate::tui::markdown::center_code_blocks();
    let notices = [
        DisplayMessage::system(
            "Configured Jcode launch hotkeys (niri):\nSuper+; → jcode (/home/user/project)\n\nBound system-wide.",
        )
        .with_title("Launch hotkeys"),
        DisplayMessage::system(
            "Update diverged. Press Ctrl+Y to let a jcode agent merge local and upstream (or run `git pull` / `git rebase` yourself).",
        )
        .with_title("Update"),
    ];

    for centered in [false, true] {
        crate::tui::markdown::set_center_code_blocks(centered);
        for msg in &notices {
            for width in [80_u16, 50, 30] {
                let lines = render_system_message(msg, width, crate::config::DiffDisplayMode::Off);
                assert_eq!(
                    lines.len(),
                    1,
                    "compact notice wrapped at width {width} (centered={centered}): {lines:?}"
                );
                let text = extract_line_text(&lines[0]);
                assert!(!text.contains('\n'), "notice contains a newline: {text:?}");
                assert!(
                    lines[0].width() <= width as usize,
                    "notice width {} exceeds {width}: {text:?}",
                    lines[0].width()
                );
                if width < 80 {
                    assert!(
                        text.ends_with('…'),
                        "narrow compact notice should end in an ellipsis: {text:?}"
                    );
                }
            }
        }
    }

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
fn render_system_message_renders_markdown_formatting() {
    let msg = DisplayMessage::system(
        "**bold** and `code` and # heading\n- bullet item\n[link](http://example.com)",
    );

    let lines = render_system_message(&msg, 200, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        lines.len(),
        1,
        "ordinary markdown notices must stay compact"
    );
    // System messages render markdown: the inline markers are consumed and
    // the underlying text survives. Bold/code markers should no longer appear
    // literally, while the text content and a bullet glyph remain.
    assert!(plain.contains("bold"), "keeps bold text: {plain:?}");
    assert!(
        !plain.contains("**bold**"),
        "strips bold markers: {plain:?}"
    );
    assert!(plain.contains("code"), "keeps code text: {plain:?}");
    assert!(plain.contains("heading"), "keeps heading text: {plain:?}");
    assert!(
        plain.contains("bullet item"),
        "keeps bullet text: {plain:?}"
    );
    // The link text renders without the raw markdown link syntax.
    assert!(plain.contains("link"), "keeps link text: {plain:?}");
    assert!(
        !plain.contains("[link](http://example.com)"),
        "strips raw link syntax: {plain:?}"
    );

    // Color is still forced to the system color over every span.
    for line in &lines {
        for span in &line.spans {
            assert_eq!(span.style.fg, Some(system_message_color()));
        }
    }
}

#[test]
fn explicit_system_report_preserves_indentation_and_newlines() {
    let msg = DisplayMessage::system("Header line\n  indented detail\n\nNext block")
        .with_title("Context");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let rendered = lines.iter().map(extract_line_text).collect::<Vec<_>>();

    // Centered mode may add uniform left padding; compare relative structure.
    assert_eq!(rendered.len(), 4, "got: {rendered:?}");
    assert!(
        rendered[0].trim_end().ends_with("Header line"),
        "got: {rendered:?}"
    );
    assert!(
        rendered[1].trim_end().ends_with("indented detail"),
        "got: {rendered:?}"
    );
    assert!(
        rendered[2].trim().is_empty(),
        "blank line preserved, got: {rendered:?}"
    );
    assert!(
        rendered[3].trim_end().ends_with("Next block"),
        "got: {rendered:?}"
    );

    // The detail line keeps exactly two more leading spaces than the header.
    assert_eq!(
        leading_spaces(&rendered[1]),
        leading_spaces(&rendered[0]) + 2,
        "indentation should be preserved, got: {rendered:?}"
    );
}

#[test]
fn render_plaintext_lines_hang_indents_wrapped_continuations() {
    // An indented line longer than the wrap width keeps its indent on the wrap.
    let lines = render_plaintext_lines("  alpha beta gamma delta", 12);
    let rendered = lines.iter().map(extract_line_text).collect::<Vec<_>>();

    assert!(rendered.len() >= 2, "expected wrapping, got: {rendered:?}");
    for line in &rendered {
        assert!(
            line.is_empty() || line.starts_with("  "),
            "continuation lines should keep indent, got: {rendered:?}"
        );
        assert!(line.width() <= 12, "line too wide: {line:?}");
    }
}

#[test]
fn render_system_message_centered_mode_left_aligns_with_padding() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(true);
    let msg = DisplayMessage::system("Reload complete - continuing.");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);

    assert!(!lines.is_empty(), "expected rendered system message lines");
    for line in &lines {
        assert_eq!(
            line.alignment,
            Some(ratatui::layout::Alignment::Left),
            "centered system lines should be left-aligned with padding"
        );
        assert!(
            line.spans
                .first()
                .is_some_and(|span| span.content.starts_with(' ')),
            "centered system lines should start with padding"
        );
    }
    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
fn render_system_message_uses_width_stable_titles_on_kitty() {
    let _kitty = WidthStableGlyphsGuard::kitty();

    let msg = DisplayMessage::system(
        "⚡ Connection lost - retrying (attempt 2, 7s) - connection reset by server",
    )
    .with_title("Connection");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("Connection lost - retrying"));
    assert!(!plain.contains('⚡'));
}

#[test]
fn render_background_task_failure_is_compact() {
    let called = (chrono::Utc::now() - chrono::Duration::seconds(125)).to_rfc3339();
    let msg = DisplayMessage::background_task(format!(
        "**Background task** `bg123` · `Comparison trace` (`bash`) · ✗ failed · 11.0s · exit 101\n\n_Called at:_ `{called}`\n\n```text\n[stderr] justrust: remote build\n[stderr] warning: unused variable\n[stderr] error[E0308]: mismatched types\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`"
    ));
    let lines = render_background_task_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    assert_eq!(lines.len(), 2);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plain.contains("✗ bash · Comparison trace · 2m ago"),
        "{plain}"
    );
    assert!(
        plain.contains("╰─exit 101 · [E0308]: mismatched types"),
        "{plain}"
    );
    assert!(!plain.contains("remote build"));
    assert_eq!(
        extract_line_text(&lines[1]).find('╰').unwrap(),
        extract_line_text(&lines[0]).find('✗').unwrap()
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content == "bash" && span.style.fg == Some(tool_color()))
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content.contains('✗') && span.style.fg == Some(rgb(220, 100, 100)))
    );
    assert!(
        lines[1]
            .spans
            .iter()
            .all(|span| span.content.trim().is_empty() || span.style.fg == Some(dim_color()))
    );
    assert!(
        msg.content
            .contains("bg action=\"output\" task_id=\"bg123\"")
    );
    for width in [0, 1, 2, 8, 20, 40] {
        let lines =
            render_background_task_message(&msg, width, crate::config::DiffDisplayMode::Off);
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert!(UnicodeWidthStr::width(extract_line_text(&line).as_str()) <= width as usize);
        }
    }
}

#[test]
fn render_background_task_failure_without_diagnostic_is_honest() {
    let msg = DisplayMessage::background_task(
        "**Background task** `bg123` · `bash` · ✗ failed · 7.1s · exit 1\n\n_No output captured._\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`",
    );
    let plain = render_background_task_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plain.contains("✗ bash · took 7.1s"), "{plain}");
    assert!(plain.contains("╰─exit 1 · no diagnostic captured"));
}

#[test]
fn selfdev_build_status_uses_one_tool_row() {
    for (status, label) in [
        ("running", "◌ selfdev build · running"),
        ("↻ superseded", "↻ selfdev build · superseded"),
    ] {
        let msg = DisplayMessage::background_task(format!(
            "**Background task** `bg123` · `selfdev-build` · {status} · 84.0s · exit 0\n\n```text\ncompiler output\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`"
        ));
        for width in [20, 80] {
            let lines =
                render_background_task_message(&msg, width, crate::config::DiffDisplayMode::Off);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].width() <= width as usize);
            if width == 80 {
                assert!(lines[0].to_string().contains(label));
            }
        }
    }
}

#[test]
fn render_background_task_success_is_compact_and_preserves_raw_output() {
    let msg = DisplayMessage::background_task(
        "**Background task** `bg123` · `bash` · ✓ completed · 7.1s · exit 0\n\n```text\nline 1\nline 2\nline 3\nline 4\nline 5\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`",
    );

    let lines = render_background_task_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(lines.len(), 1);
    assert!(plain.contains("✓ bash · took 7.1s"), "{plain}");
    assert!(!plain.contains("exit 0"));
    assert!(!plain.contains("line 1"));
    assert!(!plain.contains("more line"));
    assert!(!plain.contains('╭') && !plain.contains('│') && !plain.contains('╰'));
    assert!(
        lines[0].spans.iter().any(|span| {
            span.content.contains('✓') && span.style.fg == Some(rgb(120, 210, 140))
        })
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| { span.content == "bash" && span.style.fg == Some(tool_color()) })
    );
    assert!(msg.content.contains("line 5"));
    assert!(
        msg.content
            .contains("bg action=\"output\" task_id=\"bg123\"")
    );
    assert!(!plain.contains("task bg123 · bash"));
    assert!(!plain.contains("Preview"));
    assert!(!plain.contains("Full output"));
    assert!(!plain.contains("bg action=\"output\" task_id=\"bg123\""));
}

#[test]
fn render_background_task_success_hides_ansi_preview_without_changing_raw() {
    let msg = DisplayMessage::background_task(
        "**Background task** `bg123` · `bash` · ✓ completed · 0.1s · exit 0\n\n```text\n\u{1b}[32m✓\u{1b}[39m passes \u{1b}[2m12ms\u{1b}[22m\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`",
    );

    let raw = msg.content.clone();
    let plain = render_background_task_message(&msg, 80, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("✓ bash · took 0.1s"), "{plain}");
    assert!(!plain.contains("passes"));
    assert_eq!(msg.content, raw);
    assert!(!plain.contains('\u{1b}'));
    assert!(!plain.contains("[32m"));
    assert!(!plain.contains("[2m"));
}

#[test]
fn render_background_task_success_fits_unicode_and_narrow_widths() {
    let raw = "**Background task** `bg-unicode` · `验证界面 e\u{301} 🦀 long background intent` (`bash`) · ✓ completed · 12.3s · exit 0\n\n```text\nlarge output must stay hidden\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg-unicode\"`";
    let msg = DisplayMessage::background_task(raw);
    for width in [0, 1, 2, 3, 8, 16, 20, 40, 100] {
        let lines =
            render_background_task_message(&msg, width, crate::config::DiffDisplayMode::Off);
        assert_eq!(lines.len(), 1);
        let text = extract_line_text(&lines[0]);
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= width as usize,
            "width={width}: {text}"
        );
        assert!(!text.contains("large output"));
        if width >= 40 {
            assert!(text.contains("took 12.3s"), "{text}");
        }
        if width == 100 {
            assert!(
                text.contains("✓ bash · 验证界面 e\u{301} 🦀 long background intent"),
                "{text}"
            );
        }
    }
    assert_eq!(msg.content, raw);
}

#[path = "tests/todo_card_tests.rs"]
mod todo_card_tests;
#[allow(unused_imports)]
pub(crate) use todo_card_tests::*;

#[test]
fn parse_todo_tool_output_accepts_timestamp_only_header() {
    let todos = vec![crate::todo::TodoItem {
        id: "timed".to_string(),
        content: "Render the restored todo".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        ..Default::default()
    }];
    let content = format!(
        "[2026-07-13T19:51:50.261Z] [todo] {}",
        serde_json::to_string(&todos).unwrap()
    );

    let parsed = parse_todo_tool_output(&content).expect("timestamped todo payload");
    assert_eq!(parsed.todos.len(), 1);
    assert_eq!(parsed.todos[0].id, todos[0].id);
    assert_eq!(parsed.todos[0].content, todos[0].content);
    assert!(parsed.goals.is_empty());
    assert!(parsed.goal_updates.is_empty());
}

#[path = "tests/message_card_tests.rs"]
mod message_card_tests;
#[allow(unused_imports)]
pub(crate) use message_card_tests::*;

#[path = "tests_additional_tests.rs"]
mod additional_pinned_dependency_label_uses_available_width_and_palette;
#[path = "provider_card_tests.rs"]
mod provider_card_tests;
#[path = "tool_render_tests.rs"]
mod tool_render;
