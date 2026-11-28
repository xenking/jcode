use super::*;

pub(super) struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    pub(super) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        jcode_base::env::set_var(key, value);
        Self { key, previous }
    }

    fn set_if_missing(key: &'static str, value: &str) -> Option<Self> {
        if std::env::var_os(key).is_some() {
            return None;
        }
        Some(Self::set(key, value))
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            jcode_base::env::set_var(self.key, previous);
        } else {
            jcode_base::env::remove_var(self.key);
        }
    }
}

async fn collect_live_smoke_stream(
    mut stream: EventStream,
    timeout: std::time::Duration,
) -> Result<(usize, usize, bool)> {
    tokio::time::timeout(timeout, async move {
        let mut text_bytes = 0usize;
        let mut thinking_bytes = 0usize;
        let mut saw_message_end = false;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(text) => {
                    text_bytes += text.len();
                }
                StreamEvent::ThinkingDelta(text) => {
                    thinking_bytes += text.len();
                }
                StreamEvent::MessageEnd { .. } => {
                    saw_message_end = true;
                    break;
                }
                StreamEvent::Error { message, .. } => anyhow::bail!(message),
                _ => {}
            }
        }
        Ok((text_bytes, thinking_bytes, saw_message_end))
    })
    .await
    .context("live provider smoke timed out")?
}

#[test]
fn test_parse_sse_event() {
    let mut buffer = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_string();
    let event = parse_sse_event(&mut buffer).unwrap();
    assert_eq!(event.event_type, "message_start");
    assert!(buffer.is_empty());
}

#[tokio::test]
async fn test_available_models() {
    let provider = AnthropicProvider::new();
    let models = provider.available_models();
    assert!(models.contains(&"claude-opus-4-8"));
    // Opus 4.8 is native-1M, so there is no redundant `[1m]` alias.
    assert!(!models.contains(&"claude-opus-4-8[1m]"));
    assert!(models.contains(&"claude-opus-4-6"));
    assert!(models.contains(&"claude-opus-4-6[1m]"));
    assert!(models.contains(&"claude-sonnet-4-6"));
    assert!(models.contains(&"claude-sonnet-4-6[1m]"));
    assert!(models.contains(&"claude-haiku-4-5"));
}

#[test]
fn test_effectively_1m_requires_explicit_suffix() {
    assert!(!effectively_1m("claude-opus-4-6"));
    assert!(!effectively_1m("claude-sonnet-4-6"));
    assert!(effectively_1m("claude-opus-4-6[1m]"));
    assert!(effectively_1m("claude-sonnet-4-6[1m]"));
}

#[test]
fn test_oauth_beta_headers_require_explicit_1m_suffix() {
    assert_eq!(oauth_beta_headers("claude-opus-4-6"), OAUTH_BETA_HEADERS);
    assert_eq!(
        oauth_beta_headers("claude-opus-4-6[1m]"),
        OAUTH_BETA_HEADERS_1M
    );
}

#[path = "anthropic_tests/reasoning_tests.rs"]
mod reasoning_tests;

#[test]
fn message_start_warns_when_server_substitutes_a_different_model() {
    // Anthropic can silently alias an unavailable model id to a different model
    // (observed: claude-fable-5 -> claude-haiku-4-5). When the served model
    // differs from the requested base id, we must surface a StatusDetail warning
    // so the user is not misled about which model answered.
    let mut state = SseStreamState {
        requested_model_base: "claude-fable-5".to_string(),
        ..SseStreamState::default()
    };
    let event = SseEvent {
        event_type: "message_start".to_string(),
        data: serde_json::json!({
            "type": "message_start",
            "message": {"model": "claude-haiku-4-5-20251001", "usage": {"input_tokens": 1}}
        })
        .to_string(),
    };
    let events = process_sse_event(&event, &mut state, true);
    let warned = events.iter().any(|e| {
        matches!(e, StreamEvent::StatusDetail { detail }
            if detail.contains("claude-haiku-4-5") && detail.contains("claude-fable-5"))
    });
    assert!(
        warned,
        "expected a substitution StatusDetail, got {events:?}"
    );
    assert!(state.warned_model_substitution);

    // A matching served model must NOT warn.
    let mut state = SseStreamState {
        requested_model_base: "claude-opus-4-8".to_string(),
        ..SseStreamState::default()
    };
    let event = SseEvent {
        event_type: "message_start".to_string(),
        data: serde_json::json!({
            "type": "message_start",
            "message": {"model": "claude-opus-4-8", "usage": {"input_tokens": 1}}
        })
        .to_string(),
    };
    let events = process_sse_event(&event, &mut state, true);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamEvent::StatusDetail { .. })),
        "served model matched request; must not warn"
    );
    assert!(!state.warned_model_substitution);
}

#[test]
fn resolved_model_uses_upstream_alias_target_and_deduplicates_metadata() {
    let mut state = SseStreamState {
        requested_model_base: "claude-alias".into(),
        ..Default::default()
    };
    let event = SseEvent {
        event_type: "message_start".into(),
        data: serde_json::json!({"message": {"model": "claude-actual-20260101"}}).to_string(),
    };
    let events = process_sse_event(&event, &mut state, false);
    assert!(events.iter().any(|event| matches!(event,
        StreamEvent::ResolvedModel { model } if model == "claude-actual-20260101")));
    assert!(
        !process_sse_event(&event, &mut state, false)
            .iter()
            .any(|event| matches!(event, StreamEvent::ResolvedModel { .. }))
    );
}

#[test]
fn resolved_model_does_not_infer_missing_or_empty_metadata() {
    for message in [serde_json::json!({}), serde_json::json!({"model": " "})] {
        let mut state = SseStreamState {
            requested_model_base: "claude-alias".into(),
            ..Default::default()
        };
        let event = SseEvent {
            event_type: "message_start".into(),
            data: serde_json::json!({"message": message}).to_string(),
        };
        assert!(
            !process_sse_event(&event, &mut state, false)
                .iter()
                .any(|event| matches!(event, StreamEvent::ResolvedModel { .. }))
        );
    }
}

