use super::*;
#[path = "../ui_prepare_message_rendering.rs"]
mod message_rendering;
pub(super) use message_rendering::{render_message_into, todo_header_render_tag};

/// Immutable per-build render context shared by the full and incremental body
/// builders. Holding this in one place keeps the single per-message renderer
/// (`render_message_into`) free of divergence between the two entry points.
pub(super) struct BodyRenderCtx<'a> {
    pub(super) app: Option<&'a dyn TuiState>,
    pub(super) diff_mode: crate::config::DiffDisplayMode,
    pub(super) width: u16,
    pub(super) centered: bool,
    /// Number rendered next to the first user prompt = global prompt count +
    /// number of prompts hidden by compaction.
    pub(super) prompt_number_offset: usize,
    pub(super) total_prompts: usize,
    pub(super) pending_count: usize,
    pub(super) anchored_images: Arc<super::super::inline_image_ui::AnchoredInlineImages>,
    pub(super) inline_images_visible: bool,
    pub(super) messages: &'a [DisplayMessage],
}

/// Render saved history through the live dispatcher and wrapping pipeline.
/// No active App is borrowed: expansion state, images and in-flight swarm work
/// from the session behind the picker must never leak into this transcript.
#[cfg(test)]
pub(crate) fn prepare_history_preview(
    messages: &[DisplayMessage],
    hidden_prompts: usize,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
) -> PreparedMessages {
    prepare_history_preview_bounded(messages, hidden_prompts, width, diff_mode, None)
        .unwrap_or_else(|| preview_fallback::unavailable(width))
}

/// Choose scrollbar width without fully rendering overflowing history twice.
/// The budget excludes caller-owned header and padding rows. The flag is the
/// wide layout's strict overflow decision, not a second narrow-layout decision.
#[cfg(test)]
pub(crate) fn prepare_history_preview_for_viewport(
    messages: &[DisplayMessage],
    hidden_prompts: usize,
    wide_width: u16,
    narrow_width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    max_body_rows: usize,
) -> (PreparedMessages, bool) {
    if wide_width == narrow_width {
        let prepared = prepare_history_preview(messages, hidden_prompts, wide_width, diff_mode);
        let overflow = prepared.wrapped_lines.len() > max_body_rows;
        return (prepared, overflow);
    }
    if let Some(wide) = prepare_history_preview_bounded(
        messages,
        hidden_prompts,
        wide_width,
        diff_mode,
        Some(max_body_rows),
    ) && wide.wrapped_lines.len() <= max_body_rows
    {
        return (wide, false);
    }
    (
        prepare_history_preview(messages, hidden_prompts, narrow_width, diff_mode),
        true,
    )
}

#[cfg(test)]
pub(super) fn prepare_history_preview_bounded(
    messages: &[DisplayMessage],
    hidden_prompts: usize,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    max_body_rows: Option<usize>,
) -> Option<PreparedMessages> {
    markdown::with_center_code_blocks(false, || {
        let ctx = BodyRenderCtx {
            app: None,
            diff_mode,
            width,
            centered: false,
            prompt_number_offset: hidden_prompts,
            total_prompts: messages
                .iter()
                .filter(|msg| msg.effective_role() == "user" || msg.is_answered_ask())
                .count(),
            pending_count: 0,
            anchored_images: Arc::new(Default::default()),
            inline_images_visible: false,
            messages,
        };
        let mut acc = BodyAcc::default();
        for (idx, msg) in messages.iter().enumerate() {
            render_message_into(&ctx, &mut acc, msg, idx);
            // Wrapping preserves at least one row per input line, including
            // blanks and zero-width viewports. Equality may still fit.
            if max_body_rows.is_some_and(|budget| acc.lines.len() > budget) {
                return None;
            }
        }
        Some(wrap_lines_with_map(
            acc.lines,
            &acc.raw_plain_lines,
            &acc.line_raw_overrides,
            &acc.line_copy_offsets,
            &acc.user_line_indices,
            &acc.user_prompt_texts,
            width,
            &acc.edit_tool_line_ranges,
            &acc.copy_targets,
            &acc.segments,
        ))
    })
}

