use super::*;

#[test]
fn codemode_bridge_keeps_ordered_receipts_separate_from_the_wrapper_result() {
    let mut state = state_with_session();
    let calls = json!([
        { "tool": "swarm", "action": "message", "actor": "fox", "target": "fish", "direction": "outbound", "status": "success", "duration_ms": 20, "preview": "Проверка" },
        { "tool": "lookup", "status": "error", "duration_ms": 3, "diagnostic": "Resource not found" }
    ]);
    let body = "Замечание 🧪\n\ncalls:\nвнутри тела\n";
    let wrapper = format!("{body}\ncalls:\n  #1 swarm ok\n");
    let frames = state.legacy_event_to_api(&json!({
        "type":"tool_done", "id":"wrapper", "name":"codemode", "output":wrapper,
        "codemode_status":"error", "codemode_calls": calls, "report_body_bytes": body.len()
    }));
    let value = serde_json::to_value(&frames[0].event).unwrap();
    assert_eq!(value["call_id"], "wrapper");
    assert_eq!(value["output"], wrapper);
    assert_eq!(value["codemode_calls"], calls);
    assert_eq!(value["report_body_bytes"], body.len());
    assert_eq!(value["codemode_status"], "error");
    assert!(value.get("error").is_none());

    let row = json!({"role":"tool", "tool_data":{"id":"wrapper", "name":"codemode"},
        "codemode_status":"error", "codemode_calls": calls, "report_body_bytes": body.len()});
    let history_tool = BridgeState::history_tool(&row).unwrap();
    let history = serde_json::to_value(history_tool).unwrap();
    assert_eq!(history["codemode_calls"], calls);
    assert_eq!(history["report_body_bytes"], body.len());
}

#[test]
fn legacy_swarm_statuses_have_distinct_receive_order_stamps() {
    let mut state = state_with_session();
    let stamps = ["ready", "running"].map(|status| {
        let frames = state.legacy_event_to_api(&json!({
            "type":"swarm_status",
            "members":[{"session_id":"worker", "status":status}]
        }));
        let ApiEvent::SwarmStatus {
            updated_at_unix_ms, ..
        } = frames[0].event
        else {
            panic!("missing swarm status")
        };
        updated_at_unix_ms
    });
    assert!(stamps[0] < stamps[1]);
}

#[test]
fn swarm_spawn_ready_then_running_is_forwarded_without_dropping_either_snapshot() {
    let mut state = state_with_session();
    for (status, timestamp) in [("ready", 1_000), ("running", 1_065)] {
        let frames = state.legacy_event_to_api(&json!({
            "type": "swarm_status",
            "members": [{
                "session_id": "gorilla", "status": status,
                "report_back_to_session_id": "s1", "task_label": "worker",
                "runtime": {"snapshot_at_unix_ms": timestamp, "model": "test-model", "effort": "medium"}
            }]
        }));
        assert_eq!(frames.len(), 1, "must forward {status}, not drop it");
        let event = serde_json::to_value(&frames[0].event).unwrap();
        assert_eq!(event["ev"], "swarm_status");
        assert_eq!(event["session_id"], "s1");
        assert_eq!(event["members"][0]["status"], status);
        assert_eq!(event["members"][0]["model"], "test-model");
        assert_eq!(event["members"][0]["effort"], "medium");
        assert_eq!(event["updated_at_unix_ms"], timestamp);
    }
}

#[test]
fn swarm_worker_control_notice_is_session_scoped_and_not_a_chat_message() {
    let mut state = state_with_session();
    let mut event = json!({
        "type": "notification",
        "from_session": "jcode",
        "notification_type": {"kind": "message", "scope": "swarm_worker_control"},
        "message": "worker: model set to test-model · medium"
    });
    let frames = state.legacy_event_to_api(&event);
    assert!(matches!(&frames[..], [frame] if matches!(&frame.event,
        ApiEvent::SwarmWorkerNotice { session_id, message }
            if session_id == "s1" && message == "worker: model set to test-model · medium")));
    event["from_session"] = json!("sibling");
    assert!(state.legacy_event_to_api(&event).is_empty());
}

#[test]
fn malformed_image_metadata_is_not_a_successful_empty_history() {
    for field in ["images", "messages"] {
        let mut state = state_with_session();
        let request =
            state.api_request_to_legacy(&json!({"req":"get_compacted_history", "id":912}));
        let id = legacy_out(&request)["id"].as_u64().unwrap();
        let mut event = json!({"type":"compacted_history", "id":id, "session_id":"s1", "messages":[], "images":[]});
        event[field] = json!({"not":"a list"});
        let frames = state.legacy_event_to_api(&event);
        assert!(
            matches!(&frames[..], [frame] if frame.reply_to == Some(912) && matches!(&frame.event, ApiEvent::Error { .. })),
            "{frames:?}"
        );
    }
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type":"user_image_ids", "image_ids":{"not":"an id list"},
        "original_content":"photo", "content":"photo",
    }));
    assert!(
        matches!(&frames[..], [frame] if matches!(&frame.event, ApiEvent::Error { .. })),
        "{frames:?}"
    );
}

#[test]
fn daemon_optional_lists_accept_absence_but_reject_present_wrong_types() {
    for event in [json!({}), json!({"images":null}), json!({"images":[]})] {
        assert!(
            BridgeState::daemon_list::<Value>(&event, "images")
                .unwrap()
                .is_empty()
        );
    }
    for value in [json!({}), json!("image"), json!(false)] {
        assert!(BridgeState::daemon_list::<Value>(&json!({"images":value}), "images").is_err());
    }
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"get_history_page", "id":913}));
    let frames = state.legacy_event_to_api(
        &json!({"type":"history_page", "id":legacy_out(&out)["id"], "fragments":{}}),
    );
    assert!(
        matches!(&frames[..], [frame] if frame.reply_to == Some(913) && matches!(&frame.event, ApiEvent::Error { .. })),
        "{frames:?}"
    );
    let frames = state.legacy_event_to_api(&json!({"type":"ask_user_request", "questions":{}}));
    assert!(
        matches!(&frames[..], [frame] if matches!(&frame.event, ApiEvent::Error { .. })),
        "{frames:?}"
    );
}

#[test]
fn force_stop_is_capability_gated_and_ack_correlated() {
    let mut state = state_with_session();
    let request = json!({"req":"force_stop_turn", "id":73, "session_id":"s1"});
    let denied = state.api_request_to_legacy(&request);
    assert!(!denied.iter().any(|out| matches!(out, Outbound::Legacy(_))));
    state.force_stop_turn_supported = true;
    let out = state.api_request_to_legacy(&request);
    let wire = legacy_out(&out);
    assert_eq!(wire["type"], "force_stop_turn");
    assert_eq!(wire["session_id"], "s1");
    let id = wire["id"].as_u64().unwrap();
    assert_eq!(
        state.legacy_event_to_api(&json!({"type":"ack", "id":id})),
        vec![ServerFrame::reply(73, ApiEvent::Ok)]
    );
}

