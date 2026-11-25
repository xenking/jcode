use super::*;

/// An auth change re-resolves the route, so the push must update the caption.
#[test]
fn an_available_models_push_updates_the_model() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "available_models_updated",
        "provider_name": "anthropic", "provider_model": "claude-opus-4-5",
        "available_models": ["claude-opus-4-5"],
    }));
    match &frames[0].event {
        ApiEvent::ModelInfo {
            session_id, model, ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(model.as_deref(), Some("claude-opus-4-5"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn create_session_in_a_jcode_checkout_requests_selfdev() {
    // Regression: external client opens its own crate, and without the `selfdev`
    // flag the daemon hands back an agent with no self-dev tools or prompt.
    let mut state = BridgeState::default();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .join("crates/jcode-tui");
    let out = state.api_request_to_legacy(&json!({
        "req": "create_session",
        "id": 1,
        "working_dir": repo.display().to_string(),
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["selfdev"], json!(true));
}

#[test]
fn create_session_outside_a_checkout_leaves_selfdev_unset() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "create_session",
        "id": 1,
        "working_dir": "/",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert!(value.get("selfdev").is_none(), "got {value}");
}

/// A turn that fails ends with `error` instead of `done`. The bridge must let
/// go of the pending message, or a later unrelated `done` reusing that legacy
/// id would be reported to the client as this turn finally finishing, and a
/// client that trusts `turn_done` would unblock on a turn that never ran.
#[test]
fn a_failed_turn_clears_the_pending_message() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 11, "content": "hi",
    }));
    let Outbound::Legacy(message) = &out[0] else {
        panic!("expected a legacy message");
    };
    let legacy_id = message["id"].as_u64().expect("a legacy id");

    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": legacy_id, "message": "dns error",
    }));
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame.event, ApiEvent::Error { .. })),
        "the failure was not forwarded"
    );

    // The same id arriving as `done` afterwards is no longer this turn.
    let frames = state.legacy_event_to_api(&json!({"type": "done", "id": legacy_id}));
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.event, ApiEvent::TurnDone { .. })),
        "a failed turn reported a second, phantom completion"
    );
}

#[test]
fn background_notifications_become_progress_events() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "notification",
        "from_session": "background_task",
        "message": "**Background task progress** `t9` · `bash`\n\n[#####-----] 50% · Running tests (reported)",
    }));
    assert_eq!(frames.len(), 1);
    match &frames[0].event {
        ApiEvent::BackgroundProgress {
            session_id,
            task_id,
            percent,
            done,
            ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(task_id, "t9");
            assert_eq!(*percent, Some(50.0));
            assert!(!done);
        }
        other => panic!("unexpected background event: {other:?}"),
    }
}

#[test]
fn direct_agent_message_reaches_the_recipient_with_server_assigned_sender() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "notification",
        "from_session": "session_fox",
        "from_name": "fox",
        "notification_type": {"kind": "message", "scope": "dm"},
        "message": "DM from fox: verify the output"
    }));
    let [frame] = frames.as_slice() else {
        panic!("expected one signed DM event, got {frames:?}");
    };
    let event = serde_json::to_value(frame).unwrap();
    assert_eq!(event["ev"], "agent_message");
    assert_eq!(event["from_session"], "session_fox");
    assert_eq!(event["from_name"], "fox");
    assert_eq!(event["message"], "DM from fox: verify the output");
    assert!(
        event["session_id"]
            .as_str()
            .is_some_and(|id| id != "session_fox")
    );
}

#[test]
fn unrelated_notifications_are_dropped() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "notification",
        "from_session": "fox",
        "message": "hello from another agent",
    }));
    assert!(frames.is_empty());
}

/// The daemon answers a `ping` that arrives as the first frame on a connection
/// and then closes it, because it classifies ping as a one-shot lightweight
/// control request. Forwarding an unattached ping therefore destroys the
/// client's connection before it ever gets a session, which is the opposite of
/// what a liveness probe should do.
#[test]
fn ping_before_attach_is_answered_locally() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "ping", "id": 4}));
    match out.as_slice() {
        [Outbound::Reply(frame)] => {
            assert_eq!(frame.reply_to, Some(4));
            assert_eq!(frame.event, ApiEvent::Pong);
        }
        other => panic!("ping must not reach the daemon before attach: {other:?}"),
    }
}

/// Once attached the connection is a normal session connection, so ping is a
/// genuine round trip and should measure the daemon, not the bridge.
#[test]
fn ping_after_attach_reaches_the_daemon() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "ping", "id": 5}));
    match out.as_slice() {
        [Outbound::Legacy(value)] => assert_eq!(value["type"], "ping"),
        other => panic!("expected a forwarded ping: {other:?}"),
    }
}

/// The daemon closes the connection on a stateful request that arrives before
/// a subscribe. Forwarding one therefore does not just fail the request: it
/// destroys the client's whole connection, taking every other in-flight
/// request with it, and the SDK sees a bare EPIPE. Answer locally.
#[test]
fn stateful_requests_before_attach_are_refused_locally() {
    for req in [
        "send_message",
        "cancel",
        "soft_interrupt",
        "clear",
        "rewind",
        "rewind_transcript",
        "get_history",
    ] {
        let mut state = BridgeState::default();
        let out = state.api_request_to_legacy(&json!({
            "req": req,
            "id": 7,
            "session_id": "session_does_not_exist",
        }));
        assert_eq!(out.len(), 1, "{req} should produce exactly one reply");
        let Outbound::Reply(frame) = &out[0] else {
            panic!("{req} was forwarded to the daemon, which will close the connection");
        };
        assert_eq!(frame.reply_to, Some(7));
        match &frame.event {
            ApiEvent::Error { code, message } => {
                assert_eq!(*code, ErrorCode::UnknownSession, "{req}");
                assert!(
                    message.contains("session_does_not_exist"),
                    "{req} error should name the session: {message}"
                );
            }
            other => panic!("{req} expected an error frame, got {other:?}"),
        }
    }
}