/// Mutable accumulator for one body build. Both `prepare_body` (full) and
/// `prepare_body_incremental` (tail only) push into one of these via the shared
/// `render_message_into`, guaranteeing byte-identical per-message output and
/// recording a per-message boundary table for prefix reuse.
#[derive(Default)]
pub(super) struct BodyAcc {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) raw_plain_lines: Vec<String>,
    pub(super) line_raw_overrides: Vec<Option<WrappedLineMap>>,
    pub(super) line_copy_offsets: Vec<usize>,
    pub(super) user_line_indices: Vec<usize>,
    pub(super) user_prompt_texts: Vec<String>,
    pub(super) edit_tool_line_ranges: Vec<(usize, String, usize, usize, bool)>,
    pub(super) copy_targets: Vec<RawCopyTarget>,
    /// One entry per rendered message, in order: `(msg_hash, lines_len_after,
    /// raw_len_after, user_prompt_len_after)`. `lines_len_after` counts any
    /// separator blank pushed at the start of that message, so truncating the
    /// wrapped body at a boundary cleanly drops the message together with its
    /// leading blank. `raw_len_after` is the contiguous raw count, used to
    /// truncate the raw array in lockstep.
    pub(super) segments: Vec<(u64, usize, usize, usize)>,
    /// Next 1-based prompt number to assign. Seeded from the reused base in the
    /// incremental path so numbering continues without rescanning the prefix.
    pub(super) prompt_num: usize,
    /// 0-based ordinal of the next rendered user prompt excluding synthetic
    /// attached-image label messages; mirrors the session renderer's count.
    pub(super) anchor_prompt_ordinal: usize,
    /// True when a prior (reused) body already has content, so the first message
    /// rendered here still gets its leading separator blank.
    pub(super) body_has_content: bool,
}

impl BodyAcc {
    /// Push a fully-rendered display line whose raw selection text is the line
    /// itself (no logical-line mapping). Seeds a contiguous raw entry and an
    /// explicit full-width override so the body's `raw_plain_lines` stays
    /// message-ordered. Keeping raws contiguous is what lets a tail-edit rebuild
    /// truncate both the wrapped arrays *and* the raw array at a message
    /// boundary, avoiding an unbounded raw leak across repeated tail edits (e.g.
    /// a streaming tool result that updates many times).
    fn push_auto(&mut self, line: Line<'static>) {
        let raw_text = ui::line_plain_text(&line);
        let raw_width = unicode_width::UnicodeWidthStr::width(raw_text.as_str());
        let raw_line = self.raw_plain_lines.len();
        self.raw_plain_lines.push(raw_text);
        self.lines.push(line);
        self.line_raw_overrides.push(Some(WrappedLineMap {
            raw_line,
            start_col: 0,
            end_col: raw_width,
        }));
        self.line_copy_offsets.push(0);
    }

    /// Push a blank separator line. Routed through `push_auto` so it seeds a
    /// (empty) raw and keeps the raw array contiguous.
    fn push_blank(&mut self) {
        self.push_auto(Line::from(""));
    }
}

