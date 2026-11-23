//! Lookup oracle frozen from 3de375bd4, provider_native.rs:323-329.
//! Display conversion remains the independent semantic authority.
use super::*;
use crate::ContentBlock;
use serde_json::json;

fn original(items: &[(&str, &Value)], id: &str) -> Value {
    items
        .iter()
        .filter_map(|(provider, item)| provider_native_display(provider, item))
        .find(|display| display.id == id && display.output.is_none())
        .and_then(|display| display.input)
        .unwrap_or(Value::Null)
}

#[test]
fn frozen_lookup_oracle_malformed_duplicates_and_order() {
    let mut candidates = vec![
        Value::Null,
        json!(1),
        json!([]),
        json!({}),
        json!({"type":3}),
    ];
    for kind in [
        "server_tool_use",
        "web_search_tool_result",
        "web_search_call",
        "unknown",
    ] {
        for id in [Value::Null, json!(false), json!(""), json!("same")] {
            for status in [
                Value::Null,
                json!(3),
                json!("completed"),
                json!("failed"),
                json!("unknown_future_status"),
                json!("in_progress"),
                json!("searching"),
            ] {
                for action in [
                    Value::Null,
                    json!({"query":"", "queries":["ignored"]}),
                    json!({"query":2,"queries":["a",3,"b"],"url":"ignored"}),
                    json!({"queries":[],"url":"ignored"}),
                    json!({"url":"page"}),
                ] {
                    candidates.push(json!({"type":kind,"id":id,"tool_use_id":id,"status":status,"action":action,"input":{"query":"first"}}));
                }
            }
        }
    }
    candidates.extend([
        json!({"type":"server_tool_use","id":"same"}),
        json!({"type":"server_tool_use","id":"same","input":false}),
        json!({"type":"web_search_call","status":"searching"}),
    ]);
    let later = json!({"type":"server_tool_use","id":"same","input":"later"});
    for provider in ["anthropic", "openai", "unknown", ""] {
        for item in &candidates {
            // Direct storage deliberately exercises live OpenAI snapshots that push
            // filters today, guarding lookup semantics independently of ingestion.
            for reversed in [false, true] {
                let mut raw = [(provider, item), ("anthropic", &later)];
                if reversed {
                    raw.reverse();
                }
                let mut items = ProviderNativeItems::default();
                items.push_anthropic_thinking(0, "thinking".into(), "signature".into());
                items.items.extend(raw.map(|(provider, item)| {
                    (
                        0,
                        ContentBlock::ProviderNative {
                            provider: provider.to_owned(),
                            item: item.clone(),
                        },
                    )
                }));
                for id in ["same", "", "missing"] {
                    assert_eq!(
                        items.call_input(id),
                        original(&raw, id),
                        "{provider} {item} reversed={reversed} id={id}"
                    );
                }
            }
        }
    }
}
