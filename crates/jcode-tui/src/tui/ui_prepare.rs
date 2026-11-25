#[path = "ui_prepare/assistant_aux.rs"]
mod assistant_aux;
use assistant_aux::*;

use super::*;
use crate::tui::ui::{self, WrappedLineMap};

/// Auxiliary render data for an assistant message that is otherwise recomputed
/// by re-parsing markdown on every body rebuild. Building the body misses its
/// cache whenever `display_messages_version` changes (e.g. an in-place edit to
/// the last assistant/tool message or a streaming finalize), and each miss used
/// to re-render the same markdown two or three additional times just to derive
/// the raw-line/logical-line map used for text selection. Memoizing it keeps the
/// common edit-in-place and finalize paths cheap on long transcripts.
#[derive(Clone)]
struct AssistantAuxData {
    /// Number of leading `cached` lines that correspond to rendered markdown
    /// content (excludes appended tool-call summary lines).
    content_line_count: usize,
    /// Plain logical lines used to build the wrapped->raw line map.
    logical_plain_lines: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct AssistantAuxKey {
    message_hash: u64,
    content_len: usize,
    content_width: u16,
    centered: bool,
    diff_mode: crate::config::DiffDisplayMode,
    cached_len: usize,
}

const ASSISTANT_AUX_CACHE_LIMIT: usize = 2048;

#[derive(Default)]
struct AssistantAuxCacheState {
    entries: std::collections::HashMap<AssistantAuxKey, std::sync::Arc<AssistantAuxData>>,
    order: std::collections::VecDeque<AssistantAuxKey>,
}

fn assistant_aux_cache() -> &'static std::sync::Mutex<AssistantAuxCacheState> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<AssistantAuxCacheState>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(AssistantAuxCacheState::default()))
}

pub(super) fn diagnostic_aux_profile<'a>(
    values: impl Iterator<Item = (usize, usize, &'a Vec<String>)>,
    order_entries_count: usize,
) -> serde_json::Value {
    let mut seen = std::collections::HashSet::new();
    let mut entries_count = 0;
    let mut bytes = 0;
    for (pointer, inline_bytes, lines) in values {
        entries_count += 1;
        if seen.insert(pointer) {
            bytes += 2 * std::mem::size_of::<usize>()
                + inline_bytes
                + (lines.iter().map(|value| value.capacity()).sum::<usize>()
                    + lines.capacity() * std::mem::size_of::<String>());
        }
    }
    serde_json::json!({"entries_count": entries_count, "unique_values_count": seen.len(),
        "order_entries_count": order_entries_count, "unique_prepared_bytes": bytes})
}

/// Diagnostic-only snapshot of unique auxiliary payload allocations.
pub(super) fn debug_assistant_aux_memory_profile() -> serde_json::Value {
    let cache = assistant_aux_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    diagnostic_aux_profile(
        cache.entries.values().map(|value| {
            (
                Arc::as_ptr(value) as usize,
                std::mem::size_of::<AssistantAuxData>(),
                &value.logical_plain_lines,
            )
        }),
        cache.order.len(),
    )
}

fn content_prefers_display_as_logical_lines(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with('|') && trimmed.matches('|').count() >= 2
    })
}

fn semantic_swarm_line_text(plain: &str) -> (String, usize) {
    let trimmed = plain.trim_start_matches(' ');
    if let Some(rest) = trimmed.strip_prefix("│ ") {
        let prefix_width = unicode_width::UnicodeWidthStr::width(plain)
            .saturating_sub(unicode_width::UnicodeWidthStr::width(rest));
        (rest.to_string(), prefix_width)
    } else {
        (plain.to_string(), 0)
    }
}

fn map_display_lines_to_logical_lines(
    display_lines: &[Line<'static>],
    logical_plain_lines: &[String],
    raw_base: usize,
) -> Option<Vec<WrappedLineMap>> {
    let mut maps = Vec::with_capacity(display_lines.len());
    let mut logical_idx = 0usize;
    let mut logical_col = 0usize;

    for line in display_lines {
        while logical_idx < logical_plain_lines.len() {
            let logical_width =
                unicode_width::UnicodeWidthStr::width(logical_plain_lines[logical_idx].as_str());
            if logical_col < logical_width || logical_width == 0 {
                break;
            }
            logical_idx += 1;
            logical_col = 0;
        }

        let logical_text = logical_plain_lines.get(logical_idx)?;
        let logical_width = unicode_width::UnicodeWidthStr::width(logical_text.as_str());
        let display_width = line.width();
        let remaining = logical_width.saturating_sub(logical_col);
        if display_width > remaining {
            return None;
        }

        maps.push(WrappedLineMap {
            raw_line: raw_base + logical_idx,
            start_col: logical_col,
            end_col: logical_col + display_width,
        });
        logical_col += display_width;
    }

    Some(maps)
}

fn user_prompt_number_style(color: Color) -> Style {
    Style::default().fg(color).bg(user_bg())
}

fn user_prompt_accent_style() -> Style {
    Style::default().fg(user_color()).bg(user_bg())
}

fn user_prompt_text_style() -> Style {
    Style::default().fg(user_text()).bg(user_bg())
}

fn default_message_alignment(role: &str, centered: bool) -> ratatui::layout::Alignment {
    if centered
        && !matches!(
            role,
            "tool" | "system" | "swarm" | "background_task" | "overnight" | "todos" | "compaction"
        )
    {
        ratatui::layout::Alignment::Center
    } else {
        ratatui::layout::Alignment::Left
    }
}

fn is_error_copy_content(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("Error:") || trimmed.starts_with("error:") || trimmed.starts_with("Failed:")
}

#[path = "ui_prepare/copy_targets.rs"]
mod copy_targets;
pub(super) use copy_targets::{
    assistant_message_copy_targets, compute_image_regions, error_copy_target, offset_copy_target,
    tool_message_copy_target,
};

/// Build the inline "what changed" delta lines for a `todo` tool message, if it
/// is a successful write. `messages` is the full transcript and `abs_idx` is the
/// absolute index of the todo message, so the previous todo list can be found.
fn todo_change_lines(
    messages: &[DisplayMessage],
    abs_idx: usize,
    msg: &DisplayMessage,
    width: u16,
) -> Vec<Line<'static>> {
    if tools_ui::tool_output_looks_failed(&msg.content) {
        return Vec::new();
    }
    let Some(tc) = msg.tool_data.as_ref() else {
        return Vec::new();
    };
    if tools_ui::canonical_tool_name(&tc.name) != "todo" {
        return Vec::new();
    }
    let Some(next) = super::todo_changes::todos_from_tool_input(tc) else {
        return Vec::new();
    };
    let prev = super::todo_changes::previous_todos(messages, abs_idx);
    super::todo_changes::render_todo_change_lines(prev.as_deref(), &next, width)
}

