//! Snapcompact — bitmap-frame context compression for vision-capable LLMs.
//!
//! Adapted from oxicode (https://github.com/project-oxi/oxicode, MIT). See
//! `NOTICE` at the repository root and `docs/ref-porter/xai-org-grok-build.md`
//! section E2 for design rationale.
//!
//! ## How it works
//!
//! Instead of asking an LLM to summarize discarded history, snapcompact
//! renders discarded text as dense bitmap frames that vision-capable
//! models read back directly. Local and deterministic — no LLM call,
//! no API key, no latency beyond rendering.
//!
//! ## Scope of this crate
//!
//! Shape system, conversation serialization, shape selection, and
//! the full text→PNG rasterizer (see the [`renderer`] submodule).
//! The renderer uses bundled BDF/HEX bitmap fonts plus the bundled
//! Silver TrueType font for non-Latin glyphs, all under permissive
//! licenses (see `NOTICE.md`).
//!
//! [`compact()`] returns an error if rendering fails or the frame budget
//! cannot represent the source. Callers must fall back without discarding it.
pub mod renderer;
pub mod serialize;

// ── Shape system ──────────────────────────────────────────────────────

/// One eval-validated frame shape.
///
/// `name` is a stable identifier from the eval-validated shape table.
/// The MVP carries a hard-coded name on each shape entry; future
/// extension can compute names from fields, but the table is the source
/// of truth for now.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    /// Stable name (e.g. "11on16-bw"). Used as lookup key.
    pub name: &'static str,
    /// Bundled font.
    pub font: Font,
    pub cell_width: u32,
    pub cell_height: u32,
    pub stretch: bool,
    pub variant: Variant,
    pub stopword_dim: bool,
    pub columns: u32,
    pub line_repeat: u32,
    pub frame_size: u32,
}

/// Bundled font name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    FiveByEight,
    EightByEight,
    SixByTwelve,
    EightByThirteen,
    Silver,
}

impl Font {
    /// String identifier of the font in the shape table.
    pub fn as_str(&self) -> &'static str {
        match self {
            Font::FiveByEight => "5x8",
            Font::EightByEight => "8x8",
            Font::SixByTwelve => "6x12",
            Font::EightByThirteen => "8x13",
            Font::Silver => "silver",
        }
    }
}

/// Ink variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Cycle six hues at sentence boundaries.
    Sent,
    /// Plain black ink — best for Anthropic vision readers.
    Bw,
}

impl Variant {
    pub fn as_str(&self) -> &'static str {
        match self {
            Variant::Sent => "sent",
            Variant::Bw => "bw",
        }
    }
}

