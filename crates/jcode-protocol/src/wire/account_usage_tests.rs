use super::*;

#[test]
fn account_usage_roundtrips_as_lightweight_control() {
    let request = Request::AccountUsage { id: 71 };
    assert!(request.is_lightweight_control_request());
    assert_eq!(request.id(), 71);
    let decoded: Request = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
    assert!(matches!(decoded, Request::AccountUsage { id: 71 }));
    let snapshot = AccountUsageSnapshot {
        additional_windows: vec![AccountUsageWindow {
            label: "Future model / month".into(),
            usage_ratio: 0.61,
            resets_at: Some("2099-01-01T00:00:00Z".into()),
        }],
        label: "remote-account".into(),
        email: None,
        exhausted: false,
        primary_label: Some("5h".into()),
        five_hour_ratio: Some(0.35),
        secondary_label: None,
        seven_day_ratio: None,
        resets_at: None,
        error: None,
        banked_resets: None,
        banked_usable: None,
        banked_cached_source: None,
        banked_expires_at: None,
        banked_next_at: None,
    };
    let event = ServerEvent::AccountUsage {
        id: 71,
        anthropic: vec![snapshot.clone()],
        openai: Vec::new(),
    };
    let decoded: ServerEvent =
        serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
    let ServerEvent::AccountUsage {
        id,
        anthropic,
        openai,
    } = decoded
    else {
        panic!("wrong response")
    };
    assert_eq!(id, 71);
    assert_eq!(anthropic, vec![snapshot.clone()]);
    assert!(openai.is_empty());
    let mut legacy = serde_json::to_value(&snapshot).unwrap();
    legacy.as_object_mut().unwrap().remove("additional_windows");
    let legacy: AccountUsageSnapshot = serde_json::from_value(legacy).unwrap();
    assert!(legacy.additional_windows.is_empty());
}