#[test]
fn test_anthropic_thinking_sse_events() {
    let mut state = SseStreamState::default();
    let start = SseEvent {
        event_type: "content_block_start".to_string(),
        data: serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking", "thinking": "", "signature": "sig"}
        })
        .to_string(),
    };
    let events = process_sse_event(&start, &mut state, false);
    assert!(matches!(events.as_slice(), [StreamEvent::ThinkingStart]));
    assert!(state.current_thinking_block);

    let delta = SseEvent {
        event_type: "content_block_delta".to_string(),
        data: serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "reasoning text"}
        })
        .to_string(),
    };
    let events = process_sse_event(&delta, &mut state, false);
    assert!(
        matches!(events.as_slice(), [StreamEvent::ThinkingDelta(text)] if text == "reasoning text")
    );

    let signature = SseEvent {
        event_type: "content_block_delta".to_string(),
        data: serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "signed"}
        })
        .to_string(),
    };
    let events = process_sse_event(&signature, &mut state, false);
    assert!(
        matches!(events.as_slice(), [StreamEvent::ThinkingSignatureDelta(sig)] if sig == "signed")
    );

    let stop = SseEvent {
        event_type: "content_block_stop".to_string(),
        data: serde_json::json!({"type": "content_block_stop", "index": 0}).to_string(),
    };
    let events = process_sse_event(&stop, &mut state, false);
    assert!(matches!(events.as_slice(), [StreamEvent::ThinkingEnd]));
    assert!(!state.current_thinking_block);
}

#[test]
fn test_anthropic_signed_thinking_replayed_in_request_blocks() {
    let provider = AnthropicProvider::new();
    let blocks = provider.format_content_blocks(
        &[ContentBlock::AnthropicThinking {
            thinking: "reasoning text".to_string(),
            signature: "signed".to_string(),
        }],
        false,
    );

    let value = serde_json::to_value(&blocks).expect("serialize content blocks");
    assert_eq!(
        value,
        serde_json::json!([
            {
                "type": "thinking",
                "thinking": "reasoning text",
                "signature": "signed"
            }
        ])
    );
}

#[tokio::test]
#[ignore = "live smoke: requires ANTHROPIC_API_KEY, or set JCODE_LIVE_ANTHROPIC_ALLOW_OAUTH=1 to use Claude OAuth credentials"]
async fn live_anthropic_reasoning_smoke() -> Result<()> {
    let _env_lock = jcode_base::storage::lock_test_env();
    let using_api_key = std::env::var_os("ANTHROPIC_API_KEY").is_some();
    let allow_oauth = std::env::var_os("JCODE_LIVE_ANTHROPIC_ALLOW_OAUTH").is_some();
    if !using_api_key && !allow_oauth {
        eprintln!(
            "skipping live Anthropic smoke: set ANTHROPIC_API_KEY or JCODE_LIVE_ANTHROPIC_ALLOW_OAUTH=1"
        );
        return Ok(());
    }

    let _max_tokens = EnvVarGuard::set_if_missing("JCODE_ANTHROPIC_MAX_TOKENS", "2048");
    let model = std::env::var("JCODE_LIVE_ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("JCODE_ANTHROPIC_MODEL"))
        .unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
    let effort = std::env::var("JCODE_LIVE_ANTHROPIC_REASONING_EFFORT")
        .unwrap_or_else(|_| "low".to_string());
    let prompt = std::env::var("JCODE_LIVE_ANTHROPIC_PROMPT")
        .unwrap_or_else(|_| "Live smoke test: answer exactly OK.".to_string());
    let system = std::env::var("JCODE_LIVE_ANTHROPIC_SYSTEM").unwrap_or_else(|_| {
        "You are a live provider smoke test. Keep the answer tiny.".to_string()
    });
    let require_thinking = std::env::var_os("JCODE_LIVE_ANTHROPIC_REQUIRE_THINKING").is_some();

    let provider = AnthropicProvider::new();
    provider.set_model(&model)?;
    // Some models (e.g. Fable 5) legitimately reject any reasoning effort. Treat
    // that as "use the model default" so the live call still exercises the model
    // rather than aborting the smoke test before any request is sent.
    if let Err(err) = provider.set_reasoning_effort(&effort) {
        eprintln!(
            "model {model} does not support reasoning effort '{effort}' ({err}); using model default"
        );
    }

    let messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: prompt,
            cache_control: None,
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }];

    let stream = provider.complete(&messages, &[], &system, None).await?;
    let (text_bytes, thinking_bytes, saw_message_end) =
        collect_live_smoke_stream(stream, std::time::Duration::from_secs(90)).await?;

    eprintln!(
        "live Anthropic reasoning smoke passed: model={model}, effort={effort}, text_bytes={text_bytes}, thinking_bytes={thinking_bytes}, message_end={saw_message_end}"
    );
    assert!(
        text_bytes > 0 || thinking_bytes > 0,
        "live Anthropic response contained neither text nor thinking deltas"
    );
    if require_thinking {
        assert!(
            thinking_bytes > 0,
            "live Anthropic response did not include thinking deltas despite JCODE_LIVE_ANTHROPIC_REQUIRE_THINKING"
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_dangling_tool_use_repair() {
    let provider = AnthropicProvider::new();

    // Create messages with a dangling tool_use (no corresponding tool_result)
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check".to_string(),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: "tool_123".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "tool_456".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"file_path": "/tmp/test"}),
                    thought_signature: None,
                },
            ]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        // Missing tool_results for tool_123 and tool_456!
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    // Should have 3 messages:
    // 1. User: "Hello"
    // 2. Assistant: text + tool_uses
    // 3. User: synthetic tool_results for the dangling tool_uses
    assert_eq!(formatted.len(), 3);

    // Check the synthetic tool_result message
    let synthetic_msg = &formatted[2];
    assert_eq!(synthetic_msg.role, "user");
    assert_eq!(synthetic_msg.content.len(), 2);

    // Verify both tool_results are present
    let mut found_ids = std::collections::HashSet::new();
    for block in &synthetic_msg.content {
        if let ApiContentBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
            ..
        } = block
        {
            found_ids.insert(tool_use_id.clone());
            assert!(is_error);
            match content {
                ToolResultContent::Text(t) => assert!(t.contains("interrupted")),
                ToolResultContent::Blocks(_) => panic!("Expected text content"),
            }
        } else {
            panic!("Expected ToolResult block");
        }
    }
    assert!(found_ids.contains("tool_123"));
    assert!(found_ids.contains("tool_456"));
}