#[test]
fn bounded_desktop_attach_suppresses_unpaged_bootstrap_only_when_negotiated() {
    for (client, supported, empty) in [
        ("jcode-desktop-test", true, true),
        ("jcode-desktop-test", false, false),
        ("other-client", true, false),
    ] {
        let mut state = BridgeState {
            client_name: Some(client.into()),
            bounded_history_supported: supported,
            ..Default::default()
        };
        let out = state
            .api_request_to_legacy(&json!({"req":"attach_session", "id":1, "session_id":"s1"}));
        let subscribe = out
            .iter()
            .find_map(|o| match o {
                Outbound::Legacy(v) if v["type"] == "subscribe" => Some(v),
                _ => None,
            })
            .unwrap();
        assert_eq!(subscribe["history_base"]["recent_messages"] == 0, empty);
    }
}

#[test]
fn bounded_history_errors_resolve_only_their_request() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"get_history_page", "id":72, "session_id":"s1", "before":"stale", "limit":128}));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    let frames =
        state.legacy_event_to_api(&json!({"type":"error", "id":id, "message":"stale cursor"}));
    assert!(
        matches!(&frames[..], [frame] if frame.reply_to == Some(72) && matches!(&frame.event, ApiEvent::Error { message, .. } if message == "stale cursor"))
    );
}

#[test]
fn session_mutations_correlate_interleaved_terminals_after_receipt() {
    for scoped in [false, true] {
        let mut state = state_with_session();
        if scoped {
            state.legacy_event_to_api(&json!({
                "type":"turn_state", "session_id":state.session_id,
                "runtime_generation":"runtime", "latest_turn_id":0,
            }));
        }
        // Include every invalid environment from the issue's SDK reproduction.
        // The bridge forwards string maps for daemon validation, not success.
        let requests = [
            json!({"id":71,"req":"set_session_env","env":{"BAD=KEY":"v"}}),
            json!({"id":72,"req":"set_session_env","env":{"BAD":"nul\0value"}}),
            json!({"id":73,"req":"set_session_env","env":{"JCODE_SESSION_ID":"x"}}),
            json!({"id":74,"req":"set_session_env","env":{"":"v"}}),
            json!({"id":75,"req":"set_session_env","env":{"GOOD":"v"}}),
            json!({"id":76,"req":"rename_session","title":"idle title"}),
            json!({"id":77,"req":"set_session_saved","saved":true}),
        ];
        let mut ids = Vec::new();
        for request in &requests {
            let outbound = state.api_request_to_legacy(request);
            let [Outbound::Legacy(legacy)] = outbound.as_slice() else {
                panic!("{request}: {outbound:?}");
            };
            let id = legacy["id"].as_u64().unwrap();
            assert!(
                state
                    .legacy_event_to_api(&json!({"type":"ack","id":id}))
                    .is_empty(),
                "receipt must not resolve {request}"
            );
            ids.push(id);
        }
        assert_eq!(state.pending_simple.len(), requests.len());
        // Deliver terminals in reverse order. A terminal must resolve only its
        // own API request, even when all receipts precede all completions.
        for index in (0..requests.len()).rev() {
            let succeeds = index >= 4;
            let terminal = if succeeds {
                json!({"type":"done","id":ids[index]})
            } else {
                json!({"type":"error","id":ids[index],"message":"Invalid session environment"})
            };
            assert_eq!(
                state.legacy_event_to_api(&terminal),
                vec![ServerFrame::reply(
                    requests[index]["id"].as_u64().unwrap(),
                    if succeeds {
                        ApiEvent::Ok
                    } else {
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: "Invalid session environment".into(),
                        }
                    }
                )],
                "scoped={scoped}, request={}",
                requests[index]
            );
            assert_eq!(state.pending_simple.len(), index);
            assert!(!state.observed_turn_active);
            assert!(state.pending_message_id.is_none());
        }
        assert!(state.pending_control_done_ids.is_empty());
        assert!(state.pending_activity_snapshots.is_empty());
    }
}

#[test]
fn receipt_only_controls_keep_ack_compatibility() {
    for mut request in [
        json!({"req":"cancel"}),
        json!({"req":"soft_interrupt","content":"next"}),
        json!({"req":"notify_auth_changed","provider":"openai"}),
        json!({"req":"invalidate_usage","provider":"openai"}),
        json!({"req":"invalidate_usage","provider":"claude"}),
        json!({"req":"cancel_soft_interrupts"}),
        json!({"req":"background_tool"}),
    ] {
        let mut state = state_with_session();
        request["id"] = json!(7);
        let actions = state.api_request_to_legacy(&request);
        let [Outbound::Legacy(legacy)] = actions.as_slice() else {
            panic!("{request}: {actions:?}");
        };
        assert_eq!(
            state.legacy_event_to_api(&json!({"type":"ack","id":legacy["id"]})),
            vec![ServerFrame::reply(7, ApiEvent::Ok)],
            "{request}"
        );
        assert!(state.pending_simple.is_empty());
    }
}

#[test]
fn fallible_controls_wait_for_terminal_reply_without_settling_concurrent_turn() {
    for scoped in [false, true] {
        for (request, terminal, error) in [
            (
                json!({"req":"set_session_env","env":{"BAD=KEY":"v"}}),
                "done",
                "Invalid session environment",
            ),
            (
                json!({"req":"set_session_saved","saved":true}),
                "done",
                "Agent is busy",
            ),
            (
                json!({"req":"rename_session","title":"name"}),
                "done",
                "Agent is busy",
            ),
            (json!({"req":"clear"}), "done", "Agent is busy"),
            (
                json!({"req":"applet_action","instance":"missing","action":{}}),
                "done",
                "Applet not found",
            ),
            (
                json!({"req":"close_applet","instance":"missing"}),
                "done",
                "Applet not found",
            ),
            (
                json!({"req":"rewind","message_index":1}),
                "history",
                "Cannot rewind while a turn is processing.",
            ),
            (
                json!({"req":"rewind_transcript","target":{"selected": vec![1; 32]}}),
                "history",
                "Cannot rewind while a turn is processing.",
            ),
            (
                json!({"req":"rewind_undo"}),
                "history",
                "Cannot undo rewind while a turn is processing.",
            ),
        ] {
            for succeeds in [false, true] {
                let mut state = BridgeState {
                    session_id: Some("s1".into()),
                    observed_turn_active: true,
                    pending_message_id: Some(999),
                    ..Default::default()
                };
                if scoped {
                    state.legacy_event_to_api(&json!({"type":"turn_started","identity":{"runtime_generation":"runtime","turn_id":99}}));
                }
                let mut request = request.clone();
                request["id"] = json!(7);
                request["session_id"] = json!("s1");
                let actions = state.api_request_to_legacy(&request);
                let [Outbound::Legacy(legacy)] = actions.as_slice() else {
                    panic!("{request}: {actions:?}");
                };
                let id = legacy["id"].clone();
                assert!(
                    state
                        .legacy_event_to_api(&json!({"type":"ack","id":id}))
                        .is_empty(),
                    "{request}"
                );
                assert_eq!(state.pending_simple.len(), 1);
                // Unrelated acknowledgement must not consume this control.
                assert!(
                    state
                        .legacy_event_to_api(&json!({"type":"ack","id":123456}))
                        .is_empty()
                );
                assert_eq!(state.pending_simple.len(), 1);
                let event = if succeeds {
                    json!({"type":terminal,"id":id,"session_id":"s1","messages":[]})
                } else {
                    json!({"type":"error","id":id,"message":error})
                };
                assert_eq!(
                    state.legacy_event_to_api(&event),
                    vec![ServerFrame::reply(
                        7,
                        if succeeds {
                            ApiEvent::Ok
                        } else {
                            ApiEvent::Error {
                                code: ErrorCode::Internal,
                                message: error.into(),
                            }
                        }
                    )],
                    "{request}, scoped={scoped}, succeeds={succeeds}"
                );
                assert!(state.pending_simple.is_empty());
                assert!(state.pending_control_done_ids.is_empty());
                assert!(state.observed_turn_active);
                assert_eq!(state.pending_message_id, Some(999));
                if scoped {
                    assert!(state.turn_fence.active());
                }
            }
        }
    }
}

