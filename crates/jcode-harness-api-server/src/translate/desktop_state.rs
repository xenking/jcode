//! Desktop state requests: a dedicated connection that passes `desktop_state_*`
//! through to the daemon and maps its answers and pushes back.

use super::{BridgeState, Outbound};
use jcode_harness_api::{ApiEvent, ApiRequest, ErrorCode, ServerFrame};
use serde_json::{Value, json};

impl BridgeState {
    pub(super) fn desktop_state_request(&mut self, request: &Value, api_id: u64) -> Vec<Outbound> {
        if !self.desktop_state_supported {
            return Self::error_reply(
                api_id,
                ErrorCode::UnknownRequest,
                "This daemon does not support Desktop state sync. Update Jcode on that machine; Desktop will not restart it.",
            );
        }
        if self.session_id.is_some() || self.pending_attach_id.is_some() {
            return Self::error_reply(
                api_id,
                ErrorCode::InvalidRequest,
                "Desktop state needs a dedicated connection with no attached session",
            );
        }
        let legacy = match serde_json::from_value::<ApiRequest>(request.clone()) {
            Ok(ApiRequest::DesktopStateGet { space }) => {
                json!({"type": "desktop_state_get", "space": space})
            }
            Ok(ApiRequest::DesktopStateSubscribe { space }) => {
                json!({"type": "desktop_state_subscribe", "space": space})
            }
            Ok(ApiRequest::DesktopStatePut {
                space,
                device,
                expected_rev,
                changes,
            }) => json!({
                "type": "desktop_state_put",
                "space": space,
                "device": device,
                "expected_rev": expected_rev,
                "changes": changes,
            }),
            _ => {
                return Self::error_reply(
                    api_id,
                    ErrorCode::InvalidRequest,
                    "Invalid Desktop state request",
                );
            }
        };
        let id = self.legacy_id();
        self.pending_desktop_state.push((id, api_id));
        let mut legacy = legacy;
        legacy["id"] = json!(id);
        vec![Outbound::Legacy(legacy)]
    }