#[tokio::test]
async fn test_orphaned_tool_result_is_rewritten_as_text() {
    // Mirrors a real stuck session: the assistant called tool_a, the interrupt
    // repair answered it, then a late result for a tool_use that is not in the
    // transcript was persisted right after. Anthropic 400s on that orphan.
    let provider = AnthropicProvider::new();
    let msg = |role, content: Vec<ContentBlock>| Message {
        role,
        content: content.into(),
        timestamp: None,
        tool_duration_ms: None,
    };
    let messages = vec![
        msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "go".to_string(),
                cache_control: None,
            }],
        ),
        msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "tool_a".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
        ),
        msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "tool_a".to_string(),
                content: "ok".to_string(),
                is_error: None,
            }],
        ),
        msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "tool_ghost".to_string(),
                content: "no leftovers".to_string(),
                is_error: None,
            }],
        ),
    ];

    let formatted = provider.format_messages(&messages, false, &[]);
    let last = formatted.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(matches!(
        &last.content[0],
        ApiContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tool_a"
    ));
    for block in &last.content {
        if let ApiContentBlock::ToolResult { tool_use_id, .. } = block {
            assert_ne!(tool_use_id, "tool_ghost");
        }
    }
    assert!(last.content.iter().any(|b| matches!(
        b,
        ApiContentBlock::Text { text, .. } if text.contains("tool_ghost") && text.contains("no leftovers")
    )));
}

/// Assert the formatted history alternates roles and that every tool_use is
/// answered by a tool_result in the immediately following user message.
fn assert_tool_uses_answered_in_next_message(formatted: &[ApiMessage]) {
    for pair in formatted.windows(2) {
        assert_ne!(pair[0].role, pair[1].role, "roles must alternate");
    }
    for (i, msg) in formatted.iter().enumerate() {
        if msg.role != "assistant" {
            continue;
        }
        for block in &msg.content {
            let ApiContentBlock::ToolUse { id, .. } = block else {
                continue;
            };
            let next = formatted
                .get(i + 1)
                .unwrap_or_else(|| panic!("tool_use {id} has no next message"));
            assert_eq!(next.role, "user", "tool_use {id} not followed by user");
            assert!(
                next.content.iter().any(|b| matches!(
                    b,
                    ApiContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                )),
                "tool_use {id} not answered in message {}",
                i + 1
            );
        }
    }
}

#[tokio::test]
async fn test_displaced_tool_result_still_answers_tool_use_in_place() {
    // The only result for tool_x arrives after another assistant turn. The
    // dangling repair sees a result somewhere and skips it, and the orphan
    // rewrite turns the late result into text, so tool_x would otherwise be
    // left without a tool_result right after it (HTTP 400).
    let provider = AnthropicProvider::new();
    let msg = |role, content: Vec<ContentBlock>| Message {
        role,
        content: content.into(),
        timestamp: None,
        tool_duration_ms: None,
    };
    let text = |t: &str| ContentBlock::Text {
        text: t.to_string(),
        cache_control: None,
    };
    let messages = vec![
        msg(Role::User, vec![text("go")]),
        msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "tool_x".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
        ),
        msg(Role::User, vec![text("are you there?")]),
        msg(Role::Assistant, vec![text("yes")]),
        msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "tool_x".to_string(),
                content: "late output".to_string(),
                is_error: None,
            }],
        ),
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    let next = &formatted[2];
    assert_eq!(formatted[1].role, "assistant");
    assert_eq!(next.role, "user");
    // The late result is now moved up to directly follow its call (real
    // output, not a synthetic error), ahead of the interjected user text.
    assert!(matches!(
        &next.content[0],
        ApiContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "tool_x"
    ));
    assert!(next.content.iter().any(|b| matches!(
        b,
        ApiContentBlock::Text { text, .. } if text == "are you there?"
    )));
    let dump = serde_json::to_string(&formatted).unwrap();
    assert!(dump.contains("late output"), "{dump}");
    assert!(
        !dump.contains("Recovered orphaned tool output"),
        "the result was paired, nothing should be rewritten: {dump}"
    );

    assert_tool_uses_answered_in_next_message(&formatted);
}

#[tokio::test]
async fn test_no_repair_when_tool_results_present() {
    let provider = AnthropicProvider::new();

    // Create messages where tool_use has a corresponding tool_result
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool_123".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_123".to_string(),
                content: "file1.txt\nfile2.txt".to_string(),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    // Should have exactly 3 messages (no synthetic ones added)
    assert_eq!(formatted.len(), 3);

    // The last message should be the actual tool_result, not synthetic
    let last_msg = &formatted[2];
    if let ApiContentBlock::ToolResult { content, .. } = &last_msg.content[0] {
        match content {
            ToolResultContent::Text(t) => assert!(t.contains("file1.txt")),
            ToolResultContent::Blocks(_) => panic!("Expected text content"),
        }
    } else {
        panic!("Expected ToolResult block");
    }
}

#[tokio::test]
async fn test_parallel_image_tool_results_stay_contiguous() {
    // Regression for Anthropic 400: "`tool_use` ids were found without `tool_result`
    // blocks immediately after". When the assistant issues several parallel `read`
    // calls that return images, each tool result is stored as its own user message in
    // the form [tool_result, image, "[Attached image ...]" text]. After merging the
    // consecutive user messages, the sibling label text blocks were wedged between the
    // tool_results, which Anthropic rejects. The label must be folded into the
    // tool_result content so every tool_result stays contiguous.
    let provider = AnthropicProvider::new();

    let make_image_result = |id: &str, label: &str| Message {
        role: Role::User,
        content: vec![
            ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: format!("Image: {label}"),
                is_error: None,
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            },
            ContentBlock::Text {
                text: format!(
                    "[Attached image associated with the preceding tool result: {label}]"
                ),
                cache_control: None,
            },
        ]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    };

    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "tool_a".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"file_path": "a.png"}),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "tool_b".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"file_path": "b.png"}),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "tool_c".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"file_path": "c.png"}),
                    thought_signature: None,
                },
            ]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        make_image_result("tool_a", "a.png"),
        make_image_result("tool_b", "b.png"),
        make_image_result("tool_c", "c.png"),
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    // assistant message + merged user tool_result message
    assert_eq!(formatted.len(), 2);
    let user_msg = &formatted[1];
    assert_eq!(user_msg.role, "user");

    // Every block in the user message must be a tool_result (no sibling text blocks
    // wedged between them), and all three tool_use ids must be present.
    let mut seen = std::collections::HashSet::new();
    for block in &user_msg.content {
        match block {
            ApiContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                seen.insert(tool_use_id.clone());
                // Each image tool_result should carry its image and folded label text.
                match content {
                    ToolResultContent::Blocks(blocks) => {
                        assert!(
                            blocks
                                .iter()
                                .any(|b| matches!(b, ToolResultContentBlock::Image { .. })),
                            "image tool_result should contain an image block"
                        );
                        assert!(
                            blocks.iter().any(|b| matches!(
                                b,
                                ToolResultContentBlock::Text { text }
                                    if text.contains("[Attached image associated")
                            )),
                            "label text should be folded into the tool_result content"
                        );
                    }
                    ToolResultContent::Text(_) => {
                        panic!("image tool_result should use block content")
                    }
                }
            }
            _ => panic!("expected only tool_result blocks in the user message"),
        }
    }
    assert_eq!(
        seen,
        ["tool_a", "tool_b", "tool_c"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::HashSet<_>>()
    );
}

