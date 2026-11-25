use super::*;

impl SessionPicker {
    /// Label for the blank-session onboarding action, naming the directory
    /// the new session will start in (home abbreviated to `~`).
    pub(super) fn onboarding_start_new_label(&self) -> String {
        let dir = self.current_dir.as_deref().map(str::to_string).or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        });
        match dir {
            Some(dir) => {
                let shown = match dirs::home_dir().map(|h| h.display().to_string()) {
                    Some(home) if dir == home => "~ (home)".to_string(),
                    Some(home) => match dir.strip_prefix(&home) {
                        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
                        _ => dir,
                    },
                    None => dir,
                };
                format!("New session in {shown}")
            }
            None => "New session in the current directory".to_string(),
        }
    }

    /// Render the first-run choice: the welcome copy, then the two actions
    /// side by side with an "or" between them. When the row is too narrow,
    /// the actions stack vertically with the "or" on its own line.
    pub(super) fn render_onboarding_band(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let accent = rgb(186, 139, 255);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if inner.height == 0 {
            return;
        }

        let content_width = inner.width.min(108);
        let content_x = inner.x + inner.width.saturating_sub(content_width) / 2;
        let prompt_lines = self.onboarding_banner.clone().unwrap_or_default();
        let prompt_height = prompt_lines
            .iter()
            .map(|line| {
                let width = line.width().max(1) as u16;
                width.div_ceil(content_width.max(1))
            })
            .sum::<u16>()
            .min(inner.height.saturating_sub(2));
        let review_y = inner.y + inner.height.saturating_sub(1) / 2;
        let prompt_y = review_y
            .saturating_sub(prompt_height.saturating_add(2))
            .max(inner.y);
        let prompt_area = Rect {
            x: content_x,
            y: prompt_y,
            width: content_width,
            height: prompt_height.min(review_y.saturating_sub(prompt_y)),
        };

        if prompt_area.height > 0 {
            let prompt = Paragraph::new(prompt_lines)
                .alignment(Alignment::Center)
                .wrap(ratatui::widgets::Wrap { trim: false });
            frame.render_widget(prompt, prompt_area);
        }

        let action_spans = |label: &str, selected: bool| -> Vec<Span<'static>> {
            let (cap_style, body_style) = if selected {
                (
                    Style::default().fg(accent),
                    Style::default()
                        .fg(rgb(20, 24, 32))
                        .bg(accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    Style::default().fg(rgb(58, 62, 70)),
                    Style::default().fg(rgb(170, 174, 182)).bg(rgb(58, 62, 70)),
                )
            };
            vec![
                Span::styled("\u{25D6}", cap_style),
                Span::styled(format!(" {label} "), body_style),
                Span::styled("\u{25D7}", cap_style),
            ]
        };

        let review_selected = self.onboarding_review_recent_project_highlighted();
        let start_selected = self.onboarding_start_new_highlighted();
        let review = action_spans(ONBOARDING_REVIEW_LABEL, review_selected);
        let start_label = self.onboarding_start_new_label();
        let start = action_spans(&start_label, start_selected);
        let or_span = || Span::styled("or", Style::default().fg(rgb(130, 134, 142)));

        let width_of =
            |spans: &[Span<'static>]| -> u16 { spans.iter().map(|s| s.width() as u16).sum() };
        // "<review>   or   <start>"
        let side_by_side_width = width_of(&review) + 8 + width_of(&start);
        let row = |y: u16| Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };

        if side_by_side_width <= inner.width {
            let mut spans = review;
            spans.push(Span::raw("   "));
            spans.push(or_span());
            spans.push(Span::raw("   "));
            spans.extend(start);
            frame.render_widget(
                Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
                row(review_y),
            );
        } else {
            let max_y = inner.y + inner.height.saturating_sub(1);
            let lines = [
                Line::from(review),
                Line::from(vec![or_span()]),
                Line::from(start),
            ];
            for (i, line) in lines.into_iter().enumerate() {
                let y = review_y + i as u16;
                if y > max_y {
                    break;
                }
                frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), row(y));
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let at = std::time::Instant::now();
        let initial_id = self.latency_selection_id();
        self.resume_latency.initial_selection(initial_id);
        self.latency_transcript_rendered = false;
        self.render_inner(frame);
        let id = if frame.area().width > 0 && frame.area().height > 0 {
            self.latency_selection_id()
        } else {
            None
        };
        if let Some(id) = id.as_ref()
            && self.preview_load_failures.contains(id)
        {
            self.resume_latency.fail(id);
        }
        self.resume_latency
            .frame(id, self.latency_transcript_rendered, at.elapsed());
    }

    pub(super) fn render_inner(&mut self, frame: &mut Frame) {
        let has_banner = self.crashed_sessions.is_some();
        let has_search = self.search_active || !self.search_query.is_empty();
        let has_onboarding = self.onboarding_banner.is_some();
        // The first-run picker is action-only. Do not render the session list or
        // preview panes underneath it, which would make this look like `/resume`.
        if has_onboarding && self.visible_sessions.is_empty() {
            self.last_list_area = None;
            self.last_preview_area = None;
            self.reserved_panel_area = None;
            self.render_onboarding_band(frame, frame.area());
            return;
        }

        if !has_onboarding {
            self.render_resume_canvas(frame);
            return;
        }
        self.reserved_panel_area = None;

        // Build vertical constraints
        let mut v_constraints = Vec::new();
        if has_onboarding {
            // Reserve ~20% of the height for the onboarding prompt and actions,
            // clamped to a sensible band.
            let total = frame.area().height;
            let reserved = ((total as u32 * 20 / 100) as u16).clamp(7, total.saturating_sub(6));
            v_constraints.push(Constraint::Length(reserved.max(7)));
        }
        if has_banner {
            v_constraints.push(Constraint::Length(1));
        }
        if has_search {
            v_constraints.push(Constraint::Length(1));
        }
        v_constraints.push(Constraint::Min(8));

        let v_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(v_constraints)
            .split(frame.area());

        let mut chunk_idx = 0;

        // Render the onboarding band (prompt + action rows) if present.
        if has_onboarding {
            self.render_onboarding_band(frame, v_chunks[chunk_idx]);
            chunk_idx += 1;
        }

        // Render banner if present
        if has_banner {
            self.render_crash_banner(frame, v_chunks[chunk_idx]);
            chunk_idx += 1;
        }

        // Render search bar if active
        if has_search {
            let search_area = v_chunks[chunk_idx];
            chunk_idx += 1;

            let cursor_char = if self.search_active { "▎" } else { "" };
            let search_line = Line::from(vec![
                Span::styled(" 🔍 ", Style::default().fg(rgb(186, 139, 255))),
                Span::styled(
                    &self.search_query,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(cursor_char, Style::default().fg(rgb(186, 139, 255))),
                if self.search_active {
                    Span::styled("  Esc to clear", Style::default().fg(rgb(60, 60, 60)))
                } else {
                    Span::styled("  / to edit", Style::default().fg(rgb(60, 60, 60)))
                },
            ]);
            let search_widget =
                Paragraph::new(search_line).style(Style::default().bg(rgb(25, 25, 30)));
            frame.render_widget(search_widget, search_area);
        }

        let main_area = v_chunks[chunk_idx];

        // Keep the transcript on the left and the session sidebar on the right.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(main_area);

        self.last_list_area = Some(chunks[1]);
        self.last_preview_area = Some(chunks[0]);

        self.render_session_list(frame, chunks[1]);
        self.render_preview(frame, chunks[0]);
        self.render_claude_takeover_confirmation(frame);
    }
}
