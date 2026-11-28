use super::*;
use crossterm::event::KeyEventKind;

#[test]
fn cyrillic_ctrl_c_closes_standalone_picker_on_press_only() {
    use crossterm::event::KeyEvent;

    let mut picker = SessionPicker::new(vec![make_session(
        "session_picker",
        "picker",
        false,
        SessionStatus::Closed,
    )]);
    let key = |kind| KeyEvent::new_with_kind(KeyCode::Char('с'), KeyModifiers::CONTROL, kind);
    assert!(matches!(
        picker.handle_key_event(key(KeyEventKind::Release)).unwrap(),
        OverlayAction::Continue
    ));
    assert!(matches!(
        picker.handle_key_event(key(KeyEventKind::Press)).unwrap(),
        OverlayAction::Close
    ));
}

#[test]
fn test_search_mode_held_ctrl_j_k_navigate_session_list() {
    use crossterm::event::{KeyEvent, KeyEventKind};

    let sessions = (0..5)
        .map(|i| {
            make_session(
                &format!("session_{i}"),
                "match",
                false,
                SessionStatus::Closed,
            )
        })
        .collect();
    let mut picker = SessionPicker::new(sessions);
    picker
        .handle_key_event(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()))
        .unwrap();
    let first = picker.selected_session().unwrap().id.clone();
    for code in [KeyCode::Char('j'), KeyCode::Char('k')] {
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Repeat,
        ] {
            let before = picker.selected_session().unwrap().id.clone();
            picker
                .handle_key_event(KeyEvent::new_with_kind(code, KeyModifiers::CONTROL, kind))
                .unwrap();
            assert_ne!(picker.selected_session().unwrap().id, before);
            assert!(picker.search_active);
            assert!(picker.search_query.is_empty());
        }
        let before_release = picker.selected_session().unwrap().id.clone();
        picker
            .handle_key_event(KeyEvent::new_with_kind(
                code,
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            ))
            .unwrap();
        assert_eq!(picker.selected_session().unwrap().id, before_release);
    }
    assert_eq!(picker.selected_session().unwrap().id, first);
}
