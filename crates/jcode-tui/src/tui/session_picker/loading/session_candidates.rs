use super::*;

pub(super) fn session_sort_key(stem: &str) -> u64 {
    for part in stem.split('_') {
        if part.len() == 13
            && part.as_bytes().iter().all(|b| b.is_ascii_digit())
            && let Ok(ts) = part.parse::<u64>()
        {
            return ts;
        }
    }

    stem.split('_')
        .rev()
        .find_map(|part| part.parse::<u64>().ok())
        .unwrap_or(0)
}

pub(super) fn path_modified_sort_key(path: &Path) -> u128 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub(super) struct SessionCandidateMeta {
    pub(super) modified: u128,
    pub(super) sort_key: u64,
    pub(super) has_snapshot: bool,
}

impl SessionCandidateMeta {
    pub(super) fn new(stem: &str) -> Self {
        Self {
            modified: 0,
            sort_key: session_sort_key(stem),
            has_snapshot: false,
        }
    }

    pub(super) fn update(&mut self, modified: u128, has_snapshot: bool) {
        self.modified = self.modified.max(modified);
        self.has_snapshot |= has_snapshot;
    }
}

pub(super) fn session_file_stem_for_candidate(file_name: &str) -> Option<(&str, bool)> {
    if let Some(stem) = file_name.strip_suffix(".journal.jsonl") {
        return Some((stem, false));
    }

    let stem = file_name.strip_suffix(".json")?;
    if stem.ends_with(".journal") {
        return None;
    }
    Some((stem, true))
}

pub(super) fn classify_session_source(
    id: &str,
    provider_key: Option<&str>,
    model: Option<&str>,
) -> SessionSource {
    if id.starts_with("imported_cc_") {
        return SessionSource::ClaudeCode;
    }

    let provider_key = provider_key.unwrap_or_default().to_ascii_lowercase();
    let model = model.unwrap_or_default().to_ascii_lowercase();

    if provider_key == "pi" || provider_key.starts_with("pi-") {
        return SessionSource::Pi;
    }
    if provider_key == "opencode"
        || provider_key == "opencode-go"
        || provider_key.contains("opencode")
    {
        return SessionSource::OpenCode;
    }
    if provider_key.contains("codex") || model.contains("codex") || model.contains("openai-codex") {
        return SessionSource::Codex;
    }
    if provider_key == "cursor" || provider_key == "cursor-agent" {
        return SessionSource::Cursor;
    }

    SessionSource::Jcode
}

pub(super) fn collect_files_recursive(root: &Path, extension: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, extension, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case(extension))
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(root, extension, &mut files);
    files.sort_by(|a, b| {
        let a_time = std::fs::metadata(a).and_then(|meta| meta.modified()).ok();
        let b_time = std::fs::metadata(b).and_then(|meta| meta.modified()).ok();
        b_time.cmp(&a_time).then_with(|| b.cmp(a))
    });
    files
}

pub(super) fn collect_recent_files_recursive(root: &Path, extension: &str, limit: usize) -> Vec<PathBuf> {
    fn modified_sort_key(path: &Path) -> u64 {
        path.metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    fn walk(
        dir: &Path,
        extension: &str,
        limit: usize,
        out: &mut BinaryHeap<Reverse<(u64, PathBuf)>>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, extension, limit, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case(extension))
                .unwrap_or(false)
            {
                let key = (modified_sort_key(&path), path);
                if out.len() < limit {
                    out.push(Reverse(key));
                } else if out.peek().map(|smallest| key > smallest.0).unwrap_or(true) {
                    out.pop();
                    out.push(Reverse(key));
                }
            }
        }
    }

    if limit == 0 {
        return Vec::new();
    }

    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    walk(root, extension, limit, &mut heap);
    let mut files: Vec<(u64, PathBuf)> = heap.into_iter().map(|entry| entry.0).collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.into_iter().map(|(_, path)| path).collect()
}