/// The legacy protocol has no session field, so a request naming a *different*
/// session than the attached one would be applied to the attached one. A
/// `clear` or `rewind` aimed at the wrong id would then destroy a transcript
/// the caller never named.
#[test]
fn requests_for_another_session_do_not_hit_the_attached_one() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "clear",
        "id": 9,
        "session_id": "some_other_session",
    }));
    let Outbound::Reply(frame) = &out[0] else {
        panic!("clear for another session must not reach the daemon");
    };
    match &frame.event {
        ApiEvent::Error { code, message } => {
            assert_eq!(*code, ErrorCode::UnknownSession);
            assert!(message.contains("s1") && message.contains("some_other_session"));
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
}

/// The guard must not break the normal path: the attached session's own id,
/// and an omitted id, both still reach the daemon.
#[test]
fn attached_requests_still_reach_the_daemon() {
    let mut state = state_with_session();
    let named = state.api_request_to_legacy(&json!({
        "req": "get_history", "id": 1, "session_id": "s1",
    }));
    assert!(matches!(named[0], Outbound::Legacy(_)), "explicit id");

    let bare = state.api_request_to_legacy(&json!({"req": "get_history", "id": 2}));
    assert!(matches!(bare[0], Outbound::Legacy(_)), "omitted id");
}

/// Reading around without attaching is the entire point of `peek_session` and
/// `list_sessions`, so the attach guard must leave them alone.
#[test]
fn browsing_requests_work_without_attaching() {
    let _home = ScopedJcodeHome::new("browsing-without-attach");
    let mut state = BridgeState::default();
    for req in ["list_sessions", "peek_session", "ping"] {
        let out = state.api_request_to_legacy(&json!({
            "req": req, "id": 1, "session_id": "whatever",
        }));
        let Outbound::Reply(frame) = &out[0] else {
            panic!("{req} should be answered locally");
        };
        assert!(
            !matches!(frame.event, ApiEvent::Error { .. }),
            "{req} must not be refused by the attach guard: {:?}",
            frame.event
        );
    }
}

#[test]
fn context_breakdown_round_trip_without_attaching() {
    let mut bridge = BridgeState::default();
    let out = bridge.api_request_to_legacy(&json!({
        "req": "session_context_breakdown", "id": 27, "session_id": "session_saved",
        "include_snapcompact": true
    }));
    let Outbound::Legacy(request) = &out[0] else {
        panic!("context inspection must reach the daemon without attaching");
    };
    assert_eq!(request["type"], "session_context_breakdown");
    assert_eq!(request["include_snapcompact"], true);
    assert_eq!(request["session_id"], "session_saved");
    let replies = bridge.legacy_event_to_api(&json!({
        "type": "context_breakdown", "id": request["id"],
        "session_id": "session_saved", "model": null,
        "total_tokens": null, "limit_tokens": null, "estimated": true,
        "categories": [{"kind": "conversation", "tokens": 0}, {"kind": "dynamic_tools", "tokens": null}],
        "action_forecast": {
            "current_tokens": 40000, "compact": null, "shake": null,
            "compact_measurement": {"before_tokens": 40000, "after_tokens": 41000}
        }
    }));
    assert_eq!(replies.len(), 1);
    let reply = serde_json::to_value(&replies[0]).unwrap();
    assert_eq!(reply["reply_to"], 27);
    assert_eq!(reply["ev"], "context_breakdown");
    assert_eq!(reply["session_id"], "session_saved");
    assert_eq!(reply["categories"][0]["tokens"], 0);
    assert_eq!(reply["categories"][1]["tokens"], serde_json::Value::Null);
    assert_eq!(reply["total_tokens"], serde_json::Value::Null);
    assert_eq!(
        reply["action_forecast"]["compact_measurement"],
        json!({
            "before_tokens": 40000, "after_tokens": 41000,
        })
    );
}

#[test]
fn shake_tools_round_trip_does_not_attach_or_retarget() {
    let mut bridge = BridgeState::default();
    let apply = json!({
        "req": "session_shake_tools", "id": 52, "session_id": "session_selected",
        "action": {"kind": "apply", "confirmation": "bound-to-selected"},
    });
    let out = bridge.api_request_to_legacy(&apply);
    let [Outbound::Legacy(request)] = out.as_slice() else {
        panic!("shake must send exactly one scoped request without attaching");
    };
    assert_eq!(request["type"], "session_shake_tools");
    assert_eq!(request["session_id"], apply["session_id"]);
    assert_eq!(request["action"], apply["action"]);
    let response = bridge.legacy_event_to_api(&json!({
        "type": "shake_tools", "id": request["id"], "session_id": "session_selected",
        "result": {"kind": "applied", "stats": {"outputs_elided": 1, "estimated_tokens_saved": 2000}},
    }));
    let reply = serde_json::to_value(&response[0]).unwrap();
    assert_eq!(reply["reply_to"], 52);
    assert_eq!(reply["ev"], "shake_tools");
    assert_eq!(reply["session_id"], "session_selected");
    assert_eq!(reply["result"]["stats"]["estimated_tokens_saved"], 2000);

    let mut draft = apply.clone();
    draft["session_id"] = Value::Null;
    for rejected in [
        draft,
        json!({
            "req": "session_shake_tools", "id": 53, "session_id": "session_selected",
            "action": {"kind": "apply"},
        }),
    ] {
        assert!(matches!(
            bridge.api_request_to_legacy(&rejected).as_slice(),
            [Outbound::Reply(_)]
        ));
    }
    bridge.session_id = Some("session_neighbor".into());
    assert!(matches!(
        bridge.api_request_to_legacy(&apply).as_slice(),
        [Outbound::Reply(_)]
    ));
}

/// A client may pipeline: `create_session` then `send_message` without
/// awaiting the attach. The subscribe is already on the wire, so the daemon
/// will have a session by the time the message lands. Refusing here would
/// break the SDK's own `run()` path.
#[test]
fn a_message_pipelined_behind_create_session_is_forwarded() {
    let mut state = BridgeState::default();
    state.api_request_to_legacy(&json!({"req": "create_session", "id": 1}));
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 2, "content": "hi",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("a pipelined message must reach the daemon, not be refused");
    };
    assert_eq!(value["type"], "message");
}

/// The catalog arrives on attach, so a picker must open without a round trip.
#[test]
fn list_models_is_answered_from_the_cached_catalog() {
    let mut state = state_with_session();
    state.legacy_event_to_api(&json!({
        "type": "available_models_updated",
        "provider_model": "claude-opus-5",
        "available_models": ["claude-opus-5", "claude-fable-5"],
    }));

    let out = state.api_request_to_legacy(&json!({"id": 9, "req": "list_models"}));
    match &out[..] {
        [Outbound::Reply(frame)] => match &frame.event {
            ApiEvent::Models {
                models, current, ..
            } => {
                assert_eq!(models, &["claude-opus-5", "claude-fable-5"]);
                assert_eq!(current.as_deref(), Some("claude-opus-5"));
            }
            other => panic!("unexpected: {other:?}"),
        },
        other => panic!("expected one local reply, got {other:?}"),
    }
}

