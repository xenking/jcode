use super::*;

#[test]
fn session_image_requests_round_trip() {
    let request: Request = serde_json::from_value(serde_json::json!({
        "type": "get_session_image", "id": 3,
        "session_id": "s", "image_id": "img_abc", "offset": 0
    }))
    .unwrap();
    assert!(
        matches!(request, Request::GetSessionImage { id: 3, ref image_id, .. } if image_id == "img_abc")
    );
    let request: Request = serde_json::from_value(serde_json::json!({
        "type": "list_session_images", "id": 4, "session_id": "s"
    }))
    .unwrap();
    assert!(matches!(request, Request::ListSessionImages { id: 4, .. }));
}