#[test]
fn test_cache_breakpoint_no_messages() {
    let mut messages: Vec<ApiMessage> = vec![];
    add_message_cache_breakpoint(&mut messages);
    // Should not panic, just return early
    assert!(messages.is_empty());
}

#[test]
fn test_cache_breakpoint_before_first_assistant_response() {
    let mut messages = vec![
        ApiMessage {
            role: "user".to_string(),
            content: vec![ApiContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![ApiContentBlock::Text {
                text: "World".to_string(),
                cache_control: None,
            }],
        },
    ];
    add_message_cache_breakpoint(&mut messages);
    let wire = serde_json::to_value(&messages).unwrap();
    assert!(wire[0]["content"][0].get("cache_control").is_none());
    assert_eq!(wire[1]["content"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn cache_breakpoint_finds_latest_eligible_block_in_assistant_prefill() {
    let mut messages = vec![ApiMessage {
        role: "assistant".into(),
        content: vec![
            ApiContentBlock::Text {
                text: "Running command".into(),
                cache_control: None,
            },
            ApiContentBlock::ToolUse {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
                cache_control: None,
            },
            ApiContentBlock::Thinking {
                thinking: "more context".into(),
                signature: "signed".into(),
            },
        ],
    }];
    add_message_cache_breakpoint(&mut messages);
    let wire = serde_json::to_value(&messages).unwrap();
    assert!(wire[0]["content"][0].get("cache_control").is_none());
    assert_eq!(wire[0]["content"][1]["cache_control"]["type"], "ephemeral");
    assert!(wire[0]["content"][2].get("cache_control").is_none());
}

#[test]
fn cache_system_prefix_keeps_only_static_and_oauth_attribution() {
    for oauth in [false, true] {
        let Some(ApiSystem::Blocks(blocks)) = build_system_param("stable instructions", oauth)
        else {
            panic!("expected system blocks");
        };
        assert_eq!(blocks.len(), 1 + usize::from(oauth));
        if oauth {
            assert!(blocks[0].cache_control.is_none());
            assert!(blocks[0].text.starts_with("x-anthropic-billing-header: "));
            assert!(!blocks[0].text.contains("Claude Code"));
        }
        assert_eq!(blocks.last().unwrap().text, "stable instructions");
        assert!(blocks.last().unwrap().cache_control.is_some());
    }
}

#[test]
fn cache_keeps_prior_request_user_marker_and_advances_tail_with_two_slots() {
    let mut history = vec![Message::user("first question")];
    for round in 0..20 {
        let messages = jcode_provider_anthropic::format_messages_for_request(
            &history,
            false,
            &[],
            false,
            "",
            true,
        );
        let wire = serde_json::to_value(&messages).unwrap();
        let indices: Vec<_> = wire
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|block| block.get("cache_control").is_some())
                    .then_some(index)
            })
            .collect();
        let tail = 2 * round;
        assert_eq!(
            indices,
            if round == 0 {
                vec![0]
            } else {
                vec![tail - 2, tail]
            }
        );
        history.push(Message {
            role: jcode_base::message::Role::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("answer {round}"),
                cache_control: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
        history.push(Message::user(&format!("question {round}")));
    }
}

#[tokio::test]
async fn test_sanitize_tool_ids_with_dots() {
    let provider = AnthropicProvider::new();

    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "chatcmpl-BF2xX.tool_call.0".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "chatcmpl-BF2xX.tool_call.0".to_string(),
                content: "file1.txt".to_string(),
                is_error: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    let sanitized_id = "chatcmpl-BF2xX_tool_call_0";
    for msg in &formatted {
        for block in &msg.content {
            match block {
                ApiContentBlock::ToolUse { id, .. } => {
                    assert_eq!(id, sanitized_id);
                }
                ApiContentBlock::ToolResult { tool_use_id, .. } => {
                    assert_eq!(tool_use_id, sanitized_id);
                }
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn test_sanitize_dangling_tool_ids_with_dots() {
    let provider = AnthropicProvider::new();

    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call.with.dots".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "crash"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
    ];

    let formatted = provider.format_messages(&messages, false, &[]);

    let sanitized_id = "call_with_dots";
    for msg in &formatted {
        for block in &msg.content {
            match block {
                ApiContentBlock::ToolUse { id, .. } => {
                    assert_eq!(id, sanitized_id);
                }
                ApiContentBlock::ToolResult { tool_use_id, .. } => {
                    assert_eq!(tool_use_id, sanitized_id);
                }
                _ => {}
            }
        }
    }
}

/// The runtime-provider identity that `set_credential_mode` writes must decode
/// back to the exact same credential mode. This guards the model picker / header
/// widget from reporting OAuth when an API key is in use (or vice versa): the
/// env key is the single source of truth those surfaces read, so an asymmetric
/// mapping here would surface an inaccurate auth method to the user.
#[test]
fn credential_mode_runtime_provider_identity_round_trips() {
    let _guard = jcode_base::storage::lock_test_env();
    let previous = std::env::var_os("JCODE_RUNTIME_PROVIDER");

    jcode_base::env::set_var("JCODE_RUNTIME_PROVIDER", "claude");
    assert_eq!(
        AnthropicCredentialMode::from_runtime_env(jcode_provider_core::DualAuthProvider::Anthropic),
        AnthropicCredentialMode::OAuth,
        "OAuth selection must surface as the OAuth runtime identity"
    );

    jcode_base::env::set_var("JCODE_RUNTIME_PROVIDER", "claude-api");
    assert_eq!(
        AnthropicCredentialMode::from_runtime_env(jcode_provider_core::DualAuthProvider::Anthropic),
        AnthropicCredentialMode::ApiKey,
        "API-key selection must surface as the API-key runtime identity"
    );

    match previous {
        Some(value) => jcode_base::env::set_var("JCODE_RUNTIME_PROVIDER", value),
        None => jcode_base::env::remove_var("JCODE_RUNTIME_PROVIDER"),
    }
}

#[tokio::test]
async fn auto_mode_falls_back_to_api_key_when_oauth_is_expired() {
    let _guard = jcode_base::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());
    let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", "test-anthropic-api-key");
    let _runtime = EnvVarGuard::set("JCODE_RUNTIME_PROVIDER", "auto");

    jcode_base::auth::claude::upsert_account(jcode_base::auth::claude::AnthropicAccount {
        label: "claude-1".to_string(),
        access: "expired-oauth-access".to_string(),
        refresh: String::new(),
        expires: 0,
        email: None,
        subscription_type: Some("max".to_string()),
        scopes: vec!["user:inference".to_string()],
    })
    .unwrap();

    let provider = AnthropicProvider::new();
    assert_eq!(
        provider.credential_mode_snapshot(),
        AnthropicCredentialMode::Auto
    );
    assert_eq!(provider.active_resolved_credential(), None);

    let (token, is_oauth) = provider.get_access_token().await.unwrap();
    assert_eq!(token, "test-anthropic-api-key");
    assert!(
        !is_oauth,
        "automatic fallback must use API-key request semantics"
    );
    assert_eq!(
        provider.active_resolved_credential(),
        Some(jcode_provider_core::ResolvedCredential::ApiKey),
        "Auto must expose the actual API fallback rather than credential presence"
    );
    assert_eq!(provider.fork().active_resolved_credential(), None);
    provider.invalidate_credentials().await;
    assert_eq!(provider.active_resolved_credential(), None);
}

#[tokio::test]
async fn explicit_oauth_mode_does_not_silently_fall_back_to_api_key() {
    let _guard = jcode_base::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());
    let _api_key = EnvVarGuard::set("ANTHROPIC_API_KEY", "test-anthropic-api-key");
    let _runtime = EnvVarGuard::set("JCODE_RUNTIME_PROVIDER", "claude");

    jcode_base::auth::claude::upsert_account(jcode_base::auth::claude::AnthropicAccount {
        label: "claude-1".to_string(),
        access: "expired-oauth-access".to_string(),
        refresh: String::new(),
        expires: 0,
        email: None,
        subscription_type: Some("max".to_string()),
        scopes: vec!["user:inference".to_string()],
    })
    .unwrap();

    let provider = AnthropicProvider::new();
    assert_eq!(
        provider.credential_mode_snapshot(),
        AnthropicCredentialMode::OAuth
    );
    assert_eq!(
        provider.active_resolved_credential(),
        Some(jcode_provider_core::ResolvedCredential::Oauth)
    );

    let error = provider.get_access_token().await.unwrap_err().to_string();
    assert!(error.contains("expired"), "unexpected error: {error}");
}

#[tokio::test]
async fn rejected_refresh_preserves_access_until_real_expiry() {
    let _guard = jcode_base::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());
    let _runtime = EnvVarGuard::set("JCODE_RUNTIME_PROVIDER", "claude");
    let refresh = "test-permanently-rejected-refresh";
    jcode_base::auth::refresh_state::record_permanent_rejection("claude", refresh, "invalid_grant")
        .unwrap();
    for expired in [false, true] {
        jcode_base::auth::claude::upsert_account(jcode_base::auth::claude::AnthropicAccount {
            label: "claude-otter".into(),
            access: "test-live-access".into(),
            refresh: refresh.into(),
            expires: chrono::Utc::now().timestamp_millis()
                + if expired { -60_000 } else { 120_000 },
            email: None,
            subscription_type: Some("max".into()),
            scopes: vec!["user:inference".into()],
        })
        .unwrap();
        let provider = AnthropicProvider::new();
        let result = provider.get_oauth_access_token().await;
        if expired {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("previously rejected")
            );
        } else {
            assert_eq!(result.unwrap(), ("test-live-access".into(), true));
        }
    }
}

#[test]
fn test_anthropic_fable_5_sends_reasoning_fields() {
    // `claude-fable-5` rejected reasoning fields during its preview, but the
    // released model accepts an adaptive `thinking` block and an
    // `output_config` effort (verified live 2026-07-01). The request builder
    // must send both when an effort is configured.
    let provider = AnthropicProvider::new();
    *provider.reasoning_effort.write().unwrap() = Some("high".to_string());

    let (thinking, output_config, temperature) =
        provider.build_reasoning_request_parts_inner("claude-fable-5", true, false);
    assert!(
        matches!(thinking, Some(ApiThinking::Adaptive { .. })),
        "Fable 5 should send an adaptive thinking block"
    );
    assert_eq!(
        output_config.as_ref().map(|c| c.effort.as_str()),
        Some("high"),
        "Fable 5 should send the configured output_config effort"
    );
    assert_eq!(temperature, None);

    // Fable 5 supports the real `max` API level, so `max` is sent verbatim.
    *provider.reasoning_effort.write().unwrap() = Some("max".to_string());
    let (_thinking, output_config, _temp) =
        provider.build_reasoning_request_parts_inner("claude-fable-5", true, false);
    assert_eq!(
        output_config.as_ref().map(|c| c.effort.as_str()),
        Some("max")
    );

    // The effort picker surfaces levels for Fable 5.
    assert!(AnthropicProvider::model_supports_reasoning_effort(
        "claude-fable-5"
    ));
}

#[test]
fn detects_anthropic_reasoning_unsupported_errors() {
    // The real 400 bodies returned when Fable 5 is sent reasoning fields.
    let thinking_400 = "anthropic api error (400 bad request): {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"adaptive thinking is not supported on this model\"}}";
    assert!(is_reasoning_unsupported_error(thinking_400));
    let effort_400 = "anthropic api error (400 bad request): {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"this model does not support the effort parameter.\"}}";
    assert!(is_reasoning_unsupported_error(effort_400));

    // Unrelated 400s must not trigger the reasoning self-heal path.
    assert!(!is_reasoning_unsupported_error(
        "anthropic api error (400 bad request): {\"type\":\"invalid_request_error\",\"message\":\"max_tokens too large\"}"
    ));
    // A thinking-mentioning error that is not a 400 must not match either.
    assert!(!is_reasoning_unsupported_error(
        "anthropic api error (429 too many requests): rate_limit on thinking budget"
    ));
    // Model-not-found is a different recovery path.
    assert!(!is_reasoning_unsupported_error(
        "anthropic api error (404 not found): {\"type\":\"not_found_error\",\"message\":\"model not found\"}"
    ));
}

#[test]
fn detects_anthropic_model_not_found_errors() {
    // The real 404 body returned when a model id was retired (e.g. Fable 5).
    let real = "anthropic api error (404 not found): {\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"claude fable 5 is not available. please use opus 4.8.\"}}";
    assert!(is_model_not_found_error(real));

    // Structural marker alone (lowercased error chain).
    assert!(is_model_not_found_error(
        "model claude-foo not found (not_found_error)"
    ));

    // Unrelated failures must not trigger the model fallback path.
    assert!(!is_model_not_found_error(
        "anthropic api error (401 unauthorized): invalid authentication credentials"
    ));
    assert!(!is_model_not_found_error(
        "anthropic api error (429 too many requests): rate_limit"
    ));
    assert!(!is_model_not_found_error(
        "anthropic api error (404 not found): resource missing"
    ));
}

#[test]
fn anthropic_fallback_prefers_best_available_and_skips_tried_and_retired() {
    // The fallback logic reads the process-global model catalog; lock and
    // reset it so fixture models hydrated by other tests cannot leak in.
    let _guard = jcode_base::storage::lock_test_env();
    jcode_base::provider::models::reset_model_catalog_services_for_tests();
    let known = jcode_base::provider::known_anthropic_model_ids();
    assert!(
        !known.is_empty(),
        "expected a non-empty Anthropic model catalog"
    );

    // With nothing tried, the fallback offers the highest-quality (flagship)
    // model, NOT merely the first catalog entry. The curated order ranks Opus
    // ahead of Haiku, so the chosen model must not be a Haiku/retired tier when
    // a stronger one exists.
    let first = anthropic_fallback_model(&[], "").expect("a fallback should exist");
    let first_key = AnthropicProvider::normalized_model_key(&first);
    assert!(
        !first_key.contains("haiku"),
        "fallback must not downgrade to Haiku when a flagship is available, got {first}"
    );
    assert!(
        !anthropic_model_is_retired(&first),
        "fallback must never pick a retired model, got {first}"
    );

    // A retired model in `tried` must never be re-offered, and the result must
    // skip retired families entirely.
    let next = anthropic_fallback_model(&["claude-mythos-1".to_string()], "")
        .expect("another fallback should exist");
    assert!(!anthropic_model_is_retired(&next));

    // Exhausting every viable known model yields None.
    let exhausted = anthropic_fallback_model(&known, "");
    assert!(
        exhausted.is_none(),
        "no fallback should remain once all known models are tried, got {exhausted:?}"
    );
}

#[test]
fn anthropic_fallback_honors_server_recommendation() {
    // The recommendation matcher scores hints against the process-global model
    // catalog; lock and reset it so fixture models hydrated by other tests
    // (e.g. claude-opus-5-preview) cannot outrank the real catalog entries.
    let _guard = jcode_base::storage::lock_test_env();
    jcode_base::provider::models::reset_model_catalog_services_for_tests();
    // The real 404 body recommends a specific replacement model. We must honor
    // it over the generic quality ranking.
    let body = "anthropic api error (404 not found): {\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"claude fable 5 is not available. please use opus 4.8. learn more: https://anthropic.com\"}}";
    let recommended =
        anthropic_recommended_model_from_error(body).expect("should parse a recommendation");
    assert_eq!(
        AnthropicProvider::normalized_model_key(&recommended),
        "claude-opus-4-8",
        "server recommendation 'Opus 4.8' should map to claude-opus-4-8"
    );

    // A server recommendation cannot override automatic failover exclusions.
    let fallback = anthropic_fallback_model(&["claude-mythos-1".to_string()], body)
        .expect("a fallback should exist");
    assert!(!AnthropicProvider::normalized_model_key(&fallback).contains("opus"));

    let opus_55 = anthropic_recommended_model_from_error("please use opus 5.5. learn more")
        .expect("decimal release recommendation should resolve");
    assert_eq!(
        AnthropicProvider::normalized_model_key(&opus_55),
        "claude-opus-5-5"
    );

    // A recommendation pointing at a retired model is ignored (falls through to
    // quality ranking).
    let retired_rec = "model x not available. please use mythos 1.";
    assert!(
        anthropic_recommended_model_from_error(retired_rec).is_none()
            || !anthropic_model_is_retired(
                &anthropic_recommended_model_from_error(retired_rec).unwrap()
            )
    );

    // No recommendation phrase -> None.
    assert!(anthropic_recommended_model_from_error("429 too many requests").is_none());
}

#[test]
fn anthropic_quality_rank_orders_opus_before_haiku_and_retired_last() {
    let opus = anthropic_model_quality_rank("claude-opus-4-8");
    let sonnet = anthropic_model_quality_rank("claude-sonnet-4-6");
    let haiku = anthropic_model_quality_rank("claude-haiku-4-5");
    let retired = anthropic_model_quality_rank("claude-mythos-1");
    // Fable 5 is live again and curated as the flagship, so it ranks first.
    let fable = anthropic_model_quality_rank("claude-fable-5");
    assert!(
        fable <= opus,
        "Fable 5 should rank at or ahead of Opus ({fable} vs {opus})"
    );
    assert!(
        opus < sonnet,
        "Opus should outrank Sonnet ({opus} vs {sonnet})"
    );
    assert!(
        sonnet < haiku,
        "Sonnet should outrank Haiku ({sonnet} vs {haiku})"
    );
    assert!(
        haiku < retired,
        "retired models must sort last ({haiku} vs {retired})"
    );
    assert_eq!(retired, usize::MAX);
    // Dated live ids must rank like their canonical base.
    assert_eq!(
        anthropic_model_quality_rank("claude-haiku-4-5-20251001"),
        haiku
    );
}

#[test]
fn fable_quota_fallback_excludes_opus_by_default() {
    let _guard = jcode_base::storage::lock_test_env();
    assert!(AnthropicProvider::best_available_opus_model("claude-fable-5").is_none());
}

#[test]
fn model_scoped_usage_does_not_route_exhausted_fable_to_opus() {
    let _guard = jcode_base::storage::lock_test_env();
    let usage = jcode_base::usage::UsageData {
        model_scoped: vec![jcode_base::usage::ModelScopedUsageWindow {
            model_name: "Fable".to_string(),
            utilization: 1.0,
            resets_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
        }],
        ..Default::default()
    };
    assert!(AnthropicProvider::fallback_for_model_scoped_usage("claude-fable-5", &usage).is_none());
    assert!(
        AnthropicProvider::fallback_for_model_scoped_usage("claude-opus-5", &usage).is_none(),
        "an exhausted Fable scope must not reroute an explicitly selected Opus"
    );

    let available = jcode_base::usage::UsageData {
        model_scoped: vec![jcode_base::usage::ModelScopedUsageWindow {
            model_name: "Fable".to_string(),
            utilization: 0.98,
            resets_at: None,
        }],
        ..Default::default()
    };
    assert!(
        AnthropicProvider::fallback_for_model_scoped_usage("claude-fable-5", &available).is_none(),
        "Fable must remain selected while its scoped quota is available"
    );
}

#[test]
fn detects_live_fable_scoped_limit_errors_without_misrouting_other_limits() {
    assert!(is_fable_scoped_limit_error(
        "claude-fable-5",
        r#"429 {"type":"rate_limit_error","message":"You have reached your weekly Fable limit"}"#,
    ));
    assert!(is_fable_scoped_limit_error(
        "claude-fable-5",
        "usage limit reached for the 7-day model window",
    ));
    assert!(!is_fable_scoped_limit_error(
        "claude-opus-5",
        "weekly Fable rate limit reached",
    ));
    assert!(!is_fable_scoped_limit_error(
        "claude-fable-5",
        "429 overloaded_error: service temporarily overloaded",
    ));
    assert!(!is_fable_scoped_limit_error(
        "claude-fable-5",
        "global 5-hour rate limit reached",
    ));
}

#[test]
fn ping_keepalive_emits_streaming_phase_event() {
    // Issue #451: during silent reasoning phases, `ping` events can be the
    // only upstream traffic. They must surface as a StreamEvent so the client
    // stall guard sees activity instead of cancelling a healthy stream.
    let mut state = SseStreamState::default();
    let event = SseEvent {
        event_type: "ping".to_string(),
        data: r#"{"type": "ping"}"#.to_string(),
    };
    let events = process_sse_event(&event, &mut state, true);
    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::ConnectionPhase {
                phase: jcode_message_types::ConnectionPhase::Streaming
            }
        )),
        "expected ping to emit a Streaming ConnectionPhase event, got {events:?}"
    );
}

