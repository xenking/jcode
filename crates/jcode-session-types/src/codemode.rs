use serde::{Deserialize, Serialize};

/// Display-only outcomes of calls actually dispatched by one codemode run.
/// The wrapper's text and provider result are independent of this summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeSummary {
    #[serde(rename = "codemode_status")]
    pub status: CodemodeStatus,
    #[serde(
        rename = "codemode_calls",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub calls: Vec<CodemodeCall>,
    #[serde(
        rename = "codemode_calls_omitted",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub calls_omitted: usize,
    /// Byte length of the displayed result body before codemode's own footer.
    /// Missing when an outer layer rewrote the result or for legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_body_bytes: Option<usize>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeCall {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<CodemodeDirection>,
    /// The daemon resolved this direct message to a different swarm.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    pub status: CodemodeCallStatus,
    pub duration_ms: u64,
    /// Redacted, single-line excerpt of a successful swarm message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Safe failure category. Never the raw tool error or its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeCallStatus {
    Success,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeDirection {
    Outbound,
}

#[test]
fn stored_display_receipts_do_not_enter_the_provider_message() {
    let legacy = serde_json::json!({
        "id":"result", "role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool", "content":"wrapper result"}]
    });
    let old: crate::StoredMessage = serde_json::from_value(legacy.clone()).unwrap();
    assert!(old.codemode.is_none());

    let mut with_receipt = legacy;
    with_receipt["codemode"] = serde_json::json!({
        "codemode_status":"error", "codemode_calls":[{"tool":"swarm", "status":"success", "duration_ms":10}]
    });
    let stored: crate::StoredMessage = serde_json::from_value(with_receipt).unwrap();
    assert_eq!(stored.codemode.as_ref().unwrap().calls[0].tool, "swarm");
    let stored_round_trip: crate::StoredMessage =
        serde_json::from_value(serde_json::to_value(&stored).unwrap()).unwrap();
    assert_eq!(stored_round_trip.codemode, stored.codemode);
    assert_eq!(
        serde_json::to_value(stored.to_message()).unwrap(),
        serde_json::to_value(old.to_message()).unwrap()
    );
}