#[test]
fn model_usage_survives_catalogs_and_live_updates_without_a_round_trip() {
    let mut state = state_with_session();
    let mut route = json!({"model":"test-model","provider":"OpenAI","api_method":"openai-oauth",
        "available":true,"detail":"ready", "usage":{"count":2,"last_used_unix_secs":30,
        "tracking_started_unix_secs":10,"selection_count":5,"last_selected_unix_secs":9}});
    let catalog = json!({"type":"available_models_updated","provider_model":"test-model",
        "available_models":["test-model"],"available_model_routes":[route.clone()]});
    state.legacy_event_to_api(&catalog);
    route["usage"]["count"] = json!(3);
    route["usage"]["last_used_unix_secs"] = json!(40);
    let frames = state.legacy_event_to_api(&json!({"type":"model_usage_updated","route":route}));
    let ApiEvent::RuntimeInfo { routes, .. } = &frames[0].event else {
        panic!("usage must push runtime info");
    };
    assert_eq!(routes[0].usage.as_ref().unwrap().count, 3);
    // A pending catalog reply must not undo a newer usage delta.
    state.legacy_event_to_api(&catalog);
    let out = state.api_request_to_legacy(&json!({"id":88,"req":"get_runtime_info"}));
    let [Outbound::Reply(frame)] = &out[..] else {
        panic!("warm cache must answer locally");
    };
    let ApiEvent::RuntimeInfo { routes, .. } = &frame.event else {
        panic!("expected runtime info");
    };
    let usage = routes[0].usage.as_ref().unwrap();
    assert_eq!(usage.count, 3);
    assert_eq!(usage.last_used_unix_secs, Some(40));
    assert_eq!(usage.selection_count, 5);
    assert_eq!(usage.tracking_started_unix_secs, Some(10));
}

#[test]
fn model_usage_before_catalog_is_retained_and_legacy_routes_stay_optional() {
    let mut state = state_with_session();
    let route = json!({"model":"m","provider":"OpenAI","api_method":"openai-oauth",
        "available":true,"detail":"ready"});
    let mut update = route.clone();
    update["usage"] = json!({"count":1,"last_used_unix_secs":20,"tracking_started_unix_secs":10});
    state.legacy_event_to_api(&json!({"type":"model_usage_updated","route":update}));
    state.legacy_event_to_api(
        &json!({"type":"available_models_updated","available_models":["m"],
        "available_model_routes":[route.clone()]}),
    );
    assert_eq!(state.available_routes[0].usage.as_ref().unwrap().count, 1);
    let legacy: jcode_harness_api::ModelRouteInfo = serde_json::from_value(route).unwrap();
    assert_eq!(legacy.usage, None);
    assert!(serde_json::to_value(legacy).unwrap().get("usage").is_none());
    assert!(
        state
            .legacy_event_to_api(&json!({"type":"model_usage_updated","route":{"usage":"bad"}}))
            .is_empty()
    );
}

#[test]
fn route_capability_metadata_preserves_explicit_and_omitted_effort() {
    let mut state = state_with_session();
    state.legacy_event_to_api(&json!({
        "type":"available_models_updated", "available_model_routes":[
            {"model":"m","provider":"OpenAI","api_method":"openai-oauth@one",
             "reasoning_efforts":["none","high"],"speed_options":[{"mode":"ultrafast","availability":"available"}]},
            {"model":"m","provider":"OpenAI","api_method":"openai-oauth@two","reasoning_efforts":[]},
            {"model":"m","provider":"OpenAI","api_method":"openai-oauth@three"}
        ]
    }));
    let ApiEvent::RuntimeInfo { routes, .. } = state.runtime_info() else {
        unreachable!()
    };
    assert_eq!(
        routes[0].reasoning_efforts,
        Some(vec!["none".into(), "high".into()])
    );
    assert_eq!(routes[1].reasoning_efforts, Some(vec![]));
    assert_eq!(routes[2].reasoning_efforts, None);
    assert_eq!(
        routes[0].speed_options[0].availability,
        jcode_harness_api::SpeedAvailability::Available
    );

    for effort in ["", "none"] {
        let out = state
            .api_request_to_legacy(&json!({"id": 9,"req":"set_reasoning_effort","effort":effort}));
        assert!(matches!(&out[..], [Outbound::Legacy(value)] if value["effort"] == effort));
    }
}

/// A client can ask before the catalog lands. Answering "no models" then would
/// be a lie that empties its picker, so the request waits for the real answer.
#[test]
fn list_models_before_the_catalog_asks_the_daemon() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"id": 9, "req": "list_models"}));
    match &out[..] {
        [Outbound::Legacy(value)] => assert_eq!(value["type"], "get_model_catalog"),
        other => panic!("expected a daemon round trip, got {other:?}"),
    }

    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "history", "id": legacy_id, "session_id": "s1",
        "available_models": ["a", "b"], "provider_model": "a",
    }));
    match &frames[0].event {
        ApiEvent::Models { models, .. } => assert_eq!(models, &["a", "b"]),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(frames[0].reply_to, Some(9));
}

/// A switch must resolve the caller's request *and* tell every other client
/// watching the session that the model moved under them.
#[test]
fn a_requested_model_change_replies_and_broadcasts() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 4, "req": "set_model", "model": "claude-fable-5",
    }));
    let legacy_id = match &out[..] {
        [Outbound::Legacy(value)] => {
            assert_eq!(value["type"], "set_model");
            assert_eq!(value["model"], "claude-fable-5");
            value["id"].as_u64().unwrap()
        }
        other => panic!("expected a daemon request, got {other:?}"),
    };

    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": legacy_id,
        "model": "claude-fable-5", "provider_name": "anthropic",
    }));
    assert_eq!(frames.len(), 2, "expected a reply and a broadcast");
    assert_eq!(frames[0].reply_to, Some(4));
    assert!(matches!(frames[0].event, ApiEvent::Ok));
    assert_eq!(frames[1].reply_to, None);
    assert!(matches!(frames[1].event, ApiEvent::ModelInfo { .. }));
    // The cache must follow, or a picker reopened after the switch is wrong.
    assert_eq!(state.current_model.as_deref(), Some("claude-fable-5"));
}

/// A switch the daemon cannot apply mid-turn is answered at once, so the
/// client's request resolves instead of timing out behind the turn. The model
/// stays as it was until the switch lands, and every step is reported.
#[test]
fn a_queued_model_change_answers_immediately_and_settles_later() {
    let mut state = state_with_session();
    state.current_model = Some("claude-opus-5-5".into());
    let out = state.api_request_to_legacy(&json!({
        "id": 4, "req": "set_model", "model": "gpt-6.2-sol",
    }));
    let legacy_id = match &out[..] {
        [Outbound::Legacy(value)] => value["id"].as_u64().unwrap(),
        other => panic!("expected a daemon request, got {other:?}"),
    };

    let queued = state.legacy_event_to_api(&json!({
        "type": "model_change_pending", "id": legacy_id, "model": "gpt-6.2-sol",
    }));
    assert_eq!(queued.len(), 2, "a queued event and the reply");
    assert!(matches!(
        &queued[0].event,
        ApiEvent::ModelSwitch { state: ModelSwitchState::Queued, model: Some(model), .. }
            if model == "gpt-6.2-sol"
    ));
    assert_eq!(queued[1].reply_to, Some(4));
    assert!(matches!(
        queued[1].event,
        ApiEvent::ModelSwitch {
            state: ModelSwitchState::Queued,
            ..
        }
    ));
    assert_eq!(
        state.current_model.as_deref(),
        Some("claude-opus-5-5"),
        "a queued switch must not move the running model"
    );

    let applied = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": legacy_id,
        "model": "gpt-6.2-sol", "provider_name": "openai",
    }));
    assert!(
        applied.iter().all(|frame| frame.reply_to.is_none()),
        "the request was already answered"
    );
    assert!(
        applied
            .iter()
            .any(|frame| matches!(frame.event, ApiEvent::ModelInfo { .. }))
    );
    assert!(applied.iter().any(|frame| matches!(
        &frame.event,
        ApiEvent::ModelSwitch { state: ModelSwitchState::Applied, model: Some(model), .. }
            if model == "gpt-6.2-sol"
    )));
    assert_eq!(state.current_model.as_deref(), Some("gpt-6.2-sol"));
}