#[test]
fn test_anthropic_opus_5_low_effort_reaches_the_wire() {
    // Benchmark campaigns pin `claude-opus-5` at `low` effort. Opus 5 also
    // *defaults* to `low`, and an
    // explicit `low` must survive normalization, must NOT be silently
    // promoted, and must land in `output_config.effort` on the request.
    assert!(AnthropicProvider::model_supports_output_effort(
        "claude-opus-5"
    ));
    assert_eq!(
        AnthropicProvider::default_reasoning_effort_for_model("claude-opus-5").as_deref(),
        Some("low"),
    );
    // Opus 5.5 is jcode's default Claude model and also defaults to `low`.
    assert_eq!(
        AnthropicProvider::default_reasoning_effort_for_model("claude-opus-5-5").as_deref(),
        Some("low"),
    );
    assert_eq!(
        AnthropicProvider::normalize_reasoning_effort("low").as_deref(),
        Some("low"),
    );
    // Downward selection is never clamped upward toward the model default.
    assert_eq!(
        AnthropicProvider::actual_effort_for_model("claude-opus-5", "low"),
        "low",
    );
    assert_eq!(
        AnthropicProvider::store_effort_for_model("claude-opus-5", "low"),
        "low",
    );

    let provider = AnthropicProvider::new();
    *provider
        .model
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = "claude-opus-5".to_string();
    provider.set_reasoning_effort("low").unwrap();
    assert_eq!(provider.reasoning_effort().as_deref(), Some("low"));

    let (thinking, output_config, _temp) =
        provider.build_reasoning_request_parts_inner("claude-opus-5", true, false);
    assert_eq!(
        output_config
            .expect("explicit low effort should set output_config")
            .effort,
        "low",
    );
    // Opus 5 rejects `thinking.type.enabled`; it requires adaptive thinking.
    assert!(matches!(thinking, Some(ApiThinking::Adaptive { .. })));
}

