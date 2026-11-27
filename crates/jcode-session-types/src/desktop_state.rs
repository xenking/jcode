//! Per-machine Desktop state: the wire types and the server-side allowlist.
//!
//! A daemon keeps one keyed document per *space* (a Desktop workspace group).
//! Every entry is a small typed value (layout, focus, panel, draft). The daemon
//! re-serializes each value through the types below, so a field that is not
//! listed here (launch env/args, tokens, paths of another machine) can never be
//! stored: unknown fields are rejected, not ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Pong`/`hello_ok` capability of daemons that serve Desktop state.
pub const DESKTOP_STATE_CAPABILITY: &str = "desktop_state_v1";
/// Value of the `v` field every entry must carry.
pub const DESKTOP_STATE_SCHEMA: u32 = 1;
pub const MAX_SPACE_ID_LEN: usize = 64;
pub const MAX_KEY_LEN: usize = 96;
pub const MAX_DEVICE_LEN: usize = 64;
pub const MAX_CHANGES_PER_PUT: usize = 64;
pub const MAX_ENTRIES_PER_SPACE: usize = 256;
pub const MAX_SPACES: usize = 16;
/// Largest serialized value of one entry (a draft with inline attachments).
pub const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;
/// Largest total of live entry values in one space.
pub const MAX_SPACE_BYTES: usize = 16 * 1024 * 1024;

/// One stored entry as seen by clients. `value: None` is a deletion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopStateEntry {
    pub key: String,
    /// Space revision at which this entry last changed.
    pub rev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// A requested change. `value: None` deletes the entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopStateChange {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Why a put was refused outright (as opposed to a revision conflict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopStateRejection {
    /// Unknown key, unknown or credential-like field, wrong type or version.
    Invalid,
    /// A size or count limit was exceeded.
    TooLarge,
    /// The store could not be read or written.
    Unavailable,
}

/// A refused change, with the reason a client can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopStateViolation {
    pub rejection: DesktopStateRejection,
    pub message: String,
}

impl DesktopStateViolation {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            rejection: DesktopStateRejection::Invalid,
            message: message.into(),
        }
    }
}

/// `layout`: rows of panels, in order. Any number of rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutDoc {
    pub v: u32,
    pub rows: Vec<Vec<LayoutSlot>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSlot {
    pub panel: String,
    /// Fraction of the viewport width.
    pub width: f32,
}

/// `focus`: where the user last was. Each Desktop keeps its own live focus;
/// this is what a new attach starts from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusDoc {
    pub v: u32,
    pub row: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    /// A conversation on this machine's daemon.
    Session,
    /// A composer that has not created its session yet.
    Compose,
}

/// `panel/<id>`: one open panel. Sessions are named by this daemon's own ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelDoc {
    pub v: u32,
    pub kind: PanelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `draft/<panel id>`: unsent composer content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftDoc {
    pub v: u32,
    pub text: String,
    #[serde(default)]
    pub cursor: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<DraftAttachment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachment {
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Base64 payload.
    pub data: String,
}

fn identifier(text: &str, max: usize, what: &str) -> Result<(), DesktopStateViolation> {
    let ok = !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(DesktopStateViolation::invalid(format!(
            "{what} must be 1-{max} characters of [A-Za-z0-9._-]"
        )))
    }
}

/// A space id as a client may name it.
pub fn validate_space_id(space: &str) -> Result<(), DesktopStateViolation> {
    identifier(space, MAX_SPACE_ID_LEN, "space")
}