#[expect(
    clippy::too_many_arguments,
    reason = "User prompt rendering updates the prepared-line side tables together"
)]
fn push_user_prompt_lines(
    lines: &mut Vec<Line<'static>>,
    raw_plain_lines: &mut Vec<String>,
    line_raw_overrides: &mut Vec<Option<WrappedLineMap>>,
    line_copy_offsets: &mut Vec<usize>,
    user_line_indices: &mut Vec<usize>,
    prompt_num: usize,
    num_color: Color,
    content: &str,
    align: ratatui::layout::Alignment,
) {
    let prefix_width = unicode_width::UnicodeWidthStr::width(prompt_num.to_string().as_str())
        + unicode_width::UnicodeWidthStr::width("› ");
    // Callers supply already projected user-facing content.
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    for (line_idx, content_line) in normalized.split('\n').enumerate() {
        let raw_line = raw_plain_lines.len();
        raw_plain_lines.push(content_line.to_string());
        let prompt_width = unicode_width::UnicodeWidthStr::width(content_line);
        let rendered_line_idx = lines.len();
        let is_first_line = line_idx == 0;
        if is_first_line {
            user_line_indices.push(rendered_line_idx);
        }

        let prefix_spans = if is_first_line {
            vec![
                Span::styled(
                    format!("{}", prompt_num),
                    user_prompt_number_style(num_color),
                ),
                Span::styled("› ", user_prompt_accent_style()),
            ]
        } else {
            vec![Span::styled(
                " ".repeat(prefix_width),
                user_prompt_accent_style(),
            )]
        };
        let mut spans = prefix_spans;
        spans.push(Span::styled(
            content_line.to_string(),
            user_prompt_text_style(),
        ));
        lines.push(Line::from(spans).alignment(align));
        line_raw_overrides.push(Some(WrappedLineMap {
            raw_line,
            start_col: 0,
            end_col: prompt_width,
        }));
        line_copy_offsets.push(prefix_width);
    }
}

fn empty_prepared_messages() -> PreparedMessages {
    PreparedMessages {
        wrapped_lines: Vec::new(),
        wrapped_plain_lines: Arc::new(Vec::new()),
        wrapped_copy_offsets: Arc::new(Vec::new()),
        raw_plain_lines: Arc::new(Vec::new()),
        wrapped_line_map: Arc::new(Vec::new()),
        wrapped_user_indices: Vec::new(),
        wrapped_user_prompt_starts: Vec::new(),
        wrapped_user_prompt_ends: Vec::new(),
        user_prompt_texts: Vec::new(),
        image_regions: Vec::new(),
        edit_tool_ranges: Vec::new(),
        copy_targets: Vec::new(),
        message_boundaries: Vec::new(),
        mermaid_pending_epoch: None,
    }
}

/// Stamp `prepared` with the deferred-mermaid staleness marker: `Some(epoch)`
/// when any wrapped line is the "rendering mermaid diagram..." placeholder
/// (a background render is still in flight), `None` otherwise. `epoch_before`
/// must be the deferred-render epoch read *before* the markdown was rendered,
/// so a render completing mid-build immediately reads as stale.
fn stamp_mermaid_pending(prepared: &mut PreparedMessages, epoch_before: u64) {
    prepared.mermaid_pending_epoch = prepared
        .wrapped_lines
        .iter()
        .any(markdown::line_is_mermaid_pending_placeholder)
        .then_some(epoch_before);
}