#[test]
fn runtime_commands_are_capability_gated_and_never_send_a_prompt() {
    let request = json!({"id": 501, "req": "runtime_command", "session_id": "s1", "command": "memory", "args": ["status"]});
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    let output = state.api_request_to_legacy(&request);
    assert!(matches!(
        &output[..],
        [Outbound::Reply(ServerFrame {
            event: ApiEvent::Error { .. },
            ..
        })]
    ));
    state.runtime_commands_supported = true;
    let output = state.api_request_to_legacy(&request);
    let [Outbound::Legacy(legacy)] = &output[..] else {
        panic!("expected native runtime command")
    };
    assert_eq!(legacy["type"], "runtime_command");
    assert_eq!(legacy["command"], "memory");
    assert_eq!(legacy["args"], json!(["status"]));
    assert!(legacy.get("content").is_none());
    let frames = state.legacy_event_to_api(&json!({"type": "runtime_command_result", "id": legacy["id"], "command": "memory", "data": {"enabled": true}, "message": "Memory on"}));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, Some(501));
    assert!(
        matches!(&frames[0].event, ApiEvent::RuntimeCommandResult { session_id, command: jcode_harness_api::RuntimeCommand::Memory, .. } if session_id == "s1")
    );
}

#[test]
fn runtime_commands_reject_wrong_session_unknown_commands_and_malformed_args() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        runtime_commands_supported: true,
        ..Default::default()
    };
    for request in [
        json!({"id": 1, "req": "runtime_command", "session_id": "s2", "command": "memory"}),
        json!({"id": 2, "req": "runtime_command", "session_id": "s1", "command": "nonexistent"}),
        json!({"id": 3, "req": "runtime_command", "session_id": "s1", "command": "memory", "args": "on"}),
    ] {
        let output = state.api_request_to_legacy(&request);
        assert!(matches!(
            &output[..],
            [Outbound::Reply(ServerFrame {
                event: ApiEvent::Error { .. },
                ..
            })]
        ));
    }
    assert!(state.pending_simple.is_empty());
}

#[test]
fn token_usage_preserves_cache_creation_and_missing_counters() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    for cache_creation_input in [None, Some(0), Some(42)] {
        let mut legacy = json!({
            "type": "tokens", "input": 10, "output": 5, "cache_read_input": 2
        });
        if let Some(tokens) = cache_creation_input {
            legacy["cache_creation_input"] = json!(tokens);
        }
        let frames = state.legacy_event_to_api(&legacy);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].event,
            ApiEvent::TokenUsage {
                session_id: "s1".into(),
                input: 10,
                output: 5,
                cache_read_input: Some(2),
                cache_creation_input,
                effective_budget: None,
            }
        );
    }
}

#[test]
fn context_occupancy_is_forwarded_as_a_context_snapshot() {
    // The daemon replays occupancy before the `state` reply binds the bridge
    // to a session, so the event's own session id is what scopes it.
    let mut state = BridgeState::default();
    let frames = state.legacy_event_to_api(&json!({
        "type": "context_occupancy", "session_id": "s1", "prompt_tokens": 183_000
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].event,
        ApiEvent::ContextOccupancy {
            session_id: "s1".into(),
            prompt_tokens: 183_000,
        }
    );
}

#[test]
fn unattributable_context_occupancy_is_dropped() {
    let mut state = BridgeState::default();
    let frames = state.legacy_event_to_api(&json!({
        "type": "context_occupancy", "prompt_tokens": 183_000
    }));
    assert!(frames.is_empty(), "{frames:?}");
}

#[test]
fn cache_keepalive_is_an_attach_safe_usage_snapshot() {
    let mut state = BridgeState::default();
    let usage = jcode_harness_api::CacheKeepaliveUsage {
        attempted: 2,
        warmed: 1,
        cost_micros: 180_000,
        unconfirmed_micros: 7_200_000,
        last_hit: None,
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "cache_keepalive", "session_id": "observed", "usage": usage,
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].event,
        ApiEvent::CacheKeepalive {
            session_id: "observed".into(),
            usage
        }
    );
    assert!(state.session_id.is_none());
}

#[test]
fn kv_cache_miss_is_forwarded_with_session() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    let wire = serde_json::to_value(jcode_protocol_like_kv_miss()).unwrap();
    let frames = state.legacy_event_to_api(&wire);
    assert_eq!(
        frames.iter().map(|f| f.event.clone()).collect::<Vec<_>>(),
        vec![ApiEvent::KvCacheMiss {
            session_id: "s1".into(),
            reason: "prefix_changed".into(),
            harness_caused: true,
            missed_tokens: 46_000,
            expected_tokens: 50_000,
            read_tokens: 4_000,
            documented_cause: None,
            message: "KV cache miss: ~46K tokens resent".into(),
        }]
    );
}

