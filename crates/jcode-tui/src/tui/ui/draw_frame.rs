use super::*;

pub(super) fn draw_inner(frame: &mut Frame, app: &dyn TuiState) {
    let secondary = secondary_render_active();
    if !secondary {
        panel_image_preview::clear_regions();
    }
    let area = frame.area().intersection(*frame.buffer_mut().area());
    if area.width == 0 || area.height == 0 {
        return;
    }

    let total_start = Instant::now();
    if !secondary {
        reset_frame_perf_stats();
        begin_frame_resource_sample();
        clear_copy_viewport_snapshot();
    }

    // Clear full frame to prevent stale cells from prior layouts.
    // This is critical on macOS terminals where ratatui's diff-based updates
    // can leave outdated content when layout dimensions change between frames
    // (e.g., diagram pane toggling, streaming text clearing, tool calls finishing).
    // Uses Color::Reset (terminal default bg) so text selection highlighting works
    // natively in all terminal emulators.
    clear_area(frame, area);

    if let Some(hash) = app.panel_image_preview() {
        panel_image_preview::draw_preview(frame, area, hash);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(overlay) = app.commit_log_overlay() {
        overlays::draw_commit_log_overlay(frame, area, overlay);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(scroll) = app.changelog_scroll() {
        overlays::draw_changelog_overlay(frame, area, scroll, app);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(scroll) = app.help_scroll() {
        overlays::draw_help_overlay(frame, area, scroll, app);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some((scroll, content)) = app.model_status_overlay() {
        overlays::draw_model_status_overlay(frame, area, scroll, content);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.session_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.login_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.account_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    // Initialize visual debug capture if enabled
    let mut debug_capture = if visual_debug::is_enabled() {
        Some(FrameCaptureBuilder::new(area.width, area.height))
    } else {
        None
    };
    let swarm_page_active = app.swarm_panel_full_page();

    // Check diagram display mode and get active diagrams early so we can
    // determine the horizontal split before computing input width etc.
    let diagram_mode = app.diagram_mode();
    let diagrams = super::super::mermaid::get_active_diagrams();
    let diagram_count = diagrams.len();
    let selected_index = if diagram_count > 0 {
        app.diagram_index().min(diagram_count - 1)
    } else {
        0
    };
    let pane_enabled = app.diagram_pane_enabled();
    let pane_position = app.diagram_pane_position();
    let has_side_panel_content = !swarm_page_active && app.side_panel().focused_page().is_some();
    let diff_mode = app.diff_mode();
    let has_file_diff_edits =
        !swarm_page_active && diff_mode.is_file() && app.has_display_edit_tool_messages();
    let has_right_side_pane_content = has_side_panel_content || has_file_diff_edits;
    // Fullscreen side panel replaces the transcript area; status line and input stay.
    let side_panel_fullscreen = has_side_panel_content && app.side_panel_fullscreen();
    // Regular side-panel pages and full-file diffs share the right-hand surface.
    // Suppress a separate diagram pane to avoid a triple-split layout.
    let suppress_side_diagram = has_right_side_pane_content;
    let pinned_diagram = if !swarm_page_active
        && diagram_mode == crate::config::DiagramDisplayMode::Pinned
        && pane_enabled
        && !suppress_side_diagram
    {
        diagrams.get(selected_index).cloned()
    } else {
        None
    };
    let diagram_focus = app.diagram_focus();
    let (diagram_scroll_x, diagram_scroll_y) = app.diagram_scroll();

    // Compute layout depending on pane position (Side = right column, Top = above chat).
    let (chat_area, diagram_area) = if let Some(diagram) = pinned_diagram.as_ref() {
        match pane_position {
            crate::config::DiagramPanePosition::Side => {
                const MIN_DIAGRAM_WIDTH: u16 = 24;
                const MIN_CHAT_WIDTH: u16 = 20;
                let max_diagram = area.width.saturating_sub(MIN_CHAT_WIDTH);
                if max_diagram >= MIN_DIAGRAM_WIDTH {
                    let ratio = app.diagram_pane_ratio().clamp(25, 100) as u32;
                    let ratio_target = ((area.width as u32 * ratio) / 100) as u16;
                    let needed =
                        estimate_pinned_diagram_pane_width(diagram, area.height, MIN_DIAGRAM_WIDTH);
                    // The configured ratio is the upper bound for the pane so the
                    // transcript (which still renders the diagram inline) is never
                    // crushed. Shrink below the ratio when a diagram is narrow
                    // enough to need less, but do not grow past it: a large/tall
                    // diagram just scales down to fit the pane instead of eating
                    // the chat column.
                    let diagram_width = ratio_target
                        .min(needed.max(MIN_DIAGRAM_WIDTH))
                        .max(MIN_DIAGRAM_WIDTH)
                        .min(max_diagram);
                    let chat_width = area.width.saturating_sub(diagram_width);
                    if diagram_width > 0 && chat_width > 0 {
                        let chat = Rect {
                            x: area.x,
                            y: area.y,
                            width: chat_width,
                            height: area.height,
                        };
                        let diag = Rect {
                            x: area.x + chat_width,
                            y: area.y,
                            width: diagram_width,
                            height: area.height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
            crate::config::DiagramPanePosition::Top => {
                const MIN_DIAGRAM_HEIGHT: u16 = 6;
                const MIN_CHAT_HEIGHT: u16 = 8;
                let max_diagram = area.height.saturating_sub(MIN_CHAT_HEIGHT);
                if max_diagram >= MIN_DIAGRAM_HEIGHT {
                    let ratio = app.diagram_pane_ratio().clamp(20, 100) as u32;
                    let ratio_target = ((area.height as u32 * ratio) / 100) as u16;
                    let needed = estimate_pinned_diagram_pane_height(
                        diagram,
                        area.width,
                        MIN_DIAGRAM_HEIGHT,
                    );
                    // Cap the pane at the configured ratio so the transcript keeps
                    // its rows; shrink below it when the diagram is short. A tall
                    // diagram scales down to fit rather than swallowing the chat.
                    let diagram_height = ratio_target
                        .min(needed.max(MIN_DIAGRAM_HEIGHT))
                        .max(MIN_DIAGRAM_HEIGHT)
                        .min(max_diagram);
                    let chat_height = area.height.saturating_sub(diagram_height);
                    if diagram_height > 0 && chat_height > 0 {
                        let diag = Rect {
                            x: area.x,
                            y: area.y,
                            width: area.width,
                            height: diagram_height,
                        };
                        let chat = Rect {
                            x: area.x,
                            y: area.y + diagram_height,
                            width: area.width,
                            height: chat_height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
        }
    } else {
        (area, None)
    };

    let needs_side_pane = has_right_side_pane_content && !side_panel_fullscreen;

    let (chat_area, diff_pane_area) = if needs_side_pane {
        const MIN_DIFF_WIDTH: u16 = 30;
        const MIN_CHAT_WIDTH: u16 = 20;
        let effective_ratio = app.diagram_pane_ratio().clamp(25, 100) as u32;
        let max_diff = chat_area.width.saturating_sub(MIN_CHAT_WIDTH);
        if max_diff >= MIN_DIFF_WIDTH {
            let diff_width = (((chat_area.width as u32 * effective_ratio) / 100) as u16)
                .max(MIN_DIFF_WIDTH)
                .min(max_diff);
            let new_chat_width = chat_area.width.saturating_sub(diff_width);
            let chat = Rect {
                x: chat_area.x,
                y: chat_area.y,
                width: new_chat_width,
                height: chat_area.height,
            };
            let diff = Rect {
                x: chat_area.x + new_chat_width,
                y: chat_area.y,
                width: diff_width,
                height: chat_area.height,
            };
            (chat, Some(diff))
        } else {
            (chat_area, None)
        }
    } else {
        (chat_area, None)
    };

    // Worker activity belongs in pinned todo lanes. The retired bottom swarm
    // strip must not return when todos are absent or the swarm is focused.

    // Calculate pending messages (queued + interleave) for numbering and layout
    let pending_count = input_ui::pending_prompt_count(app);
    let queued_height = pending_count.min(3) as u16;

    // Count user messages to show next prompt number
    let next_prompt = input_ui::next_input_prompt_number(app);
    let user_count = next_prompt.saturating_sub(pending_count).saturating_sub(1);

    // Calculate input height based on the same wrapping logic used for rendering
    // (max 10 lines visible, scrolls if more).
    let base_input_height =
        input_ui::wrapped_input_line_count(app, chat_area.width, next_prompt).min(10) as u16;
    // Add 1 line for command suggestions, shell mode hints, or the Ctrl+Enter hint.
    let hint_line_height = input_ui::input_hint_line_height(app);
    let inline_block_height: u16 = inline_ui_height(app);
    let blocker_lines = messages::todo_blocker_footer(app.pinned_todos_payload(), chat_area.width);
    let blocker_height = blocker_lines
        .len()
        .min(usize::from(chat_area.height.saturating_sub(6))) as u16;
    let inline_ui_gap_height: u16 = blocker_height.max(if inline_block_height > 0 { 1 } else { 0 });
    let input_height = base_input_height + hint_line_height;

    if let Some(ref mut capture) = debug_capture {
        capture.step("prepare_messages");
    }
    let prep_start = Instant::now();
    let chat_left_inset = left_aligned_content_inset(chat_area.width, app.centered_mode());
    let wide_prepare_width = chat_area.width.saturating_sub(chat_left_inset);
    let narrow_prepare_width = wide_prepare_width.saturating_sub(1);
    let pinned_mermaid_aspect_ratio =
        diagram_area.and_then(|area| pinned_diagram_preferred_aspect_ratio(area, pane_position));
    let aspect_start = Instant::now();
    // Aspect-ratio goal for transcript mermaid renders (deferred and
    // synchronous): the pinned pane's aspect wins when the pane is open so
    // inline and pane share one cached PNG; otherwise a terminal-friendly
    // inline goal keeps diagrams within a readable-height budget. Best-effort:
    // falls back to None (today's 4:3 sizing) when font geometry is unknown.
    let transcript_mermaid_aspect_ratio = mermaid::transcript_preferred_aspect_ratio(
        pinned_mermaid_aspect_ratio,
        wide_prepare_width,
        chat_area.height,
    );
    note_prep_aspect(aspect_start.elapsed());
    let prepare_at = |width: u16| {
        let started = Instant::now();
        let prepared =
            mermaid::with_preferred_aspect_ratio(transcript_mermaid_aspect_ratio, || {
                prepare::prepare_messages(app, width, chat_area.height)
            });
        note_prep_prepare_at(started.elapsed());
        prepared
    };

    let onboarding_welcome = app.onboarding_welcome_active();

    // The guided onboarding phases (login import, OpenAI prompt, continue prompt)
    // are entirely key-driven and own the whole chat column: they render their own
    // telemetry header, a prominent donut, and the welcome body. Suppress the
    // normal chat chrome (status line, input box, notification, idle hint) so the
    // screen stays focused and the donut gets the full height. The resting
    // Suggestions screen keeps the input box so the user can type to start.
    let onboarding_takes_over = onboarding_welcome
        && !matches!(
            app.onboarding_welcome_kind(),
            crate::tui::OnboardingWelcomeKind::Suggestions
        );
    if onboarding_takes_over {
        onboarding::draw_onboarding_welcome(frame, app, chat_area);
        finalize_frame_metrics(
            app,
            total_start,
            prep_start.elapsed(),
            total_start.elapsed(),
            None,
        );
        return;
    }

    let show_donut = !onboarding_welcome && crate::tui::idle_donut::idle_donut_active(app);
    let donut_height: u16 = idle_donut_reserved_height(show_donut, input_height);
    let notification_height =
        input_ui::notification_height(app, chat_area.width).min(chat_area.height.saturating_sub(4));
    // Session status line (dir, branch, context, provider, model), always
    // pinned directly below the input line.
    let overscroll_height: u16 = 1;
    let fixed_height = 1
        + queued_height
        + notification_height
        + inline_block_height
        + inline_ui_gap_height
        + input_height
        + overscroll_height
        + donut_height; // status + queued + swarm strip + notification + inline UI + gap + input + overscroll + donut
    let available_height = chat_area.height;
    let stable_fixed_height = fixed_height;
    let overflows = |prepared: &PreparedChatFrame| {
        let started = Instant::now();
        let result =
            (prepared.total_wrapped_lines().max(1) as u16) + stable_fixed_height > available_height;
        note_prep_overflow(started.elapsed());
        result
    };

    // Resolving native-scrollbar overflow can require wrapping the transcript at
    // two different widths (the wide layout, and one column narrower to reserve a
    // scrollbar column). Preparing both every frame doubles the most expensive work
    // and thrashes the prep caches on long transcripts during streaming. Use the
    // previous frame's scrollbar decision as hysteresis so the steady state only
    // prepares a single width; the second width is only built at a visibility
    // transition. This is safe because narrow wraps at least as much as wide, so
    // "narrow fits" implies "wide fits".
    let scrollbar_enabled = app.chat_native_scrollbar() && chat_area.width > 1;
    let initial_content_height;
    let (prepared, chat_scrollbar_visible) = if !scrollbar_enabled {
        let prepared_wide = prepare_at(wide_prepare_width);
        initial_content_height = prepared_wide.total_wrapped_lines().max(1) as u16;
        (prepared_wide, false)
    } else if last_chat_scrollbar_visible()
        || transcript_definitely_overflows(
            app.display_messages(),
            available_height.saturating_sub(stable_fixed_height) as usize,
        )
    {
        // Prior visibility or the source-row lower bound suggests starting
        // narrow. Still check the actual layout before keeping the scrollbar.
        let prepared_narrow = prepare_at(narrow_prepare_width);
        initial_content_height = prepared_narrow.total_wrapped_lines().max(1) as u16;
        if overflows(&prepared_narrow) {
            (prepared_narrow, true)
        } else {
            // Content shrank enough to fit even at the narrower width, so the wide
            // layout (which wraps no more) also fits: drop the scrollbar.
            (prepare_at(wide_prepare_width), false)
        }
    } else {
        // No scrollbar last frame: prepare the wide layout first. Only when it
        // overflows do we evaluate the narrow layout to decide on the scrollbar.
        let prepared_wide = prepare_at(wide_prepare_width);
        initial_content_height = prepared_wide.total_wrapped_lines().max(1) as u16;
        if !overflows(&prepared_wide) {
            (prepared_wide, false)
        } else {
            let prepared_narrow = prepare_at(narrow_prepare_width);
            if overflows(&prepared_narrow) {
                (prepared_narrow, true)
            } else {
                // Reserving a scrollbar column changed the wrapped content enough to
                // make it fit. Prefer the wide layout without the native scrollbar so
                // the UI does not oscillate between two self-contradictory states
                // across consecutive frames.
                (prepared_wide, false)
            }
        }
    };
    set_last_chat_scrollbar_visible(chat_scrollbar_visible);
    if let Some(ref mut capture) = debug_capture {
        capture.set_image_regions(
            prepared
                .image_regions
                .iter()
                .map(|region| ImageRegionCapture {
                    hash: format!("{:016x}", region.hash),
                    abs_line_idx: region.abs_line_idx,
                    height: region.height,
                })
                .collect(),
        );
    }
    let prep_elapsed = prep_start.elapsed();
    let clear_start = terminal_clear_start(app, &prepared);
    // The pinned todo band consumes transcript rows too, including in a fresh
    // session. Reserve it before packing the view so the first todo update
    // cannot squeeze the user prompt out of the messages area.
    let pinned_band_height = {
        let width = chat_area
            .width
            .saturating_sub(u16::from(chat_scrollbar_visible));
        let width = width.saturating_sub(left_aligned_content_inset(width, app.centered_mode()));
        viewport::pinned_todo_band_lines(app, width, available_height.saturating_sub(fixed_height))
            .0
            .len()
    };
    // A fresh cloud transcript is short even on a tall terminal. Keep enough
    // packed rows for the cloud card stack instead of hiding directory counts
    // above an otherwise unused lower half of the screen.
    let widget_data_start = Instant::now();
    let widget_data = app.info_widget_data();
    let widget_data_elapsed = widget_data_start.elapsed();
    let cloud_card_height = if app.info_widget_overlays_enabled()
        && widget_data.cloud_info.is_some()
        && chat_area.width >= 120
        && !show_donut
        && !swarm_page_active
        && !side_panel_fullscreen
    {
        [
            info_widget::WidgetKind::Overview,
            info_widget::WidgetKind::CloudVm,
            info_widget::WidgetKind::CloudDirectories,
        ]
        .into_iter()
        .map(|kind| info_widget::calculate_widget_height(kind, &widget_data, 40, available_height))
        .fold(0u16, u16::saturating_add)
        // Placement reserves breathing room around the card stack.
        .saturating_add(8)
        .min(available_height.saturating_sub(fixed_height))
    } else {
        0
    };
    let content_height = prepared
        .total_wrapped_lines()
        .saturating_sub(clear_start.unwrap_or(0))
        .saturating_add(pinned_band_height)
        .max(1)
        .min(u16::MAX as usize) as u16;
    let content_height = content_height.max(cloud_card_height);

    // Terminal-style clear (Ctrl+L): the trailing spacer makes every visible
    // transcript row blank, so a normal bottom-anchored layout would park the
    // status line and numbered prompt at the *bottom* of a screenful of
    // blanks. Collapse the messages area instead, exactly like a terminal
    // after `clear`: the prompt sits at the top and the history is one scroll
    // away. New output expands only the post-clear content. Scrolling up
    // restores the full transcript layout.
    let terminal_clear_collapsed = !swarm_page_active && app.terminal_clear_collapsed();
    let content_height = if terminal_clear_collapsed {
        0
    } else {
        content_height
    };

    // Use packed layout when content fits, scrolling layout otherwise
    let use_packed = terminal_clear_collapsed
        || (!swarm_page_active
            && !side_panel_fullscreen
            && content_height.saturating_add(fixed_height) <= available_height);

    // Layout: messages (includes header), queued, status, notification, inline UI, gap, input, donut
    // All vertical chunks are within the chat_area (left column).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if use_packed {
            vec![
                Constraint::Length(if terminal_clear_collapsed {
                    0
                } else {
                    content_height.max(1)
                }), // 0 Messages (exact height; 0 when terminal-cleared)
                Constraint::Length(queued_height), // 1 Queued messages (above status)
                Constraint::Length(0),             // 2 Retired swarm strip slot
                Constraint::Length(1),             // 3 Status line
                Constraint::Length(notification_height), // 4 Notification line
                Constraint::Length(inline_block_height), // 5 Inline UI
                Constraint::Length(inline_ui_gap_height), // 6 Inline UI/input spacing
                Constraint::Length(input_height),  // 7 Input
                Constraint::Length(overscroll_height), // 8 Overscroll status line
                Constraint::Length(donut_height),  // 9 Donut animation
            ]
        } else {
            vec![
                Constraint::Min(3),                       // 0 Messages (scrollable)
                Constraint::Length(queued_height),        // 1 Queued messages (above status)
                Constraint::Length(0),                    // 2 Retired swarm strip slot
                Constraint::Length(1),                    // 3 Status line
                Constraint::Length(notification_height),  // 4 Notification line
                Constraint::Length(inline_block_height),  // 5 Inline UI
                Constraint::Length(inline_ui_gap_height), // 6 Inline UI/input spacing
                Constraint::Length(input_height),         // 7 Input
                Constraint::Length(overscroll_height),    // 8 Overscroll status line
                Constraint::Length(donut_height),         // 9 Donut animation
            ]
        })
        .split(chat_area);
    record_status_area(chunks[3]);

    if let Some(ref mut capture) = debug_capture {
        let probe = debug_capture::RenderProbe::new(
            app,
            pending_count,
            (diagram_mode, diagram_focus, selected_index, diagram_count),
            (diagram_scroll_x, diagram_scroll_y),
        );
        capture.record_frame(
            &probe,
            jcode_tui_visual_debug::LayoutCapture {
                use_packed,
                estimated_content_height: content_height as usize,
                messages_area: Some(chunks[0].into()),
                queued_area: (queued_height > 0).then(|| chunks[1].into()),
                status_area: Some(chunks[3].into()),
                input_area: Some(chunks[7].into()),
                input_lines_raw: app.input().lines().count().max(1),
                input_lines_wrapped: base_input_height as usize,
                ..Default::default()
            },
        );
    }

    if let Some(ref mut capture) = debug_capture {
        capture.step("draw_messages");
    }
    let draw_start = Instant::now();

    // Messages area is chunks[0] within the chat column (already excludes diagram).
    let messages_area = chunks[0];
    let diff_pane_area = if side_panel_fullscreen {
        Some(messages_area)
    } else {
        diff_pane_area
    };
    note_chat_layout(ChatLayoutMetrics {
        chat_area,
        messages_area,
        initial_content_height: initial_content_height as usize,
        content_height: content_height as usize,
        chat_scrollbar_visible,
        use_packed_layout: use_packed,
        has_side_panel_content,
        has_file_diff_edits,
    });

    if let Some(ref mut capture) = debug_capture {
        capture.record_pane_geometry(messages_area, diagram_area);
    }
    record_layout_snapshot(messages_area, diagram_area, diff_pane_area, Some(chunks[7]));

    let mut chat_scrollbar: Option<NativeScrollbar> = None;
    let margins = if onboarding_welcome {
        onboarding::draw_onboarding_welcome(frame, app, messages_area);
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: app.centered_mode(),
            ..Default::default()
        }
    } else if swarm_page_active {
        let members = app.inline_swarm_members();
        let spinner_frame =
            (app.animation_elapsed() * jcode_tui_render::swarm_gallery::STRIP_SPINNER_FPS) as usize;
        let lines = super::super::info_widget::swarm_gallery::render_swarm_page_lines(
            &members,
            app.swarm_panel_selected(),
            spinner_frame,
            messages_area.width as usize,
            messages_area.height as usize,
        );
        clear_area(frame, messages_area);
        frame.render_widget(Paragraph::new(lines), messages_area);
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: false,
            ..Default::default()
        }
    } else if terminal_clear_collapsed || side_panel_fullscreen {
        if side_panel_fullscreen {
            clear_area(frame, messages_area);
        }
        // Collapsed terminal-style clear: the messages chunk is zero-height, so
        // there is nothing to draw. Deliberately skip `draw_messages` so it does
        // not publish a zero-height viewport/max-scroll geometry that the scroll
        // handlers would then resolve against; the last real geometry stays
        // authoritative until the first scroll-up restores the full layout.
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: app.centered_mode(),
            ..Default::default()
        }
    } else {
        let (margins, scrollbar) = draw_messages(
            frame,
            app,
            messages_area,
            prepared.clone(),
            chat_scrollbar_visible,
        );
        chat_scrollbar = scrollbar;
        margins
    };

    crate::tui::reset_pinned_diagram_debug_snapshot();
    // Render pinned diagram if we have one
    if let (Some(diagram_info), Some(area)) = (&pinned_diagram, diagram_area) {
        if let Some(ref mut capture) = debug_capture {
            capture.step("draw_pinned_diagram");
        }
        draw_pinned_diagram(
            frame,
            diagram_info,
            area,
            selected_index,
            diagram_count,
            diagram_focus,
            diagram_scroll_x,
            diagram_scroll_y,
            app.diagram_zoom(),
            pane_position,
            app.diagram_pane_animating(),
        );
    }

    crate::tui::clear_side_panel_debug_snapshot();
    if let Some(diff_area) = diff_pane_area {
        if has_side_panel_content {
            if let Some(ref mut capture) = debug_capture {
                capture.step("draw_side_panel_markdown");
            }
            draw_side_panel_markdown(
                frame,
                diff_area,
                app,
                app.side_panel(),
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
                app.centered_mode(),
            );
        } else if has_file_diff_edits {
            if let Some(ref mut capture) = debug_capture {
                capture.step("draw_file_diff_view");
            }
            draw_file_diff_view(
                frame,
                diff_area,
                app,
                prepared.as_ref(),
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
            );
        }
    }

    let messages_draw = draw_start.elapsed();

    if let Some(ref mut capture) = debug_capture {
        capture.record_margins(MarginsCapture {
            left_widths: margins.left_widths.clone(),
            right_widths: margins.right_widths.clone(),
            centered: margins.centered,
        });
    }
    let chrome_start = Instant::now();
    if queued_height > 0 {
        if let Some(ref mut capture) = debug_capture {
            capture.step("draw_queued");
        }
        input_ui::draw_queued(frame, app, chunks[1], user_count + 1);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.step("draw_status");
    }
    input_ui::draw_status(frame, app, chunks[3], pending_count);
    if notification_height > 0 {
        input_ui::draw_notification(frame, app, chunks[4]);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.step("draw_input");
    }
    // Draw inline UI if active
    if inline_block_height > 0 {
        draw_inline_ui(frame, app, chunks[5]);
    }

    if blocker_height > 0 {
        frame.render_widget(Paragraph::new(blocker_lines), chunks[6]);
    }

    let _input_cursor =
        input_ui::draw_input(frame, app, chunks[7], next_prompt, &mut debug_capture);

    if overscroll_height > 0 {
        input_ui::draw_overscroll_status(frame, app, chunks[8]);
    }

    if donut_height > 0 {
        animations::draw_idle_animation(frame, app, chunks[9]);
    }
    let chrome_elapsed = chrome_start.elapsed();

    // Draw info widget overlays (skip during idle animation - they look out of place)
    let mut widget_render_ms: Option<f32> = None;
    let mut placements: Vec<info_widget::WidgetPlacement> = Vec::new();
    let widget_bounds = messages_area;
    if app.info_widget_overlays_enabled()
        && !widget_data.is_empty()
        && !show_donut
        && !swarm_page_active
        && !side_panel_fullscreen
    {
        if let Some(ref mut capture) = debug_capture {
            capture.step("render_info_widgets");
        }
        placements = info_widget::calculate_placements(widget_bounds, &margins, &widget_data);

        if let Some(ref mut capture) = debug_capture {
            let placement_captures = capture_widget_placements(&placements);
            capture.set_info_widgets(InfoWidgetCapture {
                summary: build_info_widget_summary(&widget_data),
                placements: placement_captures,
            });

            // Detect overlaps with used content. Info widgets live inside the
            // messages rectangle by design, so a whole-area overlap check is
            // always true and useless; instead verify each placement still fits
            // within the free margin the layout reported for the rows it covers.
            for placement in &placements {
                if widget_overlaps_content(placement, widget_bounds, &margins) {
                    capture.anomaly(format!(
                        "Info widget {:?} intrudes into content (rect {:?})",
                        placement.kind, placement.rect
                    ));
                }
                if !rect_within_bounds(placement.rect, area) {
                    capture.anomaly(format!(
                        "Info widget {:?} out of bounds {:?}",
                        placement.kind, placement.rect
                    ));
                }
                if let Some(diagram_area) = diagram_area
                    && rects_overlap(placement.rect, diagram_area)
                {
                    capture.anomaly(format!(
                        "Info widget {:?} overlaps diagram area",
                        placement.kind
                    ));
                }
            }
            for i in 0..placements.len() {
                for j in (i + 1)..placements.len() {
                    if rects_overlap(placements[i].rect, placements[j].rect) {
                        capture.anomaly(format!(
                            "Info widgets overlap: {:?} and {:?}",
                            placements[i].kind, placements[j].kind
                        ));
                    }
                }
            }
        }

        let widget_start = Instant::now();
        info_widget::render_all(frame, &placements, &widget_data);
        // Merge docked widget borders into the shared scrollbar column, then
        // highlight the thumb without erasing horizontal border connections.
        if let Some(scrollbar) = chat_scrollbar
            && placements
                .iter()
                .any(|placement| rects_overlap(placement.rect, scrollbar.area))
        {
            scrollbar.render_thumb_overlay(frame);
        }
        widget_render_ms = Some(widget_start.elapsed().as_secs_f32() * 1000.0);

        // Optional visual overlay for placements
    } else {
        // The widget pass did not run (idle donut takeover or no widget data),
        // so nothing from the previous frame is on screen anymore. Clear the
        // remembered placements/anchors so consumers of last-frame state (the
        // swarm strip stand-down, idle fallback facts) do not keep reacting to
        // widgets that are no longer drawn.
        info_widget::note_widget_pass_skipped();
        if let Some(ref mut capture) = debug_capture {
            capture.set_info_widgets(InfoWidgetCapture {
                summary: build_info_widget_summary(&widget_data),
                placements: Vec::new(),
            });
        }
    }

    if visual_debug::overlay_enabled() {
        overlays::draw_debug_overlay(frame, &placements, &chunks);
    }

    // Command-suggestion popover: a late overlay pass so the palette floats
    // over existing rows (blank space, pinned footer, or the transcript tail)
    // instead of reserving layout height and shoving everything around.
    input_ui::draw_command_suggestions_overlay(frame, app, chunks[7]);

    // Ctrl+R reverse prompt-history search overlay (drawn after the command
    // palette so it wins when both could be visible).
    input_ui::draw_prompt_history_search_overlay(frame, app, chunks[7]);

    // Observe the rendered messages area for the anchor-stability (smoothness)
    // report. Runs on the final buffer so it sees exactly what the user sees.
    smoothness::observe_frame(
        frame.buffer_mut(),
        messages_area,
        app.scroll_offset(),
        !app.auto_scroll_paused(),
    );

    let frame_elapsed = total_start.elapsed();
    if frame_elapsed >= Duration::from_millis(250) {
        crate::logging::warn(&format!(
            "TUI_RENDER_PHASES prepare={}ms messages={}ms chrome={}ms widget_data={}ms widget_render={}ms final={}ms total={}ms",
            prep_elapsed.as_millis(),
            messages_draw.as_millis(),
            chrome_elapsed.as_millis(),
            widget_data_elapsed.as_millis(),
            widget_render_ms.unwrap_or_default(),
            frame_elapsed
                .saturating_sub(prep_elapsed)
                .saturating_sub(messages_draw)
                .saturating_sub(chrome_elapsed)
                .saturating_sub(widget_data_elapsed)
                .saturating_sub(Duration::from_secs_f32(
                    widget_render_ms.unwrap_or_default() / 1000.0,
                ))
                .as_millis(),
            frame_elapsed.as_millis(),
        ));
    }

    // Record the frame capture if enabled
    if let Some(capture) = debug_capture {
        let total_draw = draw_start.elapsed();
        let render_timing = RenderTimingCapture {
            prepare_ms: prep_elapsed.as_secs_f32() * 1000.0,
            draw_ms: total_draw.as_secs_f32() * 1000.0,
            total_ms: total_start.elapsed().as_secs_f32() * 1000.0,
            messages_ms: Some(messages_draw.as_secs_f32() * 1000.0),
            widgets_ms: widget_render_ms,
        };

        let mut capture = capture;
        capture.attach_diagnostics(visual_debug::FrameDiagnostics {
            render_timing,
            mermaid: crate::tui::mermaid::debug_stats_json(),
            side_panel: crate::tui::side_panel_debug_json(),
            markdown: crate::tui::markdown::debug_stats_json(),
            theme: overlays::debug_palette_json(),
        });
        if !secondary_render_active() {
            visual_debug::record_frame(capture.build());
        }
    }

    finalize_frame_metrics(
        app,
        total_start,
        prep_elapsed,
        draw_start.elapsed(),
        Some(messages_draw.as_secs_f64() * 1000.0),
    );
}
