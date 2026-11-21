//! Mid-conversation reasoning-effort controls.
//!
//! Anthropic keys its prompt cache on the top-level `output_config` (and the
//! thinking config). Changing the effort there mid-session throws away the
//! whole cached conversation. Supported models instead accept a bare
//! `{"role": "system", "content": [], "output_config": {"effort": ...}}`
//! message that changes the effort from that point on (beta
//! `mid-conversation-output-config-2026-07-01`).
//!
//! Each response records the effort its request ran at as a hidden
//! [`ANTHROPIC_EFFORT_CONTROL_TYPE`] item stored with the assistant message.
//! Later requests keep the recorded top-level effort byte-stable and replay
//! every recorded change as a control at the position its request placed it,
//! so the control never moves and the prefix before it stays cacheable.

use jcode_message_types::provider_native::{
    ANTHROPIC_EFFORT_CONTROL_TYPE, PROVIDER_NATIVE_ANTHROPIC,
};
use jcode_message_types::{ContentBlock, Message, Role};
use serde_json::{Value, json};
use std::borrow::Cow;

/// Beta header that enables per-message `output_config` on Anthropic's API.
pub const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";

/// `type` of the transient marker message spliced into the provider-facing
/// history where a control goes. Never persisted.
const CONTROL_SLOT_TYPE: &str = "jcode_effort_control_slot";

/// Effort controls for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortPlan {
    /// Top-level `output_config.effort`, stable for the whole conversation.
    pub top_level: String,
    /// `(index into the source messages, effort)`: a control goes right before
    /// that message (or at the end when the index equals the length).
    pub inserts: Vec<(usize, String)>,
    /// The effort this request runs at.
    pub effort: String,
    /// Hidden item recording this request, stored with its response.
    pub record: Value,
}

struct Record<'a> {
    index: usize,
    model: &'a str,
    top_level: &'a str,
    effort: &'a str,
}

/// The recorded effort controls of an assistant message, if any.
fn record_of(index: usize, message: &Message) -> Option<Record<'_>> {
    if message.role != Role::Assistant {
        return None;
    }
    message.content.iter().find_map(|block| match block {
        ContentBlock::ProviderNative { provider, item }
            if provider == PROVIDER_NATIVE_ANTHROPIC
                && item.get("type").and_then(Value::as_str)
                    == Some(ANTHROPIC_EFFORT_CONTROL_TYPE) =>
        {
            Some(Record {
                index,
                model: item.get("model")?.as_str()?,
                top_level: item.get("top_level")?.as_str()?,
                effort: item.get("effort")?.as_str()?,
            })
        }
        _ => None,
    })
}

fn opens_turn(message: &Message) -> bool {
    message.role == Role::User
        && control_slot_effort(message).is_none()
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Slot for the control of the request whose response lands at `end`: before
/// the run of user turns that opened it, or right at `end` when the request
/// continues a tool loop. Never between a `tool_use` and its `tool_result`.
/// Unsaved trailing user messages (injected memory, a resume nudge) belong to
/// the same run, so the request that sent them and every later request
/// derive the same slot relative to the persisted history.
fn slot_for(messages: &[Message], end: usize) -> usize {
    let mut slot = end;
    for index in (0..end).rev() {
        if !opens_turn(&messages[index]) {
            break;
        }
        slot = index;
    }
    slot
}

/// Plan the controls for a request to `model` at `current` effort.
///
/// Only records of the same model since the last model change count: the
/// cache is per model, and another model may not accept the recorded level.
/// Without such a record (a new conversation, a model switch, or a session
/// started before records existed) the top level is `current` and nothing is
/// inserted. A request without an explicit effort (`current` is `None`) keeps
/// the level in force, since a control cannot restore the API default.
/// Returns `None` when neither an effort nor a record exists.
pub fn plan_effort_controls(
    messages: &[Message],
    model: &str,
    current: Option<&str>,
) -> Option<EffortPlan> {
    let mut records: Vec<Record<'_>> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if let Some(record) = record_of(index, message) {
            if record.model != model {
                records.clear();
                continue;
            }
            records.push(record);
        }
    }
    let top_level = records
        .last()
        .map(|record| record.top_level)
        .or(current)?
        .to_string();
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let mut push = |slot: usize, effort: &str| match inserts.last_mut() {
        Some((last, value)) if *last == slot => *value = effort.to_string(),
        _ => inserts.push((slot, effort.to_string())),
    };
    let mut tail = top_level.as_str();
    for record in &records {
        if record.effort != tail {
            push(slot_for(messages, record.index), record.effort);
            tail = record.effort;
        }
    }
    let effort = current.unwrap_or(tail);
    if effort != tail {
        push(slot_for(messages, messages.len()), effort);
    }
    // A control equal to the level already in force at its slot is a no-op,
    // which the merge above can leave behind; drop it.
    let mut in_force = top_level.clone();
    inserts.retain(|(_, value)| {
        let keep = *value != in_force;
        in_force.clone_from(value);
        keep
    });
    let record = json!({
        "type": ANTHROPIC_EFFORT_CONTROL_TYPE,
        "model": model,
        "top_level": top_level,
        "effort": effort,
    });
    Some(EffortPlan {
        effort: effort.to_string(),
        top_level,
        inserts,
        record,
    })
}

