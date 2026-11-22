use jcode_applet_types::{NodeKind, compile_express, view::ButtonVariant};
use serde_json::json;

#[test]
fn review_change_produces_keyed_native_controls() {
    let view =
        compile_express(r#"root = ReviewChange("Review patch", "src/ui.rs", "change42")"#).unwrap();
    let NodeKind::Card { title, children } = &view.kind else {
        panic!("expected card")
    };
    assert_eq!(title.as_deref(), Some("Review patch"));
    assert!(view.key.is_some());
    assert!(children.iter().all(|node| node.key.is_some()));
    assert!(
        children
            .iter()
            .any(|n| matches!(&n.kind, NodeKind::Text { text, .. } if text == "src/ui.rs"))
    );
    assert!(
        children
            .iter()
            .any(|n| matches!(&n.kind, NodeKind::Input { bind, .. } if bind == "note"))
    );
    for (label, variant, action) in [
        ("Approve", ButtonVariant::Primary, "review.approve"),
        (
            "Request changes",
            ButtonVariant::Secondary,
            "review.request_changes",
        ),
    ] {
        assert!(children.iter().any(|n| matches!(&n.kind, NodeKind::Button { label: l, variant: v, on_press, .. } if l == label && *v == variant && on_press.action == action && on_press.args == json!({"change_id":"change42"}))));
    }
}

#[test]
fn greeting_uses_custom_action_and_json_strings() {
    let view =
        compile_express(r#" root = GreetingButton("Say \"hello\"", "greet", "w\u006frld") "#)
            .unwrap();
    assert!(view.key.is_some());
    assert!(
        matches!(view.kind, NodeKind::Button { label, on_press, .. } if label == "Say \"hello\"" && on_press.action == "greet" && on_press.args == json!({"recipient":"world"}))
    );
}

#[test]
fn rejects_unsupported_or_unsafe_programs() {
    for source in [
        r#"root = GreetingButton("x", "host.open_url", "y")"#,
        r#"root = GreetingButton("x", "bad name", "y")"#,
        r#"root = GreetingButton("x", "foo..bar", "y")"#,
        r#"root = GreetingButton("x", "greet", other)"#,
        r#"root = GreetingButton("x", "greet", "y"); root = Evil()"#,
        r#"root = Unknown("x")"#,
        r#"other = GreetingButton("x", "greet", "y")"#,
        r#"root = GreetingButton("x", "greet", "y", "z")"#,
        r#"root = GreetingButton("x", "greet", "unterminated)"#,
    ] {
        assert!(compile_express(source).is_err(), "accepted {source}");
    }
    assert!(compile_express(&" ".repeat(65_537)).is_err());
}