#[test]
fn a_queued_model_change_can_be_cancelled_or_fail() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 4, "req": "set_model", "model": "gpt-6.2-sol",
    }));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    state.legacy_event_to_api(&json!({
        "type": "model_change_pending", "id": legacy_id, "model": "gpt-6.2-sol",
    }));

    let out = state.api_request_to_legacy(&json!({"id": 5, "req": "cancel_pending_model"}));
    let cancel_id = match &out[..] {
        [Outbound::Legacy(value)] => {
            assert_eq!(value["type"], "cancel_pending_model_change");
            value["id"].as_u64().unwrap()
        }
        other => panic!("expected a daemon request, got {other:?}"),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_change_cancelled", "id": cancel_id,
        "pending_id": legacy_id, "model": "gpt-6.2-sol",
    }));
    assert!(frames.iter().any(|frame| matches!(
        &frame.event,
        ApiEvent::ModelSwitch { state: ModelSwitchState::Cancelled, model: Some(model), .. }
            if model == "gpt-6.2-sol"
    )));
    assert!(
        frames
            .iter()
            .any(|frame| frame.reply_to == Some(5) && matches!(frame.event, ApiEvent::Ok))
    );

    // A queued switch that the daemon then rejects is an event, not silence.
    let out = state.api_request_to_legacy(&json!({
        "id": 6, "req": "set_model", "model": "nope",
    }));
    let failing_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    state.legacy_event_to_api(&json!({
        "type": "model_change_pending", "id": failing_id, "model": "nope",
    }));
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": failing_id, "model": "a", "error": "unknown model",
    }));
    assert!(matches!(
        &frames[..],
        [frame] if matches!(
            &frame.event,
            ApiEvent::ModelSwitch { state: ModelSwitchState::Failed, error: Some(error), .. }
                if error == "unknown model"
        )
    ));
}

/// The daemon reports a rejected switch in-band, on a success-shaped event.
/// Reporting success there would leave the client's picker showing a model
/// the session is not using.
#[test]
fn a_rejected_model_change_fails_the_request() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 4, "req": "set_model", "model": "nope",
    }));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": legacy_id,
        "model": "nope", "error": "unknown model",
    }));
    match &frames[..] {
        [frame] => {
            assert_eq!(frame.reply_to, Some(4));
            match &frame.event {
                ApiEvent::Error { code, message } => {
                    assert_eq!(*code, ErrorCode::InvalidRequest);
                    assert_eq!(message, "unknown model");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        other => panic!("expected one error reply, got {other:?}"),
    }
    assert_eq!(
        state.current_model, None,
        "a failed switch must not be cached"
    );
}

#[test]
fn an_empty_model_is_refused_locally() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"id": 4, "req": "set_model", "model": ""}));
    match &out[..] {
        [Outbound::Reply(frame)] => {
            assert!(matches!(frame.event, ApiEvent::Error { .. }));
        }
        other => panic!("expected a local rejection, got {other:?}"),
    }
}

#[test]
fn speed_mode_ack_and_attach_snapshot_are_independent_of_effort() {
    use jcode_harness_api::{SpeedAvailability, SpeedMode};
    let mut state = state_with_session();
    let options = json!([{"mode":"standard","availability":"available"}, {"mode":"fast","availability":"unknown"}]);
    state.note_models(
        &json!({"reasoning_effort":"high", "speed_mode":"standard", "speed_options":options}),
    );
    for (api_id, mode, error) in [(5, "ultrafast", Some("not entitled")), (6, "fast", None)] {
        let out = state.api_request_to_legacy(
            &json!({"id":api_id, "req":"set_speed_mode", "speed_mode":mode}),
        );
        let Outbound::Legacy(request) = &out[0] else {
            panic!("expected live request")
        };
        assert_eq!(request["type"], "set_speed_mode");
        assert_eq!(request["speed_mode"], mode);
        assert!(request.get("effort").is_none());
        let frames = state.legacy_event_to_api(&json!({"type":"speed_mode_changed", "id":request["id"], "speed_mode":mode, "speed_options":options, "error":error}));
        assert_eq!(frames[0].reply_to, Some(api_id));
        match error {
            Some(_) => assert!(matches!(frames[0].event, ApiEvent::Error { .. })),
            None => assert!(matches!(
                frames[0].event,
                ApiEvent::SpeedModeChanged {
                    speed_mode: SpeedMode::Fast,
                    ..
                }
            )),
        }
    }
    assert!(
        matches!(state.runtime_info(), ApiEvent::RuntimeInfo { speed_mode: SpeedMode::Fast, reasoning_effort: Some(effort), speed_options, .. } if effort == "high" && speed_options[1].availability == SpeedAvailability::Unknown)
    );
    state.note_models(
        &json!({"reasoning_effort":"high", "speed_mode":"standard", "speed_options":options}),
    );
    assert!(
        matches!(state.runtime_info(), ApiEvent::RuntimeInfo { speed_mode: SpeedMode::Standard, reasoning_effort: Some(effort), .. } if effort == "high")
    );
}

#[test]
fn provider_speed_observation_never_overwrites_the_selected_mode() {
    use jcode_harness_api::SpeedMode;
    let mut state = state_with_session();
    state.note_models(&json!({"speed_mode":"fast"}));
    for event in [
        json!({"type":"service_tier_reported", "requested":"priority", "tier":"default"}),
        json!({"type":"speed_reported", "requested":"fast", "actual":"standard"}),
    ] {
        let frames = state.legacy_event_to_api(&event);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].reply_to, None);
        assert!(matches!(
            frames[0].event,
            ApiEvent::SpeedModeReported {
                requested: Some(SpeedMode::Fast),
                actual: SpeedMode::Standard,
                ..
            }
        ));
    }
    assert!(matches!(
        state.runtime_info(),
        ApiEvent::RuntimeInfo {
            speed_mode: SpeedMode::Fast,
            ..
        }
    ));
    let detail =
        state.legacy_event_to_api(&json!({"type":"status_detail", "detail":"Fast cooldown"}));
    assert!(
        matches!(&detail[0].event, ApiEvent::ProviderStatusDetail { detail, .. } if detail == "Fast cooldown")
    );
}