#[test]
fn request_usage_preserves_identity_and_absent_counters() {
    let mut state = BridgeState::default();
    let frames = state.legacy_event_to_api(&json!({
        "type": "request_usage", "session_id": "s1", "request_id": "call-2",
        "provider": "openai", "model": "served-model", "input": 100,
        "output": null, "cache_read_input": 0
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        serde_json::to_value(&frames[0].event).unwrap(),
        json!({
            "ev": "request_usage", "session_id": "s1", "request_id": "call-2",
            "provider": "openai", "model": "served-model", "input": 100,
            "output": null, "cache_read_input": 0
        })
    );
}

#[test]
fn persisted_metadata_reads_large_transcripts_from_bounded_windows() {
    let home = ScopedJcodeHome::new("bounded-metadata");
    let sessions = home.path.join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions directory");
    let path = sessions.join("session_large.json");
    let mut file = std::fs::File::create(&path).expect("create large session");
    write!(
        file,
        "{{\"id\":\"session_large\",\"title\":\"Generated title\",\"messages\":[\""
    )
    .unwrap();
    for _ in 0..(2 * 1024) {
        file.write_all(&[b'x'; 1024]).unwrap();
    }
    write!(
        file,
        "\"],\"working_dir\":\"/workspace/large\",\"custom_title\":\"Pinned title\"}}"
    )
    .unwrap();
    drop(file);

    let metadata = BridgeState::resolve_session_metadata("session_large").expect("metadata");
    assert_eq!(metadata.working_dir.as_deref(), Some("/workspace/large"));
    assert_eq!(metadata.title.as_deref(), Some("Generated title"));
    assert_eq!(metadata.custom_title.as_deref(), Some("Pinned title"));
    assert_eq!(metadata.display_title().as_deref(), Some("Pinned title"));
}

#[test]
fn metadata_display_title_prefers_generated_title_over_todo_lane_label() {
    let mut metadata = PersistedSessionMetadata {
        working_dir: None,
        title: Some("Fix session titles".into()),
        custom_title: None,
        todo_title: Some("session-titles".into()),
        saved: false,
        save_label: None,
        updated_at_ms: None,
        last_active_at_ms: None,
    };
    assert_eq!(
        metadata.display_title().as_deref(),
        Some("Fix session titles")
    );

    // Lane labels only name sessions that have no generated title at all.
    metadata.title = None;
    assert_eq!(metadata.display_title().as_deref(), Some("session-titles"));
}

#[test]
fn usage_limit_outcome_is_structured_for_api_clients() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type":"usage_limit", "provider":"Claude", "model":"sonnet", "account":"work",
        "window":"weekly", "reset":"2026-11-20 00:00 UTC", "action":"failed_over",
        "target":"gpt-6.1-sol", "message":"Claude weekly limit, failed over to gpt-6.1-sol"
    }));
    assert_eq!(frames.len(), 1);
    assert!(
        matches!(&frames[0].event, ApiEvent::UsageLimit { session_id, provider, model, account, window, reset, action, target, .. }
        if session_id == "s1" && provider == "Claude" && model == "sonnet"
        && account.as_deref() == Some("work") && window == "weekly"
        && reset == "2026-11-20 00:00 UTC" && action == "failed_over"
        && target.as_deref() == Some("gpt-6.1-sol"))
    );
}

#[test]
fn connection_phase_is_forwarded_to_api_clients() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "connection_phase",
        "phase": "sending request",
    }));

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, None);
    assert_eq!(
        frames[0].event,
        ApiEvent::ConnectionPhase {
            session_id: "s1".into(),
            phase: "sending request".into(),
        }
    );
}

#[test]
fn wake_request_is_forwarded_with_explicit_session_and_payload() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "wake_requested",
        "session_id": "target",
        "reason": "background_task_completed",
        "notification": "finished",
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].event,
        ApiEvent::WakeRequested {
            session_id: "target".into(),
            reason: "background_task_completed".into(),
            notification: "finished".into(),
        }
    );
}

#[test]
fn focus_request_is_forwarded_with_sender() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "focus_requested",
        "session_id": "target",
        "from_session": "session_other_1",
        "from_name": "other",
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].event,
        ApiEvent::FocusRequested {
            session_id: "target".into(),
            from_session: Some("session_other_1".into()),
            from_name: Some("other".into()),
        }
    );
}

#[test]
fn api_sessions_enable_server_account_routing_on_create_attach_and_reconnect() {
    let mut state = BridgeState::default();
    for request in [
        json!({"req": "create_session", "id": 1}),
        json!({"req": "attach_session", "id": 2, "session_id": "s1"}),
        json!({"req": "attach_session", "id": 3, "session_id": "s1"}),
    ] {
        let outbound = state.api_request_to_legacy(&request);
        let subscription = outbound
            .iter()
            .find_map(|outbound| match outbound {
                Outbound::Legacy(value) if value["type"] == "subscribe" => Some(value),
                _ => None,
            })
            .expect("every attachment must subscribe to the daemon");
        assert_eq!(
            subscription["supports_server_failover"], true,
            "API clients must route exhausted accounts rather than retry the same account: {request}"
        );
    }
}

#[test]
fn create_session_maps_to_subscribe() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 1}));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["type"], "subscribe");
    assert!(value["working_dir"].is_string());
    assert!(value.get("system_prompt").is_none());
}

#[test]
fn create_session_forwards_full_system_prompt_including_empty() {
    for prompt in ["You are a helpful tutor.\nAnswer briefly.", ""] {
        let mut state = BridgeState::default();
        let out = state.api_request_to_legacy(&json!({
            "req": "create_session", "id": 1, "system_prompt": prompt,
        }));
        let Outbound::Legacy(value) = &out[0] else {
            panic!("expected legacy outbound");
        };
        assert_eq!(value["system_prompt"], prompt);
    }
}

#[test]
fn attach_session_does_not_forward_system_prompt_override() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "attach_session", "id": 1, "session_id": "existing",
        "system_prompt": "must not replace the existing prompt",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert!(value.get("system_prompt").is_none());
}

#[test]
fn create_session_preserves_explicit_working_dir() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "create_session",
        "id": 1,
        "working_dir": "/workspace/explicit",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["working_dir"], "/workspace/explicit");
}

#[test]
fn created_session_reports_requested_working_dir_before_first_prompt() {
    let _home = ScopedJcodeHome::new("create-unpersisted-working-dir");
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "create_session", "id": 7, "working_dir": "/workspace/fresh",
    }));
    let Outbound::Legacy(probe) = &out[1] else {
        panic!("expected state")
    };
    let reply = state.legacy_event_to_api(&json!({
        "type": "state", "id": probe["id"], "session_id": "fresh",
        "message_count": 0, "is_processing": false,
    }));
    let ApiEvent::Attached { session, .. } = &reply[0].event else {
        panic!("expected attached")
    };
    assert_eq!(session.working_dir.as_deref(), Some("/workspace/fresh"));
    assert!(state.pending_create_dir.is_none());
}

#[test]
fn attach_session_defers_to_daemon_even_with_persisted_working_dir() {
    let home = ScopedJcodeHome::new("attach-working-dir");
    let original = home.path.join("original");
    std::fs::create_dir_all(&original).unwrap();
    write_session_record(&home.path, "existing", &original);
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "attach_session",
        "id": 1,
        "session_id": "existing",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["target_session_id"], "existing");
    assert!(
        value.get("working_dir").is_none(),
        "disk cwd must not override a newer live root"
    );
}

