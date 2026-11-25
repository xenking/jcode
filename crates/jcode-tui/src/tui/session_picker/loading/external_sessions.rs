use super::*;

/// Scan every supported foreign agent CLI in parallel, or nothing at all when
/// the user opted out via `display.external_sessions` (issue #674). Returning
/// early skips the filesystem work entirely rather than filtering afterwards.
pub(super) fn load_external_sessions(enabled: bool, scan_limit: usize) -> Vec<SessionInfo> {
    if !enabled {
        return Vec::new();
    }
    std::thread::scope(|scope| {
        let claude = scope.spawn(|| load_external_claude_code_sessions(scan_limit));
        let codex = scope.spawn(|| load_external_codex_sessions(scan_limit));
        let pi = scope.spawn(|| load_external_pi_sessions(scan_limit));
        let opencode = scope.spawn(|| load_external_opencode_sessions(scan_limit));
        let cursor = scope.spawn(|| load_external_cursor_sessions(scan_limit));

        let mut external = Vec::new();
        for handle in [claude, codex, pi, opencode, cursor] {
            match handle.join() {
                Ok(sessions) => external.extend(sessions),
                Err(_) => crate::logging::warn("External session loader panicked"),
            }
        }
        external
    })
}

pub(super) fn load_external_claude_code_sessions(scan_limit: usize) -> Vec<SessionInfo> {
    let Ok(sessions) = crate::import::list_claude_code_sessions_lazy(scan_limit) else {
        return Vec::new();
    };

    sessions
        .into_iter()
        .take(scan_limit)
        .map(|session| {
            let session_id = session.session_id;
            let created_at = session.created.unwrap_or_else(chrono::Utc::now);
            let last_message_time = session.modified.or(session.created).unwrap_or(created_at);
            let working_dir = session.project_path;
            let title = session
                .summary
                .filter(|summary| !summary.trim().is_empty())
                .unwrap_or_else(|| truncate_title_text(&session.first_prompt, 72));
            let short_name = working_dir
                .as_deref()
                .and_then(|dir| Path::new(dir).file_name())
                .and_then(|name| name.to_str())
                .map(|name| name.to_string())
                .unwrap_or_else(|| {
                    format!("claude {}", jcode_core::util::truncate_str(&session_id, 8))
                });
            // Keep /resume startup focused on cheap metadata. Transcript-backed
            // search text is intentionally loaded lazily through preview loading;
            // reading tens of KiB from every external transcript can dominate the
            // initial picker load on accounts with many Claude Code sessions.
            let search_index = build_search_index(
                &format!("claude:{session_id}"),
                &short_name,
                &title,
                working_dir.as_deref(),
                None,
                &[],
            );

            SessionInfo {
                id: format!("claude:{session_id}"),
                parent_id: None,
                short_name,
                icon: "🧵".to_string(),
                title,
                message_count: session.message_count as usize,
                user_message_count: 0,
                assistant_message_count: 0,
                created_at,
                last_message_time,
                last_active_at: Some(last_message_time),
                working_dir,
                model: None,
                provider_key: Some("claude-code".to_string()),
                is_canary: false,
                is_debug: false,
                saved: false,
                save_label: None,
                status: SessionStatus::Closed,
                needs_catchup: false,
                estimated_tokens: 0,
                first_user_prompt: Some(session.first_prompt.clone()),
                messages_preview: Vec::new(),
                search_index,
                server_name: None,
                server_icon: None,
                source: SessionSource::ClaudeCode,
                resume_target: ResumeTarget::ClaudeCodeSession {
                    session_id,
                    session_path: session.full_path.clone(),
                },
                external_path: Some(session.full_path),
                user_intention: None,
                todo_statuses: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
pub(in super::super) fn load_claude_code_preview_from_path(path: &Path) -> Option<Vec<PreviewMessage>> {
    load_claude_code_external_preview(path).map(|preview| preview.messages)
}

pub(in super::super) fn load_claude_code_external_preview(path: &Path) -> Option<ExternalPreview> {
    // Only parse the tail of the transcript (see `load_codex_preview_from_path`):
    // the preview shows the last ~20 messages, so reparsing multi-MB transcripts
    // on every selection change made picker navigation lag.
    let (text, truncated) = read_file_tail_text(path, EXTERNAL_PREVIEW_TAIL_BYTES)?;
    let mut out = ExternalPreview::new();

    // If we seeked into the middle of the file, the first line is a partial
    // record; drop it. When we read the whole file the first line is a real
    // record we must keep.
    let skip = usize::from(truncated);
    for line in text.lines().skip(skip) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Boundary lines from a tail slice may be malformed; skip rather than
        // abandon the whole preview.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let entry_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if entry_type != "user" && entry_type != "assistant" {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let role = message
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or(entry_type);
        let text =
            extract_text_from_value(message.get("content").unwrap_or(&serde_json::Value::Null));
        push_preview_message(&mut out.messages, role, text);
        classify_json_into(&mut out.search, message, message_role_source(role));
    }

    out.non_empty()
}

#[cfg(test)]
pub(in super::super) fn load_claude_code_preview(session_id: &str) -> Option<Vec<PreviewMessage>> {
    let sessions = match crate::import::list_claude_code_sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            crate::logging::warn(&format!(
                "Could not discover Claude Code preview sessions: {error}"
            ));
            return None;
        }
    };
    let session = sessions
        .into_iter()
        .find(|session| session.session_id == session_id)?;
    load_claude_code_preview_from_path(Path::new(&session.full_path))
}

pub(in super::super) fn load_claude_code_external_preview_by_id(session_id: &str) -> Option<ExternalPreview> {
    let session = crate::import::list_claude_code_sessions()
        .ok()?
        .into_iter()
        .find(|session| session.session_id == session_id)?;
    load_claude_code_external_preview(Path::new(&session.full_path))
}

pub(super) fn load_external_codex_sessions(scan_limit: usize) -> Vec<SessionInfo> {
    let Ok(root) = crate::storage::user_home_path(".codex/sessions") else {
        return Vec::new();
    };
    if !root.exists() {
        return Vec::new();
    }

    let paths = collect_recent_files_recursive(&root, "jsonl", scan_limit);
    parallel_map(paths, |path| load_codex_session_stub(&path).ok().flatten())
        .into_iter()
        .flatten()
        .collect()
}

pub(super) fn load_codex_session_stub(path: &Path) -> Result<Option<SessionInfo>> {
    let file = File::open(path)?;
    let mut lines = BufReader::new(file).lines();
    let Some(first_line) = lines.next() else {
        return Ok(None);
    };
    let header: serde_json::Value = serde_json::from_str(&first_line?)?;
    let meta = if header.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
        header.get("payload").unwrap_or(&header)
    } else {
        &header
    };
    let session_id = meta
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Ok(None);
    }

    let created_at = parse_timestamp_value(meta.get("timestamp"))
        .or_else(|| parse_timestamp_value(header.get("timestamp")))
        .unwrap_or_else(chrono::Utc::now);
    let last_message_time = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or(created_at);
    let working_dir = meta
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let short_name = format!("codex {}", jcode_core::util::truncate_str(&session_id, 8));
    let title = format!(
        "Codex session {}",
        jcode_core::util::truncate_str(&session_id, 8)
    );
    let search_index = build_search_index(
        &format!("codex:{session_id}"),
        &short_name,
        &title,
        working_dir.as_deref(),
        None,
        &[],
    );

    Ok(Some(SessionInfo {
        id: format!("codex:{session_id}"),
        parent_id: None,
        short_name,
        icon: "🧠".to_string(),
        title,
        message_count: 0,
        user_message_count: 0,
        assistant_message_count: 0,
        created_at,
        last_message_time,
        last_active_at: Some(last_message_time),
        working_dir,
        model: None,
        provider_key: Some("openai-codex".to_string()),
        is_canary: false,
        is_debug: false,
        saved: false,
        save_label: None,
        status: SessionStatus::Closed,
        needs_catchup: false,
        estimated_tokens: 0,
        first_user_prompt: None,
        messages_preview: Vec::new(),
        search_index,
        server_name: None,
        server_icon: None,
        source: SessionSource::Codex,
        resume_target: ResumeTarget::CodexSession {
            session_id,
            session_path: path.to_string_lossy().to_string(),
        },
        external_path: Some(path.to_string_lossy().to_string()),
        user_intention: None,
        todo_statuses: Vec::new(),
    }))
}

