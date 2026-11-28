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
fn task_table_binds_only_to_the_owning_session() {
    let source = r#"root = TaskTable("Задачи")"#;
    let node = compile_express(source).unwrap();
    assert!(matches!(&node.kind, NodeKind::SessionTasks { title } if title == "Задачи"));
    let wire = serde_json::to_value(&node).unwrap();
    assert_eq!(wire["type"], "session_tasks");
    assert!(wire.get("rows").is_none());
    assert_eq!(
        serde_json::from_value::<jcode_applet_types::View>(wire).unwrap(),
        node
    );
    assert!(jcode_applet_types::express::TOOL_HINT.contains("TaskTable(title)"));
    for bad in [
        r#"root = TaskTable()"#,
        r#"root = TaskTable("x","foreign-session")"#,
        r#"root = TaskTable(fetch())"#,
    ] {
        assert!(compile_express(bad).is_err());
    }
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

/// Every call the tool hint advertises must compile, so a model that copies a
/// signature from the definition never gets a parse error back.
#[test]
fn every_advertised_macro_compiles_with_sample_arguments() {
    let mut advertised = 0;
    for sentence in jcode_applet_types::express::TOOL_HINT.split(". ") {
        let Some((name, args)) = sentence.strip_suffix(')').and_then(|s| s.split_once('(')) else {
            continue;
        };
        if !name.starts_with(|c: char| c.is_ascii_uppercase())
            || !name.chars().all(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        advertised += 1;
        let samples = args.split(", ").map(|_| "\"sample\"").collect::<Vec<_>>();
        let source = format!("root = {name}({})", samples.join(", "));
        let view = compile_express(&source).unwrap_or_else(|e| panic!("{source}: {e}"));
        assert!(view.key.is_some(), "{name} root is not keyed");
    }
    assert!(advertised > 0, "no macro signature found in the tool hint");
}
