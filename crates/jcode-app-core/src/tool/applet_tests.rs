#![allow(clippy::await_holding_lock)]
use super::*;
use std::sync::Arc;

fn context(session: &str, call: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        message_id: "msg".into(),
        tool_call_id: call.into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

struct Home(
    Option<std::ffi::OsString>,
    #[allow(dead_code)] tempfile::TempDir,
);
impl Drop for Home {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => crate::env::set_var("JCODE_HOME", v),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}
fn temp_home() -> Home {
    let temp = tempfile::tempdir().unwrap();
    let old = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    Home(old, temp)
}

#[test]
fn ids_are_slugs_with_hex() {
    let id = generate_instance_id("Sales Chart!");
    assert!(id.starts_with("sales-chart-"), "{id}");
    assert_eq!(id.len(), "sales-chart-".len() + 4);
    assert!(generate_instance_id("!!").starts_with("applet-"));
}

#[tokio::test]
async fn applet_requires_a_live_desktop_in_its_session() {
    let registry = crate::tool::Registry::empty();
    registry
        .register("applet".into(), Arc::new(AppletTool::new()))
        .await;
    let session = "applet-capability-test";
    let execute = |session| {
        registry.execute(
            "functions.applet",
            json!({"action":"catalog"}),
            context(session, "applet-check"),
        )
    };
    let _other = crate::tool::interactive::attach_desktop("applet-other-session");
    assert!(
        execute(session)
            .await
            .unwrap_err()
            .to_string()
            .contains("no Desktop client attached")
    );
    let desktop = crate::tool::interactive::attach_desktop(session);
    assert!(
        execute(session)
            .await
            .unwrap()
            .output
            .contains("ReviewChange")
    );
    drop(desktop);
    assert!(
        execute(session)
            .await
            .unwrap_err()
            .to_string()
            .contains("no Desktop client attached")
    );
}

#[tokio::test]
async fn mount_patch_wait_and_close() {
    let _guard = crate::storage::lock_test_env();
    let _home = temp_home();
    let tool = AppletTool::new();
    let _desktop = crate::tool::interactive::attach_desktop("s1");

    let out = tool
        .execute(
            json!({"title":"Pick","view":{"type":"button","label":"Go","on_press":{"action":"go"}}}),
            context("s1", "call-1"),
        )
        .await
        .unwrap();
    assert!(out.output.contains("revision 1"), "{}", out.output);
    let snap = crate::applets::snapshot_for_session("s1").unwrap();
    let inst = &snap.instances[0];
    assert!(inst.id.starts_with("pick-"));
    assert_eq!(
        inst.placement,
        Placement::Inline {
            session_id: "s1".into(),
            anchor: Anchor::ToolCall {
                call_id: "call-1".into()
            }
        }
    );
    let id = inst.id.clone();

    // Invalid views are rejected without changing anything.
    let err = tool
        .execute(
            json!({"action":"update","instance":id,"view":{"type":"image","source":{"path":"/etc/x"},"alt":"x"}}),
            context("s1", "call-2"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");

    tool.execute(
        json!({"action":"patch","instance":id,"ops":[{"op":"replace","path":"/title","value":"P2"}]}),
        context("s1", "call-3"),
    )
    .await
    .unwrap();
    assert_eq!(
        crate::applets::snapshot_for_session("s1")
            .unwrap()
            .instances[0]
            .document
            .title,
        "P2"
    );

    let wid = id.clone();
    let waiter = tokio::spawn(async move {
        AppletTool::new()
            .execute(
                json!({"action":"update","instance":wid,"wait":true,"placement":"panel"}),
                context("s1", "call-4"),
            )
            .await
            .unwrap()
    });
    let mut delivered = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if deliver_to_waiter("s1", &id, "[applet action] go".into()) {
            delivered = true;
            break;
        }
    }
    assert!(delivered);
    assert!(waiter.await.unwrap().output.contains("[applet action] go"));

    tool.execute(json!({"action":"close","instance":id}), context("s1", "c"))
        .await
        .unwrap();
    assert!(
        crate::applets::snapshot_for_session("s1")
            .unwrap()
            .instances
            .is_empty()
    );
}

#[tokio::test]
async fn express_mount_update_and_wait_through_registry() {
    let _guard = crate::storage::lock_test_env();
    let _home = temp_home();
    let registry = std::sync::Arc::new(crate::tool::Registry::empty());
    registry
        .register("applet".into(), Arc::new(AppletTool::new()))
        .await;
    let _desktop = crate::tool::interactive::attach_desktop("express-session");
    let definition = registry
        .definitions(None)
        .await
        .into_iter()
        .find(|definition| definition.name == "applet")
        .unwrap();
    assert!(definition.description.contains("ReviewChange"));
    let mount = registry
        .execute(
            "functions.applet",
            json!({
                "instance":"express-test", "title":"Review", "state":{"answer":"yes"},
                "express":"root = ReviewChange(\"Review patch\", \"src/ui.rs\", \"change42\")"
            }),
            context("express-session", "mount"),
        )
        .await
        .unwrap();
    assert!(mount.output.contains("revision 1"));
    let before = crate::applets::snapshot_for_session("express-session").unwrap();
    let original = before.get("express-test").unwrap();
    assert_eq!(original.document.state, json!({"answer":"yes"}));
    for args in [
        json!({"action":"update","instance":"express-test","view":{"type":"text","text":"x"},"express":"root = Invalid()"}),
        json!({"action":"update","instance":"express-test","express":"not valid express"}),
        json!({"action":"patch","instance":"express-test","express":"root = Invalid()","ops":[]}),
    ] {
        assert!(
            registry
                .execute("applet", args, context("express-session", "bad"))
                .await
                .is_err()
        );
        assert_eq!(
            crate::applets::snapshot_for_session("express-session")
                .unwrap()
                .get("express-test")
                .unwrap()
                .document
                .revision,
            1
        );
    }
    let waiter_registry = registry.clone();
    let waiter = tokio::spawn(async move {
        waiter_registry
            .execute(
                "functions.applet",
                json!({
                    "action":"update", "instance":"express-test", "wait":true,
                    "express":"root = GreetingButton(\"Go\", \"go\", \"Ada\")"
                }),
                context("express-session", "update"),
            )
            .await
            .unwrap()
    });
    let mut delivered = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let snapshot = crate::applets::snapshot_for_session("express-session").unwrap();
        let doc = &snapshot.get("express-test").unwrap().document;
        if doc.revision == 2 {
            assert_eq!(doc.state, json!({"answer":"yes"}));
            let actions = doc.view.actions();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].action, "go");
            assert_eq!(actions[0].args, json!({"recipient":"Ada"}));
            let action = actions[0].clone();
            let key = doc.view.key.clone();
            let (_, updated) = crate::applets::set_state(
                "express-session",
                "express-test",
                json!({"answer":"changed"}),
            )
            .unwrap();
            let message = format_action_message(
                "express-test",
                &updated.document.title,
                &action,
                &updated.document.state,
                key.as_deref(),
            );
            if deliver_to_waiter("express-session", "express-test", message) {
                delivered = true;
                break;
            }
        }
    }
    assert!(delivered);
    let output = waiter.await.unwrap().output;
    assert!(
        output.contains("revision 2") && output.contains("`go`") && output.contains("changed"),
        "{output}"
    );
}