pub(in super::super) fn prepare_body_incremental(
    app: &dyn TuiState,
    width: u16,
    mut prev: Arc<PreparedMessages>,
    prev_msg_count: usize,
) -> Arc<PreparedMessages> {
    let messages = app.display_messages();
    let new_messages = &messages[prev_msg_count..];
    if new_messages.is_empty() {
        return prev;
    }

    // Read before rendering the tail: a background diagram render completing
    // mid-build must leave the stamp already-stale.
    let mermaid_epoch_before = crate::tui::mermaid::deferred_render_epoch();
    let centered = app.centered_mode();
    markdown::set_center_code_blocks(centered);

    // Images anchored to transcript messages render inline right after the
    // message that produced them. An incremental base is only reused when the
    // image set is unchanged (cache key includes the image signature), so any
    // anchored image matching a *new* message must be injected here; its anchor
    // target did not exist when the base was built.
    let anchored_images = super::super::inline_image_ui::resolve_anchored_items_cached(app);

    // The number of user prompts already rendered equals the number of cached
    // user prompt texts. Re-counting `messages[..prev_msg_count]` here on every
    // incremental append rescans the whole prior transcript, making a session
    // that grows one message at a time O(n^2). `prev.user_prompt_texts` is
    // extended in lockstep with each rendered user message, so its length is the
    // exact prior prompt count.
    let prev_prompt_count = prev.user_prompt_texts.len();
    // 0-based ordinal of the next rendered user prompt, excluding synthetic
    // attached-image label messages, mirroring the session renderer's count.
    let anchor_prompt_ordinal = if anchored_images.by_prompt.is_empty() {
        0
    } else {
        messages[..prev_msg_count]
            .iter()
            .filter(|msg| {
                msg.effective_role() == "user"
                    && !crate::session::is_attached_image_label_text(&msg.content)
            })
            .count()
    };

    let ctx = BodyRenderCtx {
        app: Some(app),
        diff_mode: app.diff_mode(),
        width,
        centered,
        prompt_number_offset: app.compacted_hidden_user_prompts(),
        total_prompts: app.display_user_message_count(),
        pending_count: input_ui::pending_prompt_count(app),
        anchored_images,
        inline_images_visible: app.inline_images_visible(),
        messages,
    };

    let mut acc = BodyAcc {
        prompt_num: prev_prompt_count,
        anchor_prompt_ordinal,
        body_has_content: !prev.wrapped_lines.is_empty(),
        ..BodyAcc::default()
    };

    for (new_msg_offset, msg) in new_messages.iter().enumerate() {
        render_message_into(&ctx, &mut acc, msg, prev_msg_count + new_msg_offset);
    }

    let new_wrapped = wrap_lines_with_map(
        acc.lines,
        &acc.raw_plain_lines,
        &acc.line_raw_overrides,
        &acc.line_copy_offsets,
        &acc.user_line_indices,
        &acc.user_prompt_texts,
        width,
        &acc.edit_tool_line_ranges,
        &acc.copy_targets,
        &acc.segments,
    );

    let prepared = Arc::make_mut(&mut prev);
    let new_tail_pending = new_wrapped
        .wrapped_lines
        .iter()
        .any(markdown::line_is_mermaid_pending_placeholder);
    prepared.mermaid_pending_epoch = merge_mermaid_pending(
        prepared.mermaid_pending_epoch,
        new_tail_pending,
        mermaid_epoch_before,
    );
    let prev_len = prepared.wrapped_lines.len();
    let prev_raw_len = prepared.raw_plain_lines.len();
    let prev_prompt_len = prepared.user_prompt_texts.len();
    let edit_index_base = prepared.edit_tool_ranges.len();

    prepared.wrapped_lines.extend(new_wrapped.wrapped_lines);
    Arc::make_mut(&mut prepared.wrapped_plain_lines)
        .extend(new_wrapped.wrapped_plain_lines.iter().cloned());
    Arc::make_mut(&mut prepared.wrapped_copy_offsets)
        .extend(new_wrapped.wrapped_copy_offsets.iter().copied());
    Arc::make_mut(&mut prepared.raw_plain_lines)
        .extend(new_wrapped.raw_plain_lines.iter().cloned());

    {
        let wrapped_line_map = Arc::make_mut(&mut prepared.wrapped_line_map);
        for map in new_wrapped.wrapped_line_map.iter().copied() {
            wrapped_line_map.push(WrappedLineMap {
                raw_line: map.raw_line + prev_raw_len,
                ..map
            });
        }
    }

    prepared.wrapped_user_indices.extend(
        new_wrapped
            .wrapped_user_indices
            .into_iter()
            .map(|idx| idx + prev_len),
    );
    prepared.wrapped_user_prompt_starts.extend(
        new_wrapped
            .wrapped_user_prompt_starts
            .into_iter()
            .map(|idx| idx + prev_len),
    );
    prepared.wrapped_user_prompt_ends.extend(
        new_wrapped
            .wrapped_user_prompt_ends
            .into_iter()
            .map(|idx| idx + prev_len),
    );
    prepared
        .user_prompt_texts
        .extend(new_wrapped.user_prompt_texts);
    prepared
        .image_regions
        .extend(
            new_wrapped
                .image_regions
                .into_iter()
                .map(|region| ImageRegion {
                    abs_line_idx: region.abs_line_idx + prev_len,
                    end_line: region.end_line + prev_len,
                    ..region
                }),
        );
    prepared
        .edit_tool_ranges
        .extend(
            new_wrapped
                .edit_tool_ranges
                .into_iter()
                .map(|r| EditToolRange {
                    edit_index: edit_index_base + r.edit_index,
                    msg_index: r.msg_index,
                    file_path: r.file_path,
                    start_line: r.start_line + prev_len,
                    end_line: r.end_line + prev_len,
                    expandable: r.expandable,
                }),
        );
    prepared.copy_targets.extend(
        new_wrapped
            .copy_targets
            .into_iter()
            .map(|target| CopyTarget {
                start_line: target.start_line + prev_len,
                end_line: target.end_line + prev_len,
                badge_line: target.badge_line + prev_len,
                ..target
            }),
    );
    prepared
        .message_boundaries
        .extend(
            new_wrapped
                .message_boundaries
                .into_iter()
                .map(|b| MessageBoundary {
                    msg_hash: b.msg_hash,
                    wrapped_len: b.wrapped_len + prev_len,
                    raw_len: b.raw_len + prev_raw_len,
                    user_prompt_len: b.user_prompt_len + prev_prompt_len,
                }),
        );

    prev
}

