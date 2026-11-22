#![cfg_attr(test, allow(clippy::items_after_test_module))]

use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, FileOp, FileTouch};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_LIMIT: usize = 5000;
const MAX_LINE_LEN: usize = 2000;
const MAX_TEXT_OUTPUT: usize = 256 * 1024;
const EXACT_LINE_COUNT_MAX_BYTES: u64 = 1024 * 1024;

#[cfg(test)]
fn test_gates() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<tokio::sync::Barrier>>,
> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Barrier>>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn set_test_read_gate(
    session: &str,
    gate: Option<std::sync::Arc<tokio::sync::Barrier>>,
) {
    let mut gates = test_gates().lock().unwrap();
    if let Some(gate) = gate {
        gates.insert(session.to_string(), gate);
    } else {
        gates.remove(session);
    }
}

pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct ReadInput {
    file_path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadRangeStyle {
    OffsetLimit,
    StartEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NormalizedReadRange {
    offset: usize,
    limit: usize,
    style: ReadRangeStyle,
}

impl NormalizedReadRange {
    fn next_offset(self) -> usize {
        self.offset.saturating_add(self.limit)
    }
}

struct TextWindow {
    more_unscanned: bool,
    output: String,
    total_lines: usize,
    truncated_lines: usize,
    next_line: Option<usize>,
}

fn read_text_window(
    mut reader: impl std::io::Read,
    range: NormalizedReadRange,
    stop_at_window_end: bool,
) -> Result<TextWindow> {
    use std::fmt::Write;
    let mut output = String::new();
    let mut total_lines = 0usize;
    let mut truncated_lines = 0usize;
    let mut next_line = None;
    let mut prefix = Vec::with_capacity(MAX_LINE_LEN + 4);
    let mut line_len = 0usize;
    let mut last = 0u8;
    let mut buffer = [0u8; 8196];
    let mut pending = 0usize;
    let mut more_unscanned = false;
    let mut remaining = range.next_offset();
    let mut finish_line = |terminated: bool, prefix: &mut Vec<u8>, line_len: usize, last: u8| {
        let index = total_lines;
        total_lines += 1;
        if index >= range.offset && index < range.next_offset() {
            let len = line_len - usize::from(terminated && last == b'\r');
            if next_line.is_some()
                || output.len().saturating_add(len.min(MAX_LINE_LEN) + 32) > MAX_TEXT_OUTPUT
            {
                next_line.get_or_insert(index);
                prefix.clear();
                return;
            }
            let shown = len.min(MAX_LINE_LEN);
            let mut end = shown.min(prefix.len());
            while std::str::from_utf8(&prefix[..end]).is_err() {
                end -= 1;
            }
            let text = std::str::from_utf8(&prefix[..end]).expect("valid prefix");
            if len > MAX_LINE_LEN {
                truncated_lines += 1;
                let _ = writeln!(output, "{:>5}\t{}...", index + 1, text);
            } else {
                let _ = writeln!(output, "{:>5}\t{}", index + 1, text);
            }
        }
        prefix.clear();
    };
    loop {
        let n = reader.read(&mut buffer[pending..])?;
        if n == 0 {
            if pending != 0 {
                anyhow::bail!("stream did not contain valid UTF-8");
            }
            break;
        }
        let mut available = pending + n;
        let mut at_window_end = false;
        if stop_at_window_end && remaining != 0 {
            for (i, &b) in buffer[..available].iter().enumerate() {
                if b == b'\n' {
                    remaining -= 1;
                    if remaining == 0 {
                        let end = i + 1;
                        more_unscanned = end < available || reader.read(&mut [0u8; 1])? != 0;
                        available = end;
                        at_window_end = true;
                        break;
                    }
                }
            }
        }
        let valid = match std::str::from_utf8(&buffer[..available]) {
            Ok(_) => available,
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => anyhow::bail!("stream did not contain valid UTF-8"),
        };
        for segment in buffer[..valid].split_inclusive(|b| *b == b'\n') {
            let data = segment.strip_suffix(b"\n").unwrap_or(segment);
            let keep = data
                .len()
                .min((MAX_LINE_LEN + 4).saturating_sub(prefix.len()));
            prefix.extend_from_slice(&data[..keep]);
            line_len = line_len.saturating_add(data.len());
            if let Some(b) = data.last() {
                last = *b;
            }
            if segment.len() != data.len() {
                finish_line(true, &mut prefix, line_len, last);
                line_len = 0;
                last = 0;
            }
        }
        if at_window_end {
            break;
        }
        pending = available - valid;
        buffer.copy_within(valid..available, 0);
    }
    if line_len != 0 {
        finish_line(false, &mut prefix, line_len, last);
    }
    Ok(TextWindow {
        more_unscanned,
        output,
        total_lines,
        truncated_lines,
        next_line,
    })
}

fn normalize_read_range(params: &ReadInput) -> Result<NormalizedReadRange> {
    let has_start_end = params.start_line.is_some() || params.end_line.is_some();
    let has_mixed_offset = match (params.start_line, params.end_line, params.offset) {
        (Some(start_line), _, Some(offset)) => {
            if start_line == 0 {
                true
            } else {
                offset.checked_add(1) != Some(start_line)
            }
        }
        (None, Some(_), Some(offset)) => offset != 0,
        _ => params.offset.is_some(),
    };

    if has_start_end && has_mixed_offset {
        return Err(anyhow::anyhow!(
            "Use either start_line/end_line (1-based) or offset (0-based), not both. `limit` may be used with either style."
        ));
    }

    if has_start_end {
        let start_line = params.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(anyhow::anyhow!(
                "start_line must be 1 or greater (it is 1-based)."
            ));
        }

        let limit = if let Some(end_line) = params.end_line {
            if end_line == 0 {
                return Err(anyhow::anyhow!(
                    "end_line must be 1 or greater (it is 1-based)."
                ));
            }
            if end_line < start_line {
                return Err(anyhow::anyhow!(
                    "end_line ({}) must be greater than or equal to start_line ({}).",
                    end_line,
                    start_line
                ));
            }
            end_line - start_line + 1
        } else {
            params.limit.unwrap_or(DEFAULT_LIMIT)
        };

        return Ok(NormalizedReadRange {
            offset: start_line - 1,
            limit,
            style: ReadRangeStyle::StartEnd,
        });
    }

    Ok(NormalizedReadRange {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(DEFAULT_LIMIT),
        style: ReadRangeStyle::OffsetLimit,
    })
}

#[async_trait]
impl Tool for ReadTool {
    fn execution_class(&self, input: &Value, ctx: &ToolContext) -> jcode_tool_core::ExecutionClass {
        use jcode_tool_core::ExecutionClass::{Barrier, ParallelRead};
        let Some(object) = input.as_object() else {
            return Barrier;
        };
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "file_path"
                    | "intent"
                    | "start_line"
                    | "end_line"
                    | "offset"
                    | "limit"
                    | "accept_large_output"
            )
        }) {
            return Barrier;
        }
        let Ok(params) = serde_json::from_value::<ReadInput>(input.clone()) else {
            return Barrier;
        };
        if normalize_read_range(&params).is_err() {
            return Barrier;
        }
        let Some(file_path) = object.get("file_path").and_then(Value::as_str) else {
            return Barrier;
        };
        if file_path.contains("://") {
            return Barrier;
        }
        let path = ctx.resolve_path(Path::new(file_path));
        if is_image_file(&path)
            || is_pdf_file(&path)
            || !std::fs::metadata(&path).is_ok_and(|m| m.is_file())
        {
            return Barrier;
        }
        ParallelRead
    }

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read text, images or PDFs, including session-image://<image_id> or session-image://N for Image #N."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {
                "intent": super::intent_schema_property(),
                "file_path": {
                    "type": "string",
                    "description": "Path to a file, or session-image://<image_id> or session-image://N for Image #N in this session."
                },
                "start_line": {
                    "type": "integer",
                    "description": "1-based start line for text files."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max text lines to read. Default 5000."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        #[cfg(test)]
        {
            let gate = test_gates().lock().unwrap().get(&ctx.session_id).cloned();
            if let Some(gate) = gate {
                gate.wait().await;
            }
        }
        let params: ReadInput = super::parse_tool_args(
            input,
            "await tools.read({file_path: 'Cargo.toml', start_line: 1, limit: 10})",
        )?;
        if let Some(id) = params.file_path.strip_prefix("session-image://") {
            let session = crate::session::Session::load_for_remote_startup(&ctx.session_id)?;
            let image = session
                .session_image(id)
                .ok_or_else(|| anyhow::anyhow!("Session image not found: {id}"))?;
            let id = image.info.image_id;
            let ordinal = image.info.ordinal;
            return Ok(ToolOutput::new(format!(
                "Session image [#{id}]

Image #{ordinal}"
            ))
            .with_labeled_image(image.info.media_type, image.data, format!("[#{id}]")));
        }
        let range = normalize_read_range(&params)?;

        let path = ctx.resolve_path(Path::new(&params.file_path));

        // Check if file exists
        if !path.exists() {
            // Try to find similar files
            let suggestions = find_similar_files(&path);
            if suggestions.is_empty() {
                return Err(anyhow::anyhow!("File not found: {}", params.file_path));
            } else {
                return Err(anyhow::anyhow!(
                    "File not found: {}\nDid you mean: {}",
                    params.file_path,
                    suggestions.join(", ")
                ));
            }
        }

        // Check for image files and display in terminal if supported
        if is_image_file(&path) {
            return handle_image_file(&path, &params.file_path);
        }

        // Check for PDF files and extract text
        if is_pdf_file(&path) {
            return handle_pdf_file(&path, &params.file_path);
        }

        // Check for binary files
        if is_binary_file(&path) {
            return Ok(ToolOutput::new(format!(
                "Binary file detected: {}\nUse appropriate tools to handle binary files.",
                params.file_path
            )));
        }

        let read_path = path.clone();
        let window = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(read_path)?;
            let large = file.metadata()?.len() > EXACT_LINE_COUNT_MAX_BYTES;
            read_text_window(file, range, large)
        })
        .await??;
        let TextWindow {
            more_unscanned,
            mut output,
            total_lines,
            truncated_lines: truncated_line_count,
            next_line,
        } = window;
        let end = next_line.unwrap_or_else(|| range.next_offset().min(total_lines));

        // Publish file touch event for swarm coordination
        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: path.to_path_buf(),
            op: FileOp::Read,
            intent: None,
            added: 0,
            removed: 0,
            summary: Some(if more_unscanned {
                format!("read lines {}-{end}", range.offset.saturating_add(1))
            } else {
                format!(
                    "read lines {}-{} of {}",
                    range.offset.saturating_add(1),
                    end,
                    total_lines
                )
            }),
            detail: None,
        }));

        if truncated_line_count > 0 || end < total_lines {
            crate::logging::warn(&format!(
                "[tool:read] returned truncated output for {} in session {} (tool_call={} range={}..{} total_lines={} truncated_lines={})",
                params.file_path,
                ctx.session_id,
                ctx.tool_call_id,
                range.offset.saturating_add(1),
                end,
                total_lines,
                truncated_line_count
            ));
        }

        // Add metadata
        if end < total_lines || more_unscanned {
            let continuation_hint = match range.style {
                ReadRangeStyle::OffsetLimit => format!("offset={end}"),
                ReadRangeStyle::StartEnd => format!("start_line={}", end.saturating_add(1)),
            };
            if more_unscanned {
                output.push_str(&format!(
                    "\n... more lines (use {continuation_hint} to continue)\n"
                ));
            } else {
                output.push_str(&format!(
                    "\n... {} more lines (use {continuation_hint} to continue)\n",
                    total_lines - end
                ));
            }
        }

        if total_lines == 0 {
            output.push_str("(empty file)");
        } else if range.offset >= total_lines {
            output.push_str("(requested range is beyond EOF)");
        } else if truncated_line_count > 0 {
            output.push_str(&format!("\n({truncated_line_count} line(s) truncated)\n"));
        } else if !more_unscanned && range.offset == 0 && end == total_lines {
            output.push_str("\n(Complete file)\n");
        }
        if super::line_anchor::direct(&ctx) {
            let mut tags = String::new();
            super::line_anchor::append(
                &mut tags,
                &path,
                &ctx,
                output.lines().filter_map(|row| {
                    let (number, text) = row.split_once('\t')?;
                    Some((number.trim().parse().ok()?, text))
                }),
            );
            output.push_str(&tags);
        }
        Ok(ToolOutput::new(output))
    }
}