/// Eval-validated shape table. Names are stable identifiers.
pub const SHAPES: &[Shape] = &[
    // Redundancy-coded double-print (line_repeat=2).
    Shape {
        name: "8x8r-bw",
        font: Font::EightByEight,
        cell_width: 8,
        cell_height: 8,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 2,
        frame_size: 1568,
    },
    Shape {
        name: "8x8r-sent",
        font: Font::EightByEight,
        cell_width: 8,
        cell_height: 8,
        stretch: false,
        variant: Variant::Sent,
        stopword_dim: false,
        columns: 1,
        line_repeat: 2,
        frame_size: 1568,
    },
    // Standard 8x8 dense shapes. Note: `u` suffix is part of the name
    // even though cell==natural; it flags the variant family.
    Shape {
        name: "8x8u-bw",
        font: Font::EightByEight,
        cell_width: 8,
        cell_height: 8,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    Shape {
        name: "8x8u-sent",
        font: Font::EightByEight,
        cell_width: 8,
        cell_height: 8,
        stretch: false,
        variant: Variant::Sent,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // Squeezed 8x8 (Lanczos-scaled to 6x6 cell).
    Shape {
        name: "6x6u-bw",
        font: Font::EightByEight,
        cell_width: 6,
        cell_height: 6,
        stretch: true,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    Shape {
        name: "6x6u-sent",
        font: Font::EightByEight,
        cell_width: 6,
        cell_height: 6,
        stretch: true,
        variant: Variant::Sent,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // 5x8 at 2576px frame edge.
    Shape {
        name: "5x8-bw",
        font: Font::FiveByEight,
        cell_width: 5,
        cell_height: 8,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 2576,
    },
    Shape {
        name: "5x8-sent",
        font: Font::FiveByEight,
        cell_width: 5,
        cell_height: 8,
        stretch: false,
        variant: Variant::Sent,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 2576,
    },
    // 6x12-dim: dim stopwords in gray ink.
    Shape {
        name: "6x12-dim",
        font: Font::SixByTwelve,
        cell_width: 6,
        cell_height: 12,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: true,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // 8x13 natural-cell shape.
    Shape {
        name: "8x13-bw",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 13,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // 8x13 on extra-leading pitches.
    Shape {
        name: "8on16-bw",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 16,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    Shape {
        name: "8on22-bw",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 22,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // 11on16-bw: extra tracking (11px advance) on 8x13.
    Shape {
        name: "11on16-bw",
        font: Font::EightByThirteen,
        cell_width: 11,
        cell_height: 16,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // Silver TTF (16x16 grid) — CJK.
    Shape {
        name: "silver16-bw",
        font: Font::Silver,
        cell_width: 16,
        cell_height: 16,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 1,
        line_repeat: 1,
        frame_size: 1568,
    },
    // Newspaper column layouts (doc shapes).
    Shape {
        name: "doc-8on16-bw",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 16,
        stretch: false,
        variant: Variant::Bw,
        stopword_dim: false,
        columns: 2,
        line_repeat: 1,
        frame_size: 1568,
    },
    Shape {
        name: "doc-8on16-sent",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 16,
        stretch: false,
        variant: Variant::Sent,
        stopword_dim: false,
        columns: 2,
        line_repeat: 1,
        frame_size: 1568,
    },
    Shape {
        name: "doc-8on16-sent-dim",
        font: Font::EightByThirteen,
        cell_width: 8,
        cell_height: 16,
        stretch: false,
        variant: Variant::Sent,
        stopword_dim: true,
        columns: 2,
        line_repeat: 1,
        frame_size: 1568,
    },
];

impl Shape {
    /// The stable shape name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Number of cells in a single-column frame.
    pub fn chars_per_frame(&self) -> u32 {
        (self.frame_size / self.cell_width.max(1)).saturating_mul(self.rows_per_frame())
    }

    /// Approximate rows per frame.
    pub fn rows_per_frame(&self) -> u32 {
        self.frame_size / self.cell_height.max(1) / self.line_repeat.max(1)
    }
}

/// Per-provider default frame shape when the model line is unmeasured.
/// Unknown readers get the OpenAI/Google winner.
const FAMILY_VARIANT_UNKNOWN: &str = "8on22-bw";

/// Eval-winning variant and frame size for a model line, or `None` when the
/// line is unmeasured.
///
/// `11on16-bw` is 8x13 glyphs on an 11px advance (extra tracking): letter
/// spacing the readable cell won the tool-result legibility bench. Opus 4.7+
/// and the Fable/Mythos lines read 1932px frames natively (largest square that
/// is not downscaled under Anthropic's 4,784 visual-token cap); Gemini bills a
/// fixed per-image budget, so 2048px packs more chars for the same price.
fn ideal_shape(model_id: &str) -> Option<(&'static str, Option<u32>)> {
    let id = model_id.to_ascii_lowercase();
    let revision = model_revision(&id);
    let high_res = Some(("11on16-bw", Some(1932)));
    if id.contains("claude") {
        let family_high_res = ["fable", "mythos"].iter().any(|family| id.contains(family))
            || (id.contains("opus") && revision.is_some_and(|revision| revision >= (4, 7)));
        return Some(if family_high_res {
            high_res?
        } else {
            ("11on16-bw", None)
        });
    }
    if id.contains("gemini") {
        return Some(("8on22-bw", Some(2048)));
    }
    if id.contains("gpt") || id.contains("codex") {
        return Some(("8on22-bw", None));
    }
    if id.contains("kimi") {
        return Some(("8on22-bw", None));
    }
    if id.contains("glm") {
        return Some(("8on16-bw", None));
    }
    None
}

/// `(major, minor)` of the first `N` / `N.M` version in a model id.
/// Overflow is an unrecognized revision, not an operational error: callers use
/// the conservative family fallback for arbitrary provider-supplied model IDs.
fn model_revision(id: &str) -> Option<(u32, u32)> {
    let bytes = id.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let major: u32 = id[start..index].parse().ok()?;
        let mut minor = 0;
        if bytes.get(index) == Some(&b'.') && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            let minor_start = index + 1;
            index = minor_start;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            minor = id[minor_start..index].parse().ok()?;
        }
        return Some((major, minor));
    }
    None
}

/// Pick the eval-validated shape for `model_id`, including its frame size.
///
/// The model id selects the eval-winning shape and
/// frame size for its line, and an unmeasured model falls back to the family
/// winner at the standard 1568px frame.
pub fn resolve_shape(model_id: &str) -> Shape {
    resolve_shape_for(model_id, "")
}

/// Pick the shape for a reader, using `provider` for the fallback family
/// keyed on the wire API.
pub fn resolve_shape_for(model_id: &str, provider: &str) -> Shape {
    let variant = match ideal_shape(model_id) {
        Some((variant, frame_size)) => {
            let mut shape = lookup_owned(variant).unwrap_or_else(|| SHAPES[0].clone());
            if let Some(frame_size) = frame_size {
                shape.frame_size = frame_size;
            }
            return shape;
        }
        None => family_variant(provider),
    };
    lookup_owned(variant).unwrap_or_else(|| SHAPES[0].clone())
}

/// The eval winner for the reader's wire family.
fn family_variant(provider: &str) -> &'static str {
    let provider = provider.to_ascii_lowercase();
    if provider.contains("anthropic")
        || provider.contains("claude")
        || provider.contains("bedrock")
        || provider.contains("vertex-anthropic")
    {
        "11on16-bw"
    } else {
        FAMILY_VARIANT_UNKNOWN
    }
}

/// Images one archive may carry for `provider` (its catalog image-count
/// budget, capped at `MAX_FRAMES_DEFAULT`). Providers with no tighter rule
/// keep the default.
pub fn frame_budget(provider: &str) -> u32 {
    let provider = provider.to_ascii_lowercase();
    let budget =
        if provider.contains("xai") || provider.contains("grok") || provider.contains("cursor") {
            60
        } else if provider.contains("antigravity") {
            20
        } else if provider.contains("umans") {
            10
        } else {
            MAX_FRAMES_DEFAULT
        };
    budget.clamp(1, MAX_FRAMES_DEFAULT)
}

/// Look up a shape by its stable name.
pub fn shape_by_name(name: &str) -> Option<Shape> {
    lookup_owned(name.trim())
}

fn lookup_owned(name: &str) -> Option<Shape> {
    SHAPES.iter().find(|s| s.name == name).cloned()
}

// ── Text normalization ────────────────────────────────────────────────

/// Normalize conversation text for rasterization.
pub fn normalize(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut whitespace = false;
    let mut line_break = false;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
            continue;
        }
        let c = input[i..].chars().next().unwrap_or(' ');
        i += c.len_utf8();
        if c.is_whitespace() {
            whitespace = true;
            line_break |= matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
            continue;
        }
        if whitespace && !out.is_empty() {
            out.push(if line_break { '\u{2588}' } else { ' ' });
        }
        whitespace = false;
        line_break = false;
        out.push(match c {
            '─' | '━' => '-',
            '│' | '┃' => '|',
            '\u{2500}'..='\u{257f}' => '+',
            _ => c,
        });
    }
    out
}

// ── Preparation ──────────────────────────────────────────────────────

/// Newline glyph used inside normalized archive text.
pub const NEWLINE_GLYPH: char = '\u{2588}';

/// Head of a folded-in text summary.
pub const PREVIOUS_SUMMARY_HEAD: &str = "[Summary of earlier history]";

/// Inputs to a compaction pass.
#[derive(Debug, Clone)]
pub struct CompactPreparation {
    pub text: String,
    pub bounded_text: String,
    pub remaining_text: String,
    /// The archive opens with a condensed digest of still-older context.
    pub included_previous_summary: bool,
}

/// A previously compacted history a new pass folds in.
#[derive(Debug, Clone, Copy)]
pub struct PreviousArchive<'a> {
    pub text: &'a str,
    /// The text is an LLM summary rather than a snapcompact archive source.
    pub is_summary: bool,
}

/// Prepare structured conversation history, retaining a previous archive when
/// compacting again. Exceeding the text limit is reported by `compact`.
pub fn prepare_messages(
    messages: &[jcode_message_types::Message],
    previous_archive: Option<&str>,
    text_limit: usize,
) -> Result<CompactPreparation, String> {
    prepare_messages_with(
        messages,
        previous_archive.map(|text| PreviousArchive {
            text,
            is_summary: false,
        }),
        text_limit,
    )
}

/// Prepare structured conversation history, folding in a previous archive
/// source or a previous text summary.
pub fn prepare_messages_with(
    messages: &[jcode_message_types::Message],
    previous: Option<PreviousArchive<'_>>,
    text_limit: usize,
) -> Result<CompactPreparation, String> {
    let serialized = serialize::serialize_messages(messages)?;
    let fresh = normalize(&serialized);
    let mut included_previous_summary = false;
    let text = match previous.filter(|previous| !previous.text.is_empty()) {
        Some(PreviousArchive {
            text: previous_text,
            is_summary: true,
        }) => {
            let lead = format!("{PREVIOUS_SUMMARY_HEAD} {}", normalize(previous_text));
            included_previous_summary = true;
            if fresh.is_empty() {
                lead
            } else {
                format!("{lead} [Recent conversation] {fresh}")
            }
        }
        Some(previous) => {
            if fresh.is_empty() {
                previous.text.to_string()
            } else {
                format!("{}{NEWLINE_GLYPH}{fresh}", previous.text)
            }
        }
        None => fresh,
    };
    let bounded_text = bounded_slice(&text, text_limit);
    let remaining_text = text.chars().skip(bounded_text.chars().count()).collect();
    Ok(CompactPreparation {
        text,
        bounded_text,
        remaining_text,
        included_previous_summary,
    })
}

/// Render the conversation text into a serializable compact envelope.
pub fn prepare(input: &str, tool_result_max_chars: usize, text_limit: usize) -> CompactPreparation {
    let text = serialize_conversation(input, tool_result_max_chars);
    let bounded = bounded_slice(&text, text_limit);
    let consumed = bounded.chars().count();
    let remaining = if consumed >= text.chars().count() {
        String::new()
    } else {
        text.chars().skip(consumed).collect()
    };
    CompactPreparation {
        text,
        bounded_text: bounded,
        remaining_text: remaining,
        included_previous_summary: false,
    }
}

/// Compact conversation text to one line per turn.
pub fn serialize_conversation(input: &str, tool_result_max_chars: usize) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if line.starts_with("Tool:") {
            let truncated = truncate_tool_output(trimmed, tool_result_max_chars);
            out.push_str(&truncated);
            out.push('\n');
        } else {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}

fn truncate_tool_output(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let head_ratio = 0.6;
    let head = (max_chars as f64 * head_ratio) as usize;
    let tail = max_chars.saturating_sub(head);
    let chars: Vec<char> = line.chars().collect();
    let head_str: String = chars.iter().take(head).collect();
    let tail_str: String = chars
        .iter()
        .skip(chars.len().saturating_sub(tail))
        .collect();
    format!("{head_str}…[truncated]…{tail_str}")
}

fn bounded_slice(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

// ── Compaction envelope ──────────────────────────────────────────────

/// Options for [`compact`].
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Reading model id; selects the eval-winning frame shape.
    pub model_id: String,
    /// Reading provider; caps the archive frame count.
    pub provider: String,
    pub max_frames: u32,
    /// Forced shape; `None` = auto.
    pub shape: Option<Shape>,
    /// Forced frame edge in pixels.
    pub frame_size: Option<u32>,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            provider: String::new(),
            max_frames: MAX_FRAMES_DEFAULT,
            shape: None,
            frame_size: None,
        }
    }
}

/// Outcome of a compaction pass.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Reading guide for the frames plus the `HISTORY` section head. This is
    /// the compaction summary: a snapcompact pass makes no LLM call.
    pub summary: String,
    pub source_text: String,
    pub frames: Vec<FrameRef>,
    pub shape: Shape,
    /// Characters of the archive middle dropped to fit the frame budget.
    pub truncated_chars: usize,
    /// Characters represented by frames plus plain text.
    pub total_chars: usize,
    /// Pages actually rasterized and encoded in this pass.
    pub rendered_frames: usize,
    pub render_timings: renderer::snapcompact_render::RenderTimings,
}

