use super::*;

impl SessionPicker {
    pub(super) fn render_preview(&mut self, frame: &mut Frame, area: Rect) {
        let empty_border_color = if self.focus == PaneFocus::Preview {
            rgb(130, 130, 160)
        } else {
            rgb(50, 50, 50)
        };

        if let Some(message) = self.loading_message.as_deref() {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Preview ")
                .border_style(Style::default().fg(empty_border_color));
            let body = vec![
                Line::from(vec![
                    Span::styled("⏳ ", Style::default().fg(rgb(255, 200, 100))),
                    Span::styled(
                        message.to_string(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "The picker will update as soon as the session index is ready.",
                    Style::default().fg(Color::DarkGray),
                )]),
            ];
            let paragraph = Paragraph::new(body).block(block);
            frame.render_widget(paragraph, area);
            return;
        }

        let _ = self.poll_preview_load();
        let _ = self.poll_workspace_load();
        self.ensure_selected_preview_loading();

        let selected = self.selected_session_ref().and_then(|session_ref| {
            self.session_by_ref(session_ref)
                .map(|session| (session_ref, session))
        });
        let Some((session_ref, session)) = selected else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Preview ")
                .border_style(Style::default().fg(empty_border_color));
            let paragraph = Paragraph::new("No session selected")
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(paragraph, area);
            return;
        };

        let diff_mode = crate::config::config().display.diff_mode;
        // The narrow preview is always left-aligned, independently of the main
        // chat's alignment preference.
        let align = Alignment::Left;

        // Draw the bordered block first so we know the inner rect (which drives
        // wrapping width and the scrollbar decision) before building content.
        let preview_border_color = if self.focus == PaneFocus::Preview {
            rgb(130, 130, 160)
        } else {
            rgb(70, 70, 70)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Preview ")
            .border_style(Style::default().fg(preview_border_color));
        let inner = if self.reserved_panel_area.is_some() {
            // The conversation is the canvas, not a boxed second pane.
            area
        } else {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let preview_still_loading = session.messages_preview.is_empty()
            && self
                .pending_preview_load
                .as_ref()
                .is_some_and(|pending| pending.session_id == session.id);
        let selected_id = session.id.clone();
        if preview_still_loading {
            if let Some(shown_ref) = self.held_preview_ref(&selected_id) {
                // Most previews load within a frame or two. Keep the last one
                // up meanwhile instead of flashing a loading placeholder.
                let scroll = Some(self.scroll_offset as usize);
                self.draw_selected_view(frame, inner, shown_ref, scroll);
                self.latency_transcript_rendered = false;
                return;
            }
        } else {
            self.preview_hold = None;
            if self.preview_shown_id.as_deref() != Some(selected_id.as_str()) {
                self.preview_shown_id = Some(selected_id);
            }
        }
        let Some(content_hash) = self.selected_preview_content_hash(session_ref, diff_mode) else {
            return;
        };
        let key = PreviewCacheKey {
            content_hash,
            inner_width: inner.width,
            inner_height: inner.height,
            diff_mode,
            search_query: self.search_query.trim().to_lowercase(),
        };
        let cache_hit = self.reuse_preview(&key);
        self.resume_latency.cache_lookup(cache_hit);
        if !cache_hit {
            let Some(session) = self.session_by_ref(session_ref) else {
                return;
            };
            let rebuilt = self.build_preview_cache(session, area, inner, key, align, diff_mode);
            self.install_preview(rebuilt);
        }

        let tokens = self.active_highlight_tokens();
        let mut scroll = if self.auto_scroll_preview {
            match self.preview_cache.as_ref().and_then(|c| c.first_match_line) {
                Some(line) if !tokens.is_empty() => Some(line.saturating_sub(2)),
                _ => None,
            }
        } else if self.scroll_offset >= self.preview_max_scroll {
            // Parked at the bottom: keep following a transcript that grows.
            None
        } else {
            Some(self.scroll_offset as usize)
        };
        let mut geometry = self.draw_selected_view(frame, inner, session_ref, scroll);
        // The first match is only known once the renderer prepared the frame.
        if self.auto_scroll_preview
            && scroll.is_none()
            && !tokens.is_empty()
            && let Some(line) = self.preview_cache.as_ref().and_then(|c| c.first_match_line)
        {
            scroll = Some(line.saturating_sub(2));
            geometry = self.draw_selected_view(frame, inner, session_ref, scroll);
        }
        if !tokens.is_empty() {
            highlight_buffer_matches(frame.buffer_mut(), inner, &tokens);
        }
        self.preview_max_scroll = geometry.max_scroll.min(u16::MAX as usize) as u16;
        self.scroll_offset = geometry.scroll.min(u16::MAX as usize) as u16;
        if self.auto_scroll_preview && !preview_still_loading {
            self.auto_scroll_preview = false;
        }
        self.latency_transcript_rendered = !preview_still_loading
            && !self.pending_preview_load.as_ref().is_some_and(|pending| {
                Some(&pending.session_id) == self.selected_session().map(|s| &s.id)
            })
            && !self
                .preview_load_failures
                .contains(self.latency_selection_id().as_deref().unwrap_or(""));
    }

    /// While `loading_id` loads, the session whose preview is still on screen,
    /// for a short grace period. Past it the loading placeholder is honest.
    pub(super) fn held_preview_ref(&mut self, loading_id: &str) -> Option<SessionRef> {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(150);
        if self
            .preview_hold
            .as_ref()
            .is_none_or(|(id, _)| id != loading_id)
        {
            self.preview_hold = Some((loading_id.to_string(), std::time::Instant::now()));
        }
        let (_, since) = self.preview_hold.as_ref()?;
        if since.elapsed() >= GRACE {
            return None;
        }
        self.preview_cache.as_ref()?;
        let shown = self.preview_shown_id.clone()?;
        self.session_ref_for_id(&shown)
    }

    /// Draw the selected session through the real chat renderer.
    pub(super) fn draw_selected_view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        session_ref: SessionRef,
        scroll: Option<usize>,
    ) -> super::super::ui::SecondaryGeometry {
        let Some(session) = self.session_by_ref(session_ref) else {
            return Default::default();
        };
        let Some(cache) = self.preview_cache.as_ref() else {
            return Default::default();
        };
        let extras = self.preview_extras.get(&session.id);
        let host = (session.server_icon.as_deref() == Some("☁"))
            .then(|| session.server_name.as_deref()?.strip_prefix("cloud: "))
            .flatten()
            .map(str::to_string);
        let view = session_view::SessionView {
            messages: &cache.messages,
            version: session_view::preview_messages_version(cache.key.content_hash),
            hidden_prompts: extras.map_or(0, |extras| extras.hidden_prompts),
            info: preview_chrome::selected_info(session, extras),
            todos_payload: extras.and_then(|extras| extras.todos_payload.as_deref()),
            session_name: Some(session.short_name.clone()),
            model: session.model.clone(),
            provider: session.provider_key.clone(),
            server_name: session.server_name.clone(),
            server_icon: session.server_icon.clone(),
            is_canary: session.is_canary,
            working_dir: session.working_dir.clone(),
            local_auth: host.is_none(),
            machine: host,
            scroll,
            live: self
                .live_presence
                .get(&session.id)
                .filter(|presence| presence.streaming)
                .map(|presence| session_view::LiveTurn {
                    status: super::super::ProcessingStatus::Streaming,
                    elapsed: presence
                        .streaming_since
                        .and_then(|since| std::time::SystemTime::now().duration_since(since).ok()),
                    ..Default::default()
                }),
            now_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        };
        let (geometry, prepared) = session_view::draw_session_view(frame, area, &view);
        if let (Some(prepared), Some(cache)) = (prepared, self.preview_cache.as_mut())
            && cache.wrapped_lines.is_empty()
        {
            cache.wrapped_lines = prepared.materialize_all_lines();
            cache.image_regions = prepared.image_regions.clone();
            cache.show_scrollbar = geometry.max_scroll > 0;
            cache.user_prompt_markers = prepared
                .wrapped_user_prompt_starts
                .iter()
                .zip(prepared.user_prompt_texts.iter())
                .enumerate()
                .map(|(idx, (&line, text))| (line, idx + 1, text.clone()))
                .collect();
            let tokens = search_terms::search_query_tokens(&cache.key.search_query);
            cache.first_match_line = cache.wrapped_lines.iter().position(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let lower = text.to_lowercase();
                tokens.iter().any(|token| lower.contains(token.as_str()))
            });
        }
        geometry
    }

