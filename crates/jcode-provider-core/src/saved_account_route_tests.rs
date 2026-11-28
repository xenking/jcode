use super::*;

#[test]
fn saved_account_routes_survive_serialization_and_restore_specs() {
    for (base, runtime, model) in [
        ("claude-oauth", RuntimeKey::ClaudeOAuth, "claude-sonnet-4-6"),
        ("openai-oauth", RuntimeKey::OpenAIOAuth, "gpt-5.5"),
    ] {
        for label in ["work", "personal", "Named-Account"] {
            let route = ModelRoute {
                speed_options: Vec::new(),
                reasoning_efforts: None,
                model: model.to_string(),
                provider: "provider".to_string(),
                api_method: format!("{base}@{label}"),
                available: true,
                detail: String::new(),
                cheapness: None,
                usage: None,
                account: None,
                icon: None,
            };
            let selection = RouteSelection::from_model_route(&route);
            assert_eq!(selection.runtime_key, runtime);
            assert_eq!(selection.account_label(), Some(label));
            let json = serde_json::to_string(&selection).unwrap();
            let restored: RouteSelection = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, selection);
            assert_eq!(
                restored.routed_model_spec(),
                format!("{base}@{label}:{model}")
            );
        }
    }
}

#[test]
fn invalid_or_non_oauth_account_methods_are_not_accepted() {
    for method in [
        "claude-oauth@",
        "openai-oauth@work:bad",
        "openai-oauth@a@b",
        "openai-api-key@work",
        "claude-oauth@work\nlabel",
    ] {
        assert_eq!(oauth_account_label(method), None, "{method:?}");
    }
}

#[test]
fn account_identity_carries_the_name_a_person_recognises() {
    let detail = format!(
        "{} · t***m@example.com · requires extra usage",
        route_account_identity("claude-otter", Some("team · Max 20x"))
    );
    assert_eq!(
        detail,
        "account claude-otter [team · Max 20x] · t***m@example.com · requires extra usage"
    );
    let parsed = parse_route_account_identity(&detail).expect("identity");
    assert_eq!(parsed.label, "claude-otter");
    assert_eq!(parsed.display_name, Some("team · Max 20x"));
    assert_eq!(parsed.title(), "team · Max 20x");
    assert_eq!(parsed.email(), Some("t***m@example.com"));
}

#[test]
fn account_identity_parses_older_catalogs_and_scope_tags() {
    let parsed = parse_route_account_identity("[session] account claude-fox · a@b.io").unwrap();
    assert_eq!(parsed.display_name, None);
    assert_eq!(parsed.title(), "claude-fox");
    assert_eq!(parsed.email(), Some("a@b.io"));
    let bare = parse_route_account_identity("account claude-fox").unwrap();
    assert_eq!((bare.label, bare.rest), ("claude-fox", ""));
    assert_eq!(parse_route_account_identity("requires extra usage"), None);
    // A name that only repeats the label adds nothing.
    assert_eq!(
        route_account_identity("claude-fox", Some("claude-fox")),
        "account claude-fox"
    );
    // A bracket in a name cannot end the identity early.
    let tricky = route_account_identity("openai-otter", Some("a]b"));
    assert_eq!(
        parse_route_account_identity(&tricky).unwrap().display_name,
        Some("a)b")
    );
}