/// A reference to a rendered frame.
#[derive(Debug, Clone)]
pub struct FrameRef {
    pub index: u32,
    pub source_start: usize,
    pub source_end: usize,
    pub bytes: Vec<u8>,
}

impl CompactResult {
    /// Borrow rendered pages for a subsequent append-only compaction.
    pub fn frame_cache(&self) -> FrameCache<'_> {
        FrameCache {
            source: &self.source_text,
            render_key: format!("v1:{:?}", self.shape),
            frames: &self.frames,
        }
    }

    /// Drop the frame bytes; the bounded source + ranges remain.
    pub fn into_bounded_source(&self) -> BoundedSource {
        BoundedSource {
            lead_in: self.summary.clone(),
            text: self.source_text.clone(),
            frames: self
                .frames
                .iter()
                .map(|f| FrameRange {
                    index: f.index,
                    source_start: f.source_start,
                    source_end: f.source_end,
                })
                .collect(),
        }
    }
}

/// A cache is reusable only for the identical renderer geometry and text.
pub struct FrameCache<'a> {
    pub source: &'a str,
    pub render_key: String,
    pub frames: &'a [FrameRef],
}

/// Convert a [`Shape`] into the renderer's option struct.
///
/// The shape's `font` / `cell_width` / `cell_height` / `variant` /
/// `line_repeat` / `stretch` / `columns` fields map directly onto
/// [`self::renderer::SnapcompactRenderOptions`].
fn shape_to_render_options(shape: &Shape) -> self::renderer::SnapcompactRenderOptions {
    self::renderer::SnapcompactRenderOptions {
        size: shape.frame_size,
        font: Some(shape.font.as_str().to_string()),
        cell_width: Some(shape.cell_width),
        cell_height: Some(shape.cell_height),
        variant: Some(shape.variant.as_str().to_string()),
        line_repeat: Some(shape.line_repeat),
        stretch: Some(shape.stretch),
        columns: Some(shape.columns),
    }
}

/// A bounded archive: reading guide, plain source text, and the frame ranges
/// that render each page.
#[derive(Debug, Clone)]
pub struct BoundedSource {
    pub lead_in: String,
    pub text: String,
    pub frames: Vec<FrameRange>,
}

/// One rendered page's character range in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub index: u32,
    pub source_start: usize,
    pub source_end: usize,
}

/// Upper bound on archive frames carried per compaction: ~400k tokens of
/// high-res Anthropic frames while
/// staying under the ~100-image per-request wire cap.
pub const MAX_FRAMES_DEFAULT: u32 = 80;

/// One planned archive layout.
struct FramePlan {
    ranges: Vec<FrameRange>,
    /// Frames before the dropped middle.
    head_count: usize,
    /// Character range dropped from the middle, if the source did not fit.
    elided: Option<(usize, usize)>,
}