/// A `content_block_start` carrying an unrecognized block type must still
/// deserialize. Before the `Unknown` catch-all the whole event failed to parse
/// and was dropped, so an unknown *tool* block produced a turn that reported
/// `stop_reason: tool_use` with no tool call for the agent to run.
#[test]
fn test_anthropic_unknown_content_block_start_does_not_drop_event() {
    // Server tool blocks (`server_tool_use`, `web_search_tool_result`) are
    // captured for replay; see native_web_search_sse_tests.rs.
    for block_type in ["some_future_block", "code_execution_tool_result_future"] {
        let mut state = SseStreamState::default();
        let event = SseEvent {
            event_type: "content_block_start".to_string(),
            data: serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": block_type, "id": "srvtoolu_1", "name": "web_search"}
            })
            .to_string(),
        };
        let events = process_sse_event(&event, &mut state, false);
        assert!(
            events.is_empty(),
            "{block_type}: unknown block must not synthesize stream events"
        );
        assert!(
            state.current_tool_use.is_none(),
            "{block_type}: unknown block must not start tool accumulation"
        );
        assert!(
            !state.current_thinking_block,
            "{block_type}: unknown block must not start a thinking block"
        );
    }
}

#[test]
fn configured_swarm_root_effort_controls_adaptive_and_manual_thinking() {
    let mut provider = AnthropicProvider::new();
    provider.max_tokens_override = Some(32_768);
    for mode in ["swarm"] {
        *provider.reasoning_effort.write().unwrap() = Some(mode.to_string());
        for (effort, budget) in [
            ("minimal", 1_024),
            ("low", 1_024),
            ("medium", 4_096),
            ("high", 8_192),
            ("max", 16_384),
        ] {
            let (thinking, output, temperature) = provider
                .build_reasoning_request_parts_with_effort(
                    "claude-opus-4-5",
                    true,
                    true,
                    Some(effort),
                );
            assert!(
                matches!(thinking, Some(ApiThinking::Enabled { budget_tokens }) if budget_tokens == budget)
            );
            assert_eq!(
                output.unwrap().effort,
                if effort == "minimal" {
                    "low"
                } else if effort == "max" {
                    "high"
                } else {
                    effort
                }
            );
            assert_eq!(temperature, None);
        }
        let (thinking, output, _) = provider.build_reasoning_request_parts_with_effort(
            "claude-opus-4-8",
            false,
            true,
            Some("low"),
        );
        assert!(matches!(thinking, Some(ApiThinking::Adaptive { .. })));
        assert_eq!(output.unwrap().effort, "low");
        for model in ["claude-opus-4-8", "claude-opus-4-5"] {
            let (thinking, output, temperature) =
                provider.build_reasoning_request_parts_with_effort(model, true, true, Some("none"));
            assert!(thinking.is_none());
            assert!(output.is_none());
            assert_eq!(temperature, Some(1.0));
        }
        assert_eq!(provider.stored_reasoning_effort().as_deref(), Some(mode));
    }
    assert_eq!(
        AnthropicProvider::manual_thinking_budget("max", 8_192),
        Some(8_191)
    );
    assert_eq!(
        AnthropicProvider::manual_thinking_budget("low", 1_024),
        None
    );
}