#[test]
fn attach_session_without_persisted_working_dir_reclaims_live_target() {
    let _home = ScopedJcodeHome::new("attach-missing-working-dir");
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "attach_session", "id": 41, "session_id": "live-empty",
    }));
    assert_eq!(out.len(), 3);
    let Outbound::Legacy(subscribe) = &out[0] else {
        panic!("expected subscribe")
    };
    assert_eq!(subscribe["type"], "subscribe");
    assert_eq!(subscribe["target_session_id"], "live-empty");
    assert!(subscribe.get("working_dir").is_none());
    let Outbound::Legacy(probe) = &out[1] else {
        panic!("expected state")
    };
    let reply = state.legacy_event_to_api(&json!({
        "type": "state", "id": probe["id"], "session_id": "live-empty",
        "message_count": 0, "is_processing": false,
    }));
    assert_eq!(reply.len(), 2);
    assert_eq!(reply[0].reply_to, Some(41));
    assert!(
        matches!(&reply[0].event, ApiEvent::Attached { session, .. } if session.session_id == "live-empty")
    );
    assert_eq!(state.session_id.as_deref(), Some("live-empty"));
    assert!(state.pending_attach_id.is_none());
    assert!(state.pending_attach_subscribe_id.is_none());
}

#[test]
fn attach_session_unknown_target_error_is_correlated_and_clears_pending_attach() {
    let _home = ScopedJcodeHome::new("attach-unknown-target");
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({
        "req": "attach_session", "id": 43, "session_id": "missing",
    }));
    let Outbound::Legacy(subscribe) = &out[0] else {
        panic!("expected subscribe")
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": subscribe["id"],
        "message": "Unknown session 'missing' or session has no working directory",
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, Some(43));
    assert!(matches!(
        frames[0].event,
        ApiEvent::Error {
            code: ErrorCode::UnknownSession,
            ..
        }
    ));
    assert!(state.pending_attach_id.is_none());
    assert!(state.pending_model_probe.is_none());
    assert!(state.session_id.is_none());
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "clear_session", "id": 44, "session_id": "missing",
    })));
    assert!(matches!(event, ApiEvent::Error { .. }));
}

#[test]
fn subscribe_forwards_hello_client_name() {
    let mut state = BridgeState {
        client_name: Some("jcode-ios/0.3.0".into()),
        ..Default::default()
    };
    let out =
        state.api_request_to_legacy(&json!({"req": "attach_session", "id": 1, "session_id": "s1"}));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["client_name"], "jcode-ios/0.3.0");

    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 1}));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert!(value.get("client_name").is_none());
}

#[test]
fn mobile_sessions_continue_after_transport_disconnect() {
    for client in [
        "jcode-ios",
        "jcode-android",
        "jcode-ios/0.3.0",
        "jcode-android/1.0",
    ] {
        for req in ["create_session", "attach_session"] {
            let mut state = BridgeState {
                client_name: Some(client.into()),
                ..Default::default()
            };
            let out = state.api_request_to_legacy(&json!({
                "req": req, "id": 1, "session_id": "mobile-session",
            }));
            let Outbound::Legacy(subscribe) = &out[0] else {
                panic!("expected subscribe for {client} {req}");
            };
            assert_eq!(subscribe["continue_on_disconnect"], true, "{client} {req}");
            assert!(subscribe.get("crash_on_disconnect").is_none());
        }
    }
}

#[test]
fn non_mobile_sessions_keep_existing_disconnect_policy() {
    for client in [
        None,
        Some("jcode-desktop-linux"),
        Some("custom-sdk"),
        Some("jcode-ios-other"),
    ] {
        for req in ["create_session", "attach_session"] {
            let desktop = client == Some("jcode-desktop-linux");
            let mut state = BridgeState::with_crash_on_disconnect(desktop);
            state.client_name = client.map(str::to_owned);
            let out = state.api_request_to_legacy(&json!({
                "req": req, "id": 1, "session_id": "session",
            }));
            let Outbound::Legacy(subscribe) = &out[0] else {
                panic!("expected subscribe");
            };
            assert!(subscribe.get("continue_on_disconnect").is_none());
            assert_eq!(
                subscribe["crash_on_disconnect"].as_bool().unwrap_or(false),
                desktop
            );
        }
    }
}

#[test]
fn desktop_owned_session_requests_crash_on_disconnect() {
    let mut state = BridgeState::with_crash_on_disconnect(true);
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 1}));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["crash_on_disconnect"], true);
}

#[test]
fn detach_disarms_crash_on_disconnect() {
    let mut state = BridgeState::with_crash_on_disconnect(true);
    state.session_id = Some("abc".into());
    let out = state.api_request_to_legacy(&json!({
        "req": "detach_session",
        "id": 2,
        "session_id": "abc",
    }));
    let Outbound::Legacy(value) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(value["type"], "prepare_disconnect");
}

#[test]
fn state_event_answers_pending_attach() {
    let home = ScopedJcodeHome::new("attach-title");
    let project = home.path.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_session_record_with_titles(
        &home.path,
        "abc",
        &project,
        Some("Generated attach title"),
        Some("Persisted attach rename"),
    );
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 5}));
    assert_eq!(
        out.len(),
        3,
        "subscribe + state chase + model catalog probe"
    );
    let Outbound::Legacy(state_req) = &out[1] else {
        panic!("expected legacy state request");
    };
    assert_eq!(state_req["type"], "state");
    let state_id = state_req["id"].as_u64().unwrap();

    // A subscribe `done` must not leak a turn_done.
    let done = state.legacy_event_to_api(&json!({"type": "done", "id": 1}));
    assert!(done.is_empty());

    let frames = state.legacy_event_to_api(&json!({
        "type": "state", "id": state_id, "session_id": "abc",
        "message_count": 0, "is_processing": false, "pending_ask_ids": ["ask-live"],
    }));
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].reply_to, Some(5));
    match &frames[0].event {
        ApiEvent::Attached {
            session,
            pending_ask_ids,
        } => {
            assert_eq!(session.session_id, "abc");
            assert_eq!(session.title.as_deref(), Some("Persisted attach rename"));
            assert_eq!(session.working_dir.as_deref(), project.to_str());
            assert_eq!(pending_ask_ids.as_deref(), Some(&["ask-live".into()][..]));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(state.session_id.as_deref(), Some("abc"));
}

#[test]
fn send_message_then_done_becomes_turn_done() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(
        &json!({"req": "send_message", "id": 2, "session_id": "s1", "content": "hi"}),
    );
    let Outbound::Legacy(message) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(message["type"], "message");
    let legacy_id = message["id"].as_u64().unwrap();

    let deltas = state.legacy_event_to_api(&json!({"type": "text_delta", "text": "yo"}));
    assert!(matches!(
        &deltas[0].event,
        ApiEvent::TextDelta { session_id, text, .. } if session_id == "s1" && text == "yo"
    ));

    let done = state.legacy_event_to_api(&json!({"type": "done", "id": legacy_id}));
    assert!(matches!(
        &done[1].event,
        ApiEvent::TurnDone { session_id, .. } if session_id == "s1"
    ));
    assert!(matches!(&done[0].event, ApiEvent::TextDone { .. }));
}