/// The writer label used to attribute pushes.
pub fn validate_device(device: &str) -> Result<(), DesktopStateViolation> {
    if device.is_empty() || device.len() > MAX_DEVICE_LEN || device.chars().any(char::is_control) {
        return Err(DesktopStateViolation::invalid(format!(
            "device must be 1-{MAX_DEVICE_LEN} printable characters"
        )));
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned + Serialize>(
    value: &Value,
    key: &str,
    version: impl Fn(&T) -> u32,
) -> Result<(T, Value), DesktopStateViolation> {
    let typed: T = serde_json::from_value(value.clone())
        .map_err(|error| DesktopStateViolation::invalid(format!("entry `{key}`: {error}")))?;
    if version(&typed) != DESKTOP_STATE_SCHEMA {
        return Err(DesktopStateViolation::invalid(format!(
            "entry `{key}`: unsupported schema version {}",
            version(&typed)
        )));
    }
    // Re-serialize: the stored form is exactly what the allowlist describes.
    let canonical = serde_json::to_value(&typed)
        .map_err(|error| DesktopStateViolation::invalid(format!("entry `{key}`: {error}")))?;
    Ok((typed, canonical))
}

/// Validate one non-deleting change and return the canonical value to store.
pub fn validate_entry(key: &str, value: &Value) -> Result<Value, DesktopStateViolation> {
    if key.len() > MAX_KEY_LEN {
        return Err(DesktopStateViolation::invalid(format!(
            "key longer than {MAX_KEY_LEN} bytes"
        )));
    }
    let canonical = match key.split_once('/') {
        None => match key {
            "layout" => {
                let (doc, value) = parse::<LayoutDoc>(value, key, |doc| doc.v)?;
                for slot in doc.rows.iter().flatten() {
                    identifier(&slot.panel, MAX_SPACE_ID_LEN, "layout panel")?;
                    if !(slot.width.is_finite() && slot.width > 0.0) {
                        return Err(DesktopStateViolation::invalid(
                            "layout width must be a positive number",
                        ));
                    }
                }
                value
            }
            "focus" => {
                let (doc, value) = parse::<FocusDoc>(value, key, |doc| doc.v)?;
                if let Some(panel) = &doc.panel {
                    identifier(panel, MAX_SPACE_ID_LEN, "focus panel")?;
                }
                value
            }
            _ => {
                return Err(DesktopStateViolation::invalid(format!(
                    "unknown key `{key}`"
                )));
            }
        },
        Some((kind @ ("panel" | "draft"), id)) => {
            identifier(id, MAX_SPACE_ID_LEN, "panel id")?;
            if kind == "panel" {
                let (doc, value) = parse::<PanelDoc>(value, key, |doc| doc.v)?;
                if let Some(session) = &doc.session_id {
                    // A bare id of this daemon's own session. A routing prefix
                    // or path would name another machine's resource.
                    identifier(session, 128, "session id")?;
                }
                value
            } else {
                parse::<DraftDoc>(value, key, |doc| doc.v)?.1
            }
        }
        Some(_) => {
            return Err(DesktopStateViolation::invalid(format!(
                "unknown key `{key}`"
            )));
        }
    };
    let size = serde_json::to_vec(&canonical).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_ENTRY_BYTES {
        return Err(DesktopStateViolation {
            rejection: DesktopStateRejection::TooLarge,
            message: format!("entry `{key}` is {size} bytes; the limit is {MAX_ENTRY_BYTES}"),
        });
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rejection(key: &str, value: Value) -> DesktopStateRejection {
        validate_entry(key, &value).unwrap_err().rejection
    }

    #[test]
    fn every_entry_kind_round_trips_to_the_same_value() {
        for (key, value) in [
            (
                "layout",
                json!({"v":1,"rows":[[{"panel":"p1","width":0.5}],[]]}),
            ),
            ("focus", json!({"v":1,"row":2,"panel":"p1"})),
            (
                "panel/p1",
                json!({"v":1,"kind":"session","session_id":"session_abc_1"}),
            ),
            (
                "draft/p1",
                json!({"v":1,"text":"hi","cursor":2,"attachments":[
                    {"media_type":"image/png","label":"a.png","data":"AAAA"}]}),
            ),
        ] {
            assert_eq!(validate_entry(key, &value).unwrap(), value, "{key}");
        }
    }

    #[test]
    fn credential_like_and_machine_local_fields_are_rejected_not_dropped() {
        for (key, value) in [
            (
                "panel/p1",
                json!({"v":1,"kind":"compose","launch":{"env":{"A":"b"}}}),
            ),
            ("panel/p1", json!({"v":1,"kind":"compose","args":["--x"]})),
            ("draft/p1", json!({"v":1,"text":"","token":"secret"})),
            (
                "draft/p1",
                json!({"v":1,"text":"","working_dir":"/Users/a"}),
            ),
            ("layout", json!({"v":1,"rows":[],"api_key":"secret"})),
            ("focus", json!({"v":1,"row":0,"cwd":"/tmp"})),
        ] {
            assert_eq!(
                rejection(key, value),
                DesktopStateRejection::Invalid,
                "{key}"
            );
        }
    }

    #[test]
    fn keys_ids_and_versions_are_allowlisted() {
        let draft = json!({"v":1,"text":""});
        assert_eq!(
            rejection("settings", draft.clone()),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            rejection("other/p1", draft.clone()),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            rejection("draft/../x", draft.clone()),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            rejection("draft/p1", json!({"v":2,"text":""})),
            DesktopStateRejection::Invalid
        );
        // Another machine's resource is not a session of this daemon.
        assert_eq!(
            rejection(
                "panel/p1",
                json!({"v":1,"kind":"session","session_id":"paired://abc/def"})
            ),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            rejection(
                "layout",
                json!({"v":1,"rows":[[{"panel":"p","width":-1.0}]]})
            ),
            DesktopStateRejection::Invalid
        );
    }

    #[test]
    fn an_oversized_entry_is_too_large() {
        let text = "x".repeat(MAX_ENTRY_BYTES);
        assert_eq!(
            rejection("draft/p1", json!({"v":1,"text":text})),
            DesktopStateRejection::TooLarge
        );
    }
}