#[test]
fn configured_swarm_root_effort_reads_real_config() {
    // Run this single test in a child process so changing config cannot race
    // other provider tests or reuse an already-initialized global config cache.
    if std::env::var_os("JCODE_TEST_SWARM_ROOT_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                std::thread::current().name().unwrap(),
                "--nocapture",
            ])
            .env("JCODE_TEST_SWARM_ROOT_CHILD", "1")
            .env("JCODE_SWARM_ROOT_EFFORT", "low")
            .output()
            .expect("run isolated config test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut provider = AnthropicProvider::new();
    provider.max_tokens_override = Some(32_768);
    for mode in ["swarm"] {
        *provider.reasoning_effort.write().unwrap() = Some(mode.into());
        for model in ["claude-opus-4-8", "claude-opus-4-5"] {
            let (thinking, output, temperature) =
                provider.build_reasoning_request_parts_inner(model, true, true);
            if mode == "swarm" {
                assert_eq!(output.unwrap().effort, "low");
                assert_eq!(temperature, None);
                if model == "claude-opus-4-5" {
                    assert!(matches!(
                        thinking,
                        Some(ApiThinking::Enabled {
                            budget_tokens: 1_024
                        })
                    ));
                } else {
                    assert!(matches!(thinking, Some(ApiThinking::Adaptive { .. })));
                }
            } else {
                assert!(thinking.is_none());
                assert!(output.is_none());
                assert_eq!(temperature, Some(1.0));
            }
        }
        assert_eq!(provider.stored_reasoning_effort().as_deref(), Some(mode));
    }
}