/// Maximum number of bytes we read from the *tail* of an external transcript
/// (Codex / Claude Code) when building its preview. These JSONL transcripts can
/// be tens of MB, but the preview only ever shows the last ~20 messages, so
/// parsing the whole file on every selection change made arrow-key navigation
/// in the resume / onboarding picker lag badly (each load reparsed the entire
/// file on a fresh thread). Reading a bounded tail keeps each preview load to a
/// sub-millisecond seek + parse regardless of transcript size.
///
/// 512 KiB comfortably covers far more than 20 messages for normal transcripts
/// while bounding the worst case.
pub(super) const EXTERNAL_PREVIEW_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Read the trailing portion of a file as UTF-8 text, capped at
/// [`EXTERNAL_PREVIEW_TAIL_BYTES`]. When the file is larger than the cap we seek
/// to the tail and drop the (possibly partial) first line so we only ever parse
/// complete JSONL records. Returns `(text, truncated_from_head)` where
/// `truncated_from_head` indicates the head of the file was skipped.
pub(super) fn read_file_tail_text(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let truncated = len > max_bytes;
    if truncated {
        file.seek(SeekFrom::Start(len - max_bytes)).ok()?;
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(len) as usize);
    file.take(max_bytes).read_to_end(&mut bytes).ok()?;
    // Lossily decode: transcripts are UTF-8, but a tail seek can land mid
    // multi-byte sequence, and replacement chars are harmless for a preview.
    Some((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

pub(super) fn push_preview_message(preview: &mut Vec<PreviewMessage>, role: &str, content: String) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }
    preview.push(PreviewMessage {
        role: role.to_string(),
        content: content.to_string(),
        tool_calls: Vec::new(),
        tool_data: None,
        duration_secs: None,
        title: None,
        timestamp: None,
    });
    if preview.len() > PREVIEW_MESSAGE_LIMIT {
        let drop_count = preview.len().saturating_sub(PREVIEW_MESSAGE_LIMIT);
        preview.drain(0..drop_count);
    }
}

pub(super) fn extract_text_from_value(value: &serde_json::Value) -> String {
    fn visit(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => {
                if !text.trim().is_empty() {
                    out.push(text.trim().to_string());
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|v| v.as_str())
                    && !text.trim().is_empty()
                {
                    out.push(text.trim().to_string());
                }
                if let Some(text) = map.get("title").and_then(|v| v.as_str())
                    && !text.trim().is_empty()
                {
                    out.push(text.trim().to_string());
                }
                for value in map.values() {
                    visit(value, out);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    visit(value, &mut out);
    out.join(" ")
}

/// Classifies every OpenCode part, including tool and reasoning parts the
/// preview omits, at the weight of its own block type.
pub(super) fn classify_opencode_parts(
    search: &mut SourceBuckets,
    parts_base: &Path,
    message_id: &str,
    role: Source,
) {
    let message_parts = parts_base.join(message_id);
    if !message_parts.exists() {
        return;
    }
    for part_path in collect_files_recursive(&message_parts, "json") {
        let Ok(file) = std::fs::File::open(&part_path) else {
            continue;
        };
        let Ok(part) = serde_json::from_reader::<_, serde_json::Value>(file) else {
            continue;
        };
        let source = match part.get("type").and_then(|v| v.as_str()) {
            Some("text") => role,
            Some("tool") => Source::ToolInput,
            _ => Source::Other,
        };
        classify_json_into(search, &part, source);
    }
}

/// Extract message body text from OpenCode part files for the session-picker
/// preview. Modern OpenCode (Go storage) stores message bodies in
/// `storage/part/<messageID>/*.json`; only plain `text` parts are used for the
/// lightweight preview.
pub(super) fn extract_opencode_part_preview(parts_base: &Path, message_id: &str) -> String {
    let message_parts = parts_base.join(message_id);
    if !message_parts.exists() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    for part_path in collect_files_recursive(&message_parts, "json") {
        let Ok(file) = std::fs::File::open(&part_path) else {
            continue;
        };
        let Ok(part) = serde_json::from_reader::<_, serde_json::Value>(file) else {
            continue;
        };
        if part.get("type").and_then(|v| v.as_str()) == Some("text")
            && let Some(text) = part.get("text").and_then(|v| v.as_str())
            && !text.trim().is_empty()
        {
            out.push(text.trim().to_string());
        }
    }
    out.join(" ")
}

pub(super) fn extract_block_text_from_value(value: &serde_json::Value) -> String {
    fn extract(value: &serde_json::Value, separator: &str) -> Option<String> {
        match value {
            serde_json::Value::String(text) => {
                let trimmed = text.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            serde_json::Value::Array(items) => {
                let parts: Vec<String> =
                    items.iter().filter_map(|item| extract(item, " ")).collect();
                (!parts.is_empty()).then(|| parts.join("\n\n"))
            }
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    return (!trimmed.is_empty()).then(|| trimmed.to_string());
                }

                let mut parts = Vec::new();
                if let Some(title) = map.get("title").and_then(|v| v.as_str()) {
                    let trimmed = title.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
                for (key, nested) in map {
                    if key == "type" || key == "title" {
                        continue;
                    }
                    if let Some(text) = extract(nested, " ") {
                        parts.push(text);
                    }
                }

                (!parts.is_empty()).then(|| parts.join(separator))
            }
            _ => None,
        }
    }

    extract(value, " ").unwrap_or_default()
}

pub(super) fn truncate_title_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "Untitled".to_string();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

pub(super) fn parse_timestamp_value(
    value: Option<&serde_json::Value>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .and_then(|v| v.as_str())
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
pub(super) fn value_first_text(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(text) => Some(text.as_str()),
        serde_json::Value::Array(items) => items.iter().find_map(value_first_text),
        serde_json::Value::Object(map) => map.get("text").and_then(|text| text.as_str()),
        _ => None,
    }
}

#[cfg(test)]
pub(super) fn message_value_is_internal_system_reminder(message: &serde_json::Value) -> bool {
    message
        .get("content")
        .and_then(value_first_text)
        .is_some_and(|text| text.trim_start().starts_with("<system-reminder>"))
}

#[cfg(test)]
pub(super) fn message_value_is_visible_conversation(message: &serde_json::Value) -> bool {
    let has_display_role = message
        .get("display_role")
        .is_some_and(|value| !value.is_null());
    !has_display_role && !message_value_is_internal_system_reminder(message)
}

#[cfg(test)]
pub(super) fn snapshot_has_visible_conversation(path: &Path) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let messages = value.get("messages")?.as_array()?;
    Some(messages.iter().any(message_value_is_visible_conversation))
}

#[cfg(test)]
pub(super) fn snapshot_bytes_look_trivial_hidden_only(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let Some(messages) = value
        .get("messages")
        .and_then(|messages| messages.as_array())
    else {
        return false;
    };
    !messages.is_empty() && !messages.iter().any(message_value_is_visible_conversation)
}

#[cfg(test)]
pub(super) fn journal_has_visible_conversation(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut saw_parseable_line = false;
    for line in reader.lines().map_while(|line| line.ok()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        saw_parseable_line = true;
        let Some(messages) = value.get("append_messages").and_then(|v| v.as_array()) else {
            continue;
        };
        if messages.iter().any(message_value_is_visible_conversation) {
            return Some(true);
        }
    }
    saw_parseable_line.then_some(false)
}

#[cfg(test)]
pub(super) fn is_empty_session_file(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return true;
    };
    let mut buf = [0u8; 300];
    let n = match file.take(300).read(&mut buf) {
        Ok(n) => n,
        Err(_) => return true,
    };
    let head = &buf[..n];
    head.windows(13).any(|w| w == b"\"messages\":[]")
        || head.windows(14).any(|w| w == b"\"messages\": []")
}

#[cfg(test)]
pub(super) fn session_has_history(sessions_dir: &Path, stem: &str) -> bool {
    let snapshot_path = sessions_dir.join(format!("{stem}.json"));
    let journal_path = sessions_dir.join(format!("{stem}.journal.jsonl"));

    if journal_has_visible_conversation(&journal_path) == Some(true) {
        return true;
    }

    if let Some(has_visible) = snapshot_has_visible_conversation(&snapshot_path) {
        return has_visible;
    }

    if !is_empty_session_file(&snapshot_path) {
        return true;
    }

    journal_path
        .metadata()
        .map(|meta| meta.len() > 0)
        .unwrap_or(false)
}

pub(super) fn collect_recent_session_candidates(
    sessions_dir: &Path,
    candidate_limit: usize,
) -> Result<Vec<String>> {
    // Phase 1: a single cheap `readdir` pass to enumerate candidate files. We
    // defer the per-file `stat` (the expensive part on directories with 100k+
    // session files) to a parallel pass so it does not serialize startup.
    let mut raw: Vec<(String, bool, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some((stem, has_snapshot)) = session_file_stem_for_candidate(file_name) else {
            continue;
        };
        if stem.starts_with("imported_") {
            continue;
        }
        raw.push((stem.to_string(), has_snapshot, entry.path()));
    }

    // Phase 2: stat each file's modification time in parallel.
    let stamped = parallel_map(raw, |(stem, has_snapshot, path)| {
        (stem, has_snapshot, path_modified_sort_key(&path))
    });

    // Phase 3: merge per-stem metadata (snapshot + newest journal/snapshot mtime).
    let mut by_stem: HashMap<String, SessionCandidateMeta> = HashMap::new();
    for (stem, has_snapshot, modified) in stamped {
        by_stem
            .entry(stem.clone())
            .or_insert_with(|| SessionCandidateMeta::new(&stem))
            .update(modified, has_snapshot);
    }

    let mut candidates: BinaryHeap<Reverse<(u128, u64, String)>> = BinaryHeap::new();
    for (stem, meta) in by_stem {
        if !meta.has_snapshot {
            continue;
        }
        let key = (meta.modified, meta.sort_key, stem);
        if candidates.len() < candidate_limit {
            candidates.push(Reverse(key));
            continue;
        }

        let should_replace = candidates
            .peek()
            .map(|smallest| key > smallest.0)
            .unwrap_or(true);
        if should_replace {
            candidates.pop();
            candidates.push(Reverse(key));
        }
    }

    let mut out: Vec<(u128, u64, String)> = candidates.into_iter().map(|entry| entry.0).collect();
    out.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
    });
    Ok(out.into_iter().map(|(_, _, stem)| stem).collect())
}