#[test]
fn action_message_is_compact() {
    let msg = format_action_message(
        "chart-3f2a",
        "Title",
        &jcode_applet_types::Action {
            action: "select".into(),
            args: json!({"id":2}),
        },
        &json!({"q":"x"}),
        None,
    );
    assert_eq!(
        msg,
        "[applet action] instance `chart-3f2a` (\"Title\"): `select` args {\"id\":2}\nstate: {\"q\":\"x\"}"
    );
}

#[tokio::test]
async fn documented_shapes_validate() {
    let tool = AppletTool::new();
    let _desktop = crate::tool::interactive::attach_desktop("catalog-session");
    assert!(tool.description().contains("plain text"));
    assert!(!tool.description().contains("host.send_prompt"));
    let response = tool
        .execute(
            json!({"action":"catalog"}),
            context("catalog-session", "help"),
        )
        .await
        .unwrap();
    let description = response.output;
    assert!(description.contains("ReviewChange"));
    assert!(description.contains("GreetingButton"));
    assert!(description.contains("select{bind,options:[{value,label}]}"));
    assert!(description.contains("host.send_prompt{prompt}"));
    assert!(description.contains("table{columns:[string],rows:[[string]]}"));
    let view = json!({"type":"stack","children":[
        {"type":"select","bind":"env","options":[{"value":"a","label":"A"}]},
        {"type":"tabs","bind":"t","tabs":[{"id":"x","label":"X","children":[]}]},
        {"type":"key_value","rows":[{"key":"k","value":"v"}]},
        {"type":"button","label":"Go","variant":"danger","on_press":{"action":"host.send_prompt","args":{"prompt":"hi"}}},
        {"type":"toggle","label":"T","bind":"on"},
        {"type":"input","bind":"q","placeholder":"p","multiline":true},
        {"type":"list","children":[{"type":"list_item","title":"a","subtitle":"b","meta":"c","badges":["d"],"on_press":{"action":"open"}}]},
        {"type":"progress","value":0.5,"label":"half"},
        {"type":"text","text":"t","style":"caption","tone":"dim","max_lines":2},
        {"type":"table","columns":["Name","Email"],"rows":[["Ada","ada@example.com"]]}
    ]});
    let document: jcode_applet_types::Document =
        serde_json::from_value(json!({"revision":1,"title":"T","state":{"env":"a"},"view":view}))
            .expect("documented shapes deserialize");
    jcode_applet_types::validate_document(
        &document,
        &jcode_applet_types::agent::manifest(),
        &Default::default(),
    )
    .expect("documented shapes validate");
}

/// A rejected view must say why. Tool errors are rendered with `{}`, so a
/// cause attached via `.context` never reaches the agent, which then retries
/// blind with variations of the same mistake.
#[tokio::test]
async fn invalid_view_error_names_the_cause() {
    let _guard = crate::storage::lock_test_env();
    let _home = temp_home();
    let _desktop = crate::tool::interactive::attach_desktop("applet-invalid-view");
    let error = AppletTool::new()
        .execute(
            json!({"title":"T","view":{"type":"table",
                "columns":[{"key":"name","label":"Name"}],
                "rows":[{"name":"Ada"}]}}),
            context("applet-invalid-view", "call-1"),
        )
        .await
        .expect_err("object-shaped table columns are rejected");
    let shown = format!("{error}");
    assert!(shown.starts_with("invalid view: "), "{shown}");
    assert!(shown.contains("invalid `table` node"), "{shown}");
    assert!(shown.contains("expected a string"), "{shown}");
}