/// Truncate a prepared body so it contains exactly the first `keep_msgs`
/// messages, in place on the (uniquely owned) `prepared`. Used by prefix reuse:
/// when an incoming rebuild shares a hash prefix of length `keep_msgs` with a
/// cached body, the body is truncated here and the changed tail is re-appended
/// via `prepare_body_incremental`. The cut always lands on a message boundary,
/// so every wrapped-indexed entry belongs entirely to either the kept prefix or
/// the dropped tail; filtering by `start < wrapped_len` is exact.
///
/// Requires message-contiguous raws (guaranteed by the shared body renderer),
/// which lets the raw array be truncated too rather than leaked.
pub(in super::super) fn truncate_prepared_to_boundary(
    prepared: &mut PreparedMessages,
    keep_msgs: usize,
) {
    if keep_msgs >= prepared.message_boundaries.len() {
        return;
    }
    let (wrapped_len, raw_len, user_prompt_len) = if keep_msgs == 0 {
        (0, 0, 0)
    } else {
        let b = prepared.message_boundaries[keep_msgs - 1];
        (b.wrapped_len, b.raw_len, b.user_prompt_len)
    };

    prepared.wrapped_lines.truncate(wrapped_len);
    Arc::make_mut(&mut prepared.wrapped_plain_lines).truncate(wrapped_len);
    Arc::make_mut(&mut prepared.wrapped_copy_offsets).truncate(wrapped_len);
    Arc::make_mut(&mut prepared.wrapped_line_map).truncate(wrapped_len);
    Arc::make_mut(&mut prepared.raw_plain_lines).truncate(raw_len);
    prepared.user_prompt_texts.truncate(user_prompt_len);
    prepared.message_boundaries.truncate(keep_msgs);

    prepared.wrapped_user_indices.retain(|&i| i < wrapped_len);
    // The prompt start/end arrays are parallel; drop any prompt that begins at
    // or after the cut. A prompt cannot straddle the cut because the cut is a
    // message boundary and a prompt belongs to exactly one message.
    {
        let starts = &mut prepared.wrapped_user_prompt_starts;
        let ends = &mut prepared.wrapped_user_prompt_ends;
        let keep = starts.iter().take_while(|&&s| s < wrapped_len).count();
        starts.truncate(keep);
        ends.truncate(keep);
    }
    prepared
        .image_regions
        .retain(|r| r.abs_line_idx < wrapped_len);
    prepared
        .edit_tool_ranges
        .retain(|r| r.start_line < wrapped_len);
    prepared.copy_targets.retain(|t| t.start_line < wrapped_len);

    // The pending-mermaid placeholder may have lived in the dropped tail;
    // recompute so a stale-positive stamp cannot force rebuilds forever.
    if prepared.mermaid_pending_epoch.is_some()
        && !prepared
            .wrapped_lines
            .iter()
            .any(markdown::line_is_mermaid_pending_placeholder)
    {
        prepared.mermaid_pending_epoch = None;
    }
}

/// Longest message prefix length `k` such that `base.message_boundaries[..k]`
/// hashes match the first `k` messages of `messages`. Returns `0` when nothing
/// matches (caller then does a full rebuild). Bounded by the shorter of the two
/// lengths.
pub(in super::super) fn matching_prefix_len(
    base: &PreparedMessages,
    messages: &[DisplayMessage],
) -> usize {
    let limit = base.message_boundaries.len().min(messages.len());
    let mut k = 0;
    while k < limit && base.message_boundaries[k].msg_hash == messages[k].stable_cache_hash() {
        k += 1;
    }
    // A DM row depends on its entire adjacent run, including newly appended
    // messages. Rebuild from the start of that run rather than reuse stale text.
    while k > 0 && super::super::messages::is_collapsed_agent_dm(&messages[k - 1]) {
        k -= 1;
    }
    k
}