pub(super) fn json_bytes_saved_true(bytes: &[u8]) -> bool {
    let mut search_start = 0usize;
    while let Some(relative_idx) = bytes[search_start..]
        .windows(b"\"saved\"".len())
        .position(|window| window == b"\"saved\"")
    {
        let key_idx = search_start + relative_idx + b"\"saved\"".len();
        let mut idx = key_idx;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if bytes.get(idx) != Some(&b':') {
            search_start = key_idx;
            continue;
        }
        idx += 1;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if bytes[idx..].starts_with(b"true") {
            return true;
        }
        search_start = key_idx;
    }
    false
}

pub(super) fn file_tail_contains_saved_true(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return false;
    };
    let read_len = len.min(SAVED_METADATA_TAIL_SCAN_BYTES);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(read_len)))
        .is_err()
    {
        return false;
    }
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.take(read_len).read_to_end(&mut bytes).is_ok() && json_bytes_saved_true(&bytes)
}

pub(super) fn session_snapshot_or_journal_has_saved_metadata(snapshot_path: &Path) -> bool {
    let journal_path = session::session_journal_path_from_snapshot(snapshot_path);

    // This runs across every historical session during cold `/resume` load, so
    // never parse whole journals here. Saved metadata is persisted in snapshots
    // and repeated in journal meta updates; a bounded tail scan is enough to
    // keep saved sessions discoverable without making old multi-MB journals
    // dominate startup. A later full summary load still computes the exact
    // saved state for candidates that make it into the list.
    file_tail_contains_saved_true(snapshot_path) || file_tail_contains_saved_true(&journal_path)
}