/// Merge the pending stamp of a reused base with a freshly rendered part.
/// Keeps the earliest epoch so staleness is never masked.
fn merge_mermaid_pending(
    base: Option<u64>,
    fresh_lines_pending: bool,
    epoch_before: u64,
) -> Option<u64> {
    let fresh = fresh_lines_pending.then_some(epoch_before);
    match (base, fresh) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Transcript message index owning the first deferred-mermaid pending
/// placeholder line, for cutting a stale base at a message boundary. `None`
/// when no pending line exists.
fn first_mermaid_pending_message(prepared: &PreparedMessages) -> Option<usize> {
    let line_idx = prepared
        .wrapped_lines
        .iter()
        .position(markdown::line_is_mermaid_pending_placeholder)?;
    Some(
        prepared
            .message_boundaries
            .partition_point(|boundary| boundary.wrapped_len <= line_idx),
    )
}

fn active_codemode(app: &dyn TuiState) -> Option<crate::message::ToolCall> {
    // Connection-phase and nested-tool events can replace RunningTool while
    // the parent call is still live. ToolDone removes it from this list, so
    // pending tool calls, not the transient status label, own its lifetime.
    if !app.is_processing() {
        return None;
    }
    app.streaming_tool_calls()
        .into_iter()
        .rev()
        .find(|tool| tools_ui::canonical_tool_name(&tool.name) == "codemode")
}

fn active_batch_progress(app: &dyn TuiState) -> Option<crate::bus::BatchProgress> {
    match app.status() {
        ProcessingStatus::RunningTool(name) if name == "batch" => app.batch_progress(),
        _ => None,
    }
}

pub(super) fn active_batch_progress_hash(app: &dyn TuiState) -> u64 {
    if let Some(tool) = active_codemode(app) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tool.id.hash(&mut hasher);
        tool.input.to_string().hash(&mut hasher);
        tool.intent.hash(&mut hasher);
        super::activity_indicator_frame_index(app.animation_elapsed(), 12.5).hash(&mut hasher);
        return hasher.finish();
    }
    let Some(progress) = active_batch_progress(app) else {
        return 0;
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if progress.completed < progress.total {
        super::activity_indicator_frame_index(app.animation_elapsed(), 12.5).hash(&mut hasher);
    }
    progress.total.hash(&mut hasher);
    progress.completed.hash(&mut hasher);
    progress.last_completed.hash(&mut hasher);
    for subcall in &progress.subcalls {
        subcall.index.hash(&mut hasher);
        subcall.tool_call.id.hash(&mut hasher);
        subcall.tool_call.name.hash(&mut hasher);
        match subcall.state {
            crate::bus::BatchSubcallState::Running => 0u8,
            crate::bus::BatchSubcallState::Succeeded => 1u8,
            crate::bus::BatchSubcallState::Failed => 2u8,
        }
        .hash(&mut hasher);
        if let Ok(input) = serde_json::to_string(&subcall.tool_call.input) {
            input.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn prepare_active_batch_progress(
    app: &dyn TuiState,
    width: u16,
    prefix_blank: bool,
) -> PreparedMessages {
    if let Some(tool) = active_codemode(app) {
        let block_width = if app.centered_mode() {
            super::centered_content_block_width(width, 96)
        } else {
            width as usize
        };
        let row_width = block_width.saturating_sub(1);
        let spinner = super::activity_indicator(app.animation_elapsed(), 12.5);
        let mut lines = Vec::new();
        if prefix_blank {
            lines.push(Line::from(""));
        }
        let mut header = vec![
            Span::styled(
                format!("  {spinner} "),
                Style::default().fg(rgb(255, 193, 94)),
            ),
            Span::styled("codemode", Style::default().fg(tool_color())),
        ];
        if let Some(intent) = tool
            .intent
            .as_deref()
            .filter(|intent| !intent.trim().is_empty())
        {
            header.push(Span::styled(" · ", Style::default().fg(dim_color())));
            header.push(Span::styled(
                intent.to_owned(),
                Style::default().fg(tool_color()),
            ));
        }
        lines.push(super::truncate_line_with_ellipsis_to_width(
            &Line::from(header),
            row_width,
        ));
        lines.extend(tools_ui::codemode_preview(&tool, row_width));
        if app.centered_mode() {
            super::left_pad_lines_to_block_width(&mut lines, width, block_width);
        }
        return wrap_lines_with_map(lines, &[], &[], &[], &[], &[], width, &[], &[], &[]);
    }
    let Some(progress) = active_batch_progress(app) else {
        return empty_prepared_messages();
    };

    let centered = app.centered_mode();
    let accent = rgb(255, 193, 94);
    let spinner = super::activity_indicator(app.animation_elapsed(), 12.5);
    let block_width = if centered {
        super::centered_content_block_width(width, 96)
    } else {
        width as usize
    };
    let row_width = block_width.saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    if prefix_blank {
        lines.push(Line::from(""));
    }

    let mut header = vec![
        Span::styled(format!("  {} ", spinner), Style::default().fg(accent)),
        Span::styled("batch", Style::default().fg(tool_color())),
        Span::styled(
            format!(" · {}/{} done", progress.completed, progress.total),
            Style::default().fg(dim_color()),
        ),
    ];
    if let Some(last) = progress
        .last_completed
        .as_ref()
        .filter(|_| progress.completed < progress.total)
    {
        header.push(Span::styled(
            format!(" · last done: {}", last),
            Style::default().fg(dim_color()),
        ));
    }
    lines.push(super::truncate_line_with_ellipsis_to_width(
        &Line::from(header),
        width.saturating_sub(1) as usize,
    ));

    let mut hidden_completed = 0usize;
    for subcall in &progress.subcalls {
        let (icon, icon_color) = match subcall.state {
            crate::bus::BatchSubcallState::Running => (spinner, accent),
            crate::bus::BatchSubcallState::Succeeded => {
                hidden_completed += 1;
                continue;
            }
            crate::bus::BatchSubcallState::Failed => ("✗", rgb(220, 100, 100)),
        };

        lines.push(tools_ui::render_batch_subcall_line(
            &subcall.tool_call,
            icon,
            icon_color,
            50,
            Some(row_width),
            None,
        ));
    }

    if hidden_completed > 0 && progress.completed < progress.total {
        lines.push(Line::from(Span::styled(
            format!("    … {} completed", hidden_completed),
            Style::default().fg(dim_color()),
        )));
    }

    if centered {
        super::left_pad_lines_to_block_width(&mut lines, width, block_width);
    }

    wrap_lines_with_map(lines, &[], &[], &[], &[], &[], width, &[], &[], &[])
}

pub(super) fn prepare_messages(
    app: &dyn TuiState,
    width: u16,
    height: u16,
) -> Arc<PreparedChatFrame> {
    // A cached prepared frame intentionally owns only image ids. Recover any
    // staged source evicted by the byte budget or a visibility toggle before an
    // exact frame-cache hit can bypass the normal anchored-image resolver.
    let restage_start = Instant::now();
    super::inline_image_ui::restage_requested_payloads(app);
    super::record_prep_phase(super::PrepPhase::Restage, restage_start.elapsed());
    if prep_cache_bypassed() {
        return Arc::new(prepare_messages_inner(app, width, height));
    }

    let key = FullPrepCacheKey {
        compact_thinking: crate::config::config().display.compact_thinking,
        width,
        height,
        diff_mode: app.diff_mode(),
        // Pinning changes which tool messages participate in the transcript.
        // The live todo payload also owns the trailing blocker explanations.
        messages_version: app
            .display_messages_version()
            .wrapping_mul(2)
            .wrapping_add(todo_header_render_tag())
            .wrapping_mul(31)
            .wrapping_add(super::hash_text_for_cache(
                app.pinned_todos_payload().unwrap_or(""),
            )),
        diagram_mode: app.diagram_mode(),
        centered: app.centered_mode(),
        mermaid_aspect_bucket: crate::tui::mermaid::current_preferred_aspect_ratio_bucket(),
        is_processing: app.is_processing(),
        streaming_text_len: app.streaming_text().len(),
        streaming_text_hash: app.streaming_text_hash(),
        batch_progress_hash: active_batch_progress_hash(app),
        // An unpinned transcript must not reuse a previously prepared frame
        // containing anchored images. With no images, both modes are visually
        // identical and `(0, 0)` reuse is safe.
        inline_images_signature: if app.pin_images() {
            app.side_pane_images_signature()
        } else {
            (0, 0)
        },
        inline_images_visible: app.inline_images_visible(),
        expanded_images_version: app.expanded_images_version(),
        latex_rendering: crate::config::config().display.render_prefs().latex,
        show_link_urls: crate::config::config().display.show_link_urls,
    };

    super::note_full_prep_request();
    let cache_lookup_start = Instant::now();

    {
        let cache = match full_prep_cache().lock() {
            Ok(c) => c,
            Err(poisoned) => {
                let mut c = poisoned.into_inner();
                c.entries.clear();
                c
            }
        };
        let mut cache = cache;
        if let Some((prepared, kind, prepared_bytes)) = cache.get_exact_with_kind(&key) {
            // A completed deferred mermaid render does not bump
            // `messages_version`/`streaming_text_hash`, so an exact hit can
            // still bake in a stale "rendering..." placeholder. Fall through
            // to a rebuild, which re-renders the pending tail.
            let stale = prepared
                .mermaid_pending_epoch()
                .is_some_and(|stamp| crate::tui::mermaid::deferred_render_epoch() != stamp);
            if !stale {
                super::note_full_prep_cache_lookup(cache_lookup_start.elapsed());
                super::note_full_prep_cache_hit(kind, prepared.as_ref(), prepared_bytes);
                return prepared;
            }
        }
    }

    super::note_full_prep_cache_lookup(cache_lookup_start.elapsed());
    super::note_full_prep_cache_miss();

    let build_start = Instant::now();
    let prepared = Arc::new(prepare_messages_inner(app, width, height));
    let build_elapsed = build_start.elapsed();
    let prepared_bytes = super::estimate_prepared_chat_frame_bytes(&prepared);
    super::note_full_prep_built(prepared.as_ref(), prepared_bytes, build_elapsed);

    {
        if let Ok(mut cache) = full_prep_cache().lock() {
            cache.insert_with_bytes(key, prepared.clone(), prepared_bytes);
        }
    }

    prepared
}

/// Top padding used to vertically center the header on the initial empty
/// screen. Derived only from the persistent header height (not suggestions)
/// so the same value can be re-applied above the header once messages exist,
/// keeping the header from jumping when the first prompt is sent.
pub(crate) fn initial_header_pad_top(height: u16, header_lines: usize) -> usize {
    // Input chrome plus the always-pinned session status line below it.
    let input_reserve = 5;
    let available = (height as usize).saturating_sub(input_reserve);
    available.saturating_sub(header_lines) / 2
}

/// Build the lines that fill the top padding above the header. Unseen release
/// notes (the "Updates" box) render inside this padding, bottom-aligned so any
/// leftover space stays at the top. With no unseen updates this is just blank
/// padding. The returned vec is always exactly `pad_top` lines tall so the
/// header position never shifts.
fn build_top_pad_lines(width: u16, pad_top: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(pad_top);
    if pad_top == 0 {
        return lines;
    }
    // Leave one blank line between the box and the header.
    let box_budget = pad_top.saturating_sub(1);
    let boxed = header::build_updates_box_lines(width, box_budget);
    let blanks = pad_top.saturating_sub(boxed.len() + usize::from(!boxed.is_empty()));
    for _ in 0..blanks {
        lines.push(Line::from(""));
    }
    if !boxed.is_empty() {
        lines.extend(boxed);
        lines.push(Line::from(""));
    }
    debug_assert_eq!(lines.len(), pad_top);
    lines
}

fn prepare_messages_inner(app: &dyn TuiState, width: u16, height: u16) -> PreparedChatFrame {
    let header_start = Instant::now();
    let header_prepared = prepare_header_cached(app, width);
    let header_ms = header_start.elapsed().as_secs_f64() * 1000.0;

    let body_start = Instant::now();
    let body_prepared = prepare_body_cached(app, width);
    let body_ms = body_start.elapsed().as_secs_f64() * 1000.0;

    // Anchored images render inside the body at their producing message; only
    // images without a resolvable anchor target fall back to this trailing
    // inline section so nothing silently disappears.
    let inline_images_prepared = if app.pin_images() {
        let anchored = super::inline_image_ui::resolve_anchored_items_cached(app);
        let items = anchored.unplaced_items(app.display_messages());
        if items.is_empty() {
            Arc::new(empty_prepared_messages())
        } else {
            let prefix_blank = !body_prepared.wrapped_lines.is_empty();
            Arc::new(super::inline_image_ui::build_section(
                &items,
                width,
                height,
                prefix_blank,
                app.inline_images_visible(),
                &super::inline_image_ui::AppExpandLevels(app),
            ))
        }
    } else {
        Arc::new(empty_prepared_messages())
    };

    let batch_start = Instant::now();
    let has_batch_progress = active_batch_progress(app).is_some() || active_codemode(app).is_some();
    let batch_prefix_blank = has_batch_progress && !body_prepared.wrapped_lines.is_empty();
    let batch_progress_prepared = if has_batch_progress {
        Arc::new(prepare_active_batch_progress(
            app,
            width,
            batch_prefix_blank,
        ))
    } else {
        Arc::new(empty_prepared_messages())
    };
    let batch_ms = batch_start.elapsed().as_secs_f64() * 1000.0;

    let streaming_start = Instant::now();
    // Reasoning traces in `current` mode are anchored display messages inside
    // the body now; no separate retained/collapsing trace section exists.
    let reasoning_prepared = Arc::new(empty_prepared_messages());
    let has_streaming = app.is_processing() && !app.streaming_text().is_empty();
    let stream_prefix_blank = !(!has_streaming
        || (crate::config::config().display.compact_thinking
            && super::messages::starts_with_reasoning(app.streaming_text()))
        || (body_prepared.wrapped_lines.is_empty()
            && batch_progress_prepared.wrapped_lines.is_empty()
            && reasoning_prepared.wrapped_lines.is_empty()));
    let streaming_prepared = if has_streaming {
        Arc::new(prepare_streaming_cached(app, width, stream_prefix_blank))
    } else {
        // The reply was committed or cleared: its wrapped head is dead weight.
        STREAMING_HEAD_WRAP.with(|cache| {
            if cache.borrow().is_some() {
                *cache.borrow_mut() = None;
            }
        });
        Arc::new(empty_prepared_messages())
    };
    let streaming_ms = streaming_start.elapsed().as_secs_f64() * 1000.0;

    let is_initial_empty = app.onboarding_preview_mode()
        || (app.display_messages().is_empty()
            && !app.is_processing()
            && app.streaming_text().is_empty());

    if is_initial_empty {
        let compose_start = Instant::now();
        let suggestions = app.suggestion_prompts();
        let is_centered = app.centered_mode();
        let suggestion_align = if is_centered {
            ratatui::layout::Alignment::Center
        } else {
            ratatui::layout::Alignment::Left
        };
        let mut wrapped_lines = header_prepared.wrapped_lines.clone();

        if !suggestions.is_empty() {
            wrapped_lines.push(Line::from(""));
            for (i, (label, prompt)) in suggestions.iter().enumerate() {
                let is_login = prompt.starts_with('/');
                let pad = if is_centered { "" } else { "  " };
                let spans = if is_login {
                    vec![
                        Span::styled(
                            format!("{}{} ", pad, label),
                            Style::default()
                                .fg(rgb(138, 180, 248))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("(type {})", prompt),
                            Style::default().fg(dim_color()),
                        ),
                    ]
                } else {
                    vec![
                        Span::styled(
                            format!("{}[{}] ", pad, i + 1),
                            Style::default().fg(rgb(138, 180, 248)),
                        ),
                        Span::styled(label.clone(), Style::default().fg(rgb(200, 200, 200))),
                    ]
                };
                wrapped_lines.push(Line::from(spans).alignment(suggestion_align));
            }
            if suggestions.len() > 1 {
                wrapped_lines.push(Line::from(""));
                wrapped_lines.push(
                    Line::from(Span::styled(
                        format!(
                            "{}Press 1-{} or type anything to start",
                            if is_centered { "" } else { "  " },
                            suggestions.len()
                        ),
                        Style::default().fg(dim_color()),
                    ))
                    .alignment(suggestion_align),
                );
            }
        }

        // Vertically center the initial empty screen, but compute the padding
        // from the header height alone so the exact same padding can be
        // re-applied above the header once the conversation starts. That keeps
        // the header at the same screen position when the first prompt
        // arrives; the padding then simply scrolls away as the transcript
        // grows instead of vanishing in one jump.
        let pad_top = if app.is_replay() {
            0
        } else {
            initial_header_pad_top(height, header_prepared.wrapped_lines.len())
        };
        let mut centered = build_top_pad_lines(width, pad_top);
        centered.reserve(wrapped_lines.len());
        centered.extend(wrapped_lines);
        let wrapped_lines = centered;
        let wrapped_line_count = wrapped_lines.len();
        let wrapped_plain_lines = Arc::new(wrapped_lines.iter().map(ui::line_plain_text).collect());
        let prepared = Arc::new(PreparedMessages {
            wrapped_lines,
            wrapped_plain_lines,
            wrapped_copy_offsets: Arc::new(vec![0; wrapped_line_count]),
            raw_plain_lines: Arc::new(Vec::new()),
            wrapped_line_map: Arc::new(Vec::new()),
            wrapped_user_indices: Vec::new(),
            wrapped_user_prompt_starts: Vec::new(),
            wrapped_user_prompt_ends: Vec::new(),
            user_prompt_texts: Vec::new(),
            image_regions: Vec::new(),
            edit_tool_ranges: Vec::new(),
            copy_targets: Vec::new(),
            message_boundaries: Vec::new(),
            mermaid_pending_epoch: None,
        });
        let frame = PreparedChatFrame::from_single(prepared);
        super::note_full_prep_phase_metrics(super::FullPrepPhaseMetrics {
            header_ms,
            body_ms,
            batch_ms,
            streaming_ms,
            compose_ms: compose_start.elapsed().as_secs_f64() * 1000.0,
        });
        return frame;
    }

    let compose_start = Instant::now();
    // Re-apply the initial-screen centering pad above the header so the
    // transition from the empty screen to the first message does not shift
    // anything. The pad scrolls off naturally as the transcript grows.
    let pad_top = if app.is_replay() {
        0
    } else {
        initial_header_pad_top(height, header_prepared.wrapped_lines.len())
    };
    let padded_header = if pad_top > 0 {
        let mut lines = build_top_pad_lines(width, pad_top);
        lines.reserve(header_prepared.wrapped_lines.len());
        lines.extend(header_prepared.wrapped_lines.iter().cloned());
        let count = lines.len();
        let plain = Arc::new(lines.iter().map(ui::line_plain_text).collect());
        Arc::new(PreparedMessages {
            wrapped_lines: lines,
            wrapped_plain_lines: plain,
            wrapped_copy_offsets: Arc::new(vec![0; count]),
            raw_plain_lines: Arc::new(Vec::new()),
            wrapped_line_map: Arc::new(Vec::new()),
            wrapped_user_indices: Vec::new(),
            wrapped_user_prompt_starts: Vec::new(),
            wrapped_user_prompt_ends: Vec::new(),
            user_prompt_texts: Vec::new(),
            image_regions: Vec::new(),
            edit_tool_ranges: Vec::new(),
            copy_targets: Vec::new(),
            message_boundaries: Vec::new(),
            mermaid_pending_epoch: None,
        })
    } else {
        header_prepared
    };
    let frame = PreparedChatFrame::from_sections(vec![
        (PreparedSectionKind::Header, padded_header),
        (PreparedSectionKind::Body, body_prepared),
        (PreparedSectionKind::InlineImages, inline_images_prepared),
        (PreparedSectionKind::BatchProgress, batch_progress_prepared),
        (PreparedSectionKind::Reasoning, reasoning_prepared),
        (PreparedSectionKind::Streaming, streaming_prepared),
        (
            PreparedSectionKind::TodoBlockers,
            Arc::new(prepare_todo_blockers(app, width)),
        ),
    ]);
    super::note_full_prep_phase_metrics(super::FullPrepPhaseMetrics {
        header_ms,
        body_ms,
        batch_ms,
        streaming_ms,
        compose_ms: compose_start.elapsed().as_secs_f64() * 1000.0,
    });
    frame
}

fn prepare_todo_blockers(app: &dyn TuiState, width: u16) -> PreparedMessages {
    wrap_lines(
        messages::todo_blocker_notes(app.pinned_todos_payload()),
        &[],
        &[],
        &[],
        width,
    )
}

/// TTL + cheap-signature cache for the prepared (built + wrapped) header.
///
/// `build_persistent_header`/`build_header_lines` look cheap but are not: per
/// call they probe auth credential files (`AuthStatus::check_fast`), list and
/// deserialize every goal JSON for the goal badge, reload the project skill
/// overlay from disk, and stat release/binary channels for the update badges.
/// On this hardware that costs 30-50ms, and it ran on every full-prep cache
/// miss - i.e. on every streaming tick and every keystroke that changes the
/// input-derived cache key - which showed up directly as input lag
/// (TUI_SLOW_FRAME logs attributed 30-47ms of ~45ms slow frames to
/// `full_prep_header_ms`).
///
/// The header's inputs fall into two groups:
/// - cheap in-memory fields (model, session/server names, connection type,
///   mcp list, ...) - hashed into a signature so changes rebuild immediately;
/// - disk-backed surfaces (auth line, goal badge, skills list, update
///   badges, changelog) - refreshed only when the TTL lapses, since they
///   change rarely and independently of the render loop.
///
/// The TTL is sized to the *data*, not the frame rate. A 1s TTL made the
/// header cost bimodal: cache hits were <1ms, but every lapse paid the full
/// disk-probe rebuild (measured p50 48ms, max 273ms in TUI_SLOW_FRAME logs).
/// Since `AuthStatus` already self-caches for 30-60s, ~29 of every 30
/// second-boundary rebuilds re-walked goals, skills, and update badges only to
/// produce a byte-identical header. Matching the auth cache floor keeps the
/// refresh cadence meaningful while removing the redundant rebuilds; anything
/// a user can change from inside the TUI is already covered by the signature
/// and still repaints immediately.
const HEADER_PREP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

struct HeaderPrepCacheState {
    signature: u64,
    built_at: Instant,
    prepared: Arc<PreparedMessages>,
}

fn header_prep_cache() -> &'static std::sync::Mutex<Option<HeaderPrepCacheState>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<HeaderPrepCacheState>>> =
        std::sync::OnceLock::new();
    // Single-slot cache: a secondary (preview) render must not evict the live
    // session's header every frame, so it gets a slot of its own.
    static SECONDARY: std::sync::OnceLock<std::sync::Mutex<Option<HeaderPrepCacheState>>> =
        std::sync::OnceLock::new();
    if super::secondary_render_active() {
        return SECONDARY.get_or_init(|| std::sync::Mutex::new(None));
    }
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Drop the prepared-header cache so the next frame re-probes the disk-backed
/// surfaces (auth inventory, skills, goal badge, update badges).
///
/// The TTL alone is sized for background drift. Actions taken *inside* the TUI
/// that change those surfaces - completing `/login`, adding an account,
/// reloading skills - must be reflected immediately rather than up to a full
/// TTL later, so they call this directly.
pub(crate) fn invalidate_header_prep_cache() {
    if let Ok(mut cache) = header_prep_cache().lock() {
        *cache = None;
    }
    // Prepared frames embed the header. Clearing only the header sub-cache
    // leaves an exact full-frame hit displaying old account dots and quotas.
    if let Ok(mut cache) = full_prep_cache().lock() {
        *cache = FullPrepCacheState::default();
    }
}

/// Hash of the header inputs that are cheap to read every frame. Anything
/// expensive (disk probes) is intentionally excluded and covered by the TTL.
fn header_prep_signature(app: &dyn TuiState, width: u16) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    app.provider_model().hash(&mut hasher);
    app.provider_name().hash(&mut hasher);
    app.session_display_name().hash(&mut hasher);
    app.server_display_name().hash(&mut hasher);
    app.server_display_version().hash(&mut hasher);
    app.server_display_icon().hash(&mut hasher);
    app.connection_type().hash(&mut hasher);
    app.upstream_provider().hash(&mut hasher);
    app.routing_chip().hash(&mut hasher);
    app.is_replay().hash(&mut hasher);
    app.is_remote_mode().hash(&mut hasher);
    app.is_canary().hash(&mut hasher);
    app.server_update_available().hash(&mut hasher);
    app.mcp_servers().hash(&mut hasher);
    app.connected_clients().hash(&mut hasher);
    app.server_sessions().len().hash(&mut hasher);
    app.working_dir().hash(&mut hasher);
    // The directory row renders `(branch)`. The branch arrives from the async
    // git probe, so without this the header kept the branch-less row until an
    // unrelated field changed (or the 30s TTL lapsed), and a `git switch` did
    // not show in the header at all.
    app.git_branch().hash(&mut hasher);
    // Credential changes alter the auth inventory lines. Hashing the auth
    // generation (cheap atomic load) means `/login` and account edits repaint
    // the header on the very next frame instead of waiting out the TTL, which
    // is what lets the TTL itself be sized for slow background drift.
    crate::auth::auth_status_generation().hash(&mut hasher);
    header::account_router_generation().hash(&mut hasher);
    app.startup_oauth_account_label().hash(&mut hasher);
    // The goal badge renders the focused side-panel page title when a goal
    // page is focused; keying on it keeps focus changes instant.
    if let Some(page) = app.side_panel().focused_page() {
        page.id.hash(&mut hasher);
        page.title.hash(&mut hasher);
    }
    hasher.finish()
}

fn prepare_header_cached(app: &dyn TuiState, width: u16) -> Arc<PreparedMessages> {
    let build = || {
        let (mut all_header_lines, secondary_lines) = header::build_header_sections(app, width);
        all_header_lines.extend(secondary_lines);
        Arc::new(wrap_lines(all_header_lines, &[], &[], &[], width))
    };

    if app.is_replay() || prep_cache_bypassed() {
        return build();
    }

    let signature = header_prep_signature(app, width);

    if let Ok(cache) = header_prep_cache().lock()
        && let Some(state) = cache.as_ref()
        && state.signature == signature
        && state.built_at.elapsed() < HEADER_PREP_CACHE_TTL
    {
        return state.prepared.clone();
    }

    let prepared = build();
    if let Ok(mut cache) = header_prep_cache().lock() {
        *cache = Some(HeaderPrepCacheState {
            signature,
            built_at: Instant::now(),
            prepared: prepared.clone(),
        });
    }
    prepared
}

fn prepare_body_cached(app: &dyn TuiState, width: u16) -> Arc<PreparedMessages> {
    if prep_cache_bypassed() {
        return Arc::new(prepare_body(app, width, false));
    }

    super::note_body_request();

    let key = BodyCacheKey {
        compact_thinking: crate::config::config().display.compact_thinking,
        width,
        diff_mode: app.diff_mode(),
        // Pinning changes which tool messages participate in the transcript.
        messages_version: app
            .display_messages_version()
            .wrapping_mul(2)
            .wrapping_add(todo_header_render_tag()),
        diagram_mode: app.diagram_mode(),
        centered: app.centered_mode(),
        mermaid_aspect_bucket: crate::tui::mermaid::current_preferred_aspect_ratio_bucket(),
        pin_images: app.pin_images(),
        inline_images_visible: app.inline_images_visible(),
        images_signature: app.side_pane_images_signature(),
        expanded_images_version: app.expanded_images_version(),
        latex_rendering: crate::config::config().display.render_prefs().latex,
        show_link_urls: crate::config::config().display.show_link_urls,
    };
    let msg_count = app.display_messages().len();
    let cache_lookup_start = Instant::now();

    let cache = match body_cache().lock() {
        Ok(c) => c,
        Err(poisoned) => {
            let mut c = poisoned.into_inner();
            c.entries.clear();
            c
        }
    };

    let mut cache = cache;
    if let Some((prepared, kind, prepared_bytes)) = cache.get_exact_with_kind(&key) {
        // A deferred mermaid render completing does not bump
        // `messages_version`, so an exact hit can still be stale: it bakes in
        // a "rendering..." placeholder whose background render has since
        // finished. Fall through to the rebuild path, which truncates the
        // base at the pending message and re-renders the tail.
        let stale = prepared
            .mermaid_pending_epoch
            .is_some_and(|stamp| crate::tui::mermaid::deferred_render_epoch() != stamp);
        if !stale {
            super::note_body_cache_lookup(cache_lookup_start.elapsed());
            super::note_body_cache_hit(kind, prepared.as_ref(), prepared_bytes);
            return prepared;
        }
    }

    super::note_body_cache_lookup(cache_lookup_start.elapsed());
    super::note_body_cache_miss();

    let incremental_base = cache.take_best_incremental_base(&key);

    drop(cache);

    if let Some((base, _, _)) = incremental_base.as_ref() {
        super::release_frame_refs_to_body(base);
    }

    let build_start = Instant::now();
    let (prepared, build_path) = match incremental_base {
        Some((prev, prev_count, prev_prompt_offset)) => {
            build_body_from_base(app, width, prev, prev_count, prev_prompt_offset, msg_count)
        }
        None => (Arc::new(prepare_body(app, width, false)), "full"),
    };

    let build_elapsed = build_start.elapsed();
    let prepared_bytes = super::estimate_prepared_messages_bytes(&prepared);
    super::note_body_built(prepared.as_ref(), prepared_bytes, build_elapsed, build_path);

    let mut cache = match body_cache().lock() {
        Ok(c) => c,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache.insert_with_bytes(
        key,
        prepared.clone(),
        prepared_bytes,
        msg_count,
        app.compacted_hidden_user_prompts(),
    );
    prepared
}

#[path = "ui_prepare/body_prep.rs"]
mod body_prep;
#[allow(unused_imports)]
pub(crate) use body_prep::*;

pub(super) fn prepare_body(
    app: &dyn TuiState,
    width: u16,
    include_streaming: bool,
) -> PreparedMessages {
    // Read before rendering: a background diagram render completing mid-build
    // must leave the stamp already-stale (see `stamp_mermaid_pending`).
    let mermaid_epoch_before = crate::tui::mermaid::deferred_render_epoch();
    let centered = app.centered_mode();
    markdown::set_center_code_blocks(centered);
    let display_width = width.saturating_sub(4) as usize;

    let messages = app.display_messages();
    let images_start = Instant::now();
    // Images anchored to transcript messages render inline right after the
    // message that produced them (tool result or user prompt).
    let anchored_images = super::inline_image_ui::resolve_anchored_items_cached(app);
    let images_ms = images_start.elapsed().as_millis();
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
    let ctx_ms = images_start.elapsed().as_millis() - images_ms;

    let render_start = Instant::now();
    let mut acc = BodyAcc::default();
    let mut role_ms: std::collections::BTreeMap<&str, (u128, usize)> = Default::default();
    let mut slowest: (u128, usize, &str) = (0, 0, "");
    let mut tool_ms: std::collections::BTreeMap<String, (u128, usize)> = Default::default();
    for (msg_idx, msg) in messages.iter().enumerate() {
        let msg_start = Instant::now();
        render_message_into(&ctx, &mut acc, msg, msg_idx);
        let us = msg_start.elapsed().as_micros();
        if let Some(tool) = msg.tool_data.as_ref() {
            let entry = tool_ms
                .entry(tools_ui::canonical_tool_name(&tool.name).to_string())
                .or_default();
            entry.0 += us;
            entry.1 += 1;
        }
        let entry = role_ms.entry(msg.effective_role()).or_default();
        entry.0 += us;
        entry.1 += 1;
        if us > slowest.0 {
            slowest = (us, msg_idx, msg.effective_role());
        }
    }

    // Streaming output is appended after the message boundaries are recorded so
    // it is never treated as a reusable message segment (its content changes on
    // every token). `include_streaming` is false on the cached body path; this
    // branch only fires for the rare direct streaming preview.
    if include_streaming && app.is_processing() && !app.streaming_text().is_empty() {
        if !acc.lines.is_empty() {
            acc.lines.push(Line::from(""));
            acc.line_raw_overrides.push(None);
            acc.line_copy_offsets.push(0);
        }
        let content_width = if centered {
            display_width.clamp(1, 96)
        } else {
            display_width
        };
        let mut md_lines = app.render_streaming_markdown(content_width);
        if centered {
            markdown::recenter_structured_blocks_for_display(&mut md_lines, display_width);
        }
        let line_offset = acc.lines.len();
        acc.copy_targets.extend(
            assistant_message_copy_targets(app.streaming_text(), &md_lines)
                .into_iter()
                .map(|target| offset_copy_target(target, line_offset)),
        );
        let align = default_message_alignment("assistant", centered);
        for line in md_lines {
            acc.lines.push(align_if_unset(line, align));
            acc.line_raw_overrides.push(None);
            acc.line_copy_offsets.push(0);
        }
    }

    let render_ms = render_start.elapsed().as_millis();
    let wrap_start = Instant::now();
    let mut prepared = wrap_lines_with_map(
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
    let wrap_ms = wrap_start.elapsed().as_millis();
    if images_ms + ctx_ms + render_ms + wrap_ms >= 100 {
        crate::logging::info(&format!(
            "[TIMING] prepare_body: messages={}, images={images_ms}ms, ctx={ctx_ms}ms, render={render_ms}ms, wrap={wrap_ms}ms, wrapped_lines={}, by_role=[{}], by_tool=[{}], slowest={}us@{}({})",
            messages.len(),
            prepared.wrapped_lines.len(),
            role_ms
                .iter()
                .map(|(role, (us, n))| format!("{role}:{}ms/{n}", us / 1000))
                .collect::<Vec<_>>()
                .join(" "),
            {
                let mut tools: Vec<_> = tool_ms.iter().collect();
                tools.sort_by(|a, b| b.1.0.cmp(&a.1.0));
                tools
                    .iter()
                    .take(6)
                    .map(|(tool, (us, n))| format!("{tool}:{}ms/{n}", us / 1000))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            slowest.0,
            slowest.1,
            slowest.2
        ));
    }
    stamp_mermaid_pending(&mut prepared, mermaid_epoch_before);
    prepared
}

fn wrap_lines(
    lines: Vec<Line<'static>>,
    line_copy_offsets: &[usize],
    user_line_indices: &[usize],
    user_prompt_texts: &[String],
    width: u16,
) -> PreparedMessages {
    let full_width = width.saturating_sub(1) as usize;
    let user_width = width.saturating_sub(2) as usize;
    let mut wrapped_user_indices: Vec<usize> = Vec::new();
    let mut wrapped_user_prompt_starts: Vec<usize> = Vec::new();
    let mut wrapped_user_prompt_ends: Vec<usize> = Vec::new();
    let mut raw_plain_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut wrapped_line_map: Vec<WrappedLineMap> = Vec::new();
    let mut wrapped_copy_offsets: Vec<usize> = Vec::new();
    let mut user_line_mask = vec![false; lines.len()];
    for &idx in user_line_indices {
        if idx < user_line_mask.len() {
            user_line_mask[idx] = true;
        }
    }
    let mut wrapped_idx = 0usize;

    let mut wrapped_lines: Vec<Line> = Vec::new();
    for (orig_idx, line) in lines.into_iter().enumerate() {
        let raw_text = ui::line_plain_text(&line);
        let raw_width = unicode_width::UnicodeWidthStr::width(raw_text.as_str());
        raw_plain_lines.push(raw_text);
        let is_user_line = user_line_mask.get(orig_idx).copied().unwrap_or(false);
        let wrap_width = if is_user_line { user_width } else { full_width };
        let new_lines = markdown::wrap_line(line, wrap_width);
        let count = new_lines.len();
        let first_copy_offset = line_copy_offsets.get(orig_idx).copied().unwrap_or(0);
        // Continuation rows re-seed the gutter / hanging-indent prefix, so the
        // row widths sum to `first_copy_offset + raw_width + prefix * (rows - 1)`.
        // Recover the prefix from that instead of tracking it through the wrapper.
        let repeated_prefix_width = if count > 1 {
            new_lines
                .iter()
                .map(|line| line.width())
                .sum::<usize>()
                .saturating_sub(raw_width)
                .saturating_sub(first_copy_offset)
                / (count - 1)
        } else {
            0
        };
        let mut start_col = 0usize;

        for (row_idx, wrapped_line) in new_lines.iter().enumerate() {
            let width = wrapped_line.width();
            let visual_prefix = if row_idx == 0 {
                first_copy_offset
            } else {
                repeated_prefix_width
            };
            let source_width = width.saturating_sub(visual_prefix);
            let end_col = (start_col + source_width).min(raw_width);
            wrapped_line_map.push(WrappedLineMap {
                raw_line: orig_idx,
                start_col,
                end_col,
            });
            wrapped_copy_offsets.push(visual_prefix.min(width));
            start_col = end_col;
        }

        if is_user_line {
            wrapped_user_prompt_starts.push(wrapped_idx);
            wrapped_user_prompt_ends.push(wrapped_idx + count);
            for i in 0..count {
                wrapped_user_indices.push(wrapped_idx + i);
            }
        }

        wrapped_lines.extend(new_lines);
        wrapped_idx += count;
    }

    let image_regions = compute_image_regions(&wrapped_lines);
    shrink_wrapped_lines(&mut wrapped_lines);

    let wrapped_plain_lines = Arc::new(wrapped_lines.iter().map(ui::line_plain_text).collect());

    PreparedMessages {
        wrapped_lines,
        wrapped_plain_lines,
        wrapped_copy_offsets: Arc::new(wrapped_copy_offsets),
        raw_plain_lines: Arc::new(raw_plain_lines),
        wrapped_line_map: Arc::new(wrapped_line_map),
        wrapped_user_indices,
        wrapped_user_prompt_starts,
        wrapped_user_prompt_ends,
        user_prompt_texts: user_prompt_texts.to_vec(),
        image_regions,
        edit_tool_ranges: Vec::new(),
        copy_targets: Vec::new(),
        message_boundaries: Vec::new(),
        mermaid_pending_epoch: None,
    }
}

/// Drop spare span and line capacity before a body is cached. Wrapping builds
/// lines by pushing spans, so a long session kept ~1.7x the span slots it used,
/// and these bodies live in the body cache for the session's lifetime.
fn shrink_wrapped_lines(wrapped_lines: &mut Vec<Line<'static>>) {
    for line in wrapped_lines.iter_mut() {
        line.spans.shrink_to_fit();
    }
    wrapped_lines.shrink_to_fit();
}

#[expect(
    clippy::too_many_arguments,
    reason = "Wrapped-line preparation carries explicit render state to avoid hidden coupling"
)]
fn wrap_lines_with_map(
    lines: Vec<Line<'static>>,
    seeded_raw_plain_lines: &[String],
    line_raw_overrides: &[Option<WrappedLineMap>],
    line_copy_offsets: &[usize],
    user_line_indices: &[usize],
    user_prompt_texts: &[String],
    width: u16,
    edit_ranges: &[(usize, String, usize, usize, bool)],
    copy_ranges: &[RawCopyTarget],
    segments: &[(u64, usize, usize, usize)],
) -> PreparedMessages {
    let full_width = width.saturating_sub(1) as usize;
    let user_width = width.saturating_sub(2) as usize;
    let mut wrapped_user_indices: Vec<usize> = Vec::new();
    let mut wrapped_user_prompt_starts: Vec<usize> = Vec::new();
    let mut wrapped_user_prompt_ends: Vec<usize> = Vec::new();
    let mut raw_plain_lines: Vec<String> = seeded_raw_plain_lines.to_vec();
    let mut wrapped_line_map: Vec<WrappedLineMap> = Vec::new();
    let mut wrapped_copy_offsets: Vec<usize> = Vec::new();
    let mut user_line_mask = vec![false; lines.len()];
    for &idx in user_line_indices {
        if idx < user_line_mask.len() {
            user_line_mask[idx] = true;
        }
    }
    let mut wrapped_idx = 0usize;

    let mut raw_to_wrapped: Vec<usize> = Vec::with_capacity(lines.len() + 1);

    let mut wrapped_lines: Vec<Line> = Vec::new();
    for (orig_idx, line) in lines.into_iter().enumerate() {
        let (raw_line, start_col, end_col) =
            if let Some(Some(map)) = line_raw_overrides.get(orig_idx) {
                (map.raw_line, map.start_col, map.end_col)
            } else {
                let raw_text = ui::line_plain_text(&line);
                let raw_width = unicode_width::UnicodeWidthStr::width(raw_text.as_str());
                let raw_line = raw_plain_lines.len();
                raw_plain_lines.push(raw_text);
                (raw_line, 0usize, raw_width)
            };
        raw_to_wrapped.push(wrapped_idx);
        let is_user_line = user_line_mask.get(orig_idx).copied().unwrap_or(false);
        let wrap_width = if is_user_line { user_width } else { full_width };
        let new_lines = markdown::wrap_line(line, wrap_width);
        let count = new_lines.len();
        let first_copy_offset = line_copy_offsets.get(orig_idx).copied().unwrap_or(0);
        // Same invariant as `wrap_lines`: the row widths sum to the source span
        // plus the first row's gutter and one re-seeded prefix per later row.
        let repeated_prefix_width = if count > 1 {
            new_lines
                .iter()
                .map(|line| line.width())
                .sum::<usize>()
                .saturating_sub(end_col.saturating_sub(start_col))
                .saturating_sub(first_copy_offset)
                / (count - 1)
        } else {
            0
        };
        let mut segment_start = start_col;

        for (row_idx, wrapped_line) in new_lines.iter().enumerate() {
            let width = wrapped_line.width();
            let visual_prefix = if row_idx == 0 {
                first_copy_offset
            } else {
                repeated_prefix_width
            };
            let source_width = width.saturating_sub(visual_prefix);
            let segment_end = (segment_start + source_width).min(end_col);
            wrapped_line_map.push(WrappedLineMap {
                raw_line,
                start_col: segment_start,
                end_col: segment_end,
            });
            wrapped_copy_offsets.push(visual_prefix.min(width));
            segment_start = segment_end;
        }

        if is_user_line {
            wrapped_user_prompt_starts.push(wrapped_idx);
            wrapped_user_prompt_ends.push(wrapped_idx + count);
            for i in 0..count {
                wrapped_user_indices.push(wrapped_idx + i);
            }
        }

        wrapped_lines.extend(new_lines);
        wrapped_idx += count;
    }
    raw_to_wrapped.push(wrapped_idx);

    let image_regions = compute_image_regions(&wrapped_lines);

    let mut edit_tool_ranges = Vec::new();
    for (msg_idx, file_path, raw_start, raw_end, expandable) in edit_ranges {
        let start_line = raw_to_wrapped.get(*raw_start).copied().unwrap_or(0);
        let end_line = raw_to_wrapped
            .get(*raw_end)
            .copied()
            .unwrap_or(wrapped_lines.len());
        edit_tool_ranges.push(EditToolRange {
            edit_index: edit_tool_ranges.len(),
            msg_index: *msg_idx,
            file_path: file_path.clone(),
            start_line,
            end_line,
            expandable: *expandable,
        });
    }

    let mut copy_targets = Vec::new();
    for target in copy_ranges {
        let start_line = raw_to_wrapped
            .get(target.start_raw_line)
            .copied()
            .unwrap_or(0);
        let end_line = raw_to_wrapped
            .get(target.end_raw_line)
            .copied()
            .unwrap_or(wrapped_lines.len());
        let badge_line = raw_to_wrapped
            .get(target.badge_raw_line)
            .copied()
            .unwrap_or(start_line);
        copy_targets.push(CopyTarget {
            kind: target.kind.clone(),
            content: target.content.clone(),
            start_line,
            end_line,
            badge_line,
        });
    }

    shrink_wrapped_lines(&mut wrapped_lines);
    let wrapped_plain_lines = Arc::new(wrapped_lines.iter().map(ui::line_plain_text).collect());

    // Translate the per-message unwrapped-line boundaries into wrapped-line
    // boundaries. `raw_to_wrapped[i]` is the first wrapped index produced by
    // unwrapped line `i` (with a final sentinel at `lines.len()`), so a segment
    // that ends just before unwrapped line `L` ends at wrapped line
    // `raw_to_wrapped[L]`. This lets a later rebuild truncate the wrapped body at
    // any message boundary and reuse the unchanged prefix.
    let message_boundaries: Vec<MessageBoundary> = segments
        .iter()
        .map(
            |&(msg_hash, lines_len_after, raw_len_after, user_prompt_len_after)| MessageBoundary {
                msg_hash,
                wrapped_len: raw_to_wrapped
                    .get(lines_len_after)
                    .copied()
                    .unwrap_or(wrapped_lines.len()),
                raw_len: raw_len_after,
                user_prompt_len: user_prompt_len_after,
            },
        )
        .collect();

    PreparedMessages {
        wrapped_lines,
        wrapped_plain_lines,
        wrapped_copy_offsets: Arc::new(wrapped_copy_offsets),
        raw_plain_lines: Arc::new(raw_plain_lines),
        wrapped_line_map: Arc::new(wrapped_line_map),
        wrapped_user_indices,
        wrapped_user_prompt_starts,
        wrapped_user_prompt_ends,
        user_prompt_texts: user_prompt_texts.to_vec(),
        image_regions,
        edit_tool_ranges,
        copy_targets,
        message_boundaries,
        mermaid_pending_epoch: None,
    }
}

#[cfg(test)]
#[path = "ui_prepare/tests.rs"]
mod tests;

#[cfg(test)]
thread_local! {
    // Thread-local so a probe that opts into the caches does not make tests
    // running in parallel share (and clobber) its cache entries and metrics.
    static ENABLE_PREP_CACHE_IN_TESTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tests bypass the prep caches by default for determinism. Perf probes opt in
/// so they measure the production cached path.
#[cfg(test)]
pub(crate) fn set_prep_cache_enabled_for_tests(enabled: bool) {
    ENABLE_PREP_CACHE_IN_TESTS.with(|flag| flag.set(enabled));
}

#[inline]
fn prep_cache_bypassed() -> bool {
    #[cfg(test)]
    {
        !ENABLE_PREP_CACHE_IN_TESTS.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
#[path = "ui_prepare/preview_fallback.rs"]
mod preview_fallback;
#[cfg(test)]
#[path = "ui_prepare/viewport_preview_tests.rs"]
mod viewport_preview_tests;