#[cfg(test)]
mod tests;

fn is_binary_file(path: &Path) -> bool {
    // Check by extension first (no I/O needed)
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        let binary_exts = [
            "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "zip", "tar", "gz", "bz2", "xz",
            "7z", "rar", "exe", "dll", "so", "dylib", "o", "a", "class", "pyc", "wasm", "mp3",
            "mp4", "avi", "mov", "mkv", "flac", "ogg", "wav",
        ];
        if binary_exts.contains(&ext.as_str()) {
            return true;
        }
    }

    // Read only the first 8KB to check for binary content (not the entire file)
    use std::io::Read;
    if let Ok(mut file) = std::fs::File::open(path) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = file.read(&mut buf)
            && n > 0
        {
            let null_count = buf[..n].iter().filter(|&&b| b == 0).count();
            return null_count > n / 10;
        }
    }

    false
}

fn find_similar_files(path: &Path) -> Vec<String> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path.file_name().map(|s| s.to_string_lossy().to_lowercase());

    let mut suggestions = Vec::new();

    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if let Some(ref target) = filename {
                // Simple similarity check
                let target_str: &str = target.as_ref();
                if name.contains(target_str) || target_str.contains(&name as &str) {
                    suggestions.push(entry.path().display().to_string());
                    if suggestions.len() >= 3 {
                        break;
                    }
                }
            }
        }
    }

    suggestions
}

