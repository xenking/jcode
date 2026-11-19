use super::*;
use crate::{ApiContentBlock, ApiMessage, format_messages, format_messages_with_identity};
use jcode_message_types::provider_native;

const MODEL: &str = "claude-opus-5-5";

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(text: &str, record: Option<&Value>) -> Message {
    let mut content = Vec::new();
    if let Some(record) = record {
        content.push(ContentBlock::ProviderNative {
            provider: PROVIDER_NATIVE_ANTHROPIC.to_string(),
            item: record.clone(),
        });
    }
    content.push(ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    });
    Message {
        role: Role::Assistant,
        content: content.into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn tool_call(id: &str, record: &Value) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::ProviderNative {
                provider: PROVIDER_NATIVE_ANTHROPIC.to_string(),
                item: record.clone(),
            },
            ContentBlock::ToolUse {
                id: id.to_string(),
                name: "bash".to_string(),
                input: json!({}),
                thought_signature: None,
            },
        ]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn tool_result(id: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "ok".to_string(),
            is_error: None,
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

/// Run one request: plan, format, return the wire messages and the record
/// its response stores.
fn request(history: &[Message], effort: &str) -> (EffortPlan, Value) {
    let plan = plan_effort_controls(history, MODEL, Some(effort)).unwrap();
    let messages = insert_effort_controls(history, &plan);
    let wire = format_messages_with_identity(format_messages(&messages, true), true, false);
    (plan, serde_json::to_value(wire).unwrap())
}

fn strip_cache_control(mut value: Value) -> Value {
    for message in value.as_array_mut().into_iter().flatten() {
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if let Some(block) = block.as_object_mut() {
                block.remove("cache_control");
            }
        }
    }
    value
}

#[test]
fn effort_change_keeps_top_level_and_replays_a_stable_control() {
    // Turn 1 at medium.
    let mut history = vec![user("one")];
    let (plan1, wire1) = request(&history, "medium");
    assert_eq!(plan1.top_level, "medium");
    assert!(plan1.inserts.is_empty());
    assert!(
        !wire1
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system")
    );
    history.push(assistant("first", Some(&plan1.record)));

    // Turn 2 after switching to high.
    history.push(user("two"));
    let (plan2, wire2) = request(&history, "high");
    assert_eq!(
        plan2.top_level, "medium",
        "top-level effort must not change"
    );
    let messages2 = wire2.as_array().unwrap();
    assert_eq!(
        messages2[2],
        json!({"role": "system", "content": [], "output_config": {"effort": "high"}})
    );
    assert_eq!(messages2[3]["role"], "user");
    assert_eq!(
        strip_cache_control(Value::Array(messages2[..2].to_vec())),
        strip_cache_control(Value::Array(wire1.as_array().unwrap()[..1].to_vec()))
            .as_array()
            .map(|prefix| {
                let mut prefix = prefix.clone();
                prefix.push(strip_cache_control(json!([messages2[1].clone()]))[0].clone());
                Value::Array(prefix)
            })
            .unwrap()
    );
    history.push(assistant("second", Some(&plan2.record)));

    // Turn 3 at high: the control stays exactly where turn 2 put it, and no
    // new one is added.
    history.push(user("three"));
    let (plan3, wire3) = request(&history, "high");
    assert_eq!(plan3.top_level, "medium");
    let messages3 = wire3.as_array().unwrap();
    assert_eq!(
        strip_cache_control(Value::Array(messages3[..messages2.len()].to_vec())),
        strip_cache_control(wire2.clone()),
        "the previous request must be an exact prefix of the next one"
    );
    assert_eq!(
        messages3.iter().filter(|m| m["role"] == "system").count(),
        1
    );

    // Restart: a fresh planner over the persisted history derives the same request.
    let (replanned, rewire) = request(&history, "high");
    assert_eq!(replanned, plan3);
    assert_eq!(rewire, wire3);
}

#[test]
fn control_mid_tool_loop_lands_after_the_tool_result() {
    let first = plan_effort_controls(&[user("go")], MODEL, Some("low")).unwrap();
    let history = vec![
        user("go"),
        tool_call("t1", &first.record),
        tool_result("t1"),
    ];
    let plan = plan_effort_controls(&history, MODEL, Some("xhigh")).unwrap();
    assert_eq!(plan.inserts, vec![(3, "xhigh".to_string())]);
    let wire = serde_json::to_value(format_messages(
        &insert_effort_controls(&history, &plan),
        true,
    ))
    .unwrap();
    let roles: Vec<&str> = wire
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "system"]);
}

