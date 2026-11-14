use super::*;
use jcode_message_types::Role;

fn text(role: Role, t: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: t.to_string(),
            cache_control: None,
        }],
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
            }],
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: output.into(),
                is_error: Some(false),
            }],
            timestamp: None,
            tool_duration_ms: None,
        },
    ]
}

/// `n` tool calls each with `tokens` of output, then a protected tail.
fn session(n: usize, tokens: usize, tail_tokens: usize) -> Vec<Message> {
    let mut m = vec![text(Role::User, "start")];
    for i in 0..n {
        m.extend(tool_pair(&format!("t{i}"), "read", &char::from(b'a' + (i % 26) as u8).to_string().repeat(tokens * 4)));
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
                    assert_eq!(serde_json::to_string(x).unwrap(), serde_json::to_string(y).unwrap())
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
        text(Role::Assistant, &format!("before\n```rust\n{body}```\nafter")),
        Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "A".repeat(50_000),
            }],
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