#[test]
fn reasoning_effort_reports_provider_refusal() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 5, "req": "set_reasoning_effort", "effort": "max",
    }));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => {
            assert_eq!(value["effort"], "max");
            value["id"].as_u64().unwrap()
        }
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "reasoning_effort_changed", "id": legacy_id,
        "error": "provider does not support reasoning effort",
    }));
    assert_eq!(frames[0].reply_to, Some(5));
    assert!(matches!(frames[0].event, ApiEvent::Error { .. }));
}

/// An effort change is identity, like a model change: every attached client
/// needs to hear it, not only the requester. A change made by another client
/// (no pending request here) must still arrive as a `model_info` broadcast,
/// and the requester's own change gets the broadcast after its `Ok`.
#[test]
fn reasoning_effort_changes_are_broadcast_as_model_info() {
    let mut state = state_with_session();

    // Unsolicited change (another client's request id): broadcast only.
    let frames = state.legacy_event_to_api(&json!({
        "type": "reasoning_effort_changed", "id": 999, "effort": "high",
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, None);
    match &frames[0].event {
        ApiEvent::ModelInfo {
            reasoning_effort, ..
        } => assert_eq!(reasoning_effort.as_deref(), Some("high")),
        other => panic!("expected model_info, got {other:?}"),
    }

    // The same effort again is not news: no broadcast.
    let frames = state.legacy_event_to_api(&json!({
        "type": "reasoning_effort_changed", "id": 999, "effort": "high",
    }));
    assert!(frames.is_empty(), "unchanged effort must not re-broadcast");

    // This client's own change: Ok reply first, then the broadcast.
    let out = state.api_request_to_legacy(&json!({
        "id": 7, "req": "set_reasoning_effort", "effort": "low",
    }));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "reasoning_effort_changed", "id": legacy_id, "effort": "low",
    }));
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].reply_to, Some(7));
    assert!(matches!(frames[0].event, ApiEvent::Ok));
    assert!(matches!(
        &frames[1].event,
        ApiEvent::ModelInfo { reasoning_effort, .. }
            if reasoning_effort.as_deref() == Some("low")
    ));
}

/// Compaction can be refused (nothing to compact, a turn in flight) and the
/// daemon says so with `success: false`, not an error frame. Telling the
/// client "done" would claim work that never happened.
#[test]
fn a_refused_compaction_is_an_error_not_a_success() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"id": 6, "req": "compact"}));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "compact_result", "id": legacy_id,
        "message": "nothing to compact", "success": false,
    }));
    match &frames[0].event {
        ApiEvent::Error { message, .. } => assert_eq!(message, "nothing to compact"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn a_scheduled_compaction_reports_its_status() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"id": 6, "req": "compact"}));
    let legacy_id = match &out[0] {
        Outbound::Legacy(value) => value["id"].as_u64().unwrap(),
        _ => unreachable!(),
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "compact_result", "id": legacy_id,
        "message": "compacting in the background", "success": true,
    }));
    match &frames[0].event {
        ApiEvent::Compacted { message, .. } => assert_eq!(message, "compacting in the background"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn compaction_completion_is_forwarded_as_divider() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "compaction", "trigger": "manual", "pre_tokens": 10000,
        "post_tokens": 4000, "tokens_saved": 6000, "duration_ms": 20,
        "messages_compacted": 8
    }));
    assert!(frames.iter().any(|frame| matches!(&frame.event,
        ApiEvent::CompactionDivider { content, running: false, pre_tokens: Some(10000), post_tokens: Some(4000), .. }
            if content.contains("10000") && content.contains("4000"))));
}

#[test]
fn background_compaction_error_is_forwarded_as_finished_progress() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "compact_result", "id": 0, "success": false,
        "message": "Compaction failed: provider unavailable"
    }));
    assert!(frames.iter().any(|frame| matches!(&frame.event,
        ApiEvent::BackgroundProgress { summary, done: true, .. }
            if summary.contains("provider unavailable"))));
}

