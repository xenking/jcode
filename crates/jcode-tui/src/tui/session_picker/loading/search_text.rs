//! Bounded UTF-8-safe transcript search excerpts.
use super::super::search_terms::push_with_byte_budget;
use super::*;

pub(crate) fn build_search_index(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
    messages_preview: &[PreviewMessage],
) -> String {
    let mut combined = search_index_header(id, short_name, title, working_dir, save_label);

    let mut budget = SEARCH_CONTENT_BUDGET_BYTES;
    let mut current = None;
    for msg in messages_preview {
        let content = msg.content.trim();
        if content.is_empty() {
            continue;
        }
        let source = message_role_source(&msg.role);
        if current != Some(source) {
            push_mark(&mut combined, source);
            current = Some(source);
        } else {
            combined.push(' ');
        }
        push_with_byte_budget(&mut combined, content, &mut budget);
        if budget == 0 {
            break;
        }
    }

    combined.to_lowercase()
}

/// Session metadata at the head of every search index. The title and saved
/// label stay unmarked since ranking already treats them as user intent. The
/// ID is an identifier, and the name and directory locate the project.
pub(crate) fn search_index_header(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
) -> String {
    let mut combined = String::new();
    combined.push_str(title);
    if let Some(label) = save_label {
        combined.push(' ');
        combined.push_str(label);
    }
    push_mark(&mut combined, Source::Other);
    combined.push_str(id);
    push_mark(&mut combined, Source::Project);
    combined.push_str(short_name);
    if let Some(dir) = working_dir {
        combined.push(' ');
        combined.push_str(dir);
    }
    combined
}

/// Classifies one external transcript record into per-source samples. The
/// walker reads role and block type tags, so a Claude Code tool result inside a
/// `user` message lands in tool output instead of with the user's prompts.
pub(crate) fn classify_json_into(
    buckets: &mut SourceBuckets,
    value: &serde_json::Value,
    source: Source,
) {
    use serde::de::DeserializeSeed;
    let mut sink = BucketSink {
        buckets,
        total: SEARCH_CONTENT_BUDGET_BYTES,
    };
    let walk = Walk {
        sink: &mut sink,
        source,
    };
    // `serde_json::Value` sorts object keys, which would visit `content`
    // before the `type`/`role` tag that classifies it. Re-emit tags first so
    // the streaming walker sees the same order a transcript file has.
    let mut json = String::new();
    write_tags_first(value, &mut json);
    let mut de = serde_json::Deserializer::from_str(&json);
    if let Err(error) = walk.deserialize(&mut de) {
        crate::logging::debug(&format!("Invalid external search content: {error}"));
    }
}

fn write_tags_first(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            out.push('{');
            let tags = map
                .iter()
                .filter(|(key, _)| matches!(key.as_str(), "role" | "type"));
            let rest = map
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "role" | "type"));
            for (index, (key, value)) in tags.chain(rest).enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                write_tags_first(value, out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_tags_first(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
pub(crate) fn raw_value_search_excerpt(raw: &RawValue, budget: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let raw = raw.get();
    let budget = budget.min(MESSAGE_SEARCH_EXCERPT_BYTES);
    let mut excerpt = String::new();
    push_sampled_search_text(&mut excerpt, raw, budget);
    (!excerpt.is_empty()).then_some(excerpt)
}

pub(crate) fn raw_value_display_text(raw: &RawValue) -> Option<String> {
    fn collect_text(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::String(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_text(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            _ => {}
        }
    }

    let value: serde_json::Value = match serde_json::from_str(raw.get()) {
        Ok(value) => value,
        Err(error) => {
            crate::logging::debug(&format!("Invalid raw transcript search content: {error}"));
            return None;
        }
    };
    let mut text = String::new();
    collect_text(&value, &mut text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Per-source samples of one stored message's content. Content blocks are
/// classified by type, so tool results and reasoning inside a user or
/// assistant message are not indexed as that role's own words.
pub(crate) fn classified_search_excerpt(
    raw: &RawValue,
    role: &crate::message::Role,
    budget: usize,
) -> Option<SourceBuckets> {
    use serde::de::DeserializeSeed;
    if budget == 0 {
        return None;
    }
    let source = match role {
        crate::message::Role::User => Source::User,
        crate::message::Role::Assistant => Source::Assistant,
    };
    let mut buckets = SourceBuckets::default();
    let mut sink = BucketSink {
        buckets: &mut buckets,
        total: budget.min(MESSAGE_SEARCH_EXCERPT_BYTES),
    };
    let walk = Walk {
        sink: &mut sink,
        source,
    };
    let mut de = serde_json::Deserializer::from_str(raw.get());
    if let Err(error) = walk.deserialize(&mut de) {
        crate::logging::debug(&format!("Invalid transcript search content: {error}"));
    }
    (!buckets.is_empty()).then_some(buckets)
}