/// Plan frames for `text`, keeping both chronological ends when the source does
/// not fit `max_frames` (foveation: the least-important
/// middle of a long archive is dropped rather than failing the pass).
fn plan_frames(
    text: &str,
    shape: &Shape,
    max_frames: usize,
    previous: Option<&FrameCache<'_>>,
) -> FramePlan {
    let all = if let Some(previous) = previous.filter(|p| text.starts_with(p.source)) {
        let last = previous.frames.last().expect("validated nonempty cache");
        let mut ranges: Vec<_> = previous.frames[..previous.frames.len() - 1]
            .iter()
            .map(|frame| FrameRange {
                index: frame.index,
                source_start: frame.source_start,
                source_end: frame.source_end,
            })
            .collect();
        let tail_byte = text
            .char_indices()
            .nth(last.source_start)
            .map_or(text.len(), |(byte, _)| byte);
        ranges.extend(
            chunk_into_frames(&text[tail_byte..], shape, usize::MAX)
                .into_iter()
                .map(|mut range| {
                    range.source_start += last.source_start;
                    range.source_end += last.source_start;
                    range
                }),
        );
        for (index, range) in ranges.iter_mut().enumerate() {
            range.index = index as u32;
        }
        ranges
    } else {
        chunk_into_frames(text, shape, usize::MAX)
    };
    if all.len() <= max_frames {
        let head_count = all.len();
        return FramePlan {
            ranges: all,
            head_count,
            elided: None,
        };
    }
    let head_count = max_frames.div_ceil(2);
    let tail_count = max_frames - head_count;
    let total = all.len();
    let mut ranges: Vec<FrameRange> = all[..head_count].to_vec();
    if tail_count > 0 {
        ranges.extend(all[total - tail_count..].iter().cloned());
    }
    let elided_end = if tail_count > 0 {
        ranges[head_count].source_start
    } else {
        all[total - 1].source_end
    };
    let elided = (ranges[head_count - 1].source_end, elided_end);
    for (index, range) in ranges.iter_mut().enumerate() {
        range.index = index as u32;
    }
    FramePlan {
        ranges,
        head_count,
        elided: Some(elided),
    }
}

/// Run a compaction pass: render each frame slice through the
/// renderer (`render_snapcompact_png`) so the
/// returned bytes are real PNG-encoded image data.
///
/// Never fails for a long history: it drops the archive middle and
/// reports `truncated_chars` instead of falling back to an LLM summary.
pub fn compact(
    prep: &CompactPreparation,
    options: &CompactOptions,
) -> Result<CompactResult, String> {
    compact_reusing(prep, options, None)
}

/// Reuse only pages whose complete renderer input (including dim state) and
/// geometry are unchanged. Changed partial pages and foveated tails rebuild.
pub fn compact_reusing(
    prep: &CompactPreparation,
    options: &CompactOptions,
    previous: Option<FrameCache<'_>>,
) -> Result<CompactResult, String> {
    let mut shape: Shape = options
        .shape
        .clone()
        .unwrap_or_else(|| resolve_shape_for(&options.model_id, &options.provider));
    if let Some(frame_size) = options.frame_size {
        shape.frame_size = frame_size;
    }
    if shape.columns != 1 {
        return Err("snapcompact multi-column pagination is not implemented".into());
    }
    if prep.bounded_text.is_empty() {
        return Err("snapcompact source is empty".into());
    }
    if !prep.remaining_text.is_empty() {
        return Err("snapcompact frame budget cannot represent all source text".into());
    }
    let max_frames = options
        .max_frames
        .clamp(1, MAX_FRAMES_DEFAULT)
        .min(frame_budget(&options.provider)) as usize;
    let render_key = format!("v1:{shape:?}");
    let previous = previous.filter(|previous| {
        let chars = previous.source.chars().count();
        previous.render_key == render_key
            && !previous.frames.is_empty()
            && previous
                .frames
                .first()
                .is_some_and(|frame| frame.source_start == 0)
            && previous
                .frames
                .last()
                .is_some_and(|frame| frame.source_end == chars)
            && previous.frames.iter().all(|frame| {
                frame.source_start < frame.source_end
                    && frame.source_end <= chars
                    && !frame.bytes.is_empty()
            })
            && previous
                .frames
                .windows(2)
                .all(|pair| pair[0].source_end <= pair[1].source_start)
    });
    let plan = plan_frames(&prep.bounded_text, &shape, max_frames, previous.as_ref());
    if plan.ranges.is_empty() {
        return Err("snapcompact frame budget cannot represent all source text".into());
    }
    let render_opts = shape_to_render_options(&shape);
    let mut cached = std::collections::HashMap::new();
    if let Some(previous) = previous {
        let boundaries: Vec<_> = previous
            .source
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(previous.source.len()))
            .collect();
        let valid = previous.frames.iter().all(|frame| {
            frame.source_start < frame.source_end
                && frame.source_end < boundaries.len()
                && !frame.bytes.is_empty()
        }) && previous
            .frames
            .windows(2)
            .all(|pair| pair[0].source_end <= pair[1].source_start);
        if valid {
            let mut dim = false;
            for frame in previous.frames {
                let start = boundaries[frame.source_start];
                let end = boundaries[frame.source_end];
                let text = finish_frame_text(&previous.source[start..end], &mut dim);
                cached.insert(text, frame.bytes.as_slice());
            }
        }
    }
    let boundaries: Vec<_> = prep
        .bounded_text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(prep.bounded_text.len()))
        .collect();
    let mut dim_open = false;
    let mut pages: Vec<_> = plan
        .ranges
        .iter()
        .map(|range| {
            let slice =
                &prep.bounded_text[boundaries[range.source_start]..boundaries[range.source_end]];
            let text = finish_frame_text(slice, &mut dim_open);
            let cached = cached.get(&text);
            (
                FrameRef {
                    index: range.index,
                    source_start: range.source_start,
                    source_end: range.source_end,
                    bytes: cached.map_or_else(Vec::new, |bytes| bytes.to_vec()),
                },
                cached.is_none().then_some(text),
            )
        })
        .collect();
    let rendered_frames = pages.iter().filter(|(_, text)| text.is_some()).count();
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(8)
        .min(rendered_frames.max(1));
    let render_timings = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for batch in pages.chunks_mut(plan.ranges.len().div_ceil(workers)) {
            let options = render_opts.clone();
            handles.push(scope.spawn(move || {
                let mut timings = renderer::snapcompact_render::RenderTimings::default();
                for (frame, text) in batch {
                    if let Some(text) = text.take() {
                        let (bytes, timing) =
                            renderer::snapcompact_render::render_snapcompact_png_measured(
                                text,
                                options.clone(),
                            )
                            .map_err(|error| {
                                format!("snapcompact frame {}: {error}", frame.index)
                            })?;
                        frame.bytes = bytes;
                        timings.font += timing.font;
                        timings.raster += timing.raster;
                        timings.encode += timing.encode;
                    }
                }
                Ok::<_, String>(timings)
            }));
        }
        handles.into_iter().try_fold(
            renderer::snapcompact_render::RenderTimings::default(),
            |mut all, handle| {
                let timing = handle
                    .join()
                    .map_err(|_| "snapcompact render worker panicked".to_string())??;
                all.font += timing.font;
                all.raster += timing.raster;
                all.encode += timing.encode;
                Ok::<_, String>(all)
            },
        )
    })?;
    let mut rendered: Vec<_> = pages.into_iter().map(|(frame, _)| frame).collect();
    let truncated_chars = plan
        .elided
        .map(|(start, end)| end.saturating_sub(start))
        .unwrap_or(0);
    let (source_text, kept_ranges) = render_kept_source(&prep.bounded_text, &plan, &boundaries);
    for (frame, kept) in rendered.iter_mut().zip(kept_ranges) {
        frame.source_start = kept.source_start;
        frame.source_end = kept.source_end;
    }
    let total_chars = rendered
        .iter()
        .map(|frame| frame.source_end.saturating_sub(frame.source_start))
        .sum::<usize>();
    let summary = render_summary(&shape, rendered.len(), truncated_chars, prep);
    Ok(CompactResult {
        summary,
        source_text,
        rendered_frames,
        render_timings,
        frames: rendered,
        shape,
        truncated_chars,
        total_chars,
    })
}