/// The daemon acking the in-flight message is the only signal that the agent
/// took delivery, so it must surface as its own event rather than being
/// swallowed as a bookkeeping ack. A client that shows "sent" until the first
/// token of the reply is showing a lie for as long as the model thinks.
#[test]
fn acking_the_pending_message_reports_acceptance() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(
        &json!({"req": "send_message", "id": 2, "session_id": "s1", "content": "hi"}),
    );
    let Outbound::Legacy(message) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = message["id"].as_u64().unwrap();

    let accepted = state.legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}));
    assert!(matches!(
        &accepted[0].event,
        ApiEvent::MessageAccepted { session_id } if session_id == "s1"
    ));
    // The turn must still end normally: the acceptance event must not consume
    // the pending id the `done` boundary depends on.
    let done = state.legacy_event_to_api(&json!({"type": "done", "id": legacy_id}));
    assert!(matches!(&done[0].event, ApiEvent::TurnDone { .. }));
}

#[test]
fn image_soft_interrupt_preserves_pending_message_correlation() {
    let mut state = state_with_session();
    let normal = state.api_request_to_legacy(
        &json!({"req": "send_message", "id": 2, "session_id": "s1", "content": "first"}),
    );
    let Outbound::Legacy(normal) = &normal[0] else {
        panic!("expected legacy message");
    };
    let normal_id = normal["id"].as_u64().unwrap();

    let interrupt = state.api_request_to_legacy(&json!({
        "req": "soft_interrupt", "id": 3, "session_id": "s1", "content": "look",
        "images": [["image/png", "aW1hZ2U="]], "urgent": true
    }));
    let Outbound::Legacy(interrupt) = &interrupt[0] else {
        panic!("expected legacy soft interrupt");
    };
    assert_eq!(interrupt["type"], "soft_interrupt");
    assert_eq!(interrupt["images"], json!([["image/png", "aW1hZ2U="]]));
    let interrupt_id = interrupt["id"].as_u64().unwrap();

    let interrupt_ack = state.legacy_event_to_api(&json!({"type": "ack", "id": interrupt_id}));
    assert_eq!(interrupt_ack.len(), 1);
    assert_eq!(interrupt_ack[0].reply_to, Some(3));
    assert!(matches!(interrupt_ack[0].event, ApiEvent::Ok));

    let accepted = state.legacy_event_to_api(&json!({"type": "ack", "id": normal_id}));
    assert!(matches!(
        &accepted[0].event,
        ApiEvent::MessageAccepted { session_id } if session_id == "s1"
    ));
    let done = state.legacy_event_to_api(&json!({"type": "done", "id": normal_id}));
    assert!(matches!(&done[0].event, ApiEvent::TurnDone { .. }));
}

#[test]
fn idle_soft_interrupt_done_becomes_turn_done() {
    let mut state = state_with_session();
    let interrupt = state.api_request_to_legacy(&json!({
        "req": "soft_interrupt", "id": 3, "session_id": "s1", "content": "start"
    }));
    let Outbound::Legacy(interrupt) = &interrupt[0] else {
        panic!("expected legacy soft interrupt");
    };
    let interrupt_id = interrupt["id"].as_u64().unwrap();

    let done = state.legacy_event_to_api(&json!({"type": "done", "id": interrupt_id}));
    assert!(matches!(
        &done[0].event,
        ApiEvent::TurnDone { session_id, .. } if session_id == "s1"
    ));
}

#[test]
fn queued_prompt_bridge_forwards_and_correlates_controls_and_snapshots() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        queued_prompts_supported: true,
        ..Default::default()
    };
    let out = state.api_request_to_legacy(&json!({"req":"queue_prompt","id":9,"session_id":"s1","client_id":"p1","content":"next","mode":"after_yield","images":[],"system_reminder":"hidden collection"}));
    let legacy = legacy_out(&out);
    assert_eq!(legacy["client_id"], "p1");
    assert_eq!(legacy["mode"], "after_yield");
    assert_eq!(legacy["system_reminder"], "hidden collection");
    let id = legacy["id"].as_u64().unwrap();
    let ack = state.legacy_event_to_api(&json!({"type":"ack","id":id}));
    assert!(
        matches!(&ack[..], [frame] if frame.reply_to == Some(9) && matches!(frame.event, ApiEvent::Ok))
    );
    let out =
        state.api_request_to_legacy(&json!({"req":"get_queued_prompts","id":10,"session_id":"s1"}));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    assert!(
        state
            .legacy_event_to_api(&json!({"type":"ack","id":id}))
            .is_empty()
    );
    let reply = state.legacy_event_to_api(&json!({"type":"queued_prompts","id":id,"prompts":[{"client_id":"p1","content":"next","mode":"after_yield"}]}));
    assert!(
        matches!(&reply[..], [frame] if frame.reply_to == Some(10) && matches!(&frame.event, ApiEvent::QueuedPrompts { prompts, .. } if prompts.len() == 1 && prompts[0].client_id == "p1"))
    );
    let out = state.api_request_to_legacy(
        &json!({"req":"interrupt_queued_prompt","id":11,"session_id":"s1","client_id":"missing"}),
    );
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    let err = state.legacy_event_to_api(&json!({"type":"error","id":id,"message":"not found"}));
    assert!(
        matches!(&err[..], [frame] if frame.reply_to == Some(11) && matches!(&frame.event, ApiEvent::Error { message, .. } if message == "not found"))
    );
}

#[test]
fn queued_skill_requires_capability_and_preserves_typed_rejection() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        queued_prompts_supported: true,
        ..Default::default()
    };
    let request = json!({"req":"queue_prompt","id":41,"session_id":"s1","client_id":"p1","content":"hello","mode":"after_yield","active_skill":"missing"});
    assert!(matches!(
        &state.api_request_to_legacy(&request)[..],
        [Outbound::Reply(_)]
    ));
    state.queued_prompt_active_skill_supported = true;
    let out = state.api_request_to_legacy(&request);
    let legacy = legacy_out(&out);
    assert_eq!(legacy["active_skill"], "missing");
    let id = legacy["id"].as_u64().unwrap();
    let result = state.legacy_event_to_api(&json!({"type":"queued_prompt_rejected","id":id,"reason":"unknown_skill","skill":"missing"}));
    assert!(
        matches!(&result[..], [frame] if frame.reply_to == Some(41) && matches!(&frame.event, ApiEvent::Error { code: ErrorCode::UnknownSkill, .. }))
    );
}

#[test]
fn batch_interrupt_requires_capability_and_forwards_ids() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        queued_prompts_supported: true,
        ..Default::default()
    };
    let request =
        json!({"req":"interrupt_queued_prompts","id":22,"session_id":"s1","ids":["a","b"]});
    assert!(matches!(
        &state.api_request_to_legacy(&request)[..],
        [Outbound::Reply(_)]
    ));
    state.queued_prompt_batch_interrupt_supported = true;
    let out = state.api_request_to_legacy(&request);
    let legacy = legacy_out(&out);
    assert_eq!(legacy["ids"], json!(["a", "b"]));
    let id = legacy["id"].as_u64().unwrap();
    let result = state.legacy_event_to_api(&json!({"type":"ack","id":id}));
    assert!(
        matches!(&result[..], [frame] if frame.reply_to == Some(22) && matches!(frame.event, ApiEvent::Ok))
    );
}