/// Longest message suffix length `s` such that the last `s` boundaries of
/// `base` hash-match the last `s` entries of `messages`. Used to detect a
/// prepend (older compacted history loaded above an unchanged tail).
pub(in super::super) fn matching_suffix_len(
    base: &PreparedMessages,
    messages: &[DisplayMessage],
) -> usize {
    let base_n = base.message_boundaries.len();
    let msg_n = messages.len();
    let limit = base_n.min(msg_n);
    let mut s = 0;
    while s < limit
        && base.message_boundaries[base_n - 1 - s].msg_hash
            == messages[msg_n - 1 - s].stable_cache_hash()
    {
        s += 1;
    }
    // Never splice into the middle of a row shared by multiple messages.
    while s > 0 && super::super::messages::is_collapsed_agent_dm(&messages[msg_n - s]) {
        s -= 1;
    }
    s
}

/// Whether a cached body whose message suffix matches the current transcript
/// can be reused verbatim below a freshly rendered head. The reused lines bake
/// in per-prompt state, so this verifies that state is invariant under the
/// prepend:
///
/// - Displayed prompt numbers: the base rendered `local_num + base_offset`; a
///   full rebuild renders `new_local_num + current_offset`. They agree exactly
///   when `dropped_prompts + base_offset == head_prompts + current_offset`
///   (loading compacted history reveals prompts, decreasing the hidden-prompt
///   offset by the same amount, so this holds in the real flow).
/// - Prompt-anchored inline images embed window-relative prompt ordinals that
///   all shift under a prepend, so any `by_prompt` anchors force a rebuild.
///
/// Rainbow prompt-number colors and todo "what changed" deltas may be slightly
/// stale in the reused suffix (they can depend on messages outside it). Both
/// are cosmetic, match the existing staleness of the append/prefix-reuse path,
/// and self-correct on any full rebuild (e.g. a width change).
pub(in super::super) fn suffix_reuse_compatible(
    app: &dyn TuiState,
    messages: &[DisplayMessage],
    head_count: usize,
    base: &PreparedMessages,
    drop_msgs: usize,
    base_prompt_offset: usize,
) -> bool {
    if messages
        .get(head_count)
        .is_some_and(super::super::messages::is_collapsed_agent_dm)
    {
        return false;
    }
    if drop_msgs == 0 || drop_msgs >= base.message_boundaries.len() {
        return false;
    }
    let anchored = super::super::inline_image_ui::resolve_anchored_items_cached(app);
    if !anchored.by_prompt.is_empty() {
        return false;
    }
    let dropped_prompts = base.message_boundaries[drop_msgs - 1].user_prompt_len;
    let head_prompts = messages[..head_count.min(messages.len())]
        .iter()
        .filter(|msg| msg.effective_role() == "user" || msg.is_answered_ask())
        .count();
    dropped_prompts + base_prompt_offset == head_prompts + app.compacted_hidden_user_prompts()
}

