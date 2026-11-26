use super::*;

fn render(
    session: &Session,
) -> (
    Vec<RenderedMessage>,
    Vec<RenderedImage>,
    Option<RenderedCompactedHistoryInfo>,
) {
    render_messages_and_images_with_compacted_history(session, 64)
}
fn text(s: &str) -> ContentBlock {
    ContentBlock::Text {
        text: s.into(),
        cache_control: None,
    }
}
fn image() -> ContentBlock {
    ContentBlock::Image {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    }
}
fn call(id: &str, size: usize) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: "read".into(),
        input: serde_json::json!({"file_path": " /tmp/test.png ", "intent": "inspect image", "payload": "x".repeat(size)}),
        thought_signature: Some("signature".into()),
    }
}
fn result(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: "result".into(),
        is_error: None,
    }
}
fn native() -> ContentBlock {
    ContentBlock::ProviderNative {
        provider: "openai".into(),
        item: serde_json::json!({"type":"web_search_call", "id":"native", "status":"completed", "action":{"type":"search", "query":"test"}}),
    }
}
/// (label, source, anchor, history_message_index) per rendered image.
type ImageRow = (
    Option<String>,
    RenderedImageSource,
    Option<RenderedImageAnchor>,
    Option<usize>,
);
fn image_rows(session: &Session) -> Vec<ImageRow> {
    render(session)
        .1
        .into_iter()
        .map(|i| (i.label, i.source, i.anchor, i.history_message_index))
        .collect()
}
fn tool_image(label: Option<&str>, tool_name: &str, id: &str, index: usize) -> ImageRow {
    (
        label.map(Into::into),
        RenderedImageSource::ToolResult {
            tool_name: tool_name.into(),
        },
        Some(RenderedImageAnchor::ToolCall { id: id.into() }),
        Some(index),
    )
}

#[test]
fn tool_result_images_are_attributed_to_their_call_and_label() {
    let mut s = Session::create_with_id("image-edges".into(), None, None);
    s.add_message(Role::Assistant, vec![call("a", 0), call("b", 0)]);
    s.add_message(
        Role::User,
        vec![
            image(),
            text("prompt"),
            result("a"),
            image(),
            image(),
            text("[Attached image associated with the preceding tool result: explicit.png]"),
            text("interleaved"),
            native(),
            image(),
            call("a", 8192),
            image(),
            result("a"),
            image(),
            result("missing"),
            image(),
            result("b"),
            image(),
        ],
    );
    assert_eq!(
        image_rows(&s),
        vec![
            (None, RenderedImageSource::UserInput, None, None),
            tool_image(Some("/tmp/test.png"), "read", "a", 2),
            tool_image(Some("explicit.png"), "read", "a", 2),
            tool_image(Some("/tmp/test.png"), "read", "a", 4),
            tool_image(Some("/tmp/test.png"), "read", "a", 4),
            tool_image(Some("/tmp/test.png"), "read", "a", 5),
            tool_image(None, "tool", "missing", 6),
            tool_image(Some("/tmp/test.png"), "read", "b", 7),
        ]
    );
}

#[test]
fn tool_image_label_is_absent_without_a_usable_file_path() {
    for input in [
        serde_json::json!({}),
        serde_json::json!({"file_path":"   "}),
        serde_json::json!({"file_path":42}),
    ] {
        let mut s = Session::create_with_id("fallback-label".into(), None, None);
        s.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "fallback".into(),
                name: "read".into(),
                input,
                thought_signature: None,
            }],
        );
        s.add_message(Role::User, vec![result("fallback"), image()]);
        assert_eq!(
            image_rows(&s),
            vec![tool_image(None, "read", "fallback", 1)]
        );
    }
}

#[test]
fn user_prompt_images_anchor_to_their_prompt_ordinal() {
    let mut s = Session::create_with_id("prompt-ordinals".into(), None, None);
    s.add_message(Role::User, vec![text("before"), native(), image()]);
    s.add_message(Role::User, vec![image(), text("next prompt")]);
    let user_image = |ordinal| {
        (
            None,
            RenderedImageSource::UserInput,
            Some(RenderedImageAnchor::UserPrompt { ordinal }),
            None,
        )
    };
    assert_eq!(image_rows(&s), vec![user_image(0), user_image(1)]);
}

#[test]
fn large_text_renders_intact() {
    let big = "x".repeat(256 * 1024);
    let mut s = Session::create_with_id("final-text".into(), None, None);
    s.add_message(Role::Assistant, vec![text(&big)]);
    let (messages, _, _) = render(&s);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, big);
}
