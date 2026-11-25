use super::*;

pub(in super::super) fn handle_paste(app: &mut App, text: String) {
    app.preferred_input_column = None;
    if app.append_ssh_login_input(&text) {
        return;
    }
    paste_guard::note_paste();
    if crate::tui::is_ssh_remote() {
        // Text paths refer to the remote machine. Do not stat/read laptop
        // files or download image URLs automatically while attached over SSH.
        handle_text_paste(app, text);
        app.set_status_notice("SSH paste: text only. File paths refer to the remote host; paste image bytes from the clipboard to attach an image.");
        return;
    }
    // Note: clipboard_image() is NOT checked here. Bracketed paste events from the
    // terminal always deliver text. Checking clipboard_image() here caused a bug where
    // text pastes were misidentified as images when the clipboard also had image data
    // (common on Wayland where apps advertise multiple MIME types). Image pasting is
    // handled by explicit clipboard shortcuts instead (Ctrl+V/Alt+V/Cmd+V smart-paste).
    if let Some(paths) = parse_dropped_paths(&text) {
        let item_count = paths.len();
        let mut image_count = 0;
        let mut file_count = 0;

        for (index, path) in paths.into_iter().enumerate() {
            if index > 0 {
                insert_input_text(app, " ");
            }

            if let Some((media_type, data)) = load_dropped_image(&path) {
                attach_image(
                    app,
                    media_type,
                    base64::engine::general_purpose::STANDARD.encode(data),
                );
                image_count += 1;
            } else {
                insert_input_text(app, &format_dropped_path(&path, item_count > 1));
                file_count += 1;
            }
        }

        let notice = match (image_count, file_count) {
            (images, 0) => format!(
                "Dropped {images} image{}",
                if images == 1 { "" } else { "s" }
            ),
            (0, files) => format!("Dropped {files} file{}", if files == 1 { "" } else { "s" }),
            (images, files) => format!(
                "Dropped {images} image{} and {files} file{}",
                if images == 1 { "" } else { "s" },
                if files == 1 { "" } else { "s" }
            ),
        };
        app.set_status_notice(notice);
    } else if let Some(url) = super::super::extract_image_url(&text) {
        crate::logging::info(&format!("Downloading image from pasted URL: {}", url));
        app.set_status_notice("Downloading image...");
        let session_id = active_clipboard_session_id(app);
        spawn_blocking_or_thread(move || {
            let content = download_image_url_content(&url).unwrap_or_else(|| {
                ClipboardPasteContent::Error("Failed to download image".to_string())
            });
            publish_clipboard_result(
                session_id,
                ClipboardPasteKind::ImageUrl {
                    fallback_text: Some(text),
                },
                content,
            );
        });
    } else {
        handle_text_paste(app, text);
    }
}

pub(super) fn format_dropped_path(path: &std::path::Path, quote_for_batch: bool) -> String {
    let value = path.to_string_lossy();
    if quote_for_batch
        && value
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '\\' | '\'' | '"'))
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.into_owned()
    }
}

pub(super) fn dropped_image_files(text: &str) -> Option<Vec<(String, Vec<u8>)>> {
    let paths = parse_dropped_paths(text)?;
    paths
        .into_iter()
        .map(|path| load_dropped_image(&path))
        .collect()
}

/// Terminal emulators normally send file drops as bracketed paste, but some send
/// the path as ordinary key events. Prepare a complete path-only composer before
/// command/skill routing: attach images, or resolve quoting for ordinary files.
pub(in super::super) fn promote_dropped_images(app: &mut App) -> bool {
    if attach_dropped_images(app) {
        return true;
    }
    let Some(paths) = parse_dropped_paths(&app.input) else {
        return false;
    };
    let normalized = paths
        .iter()
        .map(|path| format_dropped_path(path, paths.len() > 1))
        .collect::<Vec<_>>()
        .join(" ");
    if normalized == app.input {
        return false;
    }
    app.remember_input_undo_state();
    app.input = normalized;
    app.cursor_pos = app.input.len();
    app.reset_tab_completion();
    true
}

pub(super) fn attach_dropped_images(app: &mut App) -> bool {
    let Some(images) = dropped_image_files(&app.input) else {
        return false;
    };
    let count = images.len();
    app.input.clear();
    app.cursor_pos = 0;
    for (media_type, data) in images {
        attach_image(
            app,
            media_type,
            base64::engine::general_purpose::STANDARD.encode(data),
        );
    }
    app.set_status_notice(format!(
        "Dropped {count} image{}",
        if count == 1 { "" } else { "s" }
    ));
    true
}

pub(in super::super) fn parse_dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    if crate::tui::is_ssh_remote() {
        return None;
    }
    let trimmed = text.trim();
    let literal_path = PathBuf::from(trimmed);
    if literal_path.is_file() {
        return Some(vec![literal_path]);
    }

    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                token.push(ch);
            }
        } else if ch.is_whitespace() && quote.is_none() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    if tokens.is_empty() {
        return None;
    }

    tokens
        .into_iter()
        .map(|token| {
            let path = if token.starts_with("file://") {
                url::Url::parse(&token).ok()?.to_file_path().ok()?
            } else {
                PathBuf::from(token)
            };
            path.is_file().then_some(path)
        })
        .collect()
}

pub(in super::super) fn handle_text_paste(app: &mut App, text: String) {
    crate::logging::info(&format!(
        "Text paste: {} chars, {} lines",
        text.len(),
        text.lines().count()
    ));

    let line_count = text.lines().count().max(1);
    if line_count < 5 {
        insert_input_text(app, &text);
        return;
    }
    if expand_matching_paste(app, &text) {
        return;
    }

    let placeholder = paste_placeholder(&text);
    app.pasted_contents.push(text);
    insert_input_text(app, &placeholder);
}

pub(super) fn expand_matching_paste(app: &mut App, text: &str) -> bool {
    let Some(content_index) = app
        .pasted_contents
        .iter()
        .rposition(|content| content == text)
    else {
        return false;
    };

    let placeholder = paste_placeholder(text);
    // Placeholders only encode a line count. Skip placeholders belonging to
    // newer stored pastes with the same shape so equal-length, different text
    // cannot cause the wrong placeholder to expand.
    let newer_same_placeholder_count = app.pasted_contents[content_index + 1..]
        .iter()
        .filter(|content| paste_placeholder(content) == placeholder)
        .count();
    let Some(placeholder_start) = app
        .input
        .rmatch_indices(placeholder.as_str())
        .map(|(position, _)| position)
        .nth(newer_same_placeholder_count)
    else {
        return false;
    };

    app.follow_chat_bottom_for_typing();
    app.remember_input_undo_state();
    app.input.replace_range(
        placeholder_start..placeholder_start + placeholder.len(),
        text,
    );
    app.cursor_pos = placeholder_start + text.len();
    app.pasted_contents.remove(content_index);
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
    true
}