/// Rebuild a body as `freshly rendered head + reused suffix of a cached base`,
/// in place on `prev`. This is the prepend analogue of
/// [`prepare_body_incremental`]: when older compacted history is loaded above
/// an unchanged tail, only the new head (marker + revealed messages) is
/// rendered and wrapped; the prepared suffix is kept and its line/raw indices
/// are shifted. Returns `Err(prev)` untouched when the stitch would not be
/// sound so the caller can fall back to the prefix/full paths.
pub(in super::super) fn prepare_body_prepended(
    app: &dyn TuiState,
    width: u16,
    mut prev: Arc<PreparedMessages>,
    drop_msgs: usize,
    head_count: usize,
) -> Result<Arc<PreparedMessages>, Arc<PreparedMessages>> {
    let messages = app.display_messages();
    if drop_msgs == 0
        || drop_msgs >= prev.message_boundaries.len()
        || head_count == 0
        || head_count > messages.len()
    {
        return Err(prev);
    }

    let (cut_wrapped, cut_raw, cut_prompt) = {
        let b = prev.message_boundaries[drop_msgs - 1];
        (b.wrapped_len, b.raw_len, b.user_prompt_len)
    };
    // The cut must land inside every parallel array, and the suffix's wrapped
    // lines must map to raws at or after the raw cut (message-contiguous raws
    // are guaranteed by the shared body renderer, but a synthetic/legacy base
    // could violate it; bail instead of stitching garbage).
    if cut_wrapped > prev.wrapped_lines.len()
        || cut_wrapped > prev.wrapped_plain_lines.len()
        || cut_wrapped > prev.wrapped_copy_offsets.len()
        || cut_wrapped > prev.wrapped_line_map.len()
        || cut_raw > prev.raw_plain_lines.len()
        || cut_prompt > prev.user_prompt_texts.len()
    {
        return Err(prev);
    }
    if prev.wrapped_line_map[cut_wrapped..]
        .iter()
        .any(|map| map.raw_line < cut_raw)
    {
        return Err(prev);
    }

    let centered = app.centered_mode();
    markdown::set_center_code_blocks(centered);

    let ctx = BodyRenderCtx {
        app: Some(app),
        diff_mode: app.diff_mode(),
        width,
        centered,
        prompt_number_offset: app.compacted_hidden_user_prompts(),
        total_prompts: app.display_user_message_count(),
        pending_count: input_ui::pending_prompt_count(app),
        anchored_images: super::super::inline_image_ui::resolve_anchored_items_cached(app),
        inline_images_visible: app.inline_images_visible(),
        messages,
    };

    // The head sits at the very top of the transcript, so it starts with the
    // same blank-separator state as a full rebuild (no content above it).
    let mut acc = BodyAcc::default();
    for (idx, msg) in messages[..head_count].iter().enumerate() {
        render_message_into(&ctx, &mut acc, msg, idx);
    }

    let head = wrap_lines_with_map(
        acc.lines,
        &acc.raw_plain_lines,
        &acc.line_raw_overrides,
        &acc.line_copy_offsets,
        &acc.user_line_indices,
        &acc.user_prompt_texts,
        width,
        &acc.edit_tool_line_ranges,
        &acc.copy_targets,
        &acc.segments,
    );

    // Blank-separator continuity at the seam: the suffix's first message baked
    // a leading blank iff the dropped prefix rendered content; a full rebuild
    // gives it one iff the head renders content. Both sides are non-empty in
    // the real compacted flow; bail on the degenerate mismatch.
    if (cut_wrapped > 0) == head.wrapped_lines.is_empty() {
        return Err(prev);
    }

    let head_pending_epoch = head.mermaid_pending_epoch;
    let head_wrapped_len = head.wrapped_lines.len();
    let head_raw_len = head.raw_plain_lines.len();
    let head_prompt_len = head.user_prompt_texts.len();
    let msg_index_delta = head_count as isize - drop_msgs as isize;
    let shift_wrapped = |idx: usize| idx - cut_wrapped + head_wrapped_len;

    let prepared = Arc::make_mut(&mut prev);

    prepared
        .wrapped_lines
        .splice(0..cut_wrapped, head.wrapped_lines);
    Arc::make_mut(&mut prepared.wrapped_plain_lines).splice(
        0..cut_wrapped,
        Arc::try_unwrap(head.wrapped_plain_lines).unwrap_or_else(|arc| (*arc).clone()),
    );
    Arc::make_mut(&mut prepared.wrapped_copy_offsets).splice(
        0..cut_wrapped,
        Arc::try_unwrap(head.wrapped_copy_offsets).unwrap_or_else(|arc| (*arc).clone()),
    );
    Arc::make_mut(&mut prepared.raw_plain_lines).splice(
        0..cut_raw,
        Arc::try_unwrap(head.raw_plain_lines).unwrap_or_else(|arc| (*arc).clone()),
    );

    {
        let map = Arc::make_mut(&mut prepared.wrapped_line_map);
        for entry in map[cut_wrapped..].iter_mut() {
            entry.raw_line = entry.raw_line - cut_raw + head_raw_len;
        }
        map.splice(
            0..cut_wrapped,
            Arc::try_unwrap(head.wrapped_line_map).unwrap_or_else(|arc| (*arc).clone()),
        );
    }

    {
        let keep_from = prepared
            .wrapped_user_indices
            .partition_point(|&idx| idx < cut_wrapped);
        let mut indices = head.wrapped_user_indices;
        indices.extend(
            prepared.wrapped_user_indices[keep_from..]
                .iter()
                .map(|&idx| shift_wrapped(idx)),
        );
        prepared.wrapped_user_indices = indices;
    }
    {
        // Starts/ends are parallel; a prompt cannot straddle the cut because the
        // cut is a message boundary.
        let keep_from = prepared
            .wrapped_user_prompt_starts
            .partition_point(|&idx| idx < cut_wrapped);
        let mut starts = head.wrapped_user_prompt_starts;
        starts.extend(
            prepared.wrapped_user_prompt_starts[keep_from..]
                .iter()
                .map(|&idx| shift_wrapped(idx)),
        );
        let mut ends = head.wrapped_user_prompt_ends;
        ends.extend(
            prepared.wrapped_user_prompt_ends[keep_from..]
                .iter()
                .map(|&idx| shift_wrapped(idx)),
        );
        prepared.wrapped_user_prompt_starts = starts;
        prepared.wrapped_user_prompt_ends = ends;
    }

    prepared
        .user_prompt_texts
        .splice(0..cut_prompt, head.user_prompt_texts);

    {
        let mut regions = head.image_regions;
        regions.extend(
            prepared
                .image_regions
                .iter()
                .filter(|region| region.abs_line_idx >= cut_wrapped)
                .map(|region| ImageRegion {
                    abs_line_idx: shift_wrapped(region.abs_line_idx),
                    end_line: shift_wrapped(region.end_line),
                    ..*region
                }),
        );
        prepared.image_regions = regions;
    }

    {
        let mut ranges = head.edit_tool_ranges;
        let head_edit_count = ranges.len();
        ranges.extend(
            prepared
                .edit_tool_ranges
                .iter()
                .filter(|range| range.start_line >= cut_wrapped)
                .enumerate()
                .map(|(seq, range)| EditToolRange {
                    edit_index: head_edit_count + seq,
                    msg_index: (range.msg_index as isize + msg_index_delta).max(0) as usize,
                    file_path: range.file_path.clone(),
                    start_line: shift_wrapped(range.start_line),
                    end_line: shift_wrapped(range.end_line),
                    expandable: range.expandable,
                }),
        );
        prepared.edit_tool_ranges = ranges;
    }

    {
        let mut targets = head.copy_targets;
        targets.extend(
            prepared
                .copy_targets
                .iter()
                .filter(|target| target.start_line >= cut_wrapped)
                .map(|target| CopyTarget {
                    kind: target.kind.clone(),
                    content: target.content.clone(),
                    start_line: shift_wrapped(target.start_line),
                    end_line: shift_wrapped(target.end_line),
                    badge_line: shift_wrapped(target.badge_line),
                }),
        );
        prepared.copy_targets = targets;
    }

    {
        let mut boundaries = head.message_boundaries;
        boundaries.extend(prepared.message_boundaries[drop_msgs..].iter().map(|b| {
            MessageBoundary {
                msg_hash: b.msg_hash,
                wrapped_len: shift_wrapped(b.wrapped_len),
                raw_len: b.raw_len - cut_raw + head_raw_len,
                user_prompt_len: b.user_prompt_len - cut_prompt + head_prompt_len,
            }
        }));
        prepared.message_boundaries = boundaries;
    }

    prepared.mermaid_pending_epoch = match (prepared.mermaid_pending_epoch, head_pending_epoch) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };

    Ok(prev)
}