/// Saving forwards the flag and label so the daemon can title the session.
#[test]
fn set_session_saved_forwards_flag_and_label() {
    let mut state = state_with_session();
    let save = state.api_request_to_legacy(&json!({
        "id": 7, "req": "set_session_saved", "saved": true, "label": "yc mcp",
    }));
    match &save[0] {
        Outbound::Legacy(value) => {
            assert_eq!(value["type"], "set_session_saved");
            assert_eq!(value["saved"], true);
            assert_eq!(value["label"], "yc mcp");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let unsave = state.api_request_to_legacy(&json!({
        "id": 8, "req": "set_session_saved", "saved": false,
    }));
    match &unsave[0] {
        Outbound::Legacy(value) => {
            assert_eq!(value["saved"], false);
            assert!(value.get("label").is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn set_session_env_requires_attachment_and_forwards_replacement() {
    let mut state = state_with_session();
    for env in [json!({"TOKEN": "private-secret", "EMPTY": ""}), json!({})] {
        let output = state.api_request_to_legacy(&json!({
            "id": 7, "req": "set_session_env", "env": env,
        }));
        let [Outbound::Legacy(legacy)] = &output[..] else {
            panic!("expected legacy setter");
        };
        assert_eq!(legacy["type"], "set_session_env");
        assert_eq!(legacy["env"], env);
    }
    for env in [Value::Null, json!([]), json!({"TOKEN": 42})] {
        let output = state.api_request_to_legacy(&json!({
            "id": 8, "req": "set_session_env", "env": env,
        }));
        assert!(matches!(
            &output[..],
            [Outbound::Reply(ServerFrame {
                event: ApiEvent::Error { .. },
                ..
            })]
        ));
    }
    let output = BridgeState::default().api_request_to_legacy(&json!({
        "id": 9, "req": "set_session_env", "env": {},
    }));
    assert!(matches!(
        &output[..],
        [Outbound::Reply(ServerFrame {
            event: ApiEvent::Error { .. },
            ..
        })]
    ));
}

/// Clearing a title is distinct from setting an empty one, so an absent title
/// must not be sent as `""`, which the daemon would store as a real title.
#[test]
fn renaming_distinguishes_clearing_from_setting() {
    let mut state = state_with_session();
    let set = state.api_request_to_legacy(&json!({
        "id": 7, "req": "rename_session", "title": "my session",
    }));
    match &set[0] {
        Outbound::Legacy(value) => assert_eq!(value["title"], "my session"),
        other => panic!("unexpected: {other:?}"),
    }

    let clear = state.api_request_to_legacy(&json!({"id": 8, "req": "rename_session"}));
    match &clear[0] {
        Outbound::Legacy(value) => assert!(
            value.get("title").is_none(),
            "a cleared title must be absent, not empty: {value}"
        ),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn a_rename_push_becomes_a_typed_event() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "session_renamed", "session_id": "s1",
        "title": "my session", "display_title": "my session",
    }));
    match &frames[0].event {
        ApiEvent::SessionRenamed {
            session_id,
            title,
            display_title,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(title.as_deref(), Some("my session"));
            assert_eq!(display_title, "my session");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Every capability request is stateful, so none may be forwarded before the
/// connection is attached: the daemon closes the connection on those.
#[test]
fn capability_requests_need_an_attached_session() {
    for (req, extra) in [
        ("list_models", json!({})),
        ("set_model", json!({"model": "x"})),
        ("set_reasoning_effort", json!({"effort": "high"})),
        ("compact", json!({})),
        ("rename_session", json!({})),
        ("rewind_undo", json!({})),
        ("cancel_soft_interrupts", json!({})),
        ("background_tool", json!({})),
    ] {
        let mut state = BridgeState::default();
        let mut request = json!({"id": 1, "req": req});
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        let out = state.api_request_to_legacy(&request);
        match &out[..] {
            [Outbound::Reply(frame)] => match &frame.event {
                ApiEvent::Error { code, .. } => assert_eq!(
                    *code,
                    ErrorCode::UnknownSession,
                    "{req} should report an unattached session"
                ),
                other => panic!("{req}: unexpected {other:?}"),
            },
            other => panic!("{req} reached the daemon unattached: {other:?}"),
        }
    }
}

#[test]
fn another_sessions_broadcast_does_not_replace_the_attachment() {
    let home = ScopedJcodeHome::new("other-session-broadcast");
    write_session_record(&home.path, "session_retriever_1_a", Path::new("/workspace"));
    let mut state = BridgeState::default();
    let attach = state.api_request_to_legacy(&json!({
        "id": 7,
        "req": "attach_session",
        "session_id": "session_retriever_1_a",
    }));
    let state_id = match &attach[1] {
        Outbound::Legacy(value) => value["id"].as_u64().expect("state request id"),
        other => panic!("unexpected attach output: {other:?}"),
    };
    state.legacy_event_to_api(&json!({
        "type": "state",
        "id": state_id,
        "session_id": "session_retriever_1_a",
    }));

    state.legacy_event_to_api(&json!({
        "type": "session",
        "session_id": "session_pawprint_2_b",
    }));

    assert!(matches!(
        state
            .api_request_to_legacy(&json!({
                "id": 8,
                "req": "send_message",
                "session_id": "session_retriever_1_a",
                "content": "still routed to the attached session",
            }))
            .as_slice(),
        [Outbound::Legacy(_)]
    ));
}

#[test]
fn another_sessions_state_does_not_replace_the_attachment() {
    let home = ScopedJcodeHome::new("other-session-state");
    write_session_record(&home.path, "session_retriever_1_a", Path::new("/workspace"));
    let mut state = BridgeState::default();
    let attach = state.api_request_to_legacy(&json!({
        "id": 7,
        "req": "attach_session",
        "session_id": "session_retriever_1_a",
    }));
    let state_id = match &attach[1] {
        Outbound::Legacy(value) => value["id"].as_u64().expect("state request id"),
        other => panic!("unexpected attach output: {other:?}"),
    };
    state.legacy_event_to_api(&json!({
        "type": "state",
        "id": state_id,
        "session_id": "session_retriever_1_a",
    }));

    state.legacy_event_to_api(&json!({
        "type": "state",
        "id": state_id + 100,
        "session_id": "session_pawprint_2_b",
    }));

    assert!(matches!(
        state
            .api_request_to_legacy(&json!({
                "id": 8,
                "req": "send_message",
                "session_id": "session_retriever_1_a",
                "content": "still routed to the attached session",
            }))
            .as_slice(),
        [Outbound::Legacy(_)]
    ));
}

#[test]
fn legacy_request_ids_are_unique_across_bridge_connections() {
    let home = ScopedJcodeHome::new("unique-legacy-request-ids");
    write_session_record(&home.path, "session_first", Path::new("/workspace/first"));
    write_session_record(&home.path, "session_second", Path::new("/workspace/second"));
    let mut first = BridgeState::default();
    let mut second = BridgeState::default();
    let first_attach = first.api_request_to_legacy(&json!({
        "id": 1,
        "req": "attach_session",
        "session_id": "session_first",
    }));
    let second_attach = second.api_request_to_legacy(&json!({
        "id": 1,
        "req": "attach_session",
        "session_id": "session_second",
    }));
    let request_id = |outbound: &[Outbound]| match &outbound[1] {
        Outbound::Legacy(value) => value["id"].as_u64().expect("state request id"),
        other => panic!("unexpected attach output: {other:?}"),
    };

    assert_ne!(request_id(&first_attach), request_id(&second_attach));
}

#[test]
fn colliding_state_id_for_another_target_does_not_complete_attach() {
    let home = ScopedJcodeHome::new("colliding-state-id");
    write_session_record(&home.path, "session_wanted", Path::new("/workspace"));
    let mut state = BridgeState::default();
    let attach = state.api_request_to_legacy(&json!({
        "id": 7,
        "req": "attach_session",
        "session_id": "session_wanted",
    }));
    let state_id = match &attach[1] {
        Outbound::Legacy(value) => value["id"].as_u64().expect("state request id"),
        other => panic!("unexpected attach output: {other:?}"),
    };

    assert!(
        state
            .legacy_event_to_api(&json!({
                "type": "state",
                "id": state_id,
                "session_id": "session_other",
            }))
            .is_empty()
    );
    assert!(state.session_id.is_none());
}

/// A session id becomes a filesystem path, so it must be treated as untrusted.
///
/// The id arrives straight off the wire and is interpolated into
/// `<home>/sessions/<id>.json`. Without validation, a traversal id is a
/// readable path, and `peek_session` returns whatever it finds there.
#[test]
fn a_session_id_cannot_escape_the_sessions_directory() {
    for hostile in [
        "../../../etc/passwd",
        "../.ssh/id_rsa",
        "a/b",
        "a\\b",
        "..",
        "",
        "with space",
        "semi;colon",
    ] {
        assert!(
            BridgeState::session_record_path(hostile).is_none(),
            "`{hostile}` must not resolve to a session record path"
        );
    }
}

#[test]
fn a_plain_session_id_still_resolves() {
    let path = BridgeState::session_record_path("session_otter_1785728596263_80eb5ad6012a1864")
        .expect("a normal session id must resolve");
    assert!(path.ends_with("session_otter_1785728596263_80eb5ad6012a1864.json"));
    assert!(
        path.parent().is_some_and(|dir| dir.ends_with("sessions")),
        "records live in the sessions directory: {}",
        path.display()
    );
}

/// Session records must be read from the *instance's* home, not the user's.
///
/// `launch()` gives an embedded instance its own `JCODE_HOME` precisely so it
/// cannot see the user's work. Reading the user's home directly made
/// `peek_session` return the real transcripts of the jcode the user runs
/// interactively, from a client that was supposed to be sandboxed.
#[test]
fn session_records_are_read_from_the_instance_home() {
    let home = ScopedJcodeHome::new("instance-home");
    let path = BridgeState::session_record_path("session_x_1_a");
    let path = path.expect("a normal session id must resolve");
    assert!(
        path.starts_with(&home.path),
        "JCODE_HOME must scope session records, got {}",
        path.display()
    );
}

#[test]
fn unattached_list_sessions_handles_no_session_candidates() {
    let home = ScopedJcodeHome::new("empty-session-discovery");
    let sessions_dir = home.path.join("sessions");
    let mut state = BridgeState::default();

    // Missing directory, empty directory, and entries that are all filtered out.
    for layout in 0..3 {
        if layout == 1 {
            std::fs::create_dir(&sessions_dir).unwrap();
        } else if layout == 2 {
            std::fs::write(sessions_dir.join("not-a-session.txt"), "ignored").unwrap();
            std::fs::create_dir(sessions_dir.join("directory.json")).unwrap();
        }
        for limit in [None, Some(0), Some(10)] {
            let event = only_reply_event(state.api_request_to_legacy(&json!({
                "req": "list_sessions", "id": 1, "limit": limit,
            })));
            assert_eq!(
                event,
                ApiEvent::Sessions {
                    sessions: vec![],
                    workers: Default::default(),
                }
            );
            assert_eq!(
                only_reply_event(state.api_request_to_legacy(&json!({"req": "ping", "id": 2}))),
                ApiEvent::Pong,
            );
        }
    }

    write_session_record(&home.path, "first_session", &home.path);
    assert_eq!(BridgeState::stored_session_ids(None), ["first_session"]);
}

/// A swarm worker's model, effort and spend live in its own record, so the
/// catalog carries them: the sidebar shows a model and a token count for a
/// worker it never opened.
#[test]
fn list_sessions_carries_each_workers_own_record_facts() {
    let home = ScopedJcodeHome::new("worker-facts");
    let sessions = home.path.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("session_lead_1_a.json"),
        json!({"working_dir": home.path, "messages": []}).to_string(),
    )
    .unwrap();
    std::fs::write(
        sessions.join("session_fox_2_b.json"),
        json!({
            "working_dir": home.path,
            "model": "deepseek-v4.1-flash",
            "provider_key": "opencode-go",
            "reasoning_effort": "high",
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "c1", "name": "bash", "input": {}}],
                "token_usage": {"prompt_tokens": 12000, "output_tokens": 3000},
            }],
        })
        .to_string(),
    )
    .unwrap();
    let swarm = home.path.join("state/swarm");
    std::fs::create_dir_all(&swarm).unwrap();
    std::fs::write(
        swarm.join("s.json"),
        json!({
            "updated_at_unix_ms": 2,
            "members": [{
                "session_id": "session_fox_2_b",
                "report_back_to_session_id": "session_lead_1_a",
                "task_label": "compact rows",
                "status": "running",
            }],
        })
        .to_string(),
    )
    .unwrap();

    let event = only_reply_event(
        BridgeState::default().api_request_to_legacy(&json!({"req": "list_sessions", "id": 1})),
    );
    let ApiEvent::Sessions { sessions, workers } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    let fox = sessions
        .iter()
        .find(|session| session.session_id == "session_fox_2_b")
        .expect("the worker is listed");
    assert_eq!(fox.parent_session_id.as_deref(), Some("session_lead_1_a"));
    assert_eq!(fox.agent_label.as_deref(), Some("compact rows"));
    let meta = workers.get("session_fox_2_b").expect("worker facts");
    assert_eq!(meta.model.as_deref(), Some("deepseek-v4.1-flash"));
    assert_eq!(meta.provider.as_deref(), Some("opencode-go"));
    assert_eq!(meta.effort.as_deref(), Some("high"));
    assert_eq!(meta.input_tokens, Some(12_000));
    assert_eq!(meta.output_tokens, Some(3_000));
    assert_eq!(meta.phase, Some(jcode_harness_api::WorkerPhase::Tool));
    assert_eq!(meta.tool.as_deref(), Some("bash"));
    // A coordinator is not a worker; reading its record would only duplicate
    // what the session catalog already knows.
    assert!(!workers.contains_key("session_lead_1_a"));
}

#[test]
fn user_connections_are_global_independent_and_fresh_in_list_metadata() {
    let home = ScopedJcodeHome::new("global-user-presence");
    for id in [
        "connected-idle",
        "connected-busy",
        "loaded-history",
        "headless",
    ] {
        write_session_record(&home.path, id, &home.path);
    }
    let mut state = BridgeState {
        user_connected_sessions: ["connected-idle".into(), "connected-busy".into()]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let ApiEvent::Sessions { sessions, .. } =
        only_reply_event(state.api_request_to_legacy(&json!({"req":"list_sessions","id":7})))
    else {
        panic!("expected list");
    };
    for session in sessions {
        assert_eq!(
            session.user_connected,
            session.session_id.starts_with("connected-")
        );
        assert_eq!(
            session.status, "idle",
            "connection truth must not overwrite lifecycle/attachment status"
        );
    }
    let ApiEvent::Sessions {
        sessions: limited, ..
    } = only_reply_event(
        state.api_request_to_legacy(&json!({"req":"list_sessions","id":9,"limit":0})),
    )
    else {
        panic!("expected list");
    };
    assert_eq!(
        limited.len(),
        2,
        "connected idle users survive an empty recency page"
    );
    assert!(limited.iter().all(|session| session.user_connected));
    state.user_connected_sessions.clear();
    let ApiEvent::Sessions { sessions, .. } =
        only_reply_event(state.api_request_to_legacy(&json!({"req":"list_sessions","id":8})))
    else {
        panic!("expected list");
    };
    assert!(sessions.iter().all(|session| !session.user_connected));
    let legacy: SessionInfo =
        serde_json::from_value(json!({"session_id":"legacy","status":"idle"})).unwrap();
    assert!(!legacy.user_connected);
}

#[test]
fn unattached_list_sessions_discovers_all_persisted_records() {
    let home = ScopedJcodeHome::new("persisted-discovery");
    let first_root = home.path.join("first-project");
    let second_root = home.path.join("second-project");
    std::fs::create_dir_all(&first_root).unwrap();
    std::fs::create_dir_all(&second_root).unwrap();
    write_session_record_with_titles(
        &home.path,
        "persisted_one",
        &first_root,
        Some("  Generated first title  "),
        None,
    );
    write_session_record_with_titles(
        &home.path,
        "persisted_two",
        &second_root,
        Some("Generated second title"),
        Some("  Custom second title  "),
    );
    std::fs::write(home.path.join("sessions/not-a-session.txt"), "ignored").unwrap();

    let event = only_reply_event(
        BridgeState::default().api_request_to_legacy(&json!({"req": "list_sessions", "id": 1})),
    );
    let ApiEvent::Sessions { sessions, .. } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        ["persisted_one", "persisted_two"]
    );
    assert_eq!(sessions[0].working_dir.as_deref(), first_root.to_str());
    assert_eq!(sessions[1].working_dir.as_deref(), second_root.to_str());
    assert_eq!(sessions[0].title.as_deref(), Some("Generated first title"));
    assert_eq!(sessions[1].title.as_deref(), Some("Custom second title"));
}

#[test]
fn limited_session_list_reads_compact_index_without_transcript_records() {
    let home = ScopedJcodeHome::new("metadata-index");
    assert!(BridgeState::recent_session_index_entries().is_empty());
    let mut connection = Connection::open(home.path.join("session-metadata-v1.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..100 {
        transaction
            .execute(
                "INSERT INTO recent_sessions (
                     session_id, working_dir, todo_title, saved, updated_at_ms,
                     last_active_at_ms, save_label
                 ) VALUES (?1, '/indexed/project', ?2, ?4, ?3, ?3, ?5)",
                params![
                    format!("indexed_{index:03}"),
                    format!("Indexed goal {index}"),
                    index,
                    index == 99,
                    (index == 99).then_some("investor catch up"),
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();

    let event = only_reply_event(
        BridgeState::default()
            .api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 100})),
    );
    let ApiEvent::Sessions { sessions, .. } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    assert_eq!(sessions.len(), 100);
    let newest = sessions
        .iter()
        .find(|session| session.session_id == "indexed_099")
        .expect("indexed newest session");
    assert!(newest.saved);
    assert_eq!(newest.save_label.as_deref(), Some("investor catch up"));
    assert_eq!(newest.updated_at_ms, Some(99));
    assert_eq!(newest.last_active_at_ms, Some(99));
    // A save label is the name the user chose, so it outranks derived titles.
    assert_eq!(newest.title.as_deref(), Some("investor catch up"));
    assert!(
        sessions
            .iter()
            .filter(|session| session.session_id != "indexed_099")
            .all(|session| {
                session
                    .title
                    .as_deref()
                    .is_some_and(|title| title.starts_with("Indexed goal "))
            })
    );
}

#[test]
fn runtime_info_reports_the_active_provider_and_complete_route_catalog() {
    let mut state = state_with_session();
    state.legacy_event_to_api(&json!({
        "type": "available_models_updated",
        "provider_name": "anthropic",
        "provider_model": "claude-sonnet",
        "reasoning_effort": "high",
        "resolved_credential": "oauth",
        "available_models": ["claude-sonnet", "gemini-pro"],
        "available_model_routes": [
            {
                "model": "claude-sonnet",
                "provider": "anthropic",
                "api_method": "messages",
                "available": true,
                "detail": "ready"
            },
            {
                "model": "gemini-pro",
                "provider": "gemini",
                "api_method": "generateContent",
                "available": false,
                "detail": "credential missing"
            }
        ]
    }));

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "get_runtime_info",
        "id": 4,
        "session_id": "s1"
    })));
    let ApiEvent::RuntimeInfo {
        session_id,
        provider,
        model,
        reasoning_effort,
        auth_method,
        routes,
        ..
    } = event
    else {
        panic!("expected runtime info, got {event:?}");
    };
    assert_eq!(session_id, "s1");
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert_eq!(model.as_deref(), Some("claude-sonnet"));
    assert_eq!(reasoning_effort.as_deref(), Some("high"));
    // The daemon's resolved credential travels with the identity, so a
    // client never has to guess it from a model that has both routes.
    assert_eq!(auth_method.as_deref(), Some("oauth"));
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[1].provider, "gemini");
    assert!(!routes[1].available);
}