    /// Fingerprint everything that affects the *content* of the preview pane so
    /// the wrapped-line cache can be reused across frames (notably while
    /// scrolling). Mirrors the main chat viewport's prepared-frame cache key.
    #[cfg(test)]
    pub(super) fn preview_content_hash(
        &self,
        session: &SessionInfo,
        diff_mode: crate::config::DiffDisplayMode,
    ) -> u64 {
        self.preview_content_hash_with_transcript(
            session,
            diff_mode,
            performance::transcript_hash(session),
        )
    }

    /// Build the fully-wrapped preview content for the current selection. This
    /// is the expensive path (markdown render + wrap of every preview message);
    /// it only runs on a cache miss (selection change, resize, preview load,
    /// config change), after which scrolling reuses the wrapped lines.
    pub(super) fn build_preview_cache(
        &self,
        session: &SessionInfo,
        _area: Rect,
        _inner: Rect,
        key: PreviewCacheKey,
        _align: Alignment,
        _diff_mode: crate::config::DiffDisplayMode,
    ) -> PreviewRenderCache {
        let mut messages = session_view::display_messages_from_preview(&session.messages_preview);
        if messages.is_empty() {
            let loading = self
                .pending_preview_load
                .as_ref()
                .is_some_and(|pending| pending.session_id == session.id);
            let text = if loading {
                "Loading preview…"
            } else if self.preview_load_failures.contains(&session.id) {
                "Preview unavailable (session could not be read)"
            } else {
                "(empty session)"
            };
            messages.push(DisplayMessage::system(text));
        }
        PreviewRenderCache {
            key,
            messages,
            wrapped_lines: Vec::new(),
            image_regions: Vec::new(),
            user_prompt_markers: Vec::new(),
            show_scrollbar: false,
            first_match_line: None,
        }
    }
}
