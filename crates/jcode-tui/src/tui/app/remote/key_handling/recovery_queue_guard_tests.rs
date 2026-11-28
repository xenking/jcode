use super::*;

#[test]
fn image_queue_preserves_payload_and_order() {
    let mut app = app_mod::tests::create_test_app();
    app.queued_messages.push("already queued".to_string());
    let images = vec![("image/png".to_string(), "private image payload".to_string())];
    input::stage_queued_input(
        &mut app,
        input::PreparedInput {
            raw_input: "[pasted text]".to_string(),
            expanded: "expanded private prompt".to_string(),
            pasted_contents: Vec::new(),
            images: images.clone(),
        },
    );
    assert!(app.input.is_empty());
    assert_eq!(app.queued_images, images);
    assert_eq!(
        app.queued_messages,
        vec!["already queued", "expanded private prompt"]
    );
}

#[test]
fn image_queue_recall_preserves_distinct_markers_and_payloads() {
    let mut app = app_mod::tests::create_test_app();
    for data in ["first", "second"] {
        app.input = "inspect [image 1]".into();
        app.pending_images.push(("image/png".into(), data.into()));
        input::queue_message(&mut app);
    }
    assert!(input::retrieve_pending_message_for_edit(&mut app));
    assert_eq!(app.input, "inspect [image 1]\n\ninspect [image 2]");
    assert_eq!(app.pending_images[0].1, "first");
    assert_eq!(app.pending_images[1].1, "second");
    assert!(app.queued_images.is_empty());
}

#[test]
fn text_queue_staging_preserves_submission_order() {
    let mut app = app_mod::tests::create_test_app();
    app.queued_messages.push("first".to_string());
    input::stage_queued_input(
        &mut app,
        input::PreparedInput {
            raw_input: "second".to_string(),
            expanded: "expanded second".to_string(),
            pasted_contents: Vec::new(),
            images: Vec::new(),
        },
    );
    assert_eq!(app.queued_messages, vec!["first", "expanded second"]);
}
