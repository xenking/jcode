#[test]
fn warm_openai_shortfall_is_not_a_miss_but_real_losses_are() {
    let route = RequestRoute {
        provider: "openai".into(),
        model: "fixture".into(),
        ..RequestRoute::default()
    };
    // Same provider/account, complete matching history. Prior uncached input
    // was already 1,993 tokens, so the 2,377-token shortfall includes only
    // 384 fewer observed reads, not a 2,377-token loss.
    for (read, history, expected_reason) in [
        (112_768, vec![10, 11], None),
        (110_080, vec![10, 11], Some(MissReason::LowRead)),
        (70_016, vec![10, 11], Some(MissReason::LowRead)),
        (112_768, vec![99, 11], Some(MissReason::PrefixChanged)),
    ] {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route.clone(), sig(&[10]));
        assert!(monitor.finish_request(115_145, Some(113_152)).is_none());
        monitor.begin_request(route.clone(), sig(&history));
        let result = monitor.finish_request_with_prefix(115_190, Some(115_190), Some(read));
        assert_eq!(result.as_ref().map(|notice| notice.reason), expected_reason);
        if let Some(notice) = result {
            assert_eq!(notice.missed_tokens, 115_145 - read);
            assert!(notice.message().contains("KV cache miss"));
        }
    }
}

#[test]
fn provider_switch_uses_the_new_providers_uncached_input() {
    let mut monitor = KvCacheMonitor::new();
    monitor.begin_request(route(), sig(&[10]));
    assert!(monitor.finish_request(205_894, Some(203_137)).is_none());
    monitor.begin_request(
        RequestRoute {
            provider: "openai-oauth".into(),
            model: "fixture".into(),
            ..RequestRoute::default()
        },
        sig(&[10, 11]),
    );
    let miss = monitor
        .finish_request_with_prefix(142_275, Some(142_275), Some(4_352))
        .unwrap();
    assert_eq!(miss.reason, MissReason::ProviderSwitch);
    assert_eq!(
        (miss.expected_tokens, miss.read_tokens, miss.missed_tokens),
        (142_275, 4_352, 137_923)
    );
    let message = miss.message();
    assert!(message.contains("138K tokens uncached"), "{message}");
    assert!(message.contains("caches are not shared"), "{message}");
    assert!(!message.contains("resent"), "{message}");
}

#[test]
fn unchanged_read_does_not_hide_an_observed_responses_suffix_change() {
    let route = RequestRoute {
        provider: "openai".into(),
        model: "fixture".into(),
        ..RequestRoute::default()
    };
    let snapshot = |item_hashes, suffix| CanonicalInput {
        provider: route.provider.clone(),
        format: "openai_responses_full".into(),
        model: route.model.clone(),
        item_hashes,
        responses_request_only_suffix: Some(suffix),
        system_hash: Some(1),
        tools_hash: Some(2),
        request_options_hash: Some(3),
    };
    let stable: Vec<u64> = (1..=396).collect();
    let mut monitor = KvCacheMonitor::new();
    monitor.begin_request(route.clone(), sig(&[10]));
    monitor.note_canonical_input(snapshot(stable.clone(), vec![500]));
    assert!(monitor.finish_request(195_557, Some(190_720)).is_none());
    monitor.begin_request(route.clone(), sig(&[10, 11]));
    monitor.note_canonical_input(snapshot([stable, vec![501, 502]].concat(), vec![503]));
    let notice = monitor
        .finish_request_with_prefix(196_490, Some(196_490), Some(190_720))
        .unwrap();
    assert_eq!(notice.wire_suffix_change, Some(396));
    assert_eq!(notice.missed_tokens, 4_837);
    assert!(notice.message().contains("[wire suffix changed]"));
    assert!(notice.message().contains("observed read drop 0 tokens"));
}

#[tokio::test]
async fn low_read_reports_wire_suffix_change_without_blame_or_resend_claim() {
    use jcode_provider_core::fingerprint::{
        capture_canonical_input, log_provider_canonical_input, note_responses_stable_item_count,
    };
    use serde_json::json;

    let route = RequestRoute {
        provider: "openai-oauth".into(),
        model: "fixture".into(),
        ..RequestRoute::default()
    };
    let mut monitor = KvCacheMonitor::new();
    for (items, stable_count, projected, total, read) in [
        (
            vec!["user", "request-only nudge"],
            1,
            vec![10],
            142_275,
            4_352,
        ),
        (
            vec!["user", "assistant", "next user"],
            3,
            vec![10, 11, 12],
            142_566,
            19_968,
        ),
    ] {
        monitor.begin_request(route.clone(), sig(&projected));
        let (_, canonical) = capture_canonical_input(async {
            let body = json!({"input": items});
            log_provider_canonical_input(
                "openai",
                &route.model,
                "openai_responses_full",
                &body,
                body["input"].as_array().unwrap(),
                None,
                None,
                None,
                &[],
            );
            note_responses_stable_item_count(stable_count);
        })
        .await;
        monitor.note_canonical_input(canonical.unwrap());
        let miss = monitor.finish_request_with_prefix(total, Some(total), Some(read));
        if read == 4_352 {
            assert!(miss.is_none());
        } else {
            let miss = miss.unwrap();
            assert_eq!(miss.reason, MissReason::LowRead);
            assert_eq!(miss.missed_tokens, 122_307);
            let message = miss.message();
            assert!(message.contains("[wire suffix changed]"), "{message}");
            assert!(message.contains("stable history"), "{message}");
            assert!(!message.contains("[prefix unchanged]"), "{message}");
            assert!(!message.contains("resent"), "{message}");
        }
    }
}

#[tokio::test]
async fn identifies_drift_in_actual_responses_input_despite_stable_agent_prefix() {
    use jcode_provider_core::fingerprint::{capture_canonical_input, log_provider_canonical_input};
    use serde_json::json;

    let route = RequestRoute {
        provider: "openai".into(),
        model: "gpt-6.2-sol".into(),
        ..RequestRoute::default()
    };
    let mut monitor = KvCacheMonitor::new();
    for (messages, input, read) in [
        (vec![10], vec![json!("user"), json!("ephemeral")], 0),
        (
            vec![10, 11],
            vec![json!("user"), json!("tool result")],
            5_000,
        ),
    ] {
        monitor.begin_request(route.clone(), sig(&messages));
        let (_, canonical) = capture_canonical_input(async {
            let body = json!({"input":input});
            log_provider_canonical_input(
                "openai",
                &route.model,
                "openai_responses_full",
                &body,
                body["input"].as_array().unwrap(),
                None,
                None,
                None,
                &[],
            );
        })
        .await;
        monitor.note_canonical_input(canonical.unwrap());
        let miss = monitor.finish_request(20_000, Some(read));
        if read > 0 {
            let miss = miss.unwrap();
            assert_eq!(miss.reason, MissReason::PrefixChanged);
            assert!(
                miss.detail.contains("canonical input item index 1"),
                "{}",
                miss.detail
            );
        } else {
            assert!(miss.is_none());
        }
    }
}
