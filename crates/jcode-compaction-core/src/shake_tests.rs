use super::*;
use jcode_message_types::Role;

fn text(role: Role, t: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: t.to_string(),
            cache_control: None,
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn tool_pair(id: &str, name: &str, output: &str) -> [Message; 2] {
    [
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({"path": "x"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: output.into(),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
    ]
}

/// `n` tool calls each with `tokens` of output, then a protected tail.
fn session(n: usize, tokens: usize, tail_tokens: usize) -> Vec<Message> {
    let mut m = vec![text(Role::User, "start")];
    for i in 0..n {
        m.extend(tool_pair(
            &format!("t{i}"),
            "read",
            &char::from(b'a' + (i % 26) as u8)
                .to_string()
                .repeat(tokens * 4),
        ));
    }
    m.push(text(Role::User, &"z".repeat(tail_tokens * 4)));
    m
}

fn json(m: &[Message]) -> String {
    serde_json::to_string(m).unwrap()
}

fn cfg() -> ShakeConfig {
    ShakeConfig {
        protect_window_tokens: 1000,
        ..ShakeConfig::default()
    }
}

#[test]
fn elides_old_tool_results_and_keeps_tail() {
    let mut m = session(10, 1000, 1200);
    let out = shake(&mut m, &cfg());
    let ShakeOutcome::Shaken {
        regions_elided,
        tokens_saved,
    } = out
    else {
        panic!("expected shaken");
    };
    assert_eq!(regions_elided, 10);
    assert!(tokens_saved > 9000, "{tokens_saved}");
    let ContentBlock::ToolResult { content, .. } = &m[2].content[0] else {
        panic!()
    };
    assert!(content.starts_with("[elided tool_result read ~1000 tokens, hash:"));
    let ContentBlock::Text { text, .. } = &m.last().unwrap().content[0] else {
        panic!()
    };
    assert_eq!(text.len(), 4800);
}

#[test]
fn protected_window_is_untouched() {
    // Tail tool result smaller than window: last pair stays verbatim.
    let mut m = session(10, 1000, 0);
    let before = m.clone();
    shake(&mut m, &cfg());
    assert_eq!(json(&m[m.len() - 2..]), json(&before[before.len() - 2..]));
}

#[test]
fn below_savings_threshold_is_no_change() {
    let mut m = session(3, 1000, 1200); // ~3000 < 4096
    let before = json(&m);
    assert_eq!(shake(&mut m, &cfg()), ShakeOutcome::NoChange);
    assert_eq!(json(&m), before);
}

#[test]
fn small_regions_are_not_elided() {
    let mut m = session(40, 300, 1200); // 12k total but each < 400
    assert_eq!(shake(&mut m, &cfg()), ShakeOutcome::NoChange);
}

#[test]
fn idempotent_and_deterministic() {
    let mut a = session(10, 1000, 1200);
    let mut b = a.clone();
    shake(&mut a, &cfg());
    shake(&mut b, &cfg());
    assert_eq!(json(&a), json(&b));
    let once = json(&a);
    assert_eq!(shake(&mut a, &cfg()), ShakeOutcome::NoChange);
    assert_eq!(json(&a), once);
}

#[test]
fn tool_use_result_pairing_preserved() {
    let mut m = session(10, 1000, 1200);
    let before = m.clone();
    shake(&mut m, &cfg());
    assert_eq!(m.len(), before.len());
    for (a, b) in m.iter().zip(&before) {
        assert_eq!(a.role, b.role);
        assert_eq!(a.content.len(), b.content.len());
        for (x, y) in a.content.iter().zip(&b.content) {
            match (x, y) {
                (
                    ContentBlock::ToolResult {
                        tool_use_id: i1,
                        is_error: e1,
                        ..
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: i2,
                        is_error: e2,
                        ..
                    },
                ) => assert_eq!((i1, e1), (i2, e2)),
                (ContentBlock::ToolUse { .. }, ContentBlock::ToolUse { .. }) => {
                    assert_eq!(
                        serde_json::to_string(x).unwrap(),
                        serde_json::to_string(y).unwrap()
                    )
                }
                (ContentBlock::Text { .. }, ContentBlock::Text { .. }) => {}
                _ => panic!("block kind changed"),
            }
        }
    }
}

#[test]
fn elides_code_blocks_but_keeps_prose_and_images() {
    let body = "fn x() {}\n".repeat(2000);
    let mut m = vec![
        text(
            Role::Assistant,
            &format!("before\n```rust\n{body}```\nafter"),
        ),
        Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "A".repeat(50_000),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        text(Role::User, &"z".repeat(4800)),
    ];
    assert!(matches!(shake(&mut m, &cfg()), ShakeOutcome::Shaken { .. }));
    let ContentBlock::Text { text, .. } = &m[0].content[0] else {
        panic!()
    };
    assert!(text.starts_with("before\n```\n[elided code block 2001 lines"));
    assert!(text.ends_with("```\nafter"));
    let ContentBlock::Image { data, .. } = &m[1].content[0] else {
        panic!()
    };
    assert_eq!(data.len(), 50_000);
    assert_eq!(shake(&mut m, &cfg()), ShakeOutcome::NoChange);
}

#[test]
fn whole_log_inside_window_is_no_change() {
    let mut m = session(10, 1000, 0);
    let c = ShakeConfig {
        protect_window_tokens: 1_000_000,
        ..cfg()
    };
    assert_eq!(shake(&mut m, &c), ShakeOutcome::NoChange);
    assert_eq!(shake_savings(&m, &c), 0);
}

// ── omp parity (packages/agent/src/compaction/shake.ts) ───────────────

/// omp `DEFAULT_SHAKE_CONFIG`: protectTokens 16000, minSavings 4000,
/// fenceMinTokens 400.
#[test]
fn defaults_match_omp_auto_shake() {
    let config = ShakeConfig::default();
    assert_eq!(config.protect_window_tokens, 16_000);
    assert_eq!(config.min_savings_tokens, 4_000);
    assert_eq!(config.min_elidable_tokens, 400);
    assert!(config.code_blocks);
    assert_eq!(config.protected_tools, PROTECTED_TOOLS_DEFAULT);
}

/// omp `isProtectedToolResult`: a skill result is never elided, even when old
/// and large, because the agent re-reads it to follow the skill.
#[test]
fn skill_results_are_never_elided() {
    let mut messages = vec![text(Role::User, "start")];
    for i in 0..10 {
        messages.extend(tool_pair(
            &format!("s{i}"),
            "skill_manage",
            &"s".repeat(4_000),
        ));
        messages.extend(tool_pair(&format!("t{i}"), "read", &"r".repeat(4_000)));
    }
    messages.push(text(Role::User, &"z".repeat(4_800)));
    let outcome = shake(&mut messages, &cfg());
    let ShakeOutcome::Shaken { regions_elided, .. } = outcome else {
        panic!("expected shaken");
    };
    assert_eq!(regions_elided, 10, "only the read results are elided");
    for message in &messages {
        let ContentBlock::ToolResult { content, .. } = &message.content[0] else {
            continue;
        };
        assert!(
            !content.starts_with("[elided tool_result skill_manage"),
            "skill result was elided"
        );
    }
}

/// omp `isSkillReadToolResult`: reading a skill file is protected too.
#[test]
fn skill_file_reads_are_protected() {
    let mut messages = vec![text(Role::User, "start")];
    for i in 0..10 {
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("k{i}"),
                name: "read".into(),
                input: serde_json::json!({"path": "/home/u/.jcode/skills/selfdev/SKILL.md"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("k{i}"),
                content: "k".repeat(4_000),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
    }
    messages.push(text(Role::User, &"z".repeat(4_800)));
    assert_eq!(shake(&mut messages, &cfg()), ShakeOutcome::NoChange);
}

/// omp `scanTextForBlockRanges`: top-level XML blocks are elided like fences.
#[test]
fn xml_blocks_in_old_text_are_elided() {
    let body = "tool output line\n".repeat(2_000);
    let mut messages = vec![
        text(
            Role::Assistant,
            &format!("before\n<bash_output>\n{body}</bash_output>\nafter"),
        ),
        text(Role::User, &"z".repeat(4_800)),
    ];
    let cfg = ShakeConfig {
        protect_window_tokens: 100,
        ..cfg()
    };
    let outcome = shake(&mut messages, &cfg);
    assert!(
        matches!(
            outcome,
            ShakeOutcome::Shaken {
                regions_elided: 1,
                ..
            }
        ),
        "{outcome:?}"
    );
    let ContentBlock::Text { text: shaken, .. } = &messages[0].content[0] else {
        panic!()
    };
    assert!(shaken.starts_with("before\n[elided "), "{shaken}");
    assert!(shaken.ends_with("after"), "{shaken}");
}

/// omp suppresses XML detection inside fences: one region, not two.
#[test]
fn xml_inside_a_fence_is_one_region() {
    let body = "fn x() {}\n".repeat(2_000);
    let mut messages = vec![
        text(
            Role::Assistant,
            &format!("```xml\n<bash_output>\n{body}</bash_output>\n```"),
        ),
        text(Role::User, &"z".repeat(4_800)),
    ];
    let cfg = ShakeConfig {
        protect_window_tokens: 100,
        ..cfg()
    };
    assert!(matches!(
        shake(&mut messages, &cfg),
        ShakeOutcome::Shaken {
            regions_elided: 1,
            ..
        }
    ));
}

/// An unterminated fence yields no region (omp is conservative).
#[test]
fn unterminated_fence_is_never_elided() {
    let body = "fn x() {}\n".repeat(2000);
    let mut messages = vec![
        text(Role::Assistant, &format!("```rust\n{body}")),
        text(Role::User, &"z".repeat(4_800)),
    ];
    let cfg = ShakeConfig {
        protect_window_tokens: 100,
        ..cfg()
    };
    assert_eq!(shake(&mut messages, &cfg), ShakeOutcome::NoChange);
}

/// omp `AGGRESSIVE_SHAKE_CONFIG` (manual `/shake`): no savings gate, shallow
/// protect window.
#[test]
fn aggressive_preset_elides_below_the_default_savings_gate() {
    // ~2950 savable tokens: under the 4000 default gate nothing happens.
    let mut messages = session(3, 1000, 1200);
    assert_eq!(shake(&mut messages, &cfg()), ShakeOutcome::NoChange);
    let no_gate = ShakeConfig {
        min_savings_tokens: 0,
        ..cfg()
    };
    assert!(matches!(
        shake(&mut messages, &no_gate),
        ShakeOutcome::Shaken {
            regions_elided: 3,
            ..
        }
    ));

    // The auto preset's 16k window protects the whole short session.
    let mut messages = session(4, 1000, 1200);
    assert_eq!(
        shake(&mut messages, &ShakeConfig::default()),
        ShakeOutcome::NoChange
    );
    assert!(matches!(
        shake(&mut messages, &ShakeConfig::aggressive()),
        ShakeOutcome::Shaken { .. }
    ));
}

/// omp `RESCUE_SHAKE_CONFIG` (compaction dead-end): reaches into the tail so an
/// oversized newest result can still be dropped.
#[test]
fn rescue_preset_reaches_into_the_tail() {
    let mut messages = session(1, 1000, 8_000);
    assert_eq!(shake(&mut messages, &cfg()), ShakeOutcome::NoChange);
    let rescue = ShakeConfig::rescue();
    assert_eq!(rescue.protect_window_tokens, 0);
    let outcome = shake(&mut messages, &rescue);
    assert!(
        matches!(outcome, ShakeOutcome::Shaken { .. }),
        "{outcome:?}"
    );
}