pub(super) fn prepare_streaming_cached(
    app: &dyn TuiState,
    width: u16,
    prefix_blank: bool,
) -> PreparedMessages {
    let streaming = app.streaming_text();
    if streaming.is_empty() {
        return empty_prepared_messages();
    }

    // Read before rendering: see `stamp_mermaid_pending`.
    let mermaid_epoch_before = crate::tui::mermaid::deferred_render_epoch();
    let centered = app.centered_mode();
    markdown::set_center_code_blocks(centered);
    let display_width = width.saturating_sub(4) as usize;

    let content_width = if centered {
        display_width.clamp(1, 96)
    } else {
        display_width
    };
    let mut md_lines = app.render_streaming_markdown(content_width);
    if centered {
        markdown::recenter_structured_blocks_for_display(&mut md_lines, display_width);
    }
    let align = if centered {
        ratatui::layout::Alignment::Center
    } else {
        ratatui::layout::Alignment::Left
    };

    // The renderer names the leading lines it did not re-render. They are
    // only reusable here when a blank line follows them: display recentering
    // above works on runs of non-blank lines, so a run that crossed the
    // boundary could have shifted them.
    let (stable_generation, stable_len) = app.streaming_markdown_stable_prefix();
    let head_md_lines = if stable_len > 0
        && md_lines
            .get(stable_len)
            .is_some_and(|line| line.spans.iter().all(|span| span.content.is_empty()))
    {
        stable_len
    } else {
        0
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    if prefix_blank {
        lines.push(Line::from(""));
    }
    for line in md_lines {
        lines.push(align_if_unset(line, align));
    }

    // Streaming replies need the same copy targets as finalized messages.
    // Extract before wrapping, then map the ranges after joining the cached
    // stable head and live tail so targets spanning that boundary stay intact.
    let copy_targets = markdown::extract_copy_targets_from_rendered_lines(&lines);
    let mut prepared = if head_md_lines == 0 {
        wrap_lines(lines, &[], &[], &[], width)
    } else {
        let head_len = head_md_lines + usize::from(prefix_blank);
        let key = StreamingHeadKey {
            generation: stable_generation,
            head_len,
            width,
            centered,
            prefix_blank,
        };
        let tail_lines = lines.split_off(head_len);
        let head = STREAMING_HEAD_WRAP.with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.as_ref() {
                Some((cached_key, head)) if *cached_key == key => Arc::clone(head),
                _ => {
                    let head = Arc::new(wrap_lines(lines, &[], &[], &[], width));
                    *cache = Some((key, Arc::clone(&head)));
                    head
                }
            }
        });
        append_wrapped_tail(&head, wrap_lines(tail_lines, &[], &[], &[], width))
    };
    prepared.copy_targets = copy_targets
        .into_iter()
        .map(|target| {
            let wrapped_row = |raw_line| {
                prepared
                    .wrapped_line_map
                    .partition_point(|map| map.raw_line < raw_line)
            };
            CopyTarget {
                kind: target.kind,
                content: target.content,
                start_line: wrapped_row(target.start_raw_line),
                end_line: wrapped_row(target.end_raw_line),
                badge_line: wrapped_row(target.badge_raw_line),
            }
        })
        .collect();
    stamp_mermaid_pending(&mut prepared, mermaid_epoch_before);
    prepared
}

