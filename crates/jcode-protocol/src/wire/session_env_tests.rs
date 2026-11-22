use super::*;

#[test]
fn session_environment_roundtrips_without_debug_secrets() {
    let request = Request::SetSessionEnv {
        id: 42,
        env: [("TOKEN".into(), "private-secret".into())].into(),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["type"], "set_session_env");
    assert_eq!(json["env"]["TOKEN"], "private-secret");
    let decoded: Request = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.id(), 42);
    assert!(!format!("{decoded:?}").contains("private-secret"));
    assert!(!format!("{decoded:?}").contains("TOKEN"));
}