/// Check if a file is an image based on extension
fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
        )
    } else {
        false
    }
}

/// Handle reading an image file: return it base64-encoded for model vision.
/// Clients render the attached image themselves. The server never writes
/// terminal escape sequences, since it may have no terminal (daemon, remote).
fn handle_image_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    let data = std::fs::read(path)?;
    let file_size = data.len() as u64;

    let dimensions = get_image_dimensions_from_data(&data);

    let dim_str = dimensions
        .map(|(w, h)| format!("{}x{}", w, h))
        .unwrap_or_else(|| "unknown".to_string());

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    const MAX_IMAGE_SIZE: u64 = 20 * 1024 * 1024;
    let mut output = if file_size <= MAX_IMAGE_SIZE {
        // Detect the format from the bytes and only ever attach a format every
        // provider accepts. A stored BMP/ICO block used to 400 every later
        // request in the session (#1712).
        match crate::image_normalize::normalize_image_bytes(data) {
            Ok(image) => {
                let converted_note = image
                    .converted_from
                    .map(|format| format!("Converted from {format} to PNG. "))
                    .unwrap_or_default();
                // The session resends this image on every later request, so it
                // is stored at the size the model reads, not the file's.
                let fitted = crate::image_normalize::fit_image_for_model(image);
                let scaled_note = fitted
                    .scaled
                    .map(|(_, (width, height))| {
                        format!("Scaled to {width}x{height} for the model; the file is unchanged. ")
                    })
                    .unwrap_or_default();
                let image = fitted.image;
                let b64 =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image.data);
                ToolOutput::new(format!(
                    "Image: {} ({})\nDimensions: {}\n{}{}Image sent to model for vision analysis.",
                    file_path, size_str, dim_str, converted_note, scaled_note
                ))
                .with_labeled_image(image.media_type, b64, file_path.to_string())
            }
            Err(reason) => ToolOutput::new(format!(
                "Image: {} ({})\nDimensions: {}\nNot sent to the model: {}.",
                file_path, size_str, dim_str, reason
            )),
        }
    } else {
        ToolOutput::new(format!(
            "Image: {} ({})\nDimensions: {}\nImage too large for vision (max 20MB).",
            file_path, size_str, dim_str
        ))
    };

    output = output.with_title(format!("📷 {}", file_path));
    Ok(output)
}