/// Archive source text a reader without image input can still use: the frames'
/// slices in chronological order, with the dropped middle replaced by a marker.
fn render_kept_source(
    text: &str,
    plan: &FramePlan,
    boundaries: &[usize],
) -> (String, Vec<FrameRange>) {
    let mut out = String::with_capacity(text.len());
    let mut ranges = Vec::with_capacity(plan.ranges.len());
    let mut cursor = 0;
    let mut previous_end = 0;
    for (position, range) in plan.ranges.iter().enumerate() {
        if position == plan.head_count
            && let Some((start, end)) = plan.elided
        {
            let marker = format!(
                "{NEWLINE_GLYPH}[snapcompact: {} chars elided from the middle of the archive]{NEWLINE_GLYPH}",
                end.saturating_sub(start)
            );
            cursor += marker.chars().count();
            out.push_str(&marker);
        } else if previous_end < range.source_start {
            // An older foveation marker is not part of any rendered page.
            let gap = &text[boundaries[previous_end]..boundaries[range.source_start]];
            cursor += range.source_start - previous_end;
            out.push_str(gap);
        }
        out.push_str(&text[boundaries[range.source_start]..boundaries[range.source_end]]);
        let end = cursor + range.source_end - range.source_start;
        ranges.push(FrameRange {
            index: range.index,
            source_start: cursor,
            source_end: end,
        });
        cursor = end;
        previous_end = range.source_end;
    }
    (out, ranges)
}