#[test]
fn turn_control_precedes_injected_trailing_user_messages() {
    let first = plan_effort_controls(&[user("a")], MODEL, Some("low")).unwrap();
    let base = vec![user("a"), assistant("ok", Some(&first.record)), user("b")];
    // The request that made the change also carried an unsaved memory message.
    let mut sent = base.clone();
    sent.push(user("<memory>"));
    let at_change = plan_effort_controls(&sent, MODEL, Some("high")).unwrap();
    assert_eq!(at_change.inserts, vec![(2, "high".to_string())]);
    // Persisted history drops the memory message; the slot is unchanged.
    let mut later = base;
    later.push(assistant("done", Some(&at_change.record)));
    later.push(user("c"));
    let replay = plan_effort_controls(&later, MODEL, Some("high")).unwrap();
    assert_eq!(replay.inserts, vec![(2, "high".to_string())]);
}

#[test]
fn history_without_records_uses_current_effort_unchanged() {
    let history = vec![user("a"), assistant("legacy", None), user("b")];
    let plan = plan_effort_controls(&history, MODEL, Some("high")).unwrap();
    assert_eq!(plan.top_level, "high");
    assert!(plan.inserts.is_empty());
    assert!(matches!(
        insert_effort_controls(&history, &plan),
        Cow::Borrowed(_)
    ));
    assert!(plan_effort_controls(&history, MODEL, None).is_none());
}

#[test]
fn records_from_another_model_start_a_fresh_baseline() {
    let other = plan_effort_controls(&[user("a")], "claude-fable-5-1", Some("low")).unwrap();
    let history = vec![user("a"), assistant("x", Some(&other.record)), user("b")];
    let plan = plan_effort_controls(&history, MODEL, Some("high")).unwrap();
    assert_eq!(plan.top_level, "high");
    assert!(plan.inserts.is_empty());
}

#[test]
fn request_without_explicit_effort_keeps_the_level_in_force() {
    let first = plan_effort_controls(&[user("a")], MODEL, Some("low")).unwrap();
    let mut history = vec![user("a"), assistant("x", Some(&first.record)), user("b")];
    let second = plan_effort_controls(&history, MODEL, Some("max")).unwrap();
    history.push(assistant("y", Some(&second.record)));
    history.push(user("c"));
    let plan = plan_effort_controls(&history, MODEL, None).unwrap();
    assert_eq!(plan.top_level, "low");
    assert_eq!(plan.effort, "max");
    assert_eq!(plan.inserts, vec![(2, "max".to_string())]);
}

#[test]
fn switching_back_replays_every_change_and_no_ops_collapse() {
    let r1 = plan_effort_controls(&[user("a")], MODEL, Some("low")).unwrap();
    let mut history = vec![user("a"), assistant("1", Some(&r1.record)), user("b")];
    let r2 = plan_effort_controls(&history, MODEL, Some("high")).unwrap();
    history.push(assistant("2", Some(&r2.record)));
    history.push(user("c"));
    let r3 = plan_effort_controls(&history, MODEL, Some("low")).unwrap();
    assert_eq!(
        r3.inserts,
        vec![(2, "high".to_string()), (4, "low".to_string())]
    );
}

#[test]
fn record_is_hidden_from_display_and_other_providers() {
    let record = plan_effort_controls(&[], MODEL, Some("high"))
        .unwrap()
        .record;
    assert!(provider_native::is_hidden_control_item(&record));
    assert!(provider_native::provider_native_display(PROVIDER_NATIVE_ANTHROPIC, &record).is_none());
    assert!(
        provider_native::provider_native_text_fallback(PROVIDER_NATIVE_ANTHROPIC, &record, None)
            .is_none()
    );
    // The Anthropic formatter itself never sends the record as content.
    let wire = serde_json::to_value(format_messages(
        &[user("a"), assistant("answer", Some(&record))],
        true,
    ))
    .unwrap();
    assert_eq!(
        wire[1]["content"],
        json!([{"type": "text", "text": "answer"}])
    );
}

#[test]
fn strip_and_beta_follow_the_controls() {
    let first = plan_effort_controls(&[user("a")], MODEL, Some("low")).unwrap();
    let history = vec![user("a"), assistant("x", Some(&first.record)), user("b")];
    let plan = plan_effort_controls(&history, MODEL, Some("high")).unwrap();
    let mut wire = format_messages(&insert_effort_controls(&history, &plan), true);
    assert!(has_effort_controls(&wire));
    assert_eq!(
        with_effort_control_beta("oauth-2025-04-20", &wire),
        format!("oauth-2025-04-20,{MID_CONVERSATION_OUTPUT_CONFIG_BETA}")
    );
    let beta = with_effort_control_beta("", &wire);
    assert_eq!(with_effort_control_beta(&beta, &wire), beta);
    assert!(strip_effort_controls(&mut wire));
    assert!(!has_effort_controls(&wire));
    assert_eq!(with_effort_control_beta("x", &wire), "x");
    let roles: Vec<&str> = wire.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert!(!strip_effort_controls(&mut wire));
    let control = ApiMessage::effort_control("high");
    assert!(control.is_effort_control());
    assert!(matches!(
        control.content[0],
        ApiContentBlock::EffortControl(_)
    ));
}