/// Identifies the wrapped form of the stable head of a streaming reply.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct StreamingHeadKey {
    pub(super) generation: u64,
    pub(super) head_len: usize,
    pub(super) width: u16,
    pub(super) centered: bool,
    pub(super) prefix_blank: bool,
}

thread_local! {
    // One entry: only the reply being streamed on this thread is ever asked for.
    pub(super) static STREAMING_HEAD_WRAP: std::cell::RefCell<Option<(StreamingHeadKey, Arc<PreparedMessages>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Join two separately wrapped runs of lines into what wrapping them together
/// would have produced. Only valid for plain lines (no user prompts, edit
/// ranges or copy targets), which is all `wrap_lines` emits for streaming.
pub(super) fn append_wrapped_tail(
    head: &PreparedMessages,
    tail: PreparedMessages,
) -> PreparedMessages {
    let head_rows = head.wrapped_lines.len();
    let head_raw = head.raw_plain_lines.len();

    let mut wrapped_lines = Vec::with_capacity(head_rows + tail.wrapped_lines.len());
    wrapped_lines.extend(head.wrapped_lines.iter().cloned());
    wrapped_lines.extend(tail.wrapped_lines);

    let mut wrapped_plain_lines = Vec::with_capacity(wrapped_lines.len());
    wrapped_plain_lines.extend(head.wrapped_plain_lines.iter().cloned());
    wrapped_plain_lines.extend(tail.wrapped_plain_lines.iter().cloned());

    let mut wrapped_copy_offsets = Vec::with_capacity(wrapped_lines.len());
    wrapped_copy_offsets.extend(head.wrapped_copy_offsets.iter().copied());
    wrapped_copy_offsets.extend(tail.wrapped_copy_offsets.iter().copied());

    let mut raw_plain_lines = Vec::with_capacity(head_raw + tail.raw_plain_lines.len());
    raw_plain_lines.extend(head.raw_plain_lines.iter().cloned());
    raw_plain_lines.extend(tail.raw_plain_lines.iter().cloned());

    let mut wrapped_line_map = Vec::with_capacity(wrapped_lines.len());
    wrapped_line_map.extend(head.wrapped_line_map.iter().copied());
    wrapped_line_map.extend(tail.wrapped_line_map.iter().map(|map| WrappedLineMap {
        raw_line: map.raw_line + head_raw,
        ..*map
    }));

    let mut image_regions = head.image_regions.clone();
    image_regions.extend(tail.image_regions.iter().map(|region| {
        let mut region = *region;
        region.abs_line_idx += head_rows;
        region.end_line += head_rows;
        region
    }));

    PreparedMessages {
        wrapped_lines,
        wrapped_plain_lines: Arc::new(wrapped_plain_lines),
        wrapped_copy_offsets: Arc::new(wrapped_copy_offsets),
        raw_plain_lines: Arc::new(raw_plain_lines),
        wrapped_line_map: Arc::new(wrapped_line_map),
        image_regions,
        ..tail
    }
}
