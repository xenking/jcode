//! Versioned local Desktop control protocol. This is separate from the harness
//! API: requests are handled by the Desktop UI, not by the agent runtime.
use serde::{Deserialize, Serialize};

pub const DESKTOP_CONTROL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesktopRequest {
    pub v: u32,
    #[serde(flatten)]
    pub action: DesktopAction,
}

/// Every operation except discovery identifies its intended target explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopAction {
    List {},
    OpenSession {
        session_id: String,
        destination: OpenDestination,
    },
    State {
        target: DesktopTarget,
    },
    Click {
        target: DesktopTarget,
        selector: String,
        #[serde(default)]
        allow_user_window: bool,
    },
    SetComposer {
        target: DesktopTarget,
        text: String,
        #[serde(default)]
        allow_user_window: bool,
    },
    Send {
        target: DesktopTarget,
        #[serde(default)]
        allow_user_window: bool,
    },
    PressKey {
        target: DesktopTarget,
        key: String,
        #[serde(default)]
        allow_user_window: bool,
    },
    Search {
        target: DesktopTarget,
        query: String,
        #[serde(default)]
        allow_user_window: bool,
    },
    ExpandTool {
        target: DesktopTarget,
        selector: String,
        #[serde(default)]
        allow_user_window: bool,
    },
    Render {
        target: DesktopTarget,
        selector: Option<String>,
        path: String,
    },
    WaitFor {
        target: DesktopTarget,
        condition: WaitCondition,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenDestination {
    NewWindow,
    BackgroundPanel { window_id: String },
}

/// Both IDs are required, so a stale panel cannot silently target a different session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopTarget {
    pub window_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    SelectorVisible { selector: String },
    TextContains { text: String },
    SessionIdle {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DesktopResponse {
    Ok { result: serde_json::Value },
    Error { code: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_and_opt_in_are_explicit() {
        let request: DesktopRequest = serde_json::from_value(serde_json::json!({
            "v": 1, "action": "set_composer", "target": {"window_id": "w", "session_id": "s"}, "text": "hello"
        })).unwrap();
        assert!(matches!(
            request.action,
            DesktopAction::SetComposer {
                allow_user_window: false,
                ..
            }
        ));
        assert!(
            serde_json::from_value::<DesktopRequest>(serde_json::json!({
                "v": 1, "action": "set_composer", "text": "hello"
            }))
            .is_err()
        );
    }
}