#[test]
fn queued_prompt_mutation_bridge_requires_capability_and_correlates_outcome() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        queued_prompts_supported: true,
        ..Default::default()
    };
    let request = json!({"req":"remove_queued_prompt","id":20,"session_id":"s1","client_id":"panel","prompt_id":"p1"});
    assert!(matches!(
        &state.api_request_to_legacy(&request)[..],
        [Outbound::Reply(_)]
    ));
    state.queued_prompt_mutations_supported = true;
    let out = state.api_request_to_legacy(&request);
    let legacy = legacy_out(&out);
    assert_eq!(legacy["prompt_id"], "p1");
    let id = legacy["id"].as_u64().unwrap();
    let result = state.legacy_event_to_api(&json!({"type":"queued_prompt_removed","id":id,"prompt_id":"p1","outcome":"already_delivered"}));
    assert!(
        matches!(&result[..], [frame] if frame.reply_to == Some(20) && matches!(&frame.event, ApiEvent::QueuedPromptRemoved { prompt_id, outcome: jcode_harness_api::QueuedPromptRemovalOutcome::AlreadyDelivered, .. } if prompt_id == "p1"))
    );
    let out = state.api_request_to_legacy(&json!({"req":"reorder_queued_prompts","id":21,"session_id":"s1","client_id":"panel","order":["p2","p1"]}));
    let legacy = legacy_out(&out);
    assert_eq!(legacy["order"], json!(["p2", "p1"]));
    let id = legacy["id"].as_u64().unwrap();
    let err = state.legacy_event_to_api(&json!({"type":"error","id":id,"message":"invalid order"}));
    assert!(
        matches!(&err[..], [frame] if frame.reply_to == Some(21) && matches!(&frame.event, ApiEvent::Error { message, .. } if message == "invalid order"))
    );
}

#[test]
fn soft_interrupt_forwards_client_id() {
    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "soft_interrupt", "id": 3, "session_id": "s1",
        "content": "steer", "client_id": "f1"
    }));
    let legacy = legacy_out(&out);
    assert_eq!(legacy["type"], "soft_interrupt");
    assert_eq!(legacy["client_id"], "f1");

    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "soft_interrupt", "id": 4, "session_id": "s1", "content": "plain"
    }));
    assert!(legacy_out(&out).get("client_id").is_none());
}

#[test]
fn soft_interrupt_injected_closes_open_text_and_carries_ids() {
    let mut state = ids_state();
    state.legacy_event_to_api(&json!({"type": "text_delta", "text": "working on it"}));
    let frames = state.legacy_event_to_api(&json!({
        "type": "soft_interrupt_injected",
        "content": "first\n\nsecond",
        "point": "D",
        "client_ids": ["f1", "f2"],
    }));
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert!(matches!(frames[0].event, ApiEvent::TextDone { .. }));
    assert_eq!(
        frames[1].event,
        ApiEvent::SoftInterruptInjected {
            session_id: "s1".into(),
            content: Some("first\n\nsecond".into()),
            client_ids: vec!["f1".into(), "f2".into()],
            point: "D".into(),
            tools_skipped: None,
            display_role: None,
        }
    );

    // Daemon-originated injections are forwarded too, with no ids.
    let frames = state.legacy_event_to_api(&json!({
        "type": "soft_interrupt_injected",
        "content": "[Background Task Completed]",
        "display_role": "background_task",
        "point": "B",
    }));
    assert!(matches!(
        &frames[..],
        [frame] if matches!(&frame.event, ApiEvent::SoftInterruptInjected {
            client_ids, display_role: Some(role), point, ..
        } if client_ids.is_empty() && role == "background_task" && point == "B")
    ));
}

#[test]
fn cancel_soft_interrupts_by_id_waits_for_the_exact_result() {
    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "cancel_soft_interrupts", "id": 9, "session_id": "s1",
        "client_ids": ["f1", "f2"]
    }));
    let legacy = legacy_out(&out);
    assert_eq!(legacy["client_ids"], json!(["f1", "f2"]));
    let legacy_id = legacy["id"].as_u64().unwrap();

    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}))
            .is_empty(),
        "the early ack is not the answer"
    );
    let frames = state.legacy_event_to_api(&json!({
        "type": "soft_interrupts_cancelled", "id": legacy_id,
        "cancelled": ["f2"], "not_queued": ["f1"]
    }));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, Some(9));
    assert_eq!(
        frames[0].event,
        ApiEvent::SoftInterruptsCancelled {
            cancelled: vec!["f2".into()],
            not_queued: vec!["f1".into()],
        }
    );
}

#[test]
fn cancel_soft_interrupts_without_ids_keeps_legacy_behavior() {
    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "cancel_soft_interrupts", "id": 9, "session_id": "s1"
    }));
    let legacy = legacy_out(&out);
    assert!(legacy.get("client_ids").is_none());
    let id = legacy["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": id}));
    assert!(matches!(&frames[..], [f] if f.reply_to == Some(9) && f.event == ApiEvent::Ok));
}

#[test]
fn filtered_cancel_is_refused_by_an_old_daemon_instead_of_clearing_everything() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "cancel_soft_interrupts", "id": 9, "session_id": "s1", "client_ids": ["f1"]
    }));
    assert!(matches!(
        &out[..],
        [Outbound::Reply(frame)] if matches!(frame.event, ApiEvent::Error { code: ErrorCode::UnknownRequest, .. })
    ));

    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "cancel_soft_interrupts", "id": 9, "session_id": "s1", "client_ids": "f1"
    }));
    assert!(matches!(
        &out[..],
        [Outbound::Reply(frame)] if matches!(frame.event, ApiEvent::Error { code: ErrorCode::InvalidRequest, .. })
    ));
}

#[test]
fn background_tool_reports_what_moved() {
    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "background_tool", "id": 5, "session_id": "s1"
    }));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": id}))
            .is_empty()
    );
    let frames = state.legacy_event_to_api(&json!({
        "type": "background_tool_result", "id": id, "moved": true,
        "tool_call_id": "call_1", "tool_name": "bash"
    }));
    assert_eq!(frames[0].reply_to, Some(5));
    assert_eq!(
        frames[0].event,
        ApiEvent::BackgroundToolResult {
            moved: true,
            tool_call_id: Some("call_1".into()),
            tool_name: Some("bash".into()),
        }
    );

    // Against an older daemon the plain ack still answers.
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "background_tool", "id": 6, "session_id": "s1"
    }));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": id}));
    assert!(matches!(&frames[..], [f] if f.reply_to == Some(6) && f.event == ApiEvent::Ok));
}

