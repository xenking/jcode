//! Regression tests for #1680: computer-use screenshots must not accumulate
//! without bound in the persisted transcript.
//!
//! Bounding must never rewrite a message that the provider has already seen:
//! replacing an old screenshot with a marker invalidates the whole prompt cache
//! from that message onward, and on 2026-11-15 that is exactly what rebuilt a
//! ~360K-token prefix mid-turn (a `/btw` side question was in flight, so the
//! fork took the blame). Decay therefore runs when a transcript is loaded and
//! the outbound request copy is trimmed at send time, never on append.

use super::*;
use anyhow::{Result, anyhow};

const SCREENSHOT_CHARS: usize = 64 * 1024;

fn add_screenshot(session: &mut Session, index: usize) {
    session.add_message(
        Role::User,
        vec![
            ContentBlock::ToolResult {
                tool_use_id: format!("call_{index}"),
                content: format!("Captured main display #{index}"),
                is_error: None,
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "A".repeat(SCREENSHOT_CHARS),
            },
        ],
    );
}

fn inline_image_count(messages: &[StoredMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter(|block| matches!(block, ContentBlock::Image { .. }))
        .count()
}

fn message_bytes(messages: &[StoredMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|message| serde_json::to_string(message).expect("serialize message"))
        .collect()
}

/// The red test for the 2026-11-15 cache-miss incident: appending a screenshot
/// past the image-count trigger must not touch any earlier message. Anything
/// that rewrites message N invalidates the provider prompt cache from N to the
/// end of the prompt, so a long computer-use turn paid a full-prefix rebuild
/// every few screenshots.
#[test]
fn appending_screenshots_never_rewrites_earlier_messages() -> Result<()> {
    let _env_lock = lock_env();
    let temp_home = tempfile::Builder::new()
        .prefix("jcode-tool-image-append-")
        .tempdir()
        .map_err(|e| anyhow!(e))?;
    let _home = EnvVarGuard::set("JCODE_HOME", temp_home.path().as_os_str());

    let session_id = "session_tool_image_append";
    let mut session = Session::create_with_id(session_id.to_string(), None, None);
    let screenshots = jcode_compaction_core::TOOL_IMAGE_DECAY_TRIGGER_COUNT * 3;
    for index in 0..screenshots {
        let before = message_bytes(&session.messages);
        add_screenshot(&mut session, index);
        let after = message_bytes(&session.messages);
        if let Some(position) = (0..before.len()).find(|&i| before[i] != after[i]) {
            panic!(
                "appending screenshot #{index} rewrote message #{position}; the provider \
                 prompt prefix is now invalid from that message on"
            );
        }
        session.save()?;
    }

    // Every screenshot is still inline in the stored transcript, and every tool
    // result is still paired, so the transcript stays replayable as-is.
    assert_eq!(session.messages.len(), screenshots);
    assert_eq!(inline_image_count(&session.messages), screenshots);
    assert!(
        session
            .messages
            .iter()
            .all(|message| !message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text.contains("Older tool screenshot omitted")
            )))
    );

    // The newest screenshot still reaches the provider.
    let provider_messages = session.provider_messages();
    let last = provider_messages.last().expect("provider message");
    assert!(
        last.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    );
    Ok(())
}

/// Resuming a transcript is a cold-cache moment, so the load path still folds
/// the older screenshots into markers and rewrites a compact snapshot.
#[test]
fn reload_compacts_a_screenshot_heavy_transcript() -> Result<()> {
    let _env_lock = lock_env();
    let temp_home = tempfile::Builder::new()
        .prefix("jcode-tool-image-reload-")
        .tempdir()
        .map_err(|e| anyhow!(e))?;
    let _home = EnvVarGuard::set("JCODE_HOME", temp_home.path().as_os_str());

    let session_id = "session_tool_image_reload";
    let mut session = Session::create_with_id(session_id.to_string(), None, None);
    let screenshots = 100;
    for index in 0..screenshots {
        add_screenshot(&mut session, index);
    }
    session.save()?;
    assert_eq!(inline_image_count(&session.messages), screenshots);

    let mut loaded = Session::load(session_id)?;
    assert_eq!(loaded.messages.len(), screenshots);
    let inline = inline_image_count(&loaded.messages);
    assert!(inline >= jcode_compaction_core::TOOL_IMAGE_DECAY_KEEP_COUNT);
    assert!(inline <= jcode_compaction_core::TOOL_IMAGE_DECAY_TRIGGER_COUNT);
    // The newest screenshot survives decay and still reaches the provider.
    assert!(
        loaded
            .provider_messages()
            .last()
            .expect("provider message")
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    );
    Ok(())
}

#[test]
fn loading_legacy_transcript_decays_old_screenshots() -> Result<()> {
    let _env_lock = lock_env();
    let temp_home = tempfile::Builder::new()
        .prefix("jcode-tool-image-legacy-")
        .tempdir()
        .map_err(|e| anyhow!(e))?;
    let _home = EnvVarGuard::set("JCODE_HOME", temp_home.path().as_os_str());

    // Simulate a transcript written before decay existed by appending the raw
    // messages without going through the decaying load path.
    let session_id = "session_tool_image_legacy";
    let mut session = Session::create_with_id(session_id.to_string(), None, None);
    let screenshots = 30;
    for index in 0..screenshots {
        add_screenshot(&mut session, index);
    }
    let mut legacy = Session::create_with_id(session_id.to_string(), None, None);
    legacy.messages = session.messages.clone();
    for message in &mut legacy.messages {
        if let [ContentBlock::ToolResult { .. }, block] = message.content.as_mut_slice() {
            *block = ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "A".repeat(SCREENSHOT_CHARS),
            };
        }
    }
    assert_eq!(inline_image_count(&legacy.messages), screenshots);
    std::fs::create_dir_all(session_path(session_id)?.parent().unwrap())?;
    crate::storage::write_json(&session_path(session_id)?, &legacy)?;

    let loaded = Session::load(session_id)?;
    assert_eq!(loaded.messages.len(), screenshots);
    assert_eq!(
        inline_image_count(&loaded.messages),
        jcode_compaction_core::TOOL_IMAGE_DECAY_KEEP_COUNT
    );
    Ok(())
}
