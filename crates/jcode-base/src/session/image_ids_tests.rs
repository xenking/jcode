use super::*;

#[test]
fn image_ids_are_unique_and_stable_for_legacy_sessions() {
    let mut session = Session::create(None, None);
    for _ in 0..2 {
        session.add_message(
            Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "same-payload".into(),
            }],
        );
    }
    let images = session.session_images();
    assert_eq!(images.len(), 2);
    assert_ne!(images[0].image_id, images[1].image_id);
    let encoded = serde_json::to_string(&session).unwrap();
    let restored: Session = serde_json::from_str(&encoded).unwrap();
    let restored_images = restored.session_images();
    assert_eq!(images, restored_images);
    assert_eq!(
        restored.session_image(&images[0].image_id).unwrap().data,
        "same-payload"
    );
}