    /// Map a daemon Desktop state event. `None` leaves other events alone.
    pub(super) fn desktop_state_event(&mut self, event: &Value) -> Option<Vec<ServerFrame>> {
        let kind = event["type"].as_str()?;
        if !kind.starts_with("desktop_state") {
            return None;
        }
        let entries = || serde_json::from_value(event["entries"].clone()).unwrap_or_default();
        let space = || event["space"].as_str().unwrap_or_default().to_string();
        let rev = event["rev"].as_u64().unwrap_or(0);
        let api_event = match kind {
            "desktop_state" => ApiEvent::DesktopState {
                space: space(),
                rev,
                entries: entries(),
            },
            "desktop_state_applied" => ApiEvent::DesktopStateApplied {
                space: space(),
                rev,
                entries: entries(),
            },
            "desktop_state_conflict" => ApiEvent::DesktopStateConflict {
                space: space(),
                rev,
                full: event["full"].as_bool().unwrap_or(false),
                entries: entries(),
            },
            "desktop_state_rejected" => ApiEvent::DesktopStateRejected {
                reason: serde_json::from_value(event["reason"].clone())
                    .unwrap_or(jcode_harness_api::DesktopStateRejection::Unavailable),
                message: event["message"].as_str().unwrap_or_default().to_string(),
            },
            "desktop_state_changed" => {
                return Some(vec![ServerFrame::event(ApiEvent::DesktopStateChanged {
                    space: space(),
                    rev,
                    device: event["device"].as_str().unwrap_or_default().to_string(),
                    entries: entries(),
                })]);
            }
            _ => return None,
        };
        // A reply names the request it answers; an unsolicited resync (id 0)
        // has none.
        let id = event["id"].as_u64().unwrap_or(0);
        let reply_to = self
            .pending_desktop_state
            .iter()
            .position(|(legacy, _)| *legacy == id)
            .map(|index| self.pending_desktop_state.remove(index).1);
        Some(vec![match reply_to {
            Some(api_id) => ServerFrame::reply(api_id, api_event),
            None => ServerFrame::event(api_event),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::DaemonCapabilities;

    fn state(advertised: &[&str]) -> BridgeState {
        let caps: Vec<String> = advertised.iter().map(|cap| cap.to_string()).collect();
        BridgeState::open(&DaemonCapabilities::from_daemon(&caps), &json!({}), "test")
    }

    fn legacy(outbound: &[Outbound]) -> &Value {
        match &outbound[0] {
            Outbound::Legacy(value) => value,
            Outbound::Reply(frame) => panic!("expected a daemon frame, got {frame:?}"),
        }
    }

    fn error_message(outbound: &[Outbound]) -> String {
        match &outbound[0] {
            Outbound::Reply(frame) => match &frame.event {
                ApiEvent::Error { message, .. } => message.clone(),
                other => panic!("unexpected {other:?}"),
            },
            Outbound::Legacy(value) => panic!("expected an error, got {value}"),
        }
    }

    #[test]
    fn a_put_passes_through_and_its_answer_names_the_request() {
        let mut bridge = state(&["desktop_state_v1"]);
        let put = json!({"v":1,"id":7,"req":"desktop_state_put","space":"main",
            "device":"laptop","expected_rev":3,
            "changes":[{"key":"draft/p1","value":{"v":1,"text":"hi"}}]});
        let outbound = bridge.api_request_to_legacy(&put);
        let sent = legacy(&outbound);
        assert_eq!(sent["type"], "desktop_state_put");
        assert_eq!(sent["expected_rev"], 3);
        assert_eq!(sent["changes"][0]["key"], "draft/p1");

        let answer = json!({"type":"desktop_state_applied","id":sent["id"],"space":"main",
            "rev":4,"entries":[{"key":"draft/p1","rev":4,"value":{"v":1,"text":"hi"}}]});
        let frames = bridge.legacy_event_to_api(&answer);
        assert_eq!(frames[0].reply_to, Some(7));
        assert!(matches!(
            &frames[0].event,
            ApiEvent::DesktopStateApplied { rev: 4, .. }
        ));
    }

    #[test]
    fn pushes_and_resyncs_have_no_request_to_answer() {
        let mut bridge = state(&["desktop_state_v1"]);
        let pushed = bridge.legacy_event_to_api(&json!({"type":"desktop_state_changed",
            "space":"main","rev":5,"device":"desktop","entries":[]}));
        assert_eq!(pushed[0].reply_to, None);
        assert!(
            matches!(&pushed[0].event, ApiEvent::DesktopStateChanged { device, .. } if device == "desktop")
        );
        let resync = bridge.legacy_event_to_api(&json!({"type":"desktop_state","id":0,
            "space":"main","rev":9,"entries":[]}));
        assert_eq!(resync[0].reply_to, None);
        assert!(matches!(
            &resync[0].event,
            ApiEvent::DesktopState { rev: 9, .. }
        ));
    }

    #[test]
    fn a_daemon_without_the_capability_is_told_to_update_not_restarted() {
        let mut bridge = state(&[]);
        let outbound = bridge
            .api_request_to_legacy(&json!({"v":1,"id":1,"req":"desktop_state_get","space":"main"}));
        let message = error_message(&outbound);
        assert!(
            message.contains("Update Jcode on that machine"),
            "{message}"
        );
    }

    #[test]
    fn a_session_connection_is_refused_locally() {
        let mut bridge = state(&["desktop_state_v1"]);
        bridge.session_id = Some("session_a".into());
        let outbound = bridge.api_request_to_legacy(
            &json!({"v":1,"id":1,"req":"desktop_state_subscribe","space":"main"}),
        );
        assert!(error_message(&outbound).contains("dedicated connection"));
    }
}