#[test]
fn turn_done_reports_interrupts_left_in_the_queue() {
    let mut state = ids_state();
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 1, "session_id": "s1", "content": "go"
    }));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    state.legacy_event_to_api(&json!({"type": "ack", "id": id}));
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "soft_interrupt_queue", "pending": 2}))
            .is_empty()
    );
    let frames = state.legacy_event_to_api(&json!({"type": "done", "id": id}));
    assert!(matches!(
        frames.last().map(|f| &f.event),
        Some(ApiEvent::TurnDone {
            pending_soft_interrupts: Some(2),
            ..
        })
    ));

    // The count belongs to that turn only.
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 2, "session_id": "s1", "content": "again"
    }));
    let id = legacy_out(&out)["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "done", "id": id}));
    assert!(matches!(
        frames.last().map(|f| &f.event),
        Some(ApiEvent::TurnDone {
            pending_soft_interrupts: None,
            ..
        })
    ));
}

#[test]
fn context_only_message_waits_for_persistence_event_and_replies_ok() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 27, "session_id": "s1",
        "content": "context", "no_reply": true
    }));
    let Outbound::Legacy(message) = &out[0] else {
        panic!("expected legacy outbound");
    };
    assert_eq!(message["type"], "message");
    assert_eq!(message["no_reply"], true);
    let legacy_id = message["id"].as_u64().unwrap();

    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}))
            .is_empty(),
        "the daemon's early ack does not prove persistence"
    );
    let frames =
        state.legacy_event_to_api(&json!({"type": "context_message_added", "id": legacy_id}));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].reply_to, Some(27));
    assert!(matches!(frames[0].event, ApiEvent::Ok));
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "done", "id": legacy_id}))
            .is_empty(),
        "context-only messages never create turn boundaries"
    );
}

#[test]
fn context_only_message_error_is_correlated_to_the_request() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "send_message", "id": 28, "session_id": "s1",
        "content": "context", "no_reply": true
    }));
    let Outbound::Legacy(message) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = message["id"].as_u64().unwrap();
    let frames = state
        .legacy_event_to_api(&json!({"type": "error", "id": legacy_id, "message": "save failed"}));
    assert_eq!(frames[0].reply_to, Some(28));
    assert!(matches!(
        &frames[0].event,
        ApiEvent::Error { message, .. } if message == "save failed"
    ));
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "context_message_added", "id": legacy_id}))
            .is_empty()
    );
}

/// An ack for an ack-terminal control is still a plain request reply:
/// promoting those to acceptance would wiggle a message that nobody sent.
#[test]
fn acking_an_unrelated_request_stays_a_reply() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "cancel", "id": 9, "session_id": "s1"}));
    let Outbound::Legacy(cancel) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = cancel["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}));
    assert_eq!(frames[0].reply_to, Some(9));
    assert!(matches!(&frames[0].event, ApiEvent::Ok));
}

#[test]
fn ping_pong_roundtrip() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "ping", "id": 9}));
    let Outbound::Legacy(ping) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = ping["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "pong", "id": legacy_id}));
    assert_eq!(frames[0].reply_to, Some(9));
    assert!(matches!(frames[0].event, ApiEvent::Pong));
}

#[test]
fn history_reply_is_mapped() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "get_history", "id": 4}));
    let Outbound::Legacy(get) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = get["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({
        "type": "history",
        "id": legacy_id,
        "session_id": "s1",
        "messages": [{"role": "user", "content": "hi"}],
    }));
    match &frames[0].event {
        ApiEvent::History { messages, .. } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role, "user");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unknown_legacy_events_are_dropped() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({"type": "swarm_event", "data": {}}));
    assert!(frames.is_empty());
}

#[test]
fn unknown_api_request_gets_error_reply() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "frobnicate", "id": 3}));
    let Outbound::Reply(frame) = &out[0] else {
        panic!("expected direct reply");
    };
    assert_eq!(frame.reply_to, Some(3));
    assert!(matches!(
        frame.event,
        ApiEvent::Error {
            code: ErrorCode::UnknownRequest,
            ..
        }
    ));
}

#[test]
fn error_routes_to_pending_request() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "clear", "id": 7}));
    let Outbound::Legacy(clear) = &out[0] else {
        panic!("expected legacy outbound");
    };
    let legacy_id = clear["id"].as_u64().unwrap();
    let frames =
        state.legacy_event_to_api(&json!({"type": "error", "id": legacy_id, "message": "nope"}));
    assert_eq!(frames[0].reply_to, Some(7));
}

/// Attaching must volunteer the model identity: a client that has to know to
/// ask would show "unknown model" forever, which is what this fixes.
#[test]
fn attaching_probes_and_reports_the_model() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 7}));
    let Outbound::Legacy(catalog) = &out[2] else {
        panic!("expected a legacy catalog probe");
    };
    assert_eq!(catalog["type"], "get_model_catalog");
    let catalog_id = catalog["id"].as_u64().unwrap();

    // The daemon answers the probe with a `history`-shaped reply carrying no
    // messages. That must become an unsolicited model_info event, not a reply
    // to some client request that never asked for history.
    let frames = state.legacy_event_to_api(&json!({
        "type": "history", "id": catalog_id, "messages": [],
        "provider_name": "anthropic", "provider_model": "claude-sonnet-4-5",
    }));
    assert_eq!(frames.len(), 2);
    assert!(matches!(frames[1].event, ApiEvent::RuntimeInfo { .. }));
    assert_eq!(
        frames[0].reply_to, None,
        "the probe was not client-initiated"
    );
    match &frames[0].event {
        ApiEvent::ModelInfo {
            provider, model, ..
        } => {
            assert_eq!(provider.as_deref(), Some("anthropic"));
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-5"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// A real `get_history` reply must still be a history reply after the probe has
/// been consumed, or the probe would swallow the client's own request.
#[test]
fn a_client_history_request_is_untouched_by_the_probe() {
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req": "create_session", "id": 1}));
    let Outbound::Legacy(catalog) = &out[2] else {
        panic!("expected a catalog probe");
    };
    let catalog_id = catalog["id"].as_u64().unwrap();
    state.legacy_event_to_api(&json!({"type": "history", "id": catalog_id, "messages": []}));

    let out = state.api_request_to_legacy(&json!({"req": "get_history", "id": 9}));
    let Outbound::Legacy(request) = &out[0] else {
        panic!("expected a legacy history request");
    };
    let history_id = request["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({
        "type": "history", "id": history_id,
        "messages": [{"role": "user", "content": "hi"}],
    }));
    assert_eq!(frames[0].reply_to, Some(9));
    assert!(matches!(frames[0].event, ApiEvent::History { .. }));
}

/// Switching model mid-session must reach the client, or the caption goes stale
/// and confidently lies about which model answered.
#[test]
fn a_model_change_is_forwarded() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": 3,
        "model": "gpt-5.6", "provider_name": "openai",
    }));
    match &frames[0].event {
        ApiEvent::ModelInfo {
            provider, model, ..
        } => {
            assert_eq!(provider.as_deref(), Some("openai"));
            assert_eq!(model.as_deref(), Some("gpt-5.6"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// A failed model change must not be reported as the active model.
#[test]
fn a_failed_model_change_is_not_reported() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": 3, "model": "nope", "error": "no such model",
    }));
    assert!(frames.is_empty());
}