#[test]
fn opus_55_request_json_supports_api_and_oauth_without_forced_tools() {
    let provider = AnthropicProvider::new();
    for model in ["claude-opus-5-5", "claude-fable-5-1"] {
        for is_oauth in [false, true] {
            for show_thinking in [false, true] {
                for effort in [None, Some("none"), Some("low"), Some("xhigh"), Some("max")] {
                    let (thinking, output_config, temperature) = provider
                        .build_reasoning_request_parts_with_effort(
                            model,
                            is_oauth,
                            show_thinking,
                            effort,
                        );
                    let request = ApiRequest {
                        model: model.to_string(),
                        max_tokens: jcode_provider_core::anthropic::anthropic_max_output_tokens(
                            model,
                        ),
                        system: None,
                        messages: vec![],
                        tools: None,
                        metadata: None,
                        thinking,
                        output_config,
                        temperature,
                        service_tier: None,
                        speed: None,
                        stream: true,
                        effort_record: None,
                    };
                    let value = serde_json::to_value(&request).unwrap();
                    assert_eq!(value["thinking"]["type"], "adaptive");
                    assert_eq!(value["thinking"]["display"], "summarized");
                    assert_eq!(
                        value["thinking"]["block_binding"]["prefix_mismatch_behavior"],
                        "drop_block"
                    );
                    assert_eq!(value["max_tokens"], 128_000);
                    assert!(value.get("temperature").is_none());
                    assert!(value.get("tool_choice").is_none());
                    match effort {
                        None => assert!(value.get("output_config").is_none()),
                        Some("none") => assert_eq!(value["output_config"]["effort"], "low"),
                        Some(effort) => assert_eq!(value["output_config"]["effort"], effort),
                    }
                }
            }
        }
    }
}

#[test]
fn opus_55_empty_signed_thinking_is_replayed_unchanged() {
    let provider = AnthropicProvider::new();
    for is_oauth in [false, true] {
        let blocks = provider.format_content_blocks(
            &[ContentBlock::AnthropicThinking {
                thinking: String::new(),
                signature: "model-and-prefix-bound-signature".to_string(),
            }],
            is_oauth,
        );
        assert_eq!(
            serde_json::to_value(blocks).unwrap(),
            serde_json::json!([{
                "type": "thinking", "thinking": "", "signature": "model-and-prefix-bound-signature"
            }])
        );
    }
}

#[path = "anthropic_tests_additional_tests.rs"]
mod additional_direct_api_url_supports_standard_and_profile_overrides;