/// Splice transient control slot markers into `messages` per `plan`.
pub fn insert_effort_controls<'a>(
    messages: &'a [Message],
    plan: &EffortPlan,
) -> Cow<'a, [Message]> {
    if plan.inserts.is_empty() {
        return Cow::Borrowed(messages);
    }
    let mut out = Vec::with_capacity(messages.len() + plan.inserts.len());
    let mut inserts = plan.inserts.iter().peekable();
    for index in 0..=messages.len() {
        while let Some((_, effort)) = inserts.next_if(|(slot, _)| *slot == index) {
            out.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ProviderNative {
                    provider: PROVIDER_NATIVE_ANTHROPIC.to_string(),
                    item: json!({"type": CONTROL_SLOT_TYPE, "effort": effort}),
                }]
                .into(),
                timestamp: None,
                tool_duration_ms: None,
            });
        }
        if let Some(message) = messages.get(index) {
            out.push(message.clone());
        }
    }
    Cow::Owned(out)
}

/// The effort of a transient control slot marker message.
pub(crate) fn control_slot_effort(message: &Message) -> Option<&str> {
    let [ContentBlock::ProviderNative { provider, item }] = message.content.as_ref() else {
        return None;
    };
    (provider == PROVIDER_NATIVE_ANTHROPIC
        && item.get("type").and_then(Value::as_str) == Some(CONTROL_SLOT_TYPE))
    .then(|| item.get("effort").and_then(Value::as_str))
    .flatten()
}

/// True when a formatted request carries any effort control.
pub fn has_effort_controls(messages: &[super::ApiMessage]) -> bool {
    messages.iter().any(super::ApiMessage::is_effort_control)
}

/// Remove every effort control, for a request that falls back to a model or
/// reasoning shape without per-message effort. Returns true when any was
/// removed.
pub fn strip_effort_controls(messages: &mut Vec<super::ApiMessage>) -> bool {
    let before = messages.len();
    messages.retain(|message| !message.is_effort_control());
    if messages.len() == before {
        return false;
    }
    // Controls separated same-role turns, which must merge again.
    let mut merged: Vec<super::ApiMessage> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        match merged.last_mut() {
            Some(last) if last.role == message.role => last.content.extend(message.content),
            _ => merged.push(message),
        }
    }
    *messages = merged;
    true
}

/// Append the per-message effort beta when the request needs it.
pub fn with_effort_control_beta(base: &str, messages: &[super::ApiMessage]) -> String {
    if !has_effort_controls(messages)
        || base
            .split(',')
            .any(|beta| beta.trim() == MID_CONVERSATION_OUTPUT_CONFIG_BETA)
    {
        return base.to_string();
    }
    if base.is_empty() {
        MID_CONVERSATION_OUTPUT_CONFIG_BETA.to_string()
    } else {
        format!("{base},{MID_CONVERSATION_OUTPUT_CONFIG_BETA}")
    }
}

#[cfg(test)]
#[path = "effort_controls_tests.rs"]
mod tests;
