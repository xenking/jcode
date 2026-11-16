use super::*;

#[test]
fn session_image_requests_and_replies_are_correlated() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    for (api_id, request, kind, event) in [
        (
            71,
            json!({"req":"list_session_images", "id":71, "session_id":"s1"}),
            "list_session_images",
            json!({"type":"session_images", "session_id":"s1", "images":[{"image_id":"img_test", "message_index":2}]}),
        ),
        (
            72,
            json!({"req":"get_session_image", "id":72, "session_id":"s1", "image_id":"img_test", "offset":0}),
            "get_session_image",
            json!({"type":"session_image", "session_id":"s1", "image_id":"img_test", "media_type":"image/png", "offset":0, "total":4, "data":"test"}),
        ),
    ] {
        let out = state.api_request_to_legacy(&request);
        let [Outbound::Legacy(wire)] = out.as_slice() else {
            panic!("unexpected response: {out:?}")
        };
        assert_eq!(wire["type"], kind);
        assert_eq!(wire["session_id"], "s1");
        let mut event = event;
        event["id"] = wire["id"].clone();
        let frames = state.legacy_event_to_api(&event);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].reply_to, Some(api_id));
        let translated = serde_json::to_value(&frames[0].event).unwrap();
        assert_eq!(translated["ev"], event["type"]);
        assert_eq!(translated["session_id"], "s1");
    }
}

#[test]
fn session_image_requests_require_the_matching_attachment() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    for req in ["list_session_images", "get_session_image"] {
        let out = state.api_request_to_legacy(
            &json!({"req":req, "id":3, "session_id":"other", "image_id":"img_test", "offset":0}),
        );
        assert!(
            matches!(&out[..], [Outbound::Reply(frame)] if matches!(frame.event, ApiEvent::Error { code: ErrorCode::UnknownSession, .. }))
        );
    }
}