/// Get image dimensions from raw data (duplicated from tui::image for convenience)
fn get_image_dimensions_from_data(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: check signature and parse IHDR chunk
    if data.len() > 24 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }

    // JPEG: look for SOF0/SOF2 markers
    if data.len() > 2 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            // SOF0 (baseline) or SOF2 (progressive)
            if marker == 0xC0 || marker == 0xC2 {
                let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((width, height));
            }
            // Skip to next marker
            if i + 3 < data.len() {
                let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                i += 2 + len;
            } else {
                break;
            }
        }
    }

    // GIF: parse header
    if data.len() > 10 && (&data[0..6] == b"GIF87a" || &data[0..6] == b"GIF89a") {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }

    None
}

/// Check if a file is a PDF based on extension
fn is_pdf_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        ext.to_string_lossy().to_lowercase() == "pdf"
    } else {
        false
    }
}

/// Handle reading a PDF file - extract text content
#[cfg(feature = "pdf")]
fn handle_pdf_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    // Get file metadata
    let metadata = std::fs::metadata(path)?;
    let file_size = metadata.len();

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    // Extract text from PDF
    match jcode_pdf::extract_text(path) {
        Ok(text) => {
            let mut output = String::new();
            output.push_str(&format!("PDF: {} ({})\n", file_path, size_str));
            output.push_str(&format!("{}\n", "=".repeat(60)));

            // Split into pages (pdf_extract uses form feed \x0c as page separator)
            let pages: Vec<&str> = text.split('\x0c').collect();
            let page_count = pages.len();

            output.push_str(&format!("Pages: {}\n\n", page_count));

            for (i, page) in pages.iter().enumerate() {
                let page_text = page.trim();
                if !page_text.is_empty() {
                    output.push_str(&format!("--- Page {} ---\n", i + 1));
                    // Limit each page to reasonable length
                    if page_text.len() > 10000 {
                        output.push_str(crate::util::truncate_str(page_text, 10000));
                        output.push_str("\n... (page truncated)\n");
                    } else {
                        output.push_str(page_text);
                    }
                    output.push_str("\n\n");
                }
            }

            Ok(ToolOutput::new(output))
        }
        Err(e) => {
            // Fall back to metadata only if text extraction fails
            Ok(ToolOutput::new(format!(
                "PDF: {} ({})\nCould not extract text: {}\nThis may be a scanned/image-based PDF.",
                file_path, size_str, e
            )))
        }
    }
}

/// Handle reading a PDF file when PDF support is not compiled in.
#[cfg(not(feature = "pdf"))]
fn handle_pdf_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    let metadata = std::fs::metadata(path)?;
    let file_size = metadata.len();

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    Ok(ToolOutput::new(format!(
        "PDF: {} ({})\nPDF text extraction is not available in this build. Rebuild with the `pdf` feature enabled to extract text.",
        file_path, size_str
    )))
}
