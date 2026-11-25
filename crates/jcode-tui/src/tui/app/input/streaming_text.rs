use super::*;

impl App {
    pub(in super::super) fn insert_thought_line(&mut self, line: String) {
        if self.thought_line_inserted || line.is_empty() {
            return;
        }
        self.thought_line_inserted = true;
        let mut prefix = line;
        if !prefix.ends_with('\n') {
            prefix.push('\n');
        }
        prefix.push('\n');
        if self.streaming.streaming_text.is_empty() {
            self.replace_streaming_text(prefix);
        } else {
            self.replace_streaming_text(format!("{}{}", prefix, self.streaming.streaming_text));
        }
    }

    /// Garbage-collect *stale* reasoning traces (every anchored trace except
    /// the most recent one) that are provably above the tail-following
    /// viewport, so their removal causes zero visible motion. Keeps `current`
    /// mode meaning "the current thought": old thoughts dissolve once they
    /// scroll out of view instead of accumulating across a long agentic turn.
    /// Skipped entirely while the user has scrolled up (their reading position
    /// must not shift).
    pub(in super::super) fn gc_offscreen_reasoning_traces(&mut self) -> bool {
        // Only the traces *before* the most recent one are stale.
        if self.turn_reasoning_traces.len() < 2 {
            return false;
        }
        if self.auto_scroll_paused {
            // User is reading history; never remove anything they might see.
            return false;
        }
        let total = crate::tui::ui::last_total_wrapped_lines();
        let viewport = crate::tui::ui::last_layout_snapshot()
            .map(|l| l.messages_area.height as usize)
            .unwrap_or(0);
        if total == 0 || viewport == 0 {
            return false;
        }
        // A trace anchored when the transcript was `at_anchor` lines tall sits
        // entirely above wrapped line `at_anchor`. While tail-following, the
        // viewport shows the last `viewport` lines, so once the transcript has
        // grown a full viewport past the anchor point (with margin for the
        // separator blank line), the trace cannot be on screen.
        let last = self.turn_reasoning_traces.len() - 1;
        let stale: Vec<usize> = self.turn_reasoning_traces[..last]
            .iter()
            .filter(|t| total.saturating_sub(t.wrapped_lines_at_anchor) > viewport + 2)
            .map(|t| t.display_index)
            .collect();
        if stale.is_empty() {
            return false;
        }
        let removed = self.remove_reasoning_trace_messages(stale.iter().copied());
        if removed > 0 {
            // Re-track surviving traces with adjusted display indices.
            self.turn_reasoning_traces.retain_mut(|t| {
                if stale.contains(&t.display_index) {
                    return false;
                }
                let shift = stale.iter().filter(|&&s| s < t.display_index).count();
                t.display_index -= shift;
                true
            });
            self.bump_display_messages_version();
            self.refresh_split_view_if_needed();
            return true;
        }
        false
    }

    /// Remove reasoning display messages at the given (pre-removal) indices.
    /// Returns how many were removed.
    pub(in super::super) fn remove_reasoning_trace_messages(
        &mut self,
        indices: impl Iterator<Item = usize>,
    ) -> usize {
        let mut sorted: Vec<usize> = indices.collect();
        sorted.sort_unstable();
        let mut removed = 0usize;
        for idx in sorted {
            let idx = idx.saturating_sub(removed);
            if idx < self.display_messages.len() && self.display_messages[idx].role == "reasoning" {
                self.display_messages.remove(idx);
                removed += 1;
            }
        }
        removed
    }