pub(super) fn collect_saved_session_candidates(sessions_dir: &Path) -> Result<Vec<String>> {
    let mut saved = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some((stem, has_snapshot)) = session_file_stem_for_candidate(file_name) else {
            continue;
        };
        if !has_snapshot || stem.starts_with("imported_") {
            continue;
        }
        let path = sessions_dir.join(format!("{stem}.json"));
        if session_snapshot_or_journal_has_saved_metadata(&path) {
            saved.push(stem.to_string());
        }
    }
    saved.sort();
    saved.dedup();
    Ok(saved)
}

#[cfg(test)]
pub(in super::super) fn collect_recent_session_stems(
    sessions_dir: &Path,
    scan_limit: usize,
) -> Result<Vec<String>> {
    let mut candidate_limit = session_candidate_window(scan_limit);

    loop {
        let candidates = collect_recent_session_candidates(sessions_dir, candidate_limit)?;
        let mut recent = Vec::with_capacity(scan_limit);
        for stem in candidates {
            if !session_has_history(sessions_dir, &stem) {
                continue;
            }
            recent.push(stem);
            if recent.len() >= scan_limit {
                break;
            }
        }

        if recent.len() >= scan_limit || candidate_limit >= MAX_SESSION_SCAN_LIMIT {
            return Ok(recent);
        }

        candidate_limit = candidate_limit
            .saturating_mul(2)
            .min(MAX_SESSION_SCAN_LIMIT);
    }
}