/// Reading guide rendered as the compaction summary.
fn render_summary(
    shape: &Shape,
    frame_count: usize,
    truncated_chars: usize,
    prep: &CompactPreparation,
) -> String {
    let cols = shape.frame_size / shape.cell_width.max(1);
    let rows = shape.rows_per_frame();
    let mut out = String::from(
        "Resume prior conversation. Earlier turns archived under HISTORY below, oldest→newest. Read HISTORY fully; continue the live conversation following it.

Archived transcript scopes:
- `¶user:`, `¶think:`, `¶ai:`, `¶call:`: user, assistant reasoning, assistant reply, tool call.
- Unprefixed following lines: current scope. Consecutive same-kind blocks omit repeated prefix.
- Tool call: `¶call:name(args)//intent`; trailing `//intent` optional. `<out>…</out>`: tool output.

Reading HISTORY:
- Plain text: verbatim transcript; rely on it exactly.
",
    );
    if frame_count > 0 {
        out.push_str(&format!(
            "- Some middle sections: images, not text. Each image: one page of that transcript, in reading order between marked delimiters. Solid black cell: newline; runs of spaces collapse to one.
  - Frame: one grid {cols} characters wide, up to {rows} rows tall; read left→right, top→bottom. No word wrap; words may break across rows.
"
        ));
        if shape.variant == Variant::Sent {
            out.push_str(
                "  - Ink: six colors, one per sentence.
",
            );
        }
        if shape.stopword_dim {
            out.push_str(
                "  - Function words: dim gray; content words: full ink.
",
            );
        }
        if shape.line_repeat > 1 {
            out.push_str(
                "  - Each line printed twice (white, then pale-yellow band); copies identical.
",
            );
        }
    }
    if prep.included_previous_summary {
        out.push_str(
            "- HISTORY opens with a condensed digest of still-older context predating archived turns.
",
        );
    }
    if truncated_chars > 0 {
        out.push_str(&format!(
            "- About {truncated_chars} characters of older middle history dropped to fit archive budget.
"
        ));
    }
    out.push_str("- If an exact earlier detail matters and a section is unclear, re-derive from workspace (re-read files, re-run commands), rather than guess.

HISTORY
===================
");
    out
}

fn finish_frame_text(text: &str, dim_open: &mut bool) -> String {
    let mut output = String::with_capacity(text.len() + usize::from(*dim_open));
    if *dim_open {
        output.push('\u{e}');
    }
    for ch in text.chars() {
        match ch {
            '\u{e}' => *dim_open = true,
            '\u{f}' => *dim_open = false,
            _ => {}
        }
        output.push(ch);
    }
    output
}

/// Infer legacy page ranges only when the persisted grid and page count agree.
/// Legacy archives do not persist geometry, so ambiguous or truncated layouts
/// intentionally return no ranges rather than guessing page numbers.
pub fn legacy_page_ranges(text: &str, summary: &str, page_count: usize) -> Vec<FrameRange> {
    if text.contains("[snapcompact:") {
        return Vec::new();
    }
    // The historical guide does not identify the font. If Silver and bitmap
    // cell widths disagree we cannot prove the old layout from the guide.
    if text.chars().any(|ch| {
        renderer::snapcompact_render::cell_units(ch as u32, true)
            != renderer::snapcompact_render::cell_units(ch as u32, false)
    }) {
        return Vec::new();
    }
    let Some(grid) = summary.split("one grid ").nth(1) else {
        return Vec::new();
    };
    let Some((columns, rest)) = grid.split_once(" characters wide, up to ") else {
        return Vec::new();
    };
    let Some(rows) = rest.split_whitespace().next() else {
        return Vec::new();
    };
    let (Ok(columns), Ok(rows)) = (columns.parse::<usize>(), rows.parse::<usize>()) else {
        return Vec::new();
    };
    if columns == 0 || rows == 0 {
        return Vec::new();
    }
    let Some(capacity) = columns.checked_mul(rows) else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    let (mut start, mut end, mut cursor) = (0, 0, 0);
    for ch in text.chars() {
        let width = renderer::snapcompact_render::cell_units(ch as u32, true);
        let pad = usize::from(width == 2 && columns >= 2 && cursor % columns == columns - 1);
        if cursor + pad + width > capacity {
            ranges.push(FrameRange {
                index: ranges.len() as u32,
                source_start: start,
                source_end: end,
            });
            start = end;
            cursor = 0;
        }
        cursor +=
            usize::from(width == 2 && columns >= 2 && cursor % columns == columns - 1) + width;
        end += 1;
    }
    if end > start {
        ranges.push(FrameRange {
            index: ranges.len() as u32,
            source_start: start,
            source_end: end,
        });
    }
    if ranges.len() == page_count {
        ranges
    } else {
        Vec::new()
    }
}

fn chunk_into_frames(text: &str, shape: &Shape, max_frames: usize) -> Vec<FrameRange> {
    let cols = (shape.frame_size / shape.cell_width.max(1)) as usize;
    let capacity = shape.chars_per_frame() as usize;
    let mut out = Vec::new();
    if cols == 0 || capacity == 0 || max_frames == 0 {
        return out;
    }
    let mut start = 0usize;
    let mut cursor = 0usize;
    let mut end = 0usize;
    for ch in text.chars() {
        let width = renderer::snapcompact_render::cell_units(ch as u32, shape.font != Font::Silver);
        let pad = usize::from(width == 2 && cols >= 2 && cursor % cols == cols - 1);
        if cursor + pad + width > capacity {
            if end == start || out.len() + 1 >= max_frames {
                break;
            }
            out.push(FrameRange {
                index: out.len() as u32,
                source_start: start,
                source_end: end,
            });
            start = end;
            cursor = 0;
        }
        cursor += usize::from(width == 2 && cols >= 2 && cursor % cols == cols - 1) + width;
        end += 1;
    }
    if end > start && out.len() < max_frames {
        out.push(FrameRange {
            index: out.len() as u32,
            source_start: start,
            source_end: end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires JCODE_SNAPCOMPACT_FIXTURE copied session"]
    fn profile_copied_archive_render() {
        let path = std::env::var("JCODE_SNAPCOMPACT_FIXTURE").unwrap();
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let source = fixture["compaction"]["snapcompact"]["source"]
            .as_str()
            .unwrap();
        let prep = super::CompactPreparation {
            text: source.into(),
            bounded_text: source.into(),
            remaining_text: String::new(),
            included_previous_summary: false,
        };
        let now = std::time::Instant::now();
        let result = super::compact(
            &prep,
            &super::CompactOptions {
                model_id: "claude-opus-5-5".into(),
                ..Default::default()
            },
        )
        .unwrap();
        eprintln!(
            "RENDER_PROFILE wall_ms={} font_ms={} raster_ms={} encode_ms={} pages={} png_bytes={}",
            now.elapsed().as_millis(),
            result.render_timings.font.as_millis(),
            result.render_timings.raster.as_millis(),
            result.render_timings.encode.as_millis(),
            result.frames.len(),
            result.frames.iter().map(|f| f.bytes.len()).sum::<usize>()
        );
    }

    #[test]
    fn foveated_archive_keeps_prior_page_boundaries_when_appending() {
        let mut shape = super::resolve_shape("claude");
        shape.frame_size = 64;
        let options = super::CompactOptions {
            shape: Some(shape),
            max_frames: 8,
            ..Default::default()
        };
        let first = super::compact(
            &super::prepare(&"abcdefghij".repeat(50), 1000, 1000),
            &options,
        )
        .unwrap();
        assert!(first.truncated_chars > 0);
        let preparation = super::prepare_messages(
            &[jcode_message_types::Message::user("tail")],
            Some(&first.source_text),
            usize::MAX,
        )
        .unwrap();
        let second =
            super::compact_reusing(&preparation, &options, Some(first.frame_cache())).unwrap();
        assert!(
            second.rendered_frames <= 2,
            "preserve old foveated page boundaries, rendered {} pages",
            second.rendered_frames
        );
        assert_eq!(second.frames[0].bytes, first.frames[0].bytes);
        assert_eq!(second.frames[1].bytes, first.frames[1].bytes);
        for frame in &second.frames {
            assert!(
                frame.source_start < frame.source_end
                    && frame.source_end <= second.source_text.chars().count()
            );
        }
    }

    #[test]
    fn second_compaction_renders_only_changed_tail_pages() {
        let mut shape = super::resolve_shape("claude");
        shape.frame_size = 64;
        let options = super::CompactOptions {
            shape: Some(shape),
            ..Default::default()
        };
        let first =
            super::compact(&super::prepare(&"a".repeat(130), 1000, 1000), &options).unwrap();
        assert!(first.frames.len() > 2);
        let preparation = super::prepare_messages(
            &[jcode_message_types::Message::user("new tail")],
            Some(&first.source_text),
            usize::MAX,
        )
        .unwrap();
        let second =
            super::compact_reusing(&preparation, &options, Some(first.frame_cache())).unwrap();
        assert_eq!(
            second.rendered_frames, 2,
            "only the partial last page and appended page need rendering"
        );
        let rebuilt = super::compact(&preparation, &options).unwrap();
        for (reused, fresh) in second.frames.iter().zip(&rebuilt.frames) {
            assert_eq!(reused.bytes, fresh.bytes);
            assert_eq!(reused.source_start, fresh.source_start);
            assert_eq!(reused.source_end, fresh.source_end);
        }
    }

    #[test]
    fn normalize_collapses_whitespace_and_folds_box_drawing() {
        assert_eq!(
            super::normalize(" \talpha  \n \tbeta │─┌\r\ngamma  "),
            "alpha█beta |-+█gamma"
        );
    }

    #[test]
    fn frame_boundary_restores_dim_state() {
        let mut open = false;
        assert_eq!(
            super::finish_frame_text("a\u{e}dim", &mut open),
            "a\u{e}dim"
        );
        assert!(open);
        assert_eq!(
            super::finish_frame_text("continued\u{f}b", &mut open),
            "\u{e}continued\u{f}b"
        );
        assert!(!open);
    }

    #[test]
    fn legacy_grid_page_ranges_are_checked_against_count() {
        let guide = "Frame: one grid 4 characters wide, up to 2 rows tall";
        let ranges = super::legacy_page_ranges("abcdefghijkl", guide, 2);
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].source_start, ranges[0].source_end), (0, 8));
        assert_eq!((ranges[1].source_start, ranges[1].source_end), (8, 12));
        assert!(super::legacy_page_ranges("abcdefghijkl", guide, 3).is_empty());
        assert!(super::legacy_page_ranges("[snapcompact: 10 chars elided]", guide, 2).is_empty());
    }

    #[test]
    fn prepare_messages_preserves_previous_archive_and_normalizes_turns() {
        let messages = [jcode_message_types::Message::user("new\nturn")];
        let prepared = super::prepare_messages(&messages, Some("old archive"), 100).unwrap();
        assert_eq!(prepared.text, "old archive█¶user:new█turn");
        assert_eq!(prepared.bounded_text, prepared.text);
        assert!(prepared.remaining_text.is_empty());
    }

    #[test]
    fn identical_input_produces_identical_png_bytes() {
        let prep = super::prepare("User: A durable early detail\nAssistant: Noted", 2000, 2000);
        let options = super::CompactOptions::default();
        let first = super::compact(&prep, &options).unwrap();
        let second = super::compact(&prep, &options).unwrap();
        assert_eq!(first.frames.len(), second.frames.len());
        assert!(!first.frames.is_empty());
        for (left, right) in first.frames.iter().zip(&second.frames) {
            assert_eq!(left.bytes, right.bytes);
        }
    }

    #[test]
    fn shape_capacity_matches_grid_geometry() {
        let shape = super::resolve_shape("claude");
        assert_eq!(shape.chars_per_frame(), (1568 / 11) * (1568 / 16));
        let repeat = super::SHAPES.iter().find(|s| s.name == "8x8r-bw").unwrap();
        assert_eq!(repeat.chars_per_frame(), (1568 / 8) * (1568 / 8 / 2));
    }

    #[test]
    fn pagination_counts_wide_cells_and_row_padding() {
        let mut shape = super::resolve_shape("claude");
        shape.frame_size = 32;
        shape.cell_width = 8;
        shape.cell_height = 8;
        let text = "abc界".repeat(4);
        let ranges = super::chunk_into_frames(&text, &shape, 10);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].source_end, 12);
        assert_eq!(ranges[1].source_end, 16);
    }

    /// A long archive drops the middle instead of failing the pass:
    /// a history too long for the frame budget still compacts, keeping both
    /// chronological ends.
    #[test]
    fn overflowing_archive_keeps_both_ends_instead_of_failing() {
        let head = "HEAD";
        let tail = "TAIL";
        let prep = super::prepare(
            &format!("{head}{}{tail}", "y".repeat(20_000)),
            10_000,
            40_000,
        );
        let mut shape = super::resolve_shape("claude");
        shape.frame_size = 32;
        let options = super::CompactOptions {
            shape: Some(shape),
            max_frames: 4,
            ..Default::default()
        };
        let result = super::compact(&prep, &options).expect("keeps both ends");
        assert_eq!(result.frames.len(), 4);
        assert!(result.truncated_chars > 0);
        assert!(result.summary.contains("dropped to fit archive budget"));
        assert!(
            result
                .source_text
                .contains("chars elided from the middle of the archive"),
            "{}",
            result.source_text
        );
        assert!(
            result.source_text.starts_with("HEAD"),
            "{}",
            result.source_text
        );
        // `prepare` keeps the serialized turn's trailing newline.
        assert!(
            result.source_text.trim_end().ends_with("TAIL"),
            "{}",
            result.source_text
        );
    }

    /// The summary is a reading guide for the frames, not a copy of the
    /// archive text.
    #[test]
    fn summary_is_a_reading_guide() {
        let prep = super::prepare(
            "User: durable detail
Assistant: noted",
            2000,
            2000,
        );
        let result = super::compact(
            &prep,
            &super::CompactOptions {
                model_id: "claude-opus-5-5".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(result.summary.starts_with("Resume prior conversation."));
        assert!(result.summary.contains("¶user:"));
        assert!(result.summary.contains("Frame: one grid"));
        assert!(result.summary.ends_with(
            "HISTORY
===================\n"
        ));
        assert!(!result.summary.contains("durable detail"));
        assert_eq!(result.frames.len(), 1);
    }

    /// Model rules for shape selection, including the
    /// high-res frame sizes.
    #[test]
    fn shape_resolution_matches_model_rules() {
        let case = |model: &str| {
            let shape = super::resolve_shape(model);
            (shape.name, shape.frame_size)
        };
        assert_eq!(case("claude-opus-5-5"), ("11on16-bw", 1932));
        assert_eq!(case("claude-opus-4-6"), ("11on16-bw", 1568));
        assert_eq!(case("claude-sonnet-4-5"), ("11on16-bw", 1568));
        assert_eq!(case("claude-fable-latest"), ("11on16-bw", 1932));
        assert_eq!(case("gemini-3.5-flash"), ("8on22-bw", 2048));
        assert_eq!(case("gpt-5.5"), ("8on22-bw", 1568));
        assert_eq!(case("gpt-6.1-codex"), ("8on22-bw", 1568));
        assert_eq!(case("kimi-k3"), ("8on22-bw", 1568));
        assert_eq!(case("glm-4.6v"), ("8on16-bw", 1568));
        // Unmeasured reader: the family winner, not the Claude shape.
        assert_eq!(case("mystery-model-9"), ("8on22-bw", 1568));
    }

    /// The archive never exceeds the provider's
    /// per-request image budget.
    #[test]
    fn frame_budget_matches_provider_image_limits() {
        assert_eq!(super::frame_budget("anthropic"), 80);
        assert_eq!(super::frame_budget("openai"), 80);
        assert_eq!(super::frame_budget("gemini"), 80);
        assert_eq!(super::frame_budget("xai"), 60);
        assert_eq!(super::frame_budget("cursor"), 60);
        assert_eq!(super::frame_budget("google-antigravity"), 20);
        assert_eq!(super::frame_budget("umans"), 10);
        assert_eq!(super::frame_budget("unknown-gateway"), 80);
    }

    /// A caller's `max_frames` is capped at the provider budget, never above it.
    #[test]
    fn compact_never_exceeds_the_provider_frame_budget() {
        let prep = super::prepare(&"z".repeat(400_000), 10_000, 500_000);
        let mut shape = super::resolve_shape("claude-sonnet-4-5");
        shape.frame_size = 32;
        let result = super::compact(
            &prep,
            &super::CompactOptions {
                shape: Some(shape),
                provider: "umans".into(),
                max_frames: 80,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.frames.len(), 10);
    }

    /// A folded-in text summary is labelled and reported.
    #[test]
    fn previous_summary_is_labelled_and_reported() {
        let messages = [jcode_message_types::Message::user("fresh turn")];
        let prepared = super::prepare_messages_with(
            &messages,
            Some(super::PreviousArchive {
                text: "older digest",
                is_summary: true,
            }),
            100,
        )
        .unwrap();
        assert!(prepared.included_previous_summary);
        assert!(
            prepared
                .text
                .starts_with("[Summary of earlier history] older digest")
        );
        assert!(prepared.text.contains("[Recent conversation]"));
    }

    #[test]
    fn compact_rejects_invalid_render_options() {
        let prep = super::prepare("hello", 1000, 1000);
        let mut shape = super::resolve_shape("claude");
        shape.columns = 3;
        let options = super::CompactOptions {
            shape: Some(shape),
            ..Default::default()
        };
        assert!(super::compact(&prep, &options).is_err());
    }
    use super::*;

    #[test]
    fn shape_table_includes_all_variants() {
        for name in &[
            "8x8r-bw",
            "8x8r-sent",
            "8x8u-bw",
            "8x8u-sent",
            "6x6u-bw",
            "6x6u-sent",
            "5x8-bw",
            "5x8-sent",
            "6x12-dim",
            "8x13-bw",
            "8on16-bw",
            "8on22-bw",
            "11on16-bw",
            "silver16-bw",
            "doc-8on16-bw",
            "doc-8on16-sent",
            "doc-8on16-sent-dim",
        ] {
            assert!(
                lookup_owned(name).is_some(),
                "missing shape `{name}` from registry"
            );
        }
    }

    #[test]
    fn shape_name_round_trip() {
        for s in SHAPES {
            assert_eq!(lookup_owned(s.name).map(|x| x.name), Some(s.name));
        }
    }

    #[test]
    fn resolve_shape_anthropic_picks_11on16() {
        let s = resolve_shape("claude-3-5-sonnet-20241022");
        assert_eq!(s.name, "11on16-bw");
    }

    #[test]
    fn resolve_shape_openai_picks_8on22() {
        let s = resolve_shape("gpt-5.5");
        assert_eq!(s.name, "8on22-bw");
    }

    #[test]
    fn resolve_shape_google_picks_8on22() {
        let s = resolve_shape("gemini-3-flash");
        assert_eq!(s.name, "8on22-bw");
    }

    #[test]
    fn resolve_shape_unknown_model_uses_the_wire_family_winner() {
        // An unmeasured model id takes the family winner,
        // which is the OpenAI/Google shape for an unknown API.
        assert_eq!(resolve_shape("unknown-provider-model-xyz").name, "8on22-bw");
        assert_eq!(
            resolve_shape_for("unknown-provider-model-xyz", "anthropic").name,
            "11on16-bw"
        );
    }

    #[test]
    fn chars_per_frame_scales_with_frame_size_and_cell() {
        // 11on16-bw: 1568px / 11 cell_width.
        let s = lookup_owned("11on16-bw").unwrap();
        assert_eq!(s.chars_per_frame(), (1568 / 11) * (1568 / 16));
    }

    #[test]
    fn normalize_strips_ansi() {
        let input = "\u{1b}[31mhello\u{1b}[0m world";
        let out = normalize(input);
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn normalize_folds_newlines_to_full_block() {
        let input = "line1\n\n\nline2";
        let out = normalize(input);
        assert!(out.contains('\u{2588}'));
        assert!(!out.contains("\n\n\n"));
    }

    #[test]
    fn normalize_replaces_box_drawing_with_ascii() {
        let input = "┌──┐\n│hi│\n└──┘";
        let out = normalize(input);
        assert!(!out.contains('┌'));
        assert!(out.contains('-'));
    }

    #[test]
    fn normalize_collapses_whitespace_runs() {
        let input = "a    b\t\tc";
        let out = normalize(input);
        assert!(!out.contains("    "));
        assert!(!out.contains('\t'));
        assert_eq!(out, "a b c");
    }

    #[test]
    fn prepare_respects_text_limit() {
        let long = "x".repeat(10_000);
        let prep = prepare(&long, 2000, 200);
        assert!(prep.bounded_text.chars().count() <= 200);
        assert!(prep.remaining_text.chars().count() >= 10_000 - 200);
    }

    #[test]
    fn prepare_short_input_has_no_remaining() {
        let prep = prepare("hello world", 2000, 200);
        assert_eq!(prep.bounded_text, "hello world\n");
        assert!(prep.remaining_text.is_empty());
    }

    #[test]
    fn serialize_conversation_truncates_tool_results() {
        let input = "Tool: a very long output that goes on and on and on";
        let text = serialize_conversation(input, 20);
        assert!(text.contains("[truncated]"));
        assert!(text.chars().count() < 60);
    }

    #[test]
    fn compact_emits_one_frame_per_chunk_with_png_bytes() {
        // Pre-port this asserted `f.bytes.is_empty()` (the old
        // NoopRenderer path). compact() now renders real PNGs, so
        // each frame should carry a non-empty payload with the
        // PNG magic header. The `frames.len() <= max_frames`
        // invariant is preserved.
        let long: String = "a".repeat(800);
        let prep = prepare(&long, 2000, 801);
        let opts = CompactOptions {
            model_id: "claude-3-5-sonnet".into(),
            ..Default::default()
        };
        let result = compact(&prep, &opts).unwrap();
        assert!(!result.frames.is_empty());
        assert!(result.frames.len() <= opts.max_frames as usize);
        for f in &result.frames {
            assert!(
                !f.bytes.is_empty(),
                "frame {} should have real PNG bytes",
                f.index
            );
            assert_eq!(
                &f.bytes[..8],
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "frame {} missing PNG magic",
                f.index
            );
        }
    }

    #[test]
    fn compact_summary_includes_lead_in_and_head() {
        let text = "User: hello\nAssistant: hi";
        let prep = prepare(text, 2000, 2000);
        let opts = CompactOptions {
            model_id: "claude".into(),
            ..Default::default()
        };
        let result = compact(&prep, &opts).unwrap();
        assert!(result.summary.starts_with("Resume prior conversation."));
    }
    #[test]
    fn compact_frame_ranges_are_disjoint_and_cover_source() {
        // Each frame holds `chars_per_frame` characters; with max_frames=5
        // and an unbounded text the frames cover exactly the first
        // `max_frames * chars_per_frame` characters.
        let long: String = (0..50).map(|i| format!("x{i}\n")).collect();
        let prep = prepare(&long, 5000, 5000);
        let opts = CompactOptions {
            model_id: "claude".into(),
            max_frames: 5,
            ..Default::default()
        };
        let result = compact(&prep, &opts).unwrap();
        assert!(!result.frames.is_empty());
        // Frame 0 starts at 0; frames are contiguous and disjoint.
        let mut prev_end = 0;
        for (i, f) in result.frames.iter().enumerate() {
            assert_eq!(f.index, i as u32);
            assert_eq!(f.source_start, prev_end);
            assert!(f.source_end > f.source_start);
            prev_end = f.source_end;
        }
        // The frames cover the first `max_frames * chars_per_frame`
        // characters of the bounded text — less than the full length
        // when `max_frames * chars_per_frame` is small.
        let chars_per_frame = result.shape.chars_per_frame() as usize;
        assert_eq!(prev_end, prep.bounded_text.chars().count());
        assert!(prev_end <= (opts.max_frames as usize) * chars_per_frame);
    }

    #[test]
    fn bounded_source_drop_is_lossless() {
        let text = "x".repeat(2000);
        let prep = prepare(&text, 5000, 2001);
        let opts = CompactOptions {
            model_id: "claude".into(),
            ..Default::default()
        };
        let result = compact(&prep, &opts).unwrap();
        let bounded = result.into_bounded_source();
        assert_eq!(bounded.text, prep.bounded_text);
    }

    #[test]
    fn compact_renders_real_png_bytes_per_frame() {
        // The compact() path used to go through a NoopRenderer that
        // emitted empty bytes; this guards against regressions to
        // that fake-success path by asserting each frame carries a
        // non-empty PNG payload.
        let long: String = "a".repeat(500);
        let prep = prepare(&long, 2000, 501);
        let opts = CompactOptions {
            model_id: "claude".into(),
            max_frames: 5,
            ..Default::default()
        };
        let result = compact(&prep, &opts).unwrap();
        assert!(!result.frames.is_empty(), "compact must produce ≥1 frame");
        for f in &result.frames {
            assert!(!f.bytes.is_empty(), "frame {} has empty bytes", f.index);
            // PNG magic header.
            assert_eq!(
                &f.bytes[..8],
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "frame {} is not a PNG",
                f.index
            );
        }
    }
}