    pub(in super::super) fn append_streaming_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Invariant: answer text is never appended *into* an open reasoning
        // region. If a region is still open when real (non-whitespace) answer
        // text arrives, close it first so the next `open_reasoning_region` still
        // inserts its blank-line separator. Without this, a stale
        // `reasoning_streaming` flag makes `open_reasoning_region` early-return
        // and the answer tail gets glued directly onto the next reasoning run
        // (e.g. `...patch + build.Ah, I see...`). Whitespace-only appends (the
        // separators emitted by the reasoning helpers themselves) never trip
        // this. `open_reasoning_region` only appends its separator *before*
        // setting the flag, so this cannot recurse.
        if self.reasoning_streaming && !text.trim().is_empty() {
            self.close_reasoning_region(None);
        }
        // A whitespace-only append skips the close above, so it pushes text
        // *past* the live reasoning tail while `reasoning_partial_len` still
        // claims the tail sits at the end of the buffer. That offset then no
        // longer describes the buffer, which is the same desync class as
        // #632/#633/#635. The tail is only rebuildable while it is still the
        // suffix, so drop it here rather than leave a stale length behind.
        if self.reasoning_partial_len > 0 {
            self.reasoning_partial_len = 0;
        }
        self.streaming.streaming_text_mut().push_str(text);
        self.refresh_split_view_if_needed();
    }

    /// Apply a batch of paced [`StreamOp`]s from the segment-aware
    /// [`StreamBuffer`](crate::tui::stream_buffer::StreamBuffer) to the live
    /// streaming view, preserving arrival order across answer text, reasoning
    /// text, and reasoning-region boundaries. Returns true when anything
    /// visible changed.
    pub(in super::super) fn apply_stream_ops(
        &mut self,
        ops: Vec<crate::tui::stream_buffer::StreamOp>,
    ) -> bool {
        use crate::tui::stream_buffer::StreamOp;
        let mut changed = false;
        for op in ops {
            match op {
                StreamOp::Text(text) => {
                    if !text.is_empty() {
                        // `append_streaming_text` enforces the invariant that real
                        // answer text closes any still-open reasoning region first
                        // (so the region's blank-line separator is preserved). The
                        // buffer also queues an explicit CloseReasoning before
                        // non-whitespace text, so this is normally already closed.
                        self.append_streaming_text(&text);
                        changed = true;
                    }
                }
                StreamOp::Reasoning(text) => {
                    if !text.is_empty() {
                        self.append_reasoning_text(&text);
                        changed = true;
                    }
                }
                StreamOp::CloseReasoning => {
                    if self.reasoning_streaming {
                        self.close_reasoning_region(None);
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// In `current` reasoning display mode, reasoning is shown live but collapsed
    /// once the assistant commits a message or runs a tool. Strip any
    /// reasoning-marked lines (identified by [`REASONING_SENTINEL`]) from text
    /// about to be committed to the transcript. Other modes pass through.
    pub(in super::super) fn collapse_reasoning_for_commit(&self, content: String) -> String {
        if !matches!(
            crate::config::config().display.reasoning_display(),
            crate::config::ReasoningDisplayMode::Current
        ) {
            return content;
        }
        strip_reasoning_lines(&content)
    }

    pub(in super::super) fn replace_streaming_text(&mut self, text: String) {
        *self.streaming.streaming_text_mut() = text;
        // The live reasoning tail belonged to the *previous* buffer. Keeping its
        // byte length would make `strip_reasoning_partial_tail` slice at an
        // offset that has no relation to the new contents (and can land
        // mid-character, panicking). Reset the reasoning tail state with it.
        self.reasoning_partial_len = 0;
        self.reasoning_pending_line.clear();
        self.reasoning_streaming = false;
        self.reasoning_block_start = None;
        self.refresh_split_view_if_needed();
    }

    pub(in super::super) fn clear_streaming_render_state(&mut self) {
        self.streaming.streaming_text_mut().clear();
        self.stream_message_ended = false;
        self.deferred_stream_done_id = None;
        self.reasoning_streaming = false;
        self.reasoning_pending_line.clear();
        self.reasoning_partial_len = 0;
        // The stream (and any block offset into it) is gone.
        self.reasoning_block_start = None;
        self.refresh_split_view_if_needed();
        self.streaming_md_renderer.borrow_mut().reset();
        crate::tui::mermaid::clear_streaming_preview_diagram();
    }

    /// Reset provider-reported usage that belongs to a transcript being fully
    /// discarded. Render-only resets intentionally preserve these counters,
    /// so full session clears must call this separately.
    pub(in super::super) fn clear_live_usage_state(&mut self) {
        self.streaming.streaming_input_tokens = 0;
        self.streaming.streaming_output_tokens = 0;
        self.streaming.streaming_cache_read_tokens = None;
        self.streaming.streaming_cache_creation_tokens = None;
        self.streaming.streaming_context_stale = false;
        self.streaming.streaming_usage_call_reset_pending = false;
        self.kv_cache.current_api_usage_recorded = false;
        self.remote_context_tokens = None;
    }

    /// Discard all client-side render state for the current streaming attempt:
    /// the live streaming buffer, in-progress tool calls, thinking-line state,
    /// and any assistant transcript messages that were already committed
    /// mid-attempt at tool-call boundaries.
    ///
    /// Used when the provider reports a `RetryRollback`: a transient transport
    /// fault interrupted the response mid-stream and the request is being
    /// replayed from the top, so everything from the aborted attempt must
    /// disappear or the replay would render duplicated output.
    pub(in super::super) fn rollback_streaming_attempt(&mut self) {
        self.streaming.response_output.reset();
        self.stream_buffer.clear();
        self.clear_streaming_render_state();
        self.streaming_tool_calls.clear();
        self.batch_progress = None;
        self.thought_line_inserted = false;
        self.thinking_prefix_emitted = false;
        self.thinking_buffer.clear();
        self.thinking_start = None;
        // Assistant text committed to the transcript during this attempt (a
        // ToolStart boundary commits the pending streamed text) must also go;
        // the retry re-streams the entire response. `push_display_message`
        // counts the trailing run of assistant messages and resets on any
        // user/tool/system fence, so this removes exactly the current
        // attempt's committed segments and never touches earlier turns.
        let to_remove = self.attempt_committed_assistant_messages;
        for _ in 0..to_remove {
            if self.display_messages.last().is_some_and(|m| {
                m.role == "assistant"
                    || super::super::state_ui_messages::is_attempt_provider_native_row(m)
            }) {
                let idx = self.display_messages.len() - 1;
                self.remove_display_message(idx);
            } else {
                break;
            }
        }
        self.attempt_committed_assistant_messages = 0;
    }

    pub(in super::super) fn take_streaming_text(&mut self) -> String {
        let content = std::mem::take(self.streaming.streaming_text_mut());
        self.stream_message_ended = false;
        self.deferred_stream_done_id = None;
        self.reasoning_streaming = false;
        self.reasoning_pending_line.clear();
        self.reasoning_partial_len = 0;
        self.reasoning_block_start = None;
        self.refresh_split_view_if_needed();
        self.streaming_md_renderer.borrow_mut().reset();
        crate::tui::mermaid::clear_streaming_preview_diagram();
        content
    }

    pub(in super::super) fn commit_pending_streaming_assistant_message(&mut self) -> bool {
        let ops = self.stream_buffer.flush();
        self.apply_stream_ops(ops);
        // A commit is a hard message boundary: end any still-open reasoning
        // region so `current` mode retains/discards the trace correctly.
        if self.reasoning_streaming {
            self.close_reasoning_region(None);
        }

        if self.streaming.streaming_text.is_empty() {
            self.stream_buffer.clear();
            // Tool-only boundary (no answer text): keep the retained trace on
            // screen so the thought stays readable while the tool runs. It
            // folds when superseded by the next trace or at end of turn.
            //
            // The ephemeral mermaid preview slot mirrors the (now empty) live
            // buffer, so any surviving entry here is stale by definition. The
            // buffer can only become empty without the slot being cleared via
            // `replace_streaming_text` (remote TextReplace, debug snapshot
            // restore); `take_streaming_text` and `clear_streaming_render_state`
            // both clear it themselves.
            crate::tui::mermaid::clear_streaming_preview_diagram();
            return false;
        }

        // `take_streaming_text` also clears the streaming mermaid preview
        // slot, so the whitespace-only early return below cannot leak it.
        let content = self.take_streaming_text();
        let content = self.collapse_reasoning_for_commit(content);
        if content.trim().is_empty() {
            // Nothing left after collapsing reasoning-only content; same
            // tool-only situation as above, keep the trace readable.
            self.stream_buffer.clear();
            return false;
        }
        self.push_display_message(DisplayMessage::assistant(content));
        self.stream_buffer.clear();
        true
    }

    pub(in super::super) fn accumulate_streaming_output_tokens(
        &mut self,
        output_tokens: u64,
        call_output_tokens_seen: &mut u64,
    ) {
        let delta = if output_tokens >= *call_output_tokens_seen {
            output_tokens - *call_output_tokens_seen
        } else {
            // Usage snapshots should be monotonic within one API call. If they are not,
            // treat this as a reset and count the full value once.
            output_tokens
        };
        if self.streaming.streaming_tps_collect_output {
            self.streaming.streaming_total_output_tokens += delta;
            if delta > 0 {
                self.snapshot_streaming_tps();
            }
        }
        *call_output_tokens_seen = output_tokens;
    }
}