pub(super) fn find_codex_session_file(session_id: &str) -> Option<PathBuf> {
    let root = crate::storage::user_home_path(".codex/sessions").ok()?;
    if !root.exists() {
        return None;
    }

    for path in collect_files_recursive(&root, "jsonl") {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut lines = BufReader::new(file).lines();
        let Some(Ok(first_line)) = lines.next() else {
            continue;
        };
        let Ok(header) = serde_json::from_str::<serde_json::Value>(&first_line) else {
            continue;
        };
        let meta = if header.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            header.get("payload").unwrap_or(&header)
        } else {
            &header
        };
        if meta.get("id").and_then(|v| v.as_str()) == Some(session_id) {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
pub(in super::super) fn load_codex_preview_from_path(path: &Path) -> Option<Vec<PreviewMessage>> {
    load_codex_external_preview(path).map(|preview| preview.messages)
}

pub(in super::super) fn load_codex_external_preview(path: &Path) -> Option<ExternalPreview> {
    // Only parse the tail of the transcript: the preview shows the last ~20
    // messages, and these rollout files can be tens of MB, so reading the whole
    // file on every selection change made picker navigation lag.
    let (text, _truncated) = read_file_tail_text(path, EXTERNAL_PREVIEW_TAIL_BYTES)?;
    let mut out = ExternalPreview::new();

    // When we read from the start we skip the first line (the `session_meta`
    // record). When we read a tail slice the first line is almost certainly a
    // partial record, so we drop it either way.
    for line in text.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A tail slice can yield malformed JSON on its boundary lines; skip
        // those instead of bailing out of the whole preview.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let line_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let (role, content_value) = if line_type == "message" {
            let role = value.get("role").and_then(|v| v.as_str())?;
            (
                role,
                value.get("content").unwrap_or(&serde_json::Value::Null),
            )
        } else if line_type == "response_item" {
            let payload = value.get("payload")?;
            if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                // Tool calls, their output, and reasoning are searchable at
                // their own weight even though the preview hides them.
                classify_json_into(&mut out.search, payload, Source::Other);
                continue;
            }
            let role = payload.get("role").and_then(|v| v.as_str())?;
            (
                role,
                payload.get("content").unwrap_or(&serde_json::Value::Null),
            )
        } else {
            continue;
        };
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = extract_block_text_from_value(content_value);
        push_preview_message(&mut out.messages, role, text);
        classify_json_into(&mut out.search, content_value, message_role_source(role));
    }

    out.non_empty()
}

pub(in super::super) fn load_codex_external_preview_by_id(session_id: &str) -> Option<ExternalPreview> {
    load_codex_external_preview(&find_codex_session_file(session_id)?)
}