#[test]
fn retention_never_archives_users_connected_in_other_windows() {
    let home = ScopedJcodeHome::new("connected-retention");
    let old_time = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
    for id in ["old-connected", "old-disconnected"] {
        let path = write_session_record(&home.path, id, &home.path);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
    }
    let mut state = BridgeState {
        user_connected_sessions: ["old-connected".into()].into_iter().collect(),
        ..Default::default()
    };
    only_reply_event(state.api_request_to_legacy(
        &json!({"req":"set_retention_policy","id":1,"archive_after_days":1}),
    ));
    let ApiEvent::Sessions { sessions, .. } = only_reply_event(
        state.api_request_to_legacy(&json!({"req":"list_sessions","id":2,"include_archived":true})),
    ) else {
        panic!("expected list");
    };
    let connected = sessions
        .iter()
        .find(|session| session.session_id == "old-connected")
        .unwrap();
    assert!(connected.user_connected);
    assert!(!connected.archived);
    assert!(
        sessions
            .iter()
            .find(|session| session.session_id == "old-disconnected")
            .unwrap()
            .archived
    );
}

#[test]
fn archive_restore_and_retention_are_reversible_and_owner_only() {
    let home = ScopedJcodeHome::new("archive");
    let root = home.path.join("project");
    std::fs::create_dir_all(&root).unwrap();
    write_session_record(&home.path, "recent_session", &root);
    let old_record = write_session_record(&home.path, "old_session", &root);
    let old_time = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&old_record)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old_time))
        .unwrap();

    let mut state = BridgeState::default();
    assert!(matches!(
        only_reply_event(state.api_request_to_legacy(&json!({
            "req": "archive_session",
            "id": 1,
            "session_id": "recent_session"
        }))),
        ApiEvent::Ok
    ));
    let ApiEvent::Sessions { sessions, .. } =
        only_reply_event(state.api_request_to_legacy(&json!({"req": "list_sessions", "id": 2})))
    else {
        panic!("expected sessions");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "old_session");

    assert!(matches!(
        only_reply_event(state.api_request_to_legacy(&json!({
            "req": "restore_session",
            "id": 3,
            "session_id": "recent_session"
        }))),
        ApiEvent::Ok
    ));
    assert!(matches!(
        only_reply_event(state.api_request_to_legacy(&json!({
            "req": "set_retention_policy",
            "id": 4,
            "archive_after_days": 1
        }))),
        ApiEvent::Ok
    ));

    let ApiEvent::Sessions { sessions, .. } =
        only_reply_event(state.api_request_to_legacy(&json!({
            "req": "list_sessions",
            "id": 5,
            "include_archived": true
        })))
    else {
        panic!("expected sessions");
    };
    let old = sessions
        .iter()
        .find(|session| session.session_id == "old_session")
        .expect("old session remains restorable");
    assert!(old.archived);
    assert!(old.archived_at_ms.is_some());
    let recent = sessions
        .iter()
        .find(|session| session.session_id == "recent_session")
        .expect("restored session is listed");
    assert!(!recent.archived);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path.join("sdk-archive.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let home_mode = std::fs::metadata(&home.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(home_mode, 0o700);
    }
}
