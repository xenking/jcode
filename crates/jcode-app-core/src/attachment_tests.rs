use jcode_session_types::Attachment;

#[test]
fn prepares_text_images_and_binary_for_model() {
    let dir = tempfile::tempdir().unwrap();
    let result = prepare(dir.path(), vec![
        Attachment::File { name: "code.rs".into(), mime: "text/plain".into(), size: 3, data: Some("YWJj".into()), path: None },
        Attachment::File { name: "photo.png".into(), mime: "image/png".into(), size: 3, data: Some("AQID".into()), path: None },
        Attachment::File { name: "data.bin".into(), mime: "application/octet-stream".into(), size: 3, data: Some("AQID".into()), path: None },
        Attachment::PastedText { text: "one\ntwo".into() },
    ]).unwrap();
    assert!(result.text.contains("code.rs"));
    assert!(result.text.contains("abc"));
    assert!(result.text.contains("one\ntwo"));
    assert_eq!(result.images, vec![("image/png".into(), "AQID".into())]);
    assert!(result.text.contains("data.bin"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(result.items.len(), 4);
    assert!(matches!(&result.items[3], jcode_session_types::StoredAttachment::PastedText { text } if text == "one\ntwo"));
    assert!(matches!(&result.items[2], jcode_session_types::StoredAttachment::File { path: Some(path), .. } if std::path::Path::new(path).is_file()));
}

#[test]
fn code_file_with_generic_mime_reaches_model() {
    let dir = tempfile::tempdir().unwrap();
    let result = prepare(dir.path(), vec![Attachment::File {
        name: "main.rs".into(), mime: "application/octet-stream".into(), size: 3,
        data: Some("YWJj".into()), path: None,
    }]).unwrap();
    assert!(result.text.contains("abc"));
    assert!(result.text.contains("main.rs"));
}

#[test]
fn rejects_mismatched_size_ambiguous_source_and_oversized_data() {
    let dir = tempfile::tempdir().unwrap();
    let file = |size, data: &str, path: Option<&str>| Attachment::File {
        name: "a".into(), mime: "text/plain".into(), size, data: Some(data.into()), path: path.map(str::to_owned),
    };
    assert!(prepare(dir.path(), vec![file(2, "YWJj", None)]).is_err());
    assert!(prepare(dir.path(), vec![file(3, "YWJj", Some("/tmp/a"))]).is_err());
    assert!(prepare(dir.path(), vec![file(MAX_FILE_BYTES as u64 + 1, "", None)]).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// 2026-11-19: a 3 MB `sample` dump attached from Finder was inlined whole and
/// the first request of a new session was rejected at 1.5M tokens.
#[test]
fn large_text_reaches_the_model_as_a_file_reference() {
    let dir = tempfile::tempdir().unwrap();
    let body = "frame 0x1045 jcode::main\n".repeat(8 * 1024);
    assert!(body.len() > MAX_INLINE_TEXT_BYTES);
    let result = prepare(dir.path(), vec![
        Attachment::File {
            name: "Sample of jcode.txt".into(), mime: "text/plain".into(), size: body.len() as u64,
            data: Some(STANDARD.encode(&body)), path: None,
        },
        Attachment::PastedText { text: body.clone() },
    ]).unwrap();

    assert!(!result.text.contains("jcode::main"), "large content must stay out of the prompt");
    assert!(result.text.len() < 2048, "prompt grew by {} bytes", result.text.len());
    let jcode_session_types::StoredAttachment::File { path: Some(path), .. } = &result.items[0] else {
        panic!("large text file must be saved: {:?}", result.items[0]);
    };
    assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    assert!(result.text.contains(path.as_str()), "the model needs the saved path: {}", result.text);
    assert!(matches!(&result.items[1], jcode_session_types::StoredAttachment::PastedText { text } if *text == body));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn text_at_the_inline_limit_is_still_inlined() {
    let dir = tempfile::tempdir().unwrap();
    let body = "a".repeat(MAX_INLINE_TEXT_BYTES);
    let result = prepare(dir.path(), vec![Attachment::File {
        name: "notes.md".into(), mime: "text/markdown".into(), size: body.len() as u64,
        data: Some(STANDARD.encode(&body)), path: None,
    }]).unwrap();
    assert!(result.text.contains(&body));
    assert_eq!(std::fs::read_dir(dir.path()).map(|d| d.count()).unwrap_or(0), 0);
}
