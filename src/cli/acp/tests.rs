use super::*;
use std::path::Path;

#[test]
fn acp_session_env_filter_keeps_seat_vars_and_drops_reserved_or_invalid_names() {
    let env = inherited_session_env(
        [
            ("FOO_SEAT", "xyz"),
            ("OPENRIG_SEAT_ID", "seat-1"),
            ("PATH", "/seat/bin:/usr/bin"),
            ("EMPTY_VALUE", ""),
            ("JCODE_SOCKET", "private"),
            ("jcode_home", "private"),
            ("JcOdE_ACP_SESSION_ENV", "inherit"),
            ("JCODE", "not-reserved"),
            ("", "invalid"),
            ("BAD=NAME", "invalid"),
            ("BAD\0NAME", "invalid"),
        ]
        .map(|(key, value)| (key.into(), value.into())),
    )
    .unwrap();
    assert_eq!(
        env,
        HashMap::from([
            ("FOO_SEAT".into(), "xyz".into()),
            ("OPENRIG_SEAT_ID".into(), "seat-1".into()),
            ("PATH".into(), "/seat/bin:/usr/bin".into()),
            ("EMPTY_VALUE".into(), "".into()),
            ("JCODE".into(), "not-reserved".into()),
        ])
    );
}

#[cfg(unix)]
#[test]
fn acp_session_env_filter_fails_closed_for_non_utf8_values() {
    use std::os::unix::ffi::OsStringExt;
    let error =
        inherited_session_env([("FOO_SEAT".into(), std::ffi::OsString::from_vec(vec![0xff]))])
            .unwrap_err();
    assert!(error.to_string().contains("non-UTF-8"));
}

#[test]
fn acp_session_env_cli_accepts_modes_and_rejects_unknown_modes() {
    use clap::Parser;
    for (mode, expected) in [
        ("none", SessionEnvMode::None),
        ("inherit", SessionEnvMode::Inherit),
    ] {
        let args =
            super::super::args::Args::try_parse_from(["jcode", "acp", "--session-env", mode])
                .unwrap();
        assert!(matches!(
            args.command,
            Some(super::super::args::Command::Acp { session_env: Some(mode) }) if mode == expected
        ));
    }
    assert!(
        super::super::args::Args::try_parse_from(["jcode", "acp", "--session-env", "everything",])
            .is_err()
    );
    assert_eq!(SessionEnvMode::default(), SessionEnvMode::None);
}

#[test]
fn acp_session_env_defaults_to_none_and_explicit_flags_override_env() {
    assert_eq!(
        SessionEnvMode::resolve(None, None).unwrap(),
        SessionEnvMode::None
    );
    assert_eq!(
        SessionEnvMode::resolve(None, Some("inherit".into())).unwrap(),
        SessionEnvMode::Inherit
    );
    assert_eq!(
        SessionEnvMode::resolve(None, Some("none".into())).unwrap(),
        SessionEnvMode::None
    );
    assert_eq!(
        SessionEnvMode::resolve(Some(SessionEnvMode::None), Some("inherit".into())).unwrap(),
        SessionEnvMode::None
    );
    assert_eq!(
        SessionEnvMode::resolve(Some(SessionEnvMode::Inherit), Some("invalid".into())).unwrap(),
        SessionEnvMode::Inherit
    );
    assert!(SessionEnvMode::resolve(None, Some("invalid".into())).is_err());
}

async fn acp_daemon_request(reader: &mut BufReader<ReadHalf>) -> Request {
    let mut line = String::new();
    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

async fn acp_daemon_event(writer: &mut WriteHalf, event: Value) {
    writer
        .write_all(format!("{event}\n").as_bytes())
        .await
        .unwrap();
}

#[test]
fn acp_new_session_model_qualifies_bare_models_without_rewriting_explicit_routes() {
    for (provider, profile, model, expected) in [
        (ProviderChoice::Auto, None, "future-model", "future-model"),
        (
            ProviderChoice::Openai,
            None,
            "future-model",
            "openai:future-model",
        ),
        (
            ProviderChoice::Auto,
            Some("seat-profile"),
            "future-model",
            "seat-profile:future-model",
        ),
        (
            ProviderChoice::Claude,
            None,
            "openai:future-model",
            "openai:future-model",
        ),
        (ProviderChoice::Ollama, None, "qwen:32b", "ollama:qwen:32b"),
        (
            ProviderChoice::Openai,
            None,
            "openai-oauth@work:future-model",
            "openai-oauth@work:future-model",
        ),
    ] {
        let runtime = AcpRuntime::new(
            AcpProfile::Standard,
            provider,
            Some(model.into()),
            profile.map(str::to_string),
            SessionEnvMode::None,
        );
        assert_eq!(runtime.new_session_model().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn acp_explicit_model_is_confirmed_for_new_sessions_but_not_applied_to_attached_sessions() {
    for resume in [false, true] {
        for reject in [false, true] {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (client, daemon) = crate::transport::stream_pair().unwrap();
                let (reader, writer) = client.into_split();
                let mut setup = tokio::spawn(async move {
                    let runtime = AcpRuntime::new(AcpProfile::Standard, ProviderChoice::Openai, Some("future-model".into()), None, SessionEnvMode::None);
                    let cwd = PathBuf::from("/acp-test");
                    if resume {
                        runtime.attach_existing_session_connected("existing".into(), cwd, false, reader, writer).await
                    } else {
                        runtime.create_new_session_connected(cwd, reader, writer).await
                    }
                });
                let (reader, mut writer) = daemon.into_split();
                let mut reader = BufReader::new(reader);
                let Request::Subscribe { id, .. } = acp_daemon_request(&mut reader).await else { panic!("expected subscription"); };
                if resume {
                    acp_daemon_event(&mut writer, json!({"type": "history", "id": id, "session_id": "existing", "messages": [], "provider_model": "saved-model"})).await;
                }
                acp_daemon_event(&mut writer, json!({"type": "done", "id": id})).await;
                if !resume {
                    let Request::SetModel { id, model } = acp_daemon_request(&mut reader).await else { panic!("new session must select explicit launch model before returning"); };
                    assert_eq!(model, "openai:future-model");
                    acp_daemon_event(&mut writer, json!({"type": "ack", "id": id})).await;
                    assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut setup).await.is_err(), "model selection must wait for confirmation");
                    acp_daemon_event(&mut writer, json!({"type": "model_changed", "id": id, "model": "future-model", "provider_name": "openai", "error": reject.then_some("model rejected")})).await;
                    if reject {
                        let error = setup.await.unwrap().err().expect("rejected model must fail session/new");
                        let message = format!("{error:#}");
                        assert!(message.contains("failed to select ACP session model"));
                        assert!(message.contains("model rejected"));
                        return;
                    }
                    let Request::GetHistory { id } = acp_daemon_request(&mut reader).await else { panic!("expected history after model confirmation"); };
                    acp_daemon_event(&mut writer, json!({"type": "history", "id": id, "session_id": "new", "messages": [], "provider_model": "future-model"})).await;
                }
                let session = setup.await.unwrap().unwrap();
                assert_eq!(session.ui_state.lock().await.model.as_deref(), Some(if resume { "saved-model" } else { "future-model" }));
                drop(session);
                let mut extra = String::new();
                assert_eq!(reader.read_line(&mut extra).await.unwrap(), 0, "attached session must not receive launch model override");
            }).await.expect("ACP model handshake timed out");
        }
    }
}

#[tokio::test]
async fn acp_session_env_is_acknowledged_before_new_or_attach_returns() {
    for resume in [false, true] {
        for mode in [SessionEnvMode::None, SessionEnvMode::Inherit] {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (client, daemon) = crate::transport::stream_pair().unwrap();
                let (reader, writer) = client.into_split();
                let mut setup = tokio::spawn(async move {
                    let runtime = AcpRuntime::new(
                        AcpProfile::Standard,
                        ProviderChoice::Claude,
                        None,
                        None,
                        mode,
                    );
                    let cwd = PathBuf::from("/acp-test");
                    if resume {
                        runtime
                            .attach_existing_session_connected("existing".into(), cwd, false, reader, writer)
                            .await
                    } else {
                        runtime.create_new_session_connected(cwd, reader, writer).await
                    }
                });
                let (reader, mut writer) = daemon.into_split();
                let mut reader = BufReader::new(reader);
                let Request::Subscribe { id, target_session_id, .. } = acp_daemon_request(&mut reader).await else {
                    panic!("expected subscription before session environment");
                };
                assert_eq!(target_session_id.as_deref(), resume.then_some("existing"));
                if resume {
                    acp_daemon_event(&mut writer, json!({"type": "history", "id": id, "session_id": "existing", "messages": []})).await;
                }
                acp_daemon_event(&mut writer, json!({"type": "done", "id": id})).await;

                if mode == SessionEnvMode::Inherit {
                    let Request::SetSessionEnv { id, env } = acp_daemon_request(&mut reader).await else {
                        panic!("inherit must send session environment immediately after subscription");
                    };
                    assert!(env.keys().all(|key| !key.to_ascii_uppercase().starts_with("JCODE_")));
                    assert_eq!(env.get("PATH"), std::env::var("PATH").ok().as_ref());
                    acp_daemon_event(&mut writer, json!({"type": "ack", "id": id})).await;
                    acp_daemon_event(&mut writer, json!({"type": "done", "id": id + 100})).await;
                    acp_daemon_event(&mut writer, json!({"type": "error", "id": id + 100, "message": "unrelated"})).await;
                    assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut setup).await.is_err(), "session must not succeed before environment confirmation");
                    acp_daemon_event(&mut writer, json!({"type": "done", "id": id})).await;
                }

                if !resume {
                    let Request::GetHistory { id } = acp_daemon_request(&mut reader).await else {
                        panic!("expected history, with no environment request in none mode");
                    };
                    acp_daemon_event(&mut writer, json!({"type": "history", "id": id, "session_id": "new", "messages": []})).await;
                }
                let session = setup.await.unwrap().unwrap();
                assert_eq!(session.session_id, if resume { "existing" } else { "new" });
                drop(session);
                let mut extra = String::new();
                assert_eq!(reader.read_line(&mut extra).await.unwrap(), 0, "unexpected extra request, including environment in none mode");
            }).await.expect("ACP session environment handshake timed out");
        }
    }
}

#[tokio::test]
async fn acp_session_env_rejection_or_disconnect_fails_new_and_attach() {
    for resume in [false, true] {
        for disconnect in [false, true] {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (client, daemon) = crate::transport::stream_pair().unwrap();
                let (reader, writer) = client.into_split();
                let setup = tokio::spawn(async move {
                    let runtime = AcpRuntime::new(
                        AcpProfile::Standard,
                        ProviderChoice::Claude,
                        None,
                        None,
                        SessionEnvMode::Inherit,
                    );
                    let cwd = PathBuf::from("/acp-test");
                    if resume {
                        runtime
                            .attach_existing_session_connected(
                                "existing".into(),
                                cwd,
                                false,
                                reader,
                                writer,
                            )
                            .await
                    } else {
                        runtime
                            .create_new_session_connected(cwd, reader, writer)
                            .await
                    }
                });
                let (reader, mut writer) = daemon.into_split();
                let mut reader = BufReader::new(reader);
                let Request::Subscribe { id, .. } = acp_daemon_request(&mut reader).await else {
                    panic!("expected subscription");
                };
                acp_daemon_event(&mut writer, json!({"type": "done", "id": id})).await;
                let Request::SetSessionEnv { id, .. } = acp_daemon_request(&mut reader).await
                else {
                    panic!("expected session environment");
                };
                if !disconnect {
                    acp_daemon_event(
                        &mut writer,
                        json!({"type": "error", "id": id, "message": "environment rejected"}),
                    )
                    .await;
                }
                drop(writer);
                drop(reader);
                let error = setup
                    .await
                    .unwrap()
                    .err()
                    .expect("environment failure must reject session setup");
                let message = format!("{error:#}");
                assert!(message.contains("failed to apply ACP session environment"));
                assert!(message.contains(if disconnect {
                    "Jcode daemon disconnected"
                } else {
                    "environment rejected"
                }));
            })
            .await
            .expect("ACP session environment failure timed out");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn acp_new_and_resumed_sessions_advertise_server_failover() {
    for resume in [false, true] {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (reader, writer) = client.into_split();
        let daemon = tokio::spawn(async move {
            let (reader, mut writer) = server.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            let Request::Subscribe {
                id,
                supports_server_failover,
                target_session_id,
                ..
            } = request
            else {
                panic!("expected ACP subscription");
            };
            assert!(
                supports_server_failover,
                "ACP must delegate account failover to the daemon"
            );
            assert_eq!(target_session_id.as_deref(), resume.then_some("existing"));
            // Stop after the subscription, without needing a live provider or daemon.
            let response = json!({"type": "error", "id": id, "message": "subscription captured"});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });
        let runtime = AcpRuntime::new(
            AcpProfile::Standard,
            ProviderChoice::Claude,
            None,
            None,
            SessionEnvMode::None,
        );
        let cwd = PathBuf::from("/acp-test");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            if resume {
                runtime
                    .attach_existing_session_connected(
                        "existing".into(),
                        cwd,
                        false,
                        reader,
                        writer,
                    )
                    .await
            } else {
                runtime
                    .create_new_session_connected(cwd, reader, writer)
                    .await
            }
        })
        .await
        .expect("ACP subscription timed out");
        daemon.await.unwrap();
        assert_eq!(
            result
                .err()
                .expect("mock daemon rejects subscription")
                .to_string(),
            "subscription captured"
        );
    }
}

#[test]
fn acp_tool_kind_maps_core_tools() {
    assert_eq!(tool_kind("read"), "read");
    assert_eq!(tool_kind("apply_patch"), "edit");
    assert_eq!(tool_kind("bash"), "execute");
    assert_eq!(tool_kind("agentgrep"), "search");
    assert_eq!(tool_kind("webfetch"), "fetch");
    assert_eq!(tool_kind("swarm"), "other");
    // Tools ACP clients gained with the full profile should not all render
    // as generic "other" cards.
    assert_eq!(tool_kind("bg"), "execute");
    assert_eq!(tool_kind("codemode"), "execute");
    assert_eq!(tool_kind("skill_manage"), "read");
    assert_eq!(tool_kind("todo"), "think");
    assert_eq!(tool_kind("websearch"), "fetch");
}

#[test]
fn json_rpc_parse_errors_use_standard_codes() {
    let (code, _) = JsonRpcMessage::parse("not json").unwrap_err();
    assert_eq!(code, JSONRPC_PARSE_ERROR);

    let (code, message) = JsonRpcMessage::parse(r#"{"method":"initialize"}"#).unwrap_err();
    assert_eq!(code, JSONRPC_INVALID_REQUEST);
    assert!(message.contains("jsonrpc"));
}

#[test]
fn prompt_from_params_accepts_text_images_and_resources() {
    let params = json!({
        "sessionId": "s1",
        "prompt": [
            {"type": "text", "text": "hello"},
            {"type": "image", "mimeType": "image/png", "data": "abc"},
            {"type": "resource", "resource": {"uri": "file:///tmp/a.rs", "text": "fn main(){}"}},
            {"type": "resource_link", "uri": "file:///tmp/b.rs", "name": "b.rs"}
        ]
    });
    let (text, images) = prompt_from_params(&params).unwrap();
    assert!(text.contains("hello"));
    assert!(text.contains("Embedded resource: file:///tmp/a.rs"));
    assert!(text.contains("Resource link: b.rs"));
    assert_eq!(images, vec![("image/png".to_string(), "abc".to_string())]);
}

#[test]
fn prompt_usage_deduplicates_live_snapshots_and_resets_per_call() {
    let mut usage = TurnUsage::default();
    usage.observe_snapshot(10, 2, Some(4), None);
    usage.observe_snapshot(10, 3, Some(4), Some(5));
    usage.observe_snapshot(10, 3, Some(4), Some(5));
    // Stale or partial snapshots must not lower the per-call watermark.
    usage.observe_snapshot(8, 1, Some(2), None);
    usage.observe_snapshot(10, 3, Some(4), Some(5));
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.cached_read_tokens, Some(4));
    assert_eq!(usage.cached_write_tokens, Some(5));
    usage.begin_call();
    usage.observe_snapshot(20, 3, Some(6), Some(7));
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.output_tokens, 6);
    assert_eq!(usage.cached_read_tokens, Some(10));
    assert_eq!(usage.cached_write_tokens, Some(12));
}

#[test]
fn prompt_response_reports_usage_accumulated_across_the_turn() {
    let mut usage = TurnUsage::default();
    usage.add(10, 2, Some(4), Some(5));
    usage.add(20, 3, Some(6), Some(7));

    assert_eq!(
        prompt_response("end_turn", &usage),
        json!({
            "stopReason": "end_turn",
            "usage": {
                "totalTokens": 57,
                "inputTokens": 30,
                "outputTokens": 5,
                "cachedReadTokens": 10,
                "cachedWriteTokens": 12,
            }
        })
    );
}

#[test]
fn prompt_response_omits_unreported_usage_and_cache_fields() {
    assert_eq!(
        prompt_response("end_turn", &TurnUsage::default()),
        json!({ "stopReason": "end_turn" })
    );

    let mut usage = TurnUsage::default();
    usage.add(10, 2, None, None);
    assert_eq!(
        prompt_response("cancelled", &usage),
        json!({
            "stopReason": "cancelled",
            "usage": {
                "totalTokens": 12,
                "inputTokens": 10,
                "outputTokens": 2,
            }
        })
    );
}

#[test]
fn initialize_standard_omits_jcode_meta() {
    let result = initialize_result(&json!({"protocolVersion": 1}), AcpProfile::Standard);
    assert_eq!(result["protocolVersion"], 1);
    assert!(result["agentCapabilities"].get("_meta").is_none());
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
}

#[test]
fn initialize_full_advertises_jcode_extension_meta() {
    let result = initialize_result(&json!({"protocolVersion": 1}), AcpProfile::Full);
    assert_eq!(
        result["agentCapabilities"]["_meta"]["jcode"]["profile"],
        "full"
    );
}

#[test]
fn event_mapper_maps_tool_lifecycle() {
    let mut mapper = EventMapper::new("session1".to_string(), AcpProfile::Standard);
    let start = mapper.map_event(ServerEvent::ToolStart {
        id: "tool1".to_string(),
        name: "bash".to_string(),
    });
    assert_eq!(start[0]["sessionUpdate"], "tool_call");
    assert_eq!(start[0]["kind"], "execute");

    let input = mapper.map_event(ServerEvent::ToolInput {
        id: None,
        delta: "{\"command\":\"true\"}".to_string(),
    });
    assert_eq!(input[0]["rawInput"]["command"], "true");

    let done = mapper.map_event(ServerEvent::ToolDone {
        codemode: None,
        ask: None,
        id: "tool1".to_string(),
        name: "bash".to_string(),
        output: "ok".to_string(),
        error: None,
    });
    assert_eq!(done[0]["status"], "completed");
    assert_eq!(done[0]["content"][0]["content"]["text"], "ok");
}

#[test]
fn event_mapper_keeps_overlapping_tool_inputs_on_their_call() {
    let mut mapper = EventMapper::new("session1".to_string(), AcpProfile::Standard);
    for id in ["a", "b"] {
        mapper.map_event(ServerEvent::ToolStart {
            id: id.to_string(),
            name: "bash".to_string(),
        });
        assert!(
            mapper
                .map_event(ServerEvent::ToolInput {
                    id: Some(id.to_string()),
                    delta: "{\"command\":".to_string(),
                })
                .is_empty()
        );
    }
    for (id, command) in [("a", "true"), ("b", "false")] {
        let updates = mapper.map_event(ServerEvent::ToolInput {
            id: Some(id.to_string()),
            delta: format!("\"{command}\"}}"),
        });
        assert_eq!(
            updates,
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "rawInput": {"command": command},
            })]
        );
    }
    for (id, command) in [("a", "true"), ("b", "false")] {
        let updates = mapper.map_event(ServerEvent::ToolExec {
            id: id.to_string(),
            name: "bash".to_string(),
        });
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["toolCallId"], id);
        assert_eq!(updates[0]["status"], "in_progress");
        assert_eq!(updates[0]["rawInput"], json!({"command": command}));
    }
}

#[test]
fn event_mapper_suppresses_incomplete_legacy_tool_input() {
    let mut mapper = EventMapper::new("session1".to_string(), AcpProfile::Standard);
    assert!(
        mapper
            .map_event(ServerEvent::ToolInput {
                id: None,
                delta: "ignored without a tool".to_string(),
            })
            .is_empty()
    );
    mapper.map_event(ServerEvent::ToolStart {
        id: "legacy".to_string(),
        name: "bash".to_string(),
    });
    for delta in ["", "{", "\"command\":"] {
        assert!(
            mapper
                .map_event(ServerEvent::ToolInput {
                    id: None,
                    delta: delta.to_string(),
                })
                .is_empty()
        );
    }
    let updates = mapper.map_event(ServerEvent::ToolInput {
        id: None,
        delta: "\"true\"}".to_string(),
    });
    assert_eq!(
        updates,
        vec![json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "legacy",
            "rawInput": {"command": "true"},
        })]
    );
}

#[test]
fn non_empty_mcp_servers_are_tolerated_until_session_scoped_mcp_is_supported() {
    let params = json!({"mcpServers": [{"name": "fs"}]});
    assert!(validate_acp_mcp_servers(&params).is_ok());

    let params = json!({"mcpServers": []});
    assert!(validate_acp_mcp_servers(&params).is_ok());
}

#[test]
fn advertised_commands_cover_all_acp_daemon_model_controls() {
    let commands = acp_available_commands();
    let names: Vec<&str> = commands
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["model", "models", "effort"]);
    assert_eq!(commands[0]["input"]["hint"], "model id (optional)");
    assert!(commands[1].get("input").is_none());
    assert!(
        commands[2]["input"]["hint"]
            .as_str()
            .unwrap()
            .contains("high")
    );
}

#[test]
fn advertised_commands_parse_to_real_dispatch_variants() {
    assert_eq!(
        parse_acp_slash_command("/model claude-sonnet-4-5")
            .unwrap()
            .unwrap(),
        AcpSlashCommand::Model(Some("claude-sonnet-4-5".to_string()))
    );
    assert_eq!(
        parse_acp_slash_command("/model ").unwrap().unwrap(),
        AcpSlashCommand::Model(None)
    );
    assert_eq!(
        parse_acp_slash_command("/models").unwrap().unwrap(),
        AcpSlashCommand::Models
    );
    assert_eq!(
        parse_acp_slash_command("/effort xhigh").unwrap().unwrap(),
        AcpSlashCommand::Effort(Some("xhigh".to_string()))
    );
    assert!(parse_acp_slash_command("/models now").unwrap().is_err());
    assert!(parse_acp_slash_command("/not-advertised").is_none());
    assert!(parse_acp_slash_command(" /model literal").is_none());
    assert!(parse_acp_slash_command("ordinary prompt").is_none());
}

fn project_skills(names: &[&str]) -> (tempfile::TempDir, crate::skill::SkillRegistry) {
    let dir = tempfile::tempdir().unwrap();
    for name in names {
        let skill_dir = dir.path().join(".jcode/skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test {name}\n---\nDo {name}.\n"),
        )
        .unwrap();
    }
    let registry = crate::skill::SkillRegistry::load_project_overlay(Some(dir.path())).unwrap();
    (dir, registry)
}

#[test]
fn slash_and_dollar_skill_invocations_resolve_with_trailing_prompt() {
    let (_dir, skills) = project_skills(&["interrogate"]);
    assert_eq!(
        resolve_acp_skill_invocation(&skills, "/interrogate"),
        Some(("interrogate".to_string(), None))
    );
    assert_eq!(
        resolve_acp_skill_invocation(&skills, "/interrogate the auth flow\n"),
        Some(("interrogate".to_string(), Some("the auth flow".to_string())))
    );
    assert_eq!(
        resolve_acp_skill_invocation(&skills, "$interrogate why"),
        Some(("interrogate".to_string(), Some("why".to_string())))
    );
}

#[test]
fn unknown_escaped_and_path_like_slash_text_is_untouched() {
    let (_dir, skills) = project_skills(&["interrogate"]);
    for text in [
        "/unknown do things",
        "/home/user/file.rs explain",
        " /interrogate escaped",
        "please /interrogate",
        "$HOME is set",
        "ordinary prompt",
    ] {
        assert_eq!(resolve_acp_skill_invocation(&skills, text), None, "{text}");
    }
}

#[test]
fn explicit_skill_metadata_is_read_from_prompt_meta() {
    assert_eq!(
        explicit_skill_from_params(&json!({"_meta": {"jcode": {"skill": "/interrogate"}}})),
        Some("interrogate".to_string())
    );
    assert_eq!(
        explicit_skill_from_params(&json!({"_meta": {"activeSkill": "$review"}})),
        Some("review".to_string())
    );
    assert_eq!(
        explicit_skill_from_params(&json!({"_meta": {"skill": " "}})),
        None
    );
    assert_eq!(explicit_skill_from_params(&json!({})), None);
}

#[test]
fn skill_commands_advertise_installed_skills_without_shadowing_builtins() {
    let (_dir, skills) = project_skills(&["interrogate", "model"]);
    let commands = acp_skill_commands(&skills);
    let names: Vec<&str> = commands
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["interrogate"]);
    assert_eq!(commands[0]["description"], "Test interrogate");
}

#[test]
fn compatibility_methods_accept_host_field_names_and_aliases() {
    assert_eq!(
        compatibility_option_value(
            &json!({"modelId": "deepseek-v4-flash"}),
            &["modelId", "model"],
            "session/set_model"
        )
        .unwrap(),
        "deepseek-v4-flash"
    );
    assert_eq!(
        compatibility_option_value(
            &json!({"reasoningEffort": "high"}),
            &["effort", "reasoningEffort"],
            "session/set_reasoning_effort"
        )
        .unwrap(),
        "high"
    );
    assert!(
        compatibility_option_value(
            &json!({"effort": ""}),
            &["effort", "reasoningEffort"],
            "session/set_reasoning_effort"
        )
        .unwrap_err()
        .contains("non-empty")
    );
}

#[test]
fn cwd_must_be_absolute() {
    let params = json!({"cwd": "relative"});
    assert!(cwd_from_params(&params).is_err());
    let params = json!({"cwd": "/tmp"});
    assert_eq!(cwd_from_params(&params).unwrap(), Path::new("/tmp"));
}

#[test]
fn config_options_include_model_selector_and_effort_ladder() {
    let state = SessionUiState {
        provider_name: Some("openai".to_string()),
        model: Some("gpt-5.2".to_string()),
        available_models: vec!["gpt-5.2".to_string(), "gpt-5.2-codex".to_string()],
        reasoning_effort: Some("high".to_string()),
    };
    let options = session_config_options(&state);
    assert_eq!(options.len(), 2);

    let model = &options[0];
    assert_eq!(model["id"], CONFIG_ID_MODEL);
    assert_eq!(model["category"], "model");
    assert_eq!(model["type"], "select");
    assert_eq!(model["currentValue"], "gpt-5.2");
    assert_eq!(model["options"].as_array().unwrap().len(), 2);

    let effort = &options[1];
    assert_eq!(effort["id"], CONFIG_ID_EFFORT);
    assert_eq!(effort["category"], "thought_level");
    assert_eq!(effort["currentValue"], "high");
    let effort_values: Vec<&str> = effort["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert!(effort_values.contains(&"medium"));
    assert!(
        !effort_values.iter().any(|value| value.starts_with("swarm")),
        "swarm sentinels are TUI-only and must not leak over ACP: {effort_values:?}"
    );
}

#[test]
fn config_options_current_model_prepended_when_not_listed() {
    let state = SessionUiState {
        provider_name: Some("anthropic".to_string()),
        model: Some("claude-opus-4-6".to_string()),
        available_models: vec!["claude-sonnet-4-5".to_string()],
        reasoning_effort: None,
    };
    let options = session_config_options(&state);
    let model_values: Vec<&str> = options[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert_eq!(model_values[0], "claude-opus-4-6");
    assert!(model_values.contains(&"claude-sonnet-4-5"));
}

#[test]
fn legacy_models_catalog_is_emitted_alongside_config_options() {
    let state = SessionUiState {
        provider_name: Some("deepseek".to_string()),
        model: Some("deepseek-v4-flash".to_string()),
        available_models: vec!["deepseek-v4-pro".to_string()],
        reasoning_effort: Some("high".to_string()),
    };
    let mut result = json!({"sessionId": "s1"});
    insert_session_configuration(&mut result, &state);

    assert!(result["configOptions"].is_array());
    assert_eq!(result["models"]["currentModelId"], "deepseek-v4-flash");
    let ids: Vec<&str> = result["models"]["availableModels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["modelId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["deepseek-v4-flash", "deepseek-v4-pro"]);
}

#[test]
fn config_options_empty_without_model_state() {
    let options = session_config_options(&SessionUiState::default());
    assert!(options.is_empty());
}

#[test]
fn context_limit_falls_back_to_default_for_unknown_models() {
    let state = SessionUiState {
        provider_name: Some("mystery".to_string()),
        model: Some("mystery-model-9000".to_string()),
        available_models: Vec::new(),
        reasoning_effort: None,
    };
    assert_eq!(
        state.context_limit(),
        crate::provider::DEFAULT_CONTEXT_LIMIT as u64
    );
}

#[test]
fn acp_turn_outcome_maps_terminal_limits_and_refusals() {
    use crate::protocol::TurnStopReason;
    let limit = |raw: Option<&str>, msg: &str| ServerEvent::TurnStopped {
        reason: TurnStopReason::LimitReached,
        message: msg.to_string(),
        provider_stop_reason: raw.map(str::to_string),
    };
    let mut o = TurnOutcome::default();
    o.observe(&ServerEvent::Ack { id: 7 }, 7);
    assert_eq!(o.stop_reason(false), "end_turn");
    o.observe(&limit(Some("max_tokens"), "truncated"), 7);
    assert_eq!(o.stop_reason(false), "max_tokens");
    // A continued model request clears an intermediate limit.
    o.observe(&ServerEvent::RetryRollback { attempt: 1, max: 3 }, 7);
    assert_eq!(o.stop_reason(false), "end_turn");
    o.observe(&limit(Some("max_turn_requests"), "limit"), 7);
    assert_eq!(o.stop_reason(false), "max_turn_requests");

    let mut o = TurnOutcome::default();
    o.observe(&ServerEvent::Ack { id: 7 }, 7);
    o.observe(
        &ServerEvent::ProviderGuardrail {
            stop_reason: Some("refusal".into()),
            message: "blocked".into(),
        },
        7,
    );
    assert_eq!(o.stop_reason(false), "refusal");
    // Cancellation wins over everything for the cancelled prompt.
    assert_eq!(o.stop_reason(true), "cancelled");

    // Non-refusal provider notices are not refusals.
    let mut o = TurnOutcome::default();
    o.observe(
        &ServerEvent::ProviderGuardrail {
            stop_reason: Some("end_turn".into()),
            message: "empty".into(),
        },
        7,
    );
    assert_eq!(o.stop_reason(false), "end_turn");
}

#[test]
fn acp_turn_outcome_ignores_stale_interrupt_before_ack() {
    let mut o = TurnOutcome::default();
    o.observe(&ServerEvent::Interrupted, 9);
    o.observe(&ServerEvent::Ack { id: 9 }, 9);
    assert_eq!(o.stop_reason(false), "end_turn");
    o.observe(&ServerEvent::Interrupted, 9);
    assert_eq!(o.stop_reason(false), "cancelled");
}

struct AcpReconnectFixture {
    runtime: AcpRuntime,
    session_id: String,
    listener: crate::transport::Listener,
    daemon: Option<(BufReader<ReadHalf>, WriteHalf)>,
    client: BufReader<tokio::io::DuplexStream>,
    env: Option<HashMap<String, String>>,
    _home: tempfile::TempDir,
}

impl AcpReconnectFixture {
    async fn new(mode: SessionEnvMode, model: Option<&str>) -> Self {
        Self::with_timeout(mode, model, reconnect::RECONNECT_TIMEOUT).await
    }

    async fn with_timeout(
        mode: SessionEnvMode,
        model: Option<&str>,
        timeout: std::time::Duration,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        let socket_path = home.path().join("acp.sock");
        // Windows named-pipe listeners accept through `&mut self`.
        #[allow(unused_mut)]
        let mut listener = crate::transport::Listener::bind(&socket_path).unwrap();
        let (output, client) = tokio::io::duplex(64 * 1024);
        let mut runtime = AcpRuntime::new(
            AcpProfile::Standard,
            ProviderChoice::Auto,
            model.map(str::to_string),
            None,
            mode,
        );
        runtime.socket_path = socket_path;
        runtime.reconnect_timeout = timeout;
        runtime.stdout = Arc::new(Mutex::new(Box::new(output)));
        let creating = {
            let runtime = runtime.clone();
            let cwd = home.path().to_path_buf();
            tokio::spawn(async move { runtime.create_new_session(cwd).await })
        };
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let Request::Subscribe {
            id,
            target_session_id,
            ..
        } = acp_daemon_request(&mut reader).await
        else {
            panic!("expected initial subscription")
        };
        assert!(target_session_id.is_none());
        acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
        let env = if mode == SessionEnvMode::Inherit {
            let Request::SetSessionEnv { id, env } = acp_daemon_request(&mut reader).await else {
                panic!("expected initial environment")
            };
            acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
            Some(env)
        } else {
            None
        };
        if let Some(expected_model) = model {
            let Request::SetModel { id, model } = acp_daemon_request(&mut reader).await else {
                panic!("expected launch model")
            };
            assert_eq!(model, expected_model);
            acp_daemon_event(
                &mut writer,
                json!({"type":"model_changed", "id":id, "model":model, "provider_name":"openai"}),
            )
            .await;
        }
        let Request::GetHistory { id } = acp_daemon_request(&mut reader).await else {
            panic!("expected initial history")
        };
        acp_daemon_event(&mut writer, json!({"type":"history", "id":id, "session_id":"persistent-acp", "messages":[], "provider_model":"saved-model"})).await;
        let session = Arc::new(creating.await.unwrap().unwrap());
        let session_id = session.session_id.clone();
        runtime
            .sessions
            .lock()
            .await
            .insert(session_id.clone(), session);
        Self {
            runtime,
            session_id,
            listener,
            daemon: Some((reader, writer)),
            client: BufReader::new(client),
            env,
            _home: home,
        }
    }

    async fn prompt(&self, id: u64) {
        self.runtime.handle_message(JsonRpcMessage {
            id: Some(json!(id)), method: Some("session/prompt".into()),
            params: json!({"sessionId":self.session_id, "prompt":[{"type":"text", "text":"continue"}]}),
        }).await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            assert_ne!(self.client.read_line(&mut line).await.unwrap(), 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response.get("id") == Some(&json!(id)) {
                return response;
            }
        }
    }

    async fn reattach(&mut self, expected_model: Option<&str>) -> (BufReader<ReadHalf>, WriteHalf) {
        let (stream, _) = self.listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let Request::Subscribe {
            id,
            target_session_id,
            working_dir,
            ..
        } = acp_daemon_request(&mut reader).await
        else {
            panic!("reconnect must attach")
        };
        assert_eq!(target_session_id.as_deref(), Some(self.session_id.as_str()));
        assert_eq!(working_dir.as_deref(), self._home.path().to_str());
        acp_daemon_event(&mut writer, json!({"type":"history", "id":id, "session_id":self.session_id, "messages":[], "provider_model":"saved-model"})).await;
        acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
        if let Some(expected_env) = &self.env {
            let Request::SetSessionEnv { id, env } = acp_daemon_request(&mut reader).await else {
                panic!("reconnect must reapply environment")
            };
            assert!(
                env == *expected_env,
                "reconnect must reuse the exact environment snapshot"
            );
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    acp_daemon_request(&mut reader)
                )
                .await
                .is_err(),
                "no prompt before environment acknowledgement"
            );
            acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
        }
        if let Some(expected_model) = expected_model {
            let Request::SetModel { id, model } = acp_daemon_request(&mut reader).await else {
                panic!("reconnect must reapply selected model")
            };
            assert_eq!(model, expected_model);
            acp_daemon_event(&mut writer, json!({"type":"model_changed", "id":id, "model":"selected-model", "provider_name":"openai"})).await;
        }
        (reader, writer)
    }
}

#[tokio::test]
async fn acp_idle_disconnect_reattaches_and_restores_env_and_model_before_following_prompt() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for mode in [SessionEnvMode::None, SessionEnvMode::Inherit] {
            for model in [None, Some("openai:launch-model")] {
                let mut fixture = AcpReconnectFixture::new(mode, model).await;
                drop(fixture.daemon.take());
                let (mut reader, mut writer) = fixture.reattach(model).await;
                fixture.prompt(42).await;
                let Request::Message { id, content, .. } = acp_daemon_request(&mut reader).await
                else {
                    panic!("expected following prompt")
                };
                assert_eq!(content, "continue");
                acp_daemon_event(&mut writer, json!({"type":"ack", "id":id})).await;
                acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
                assert_eq!(
                    fixture.response(42).await["result"]["stopReason"],
                    "end_turn"
                );
            }
        }
    })
    .await
    .expect("idle ACP recovery timed out");
}

#[tokio::test]
async fn acp_disconnect_during_turn_returns_rpc_error_and_next_prompt_recovers() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut fixture = AcpReconnectFixture::new(SessionEnvMode::Inherit, None).await;
        fixture.prompt(41).await;
        let (mut reader, mut writer) = fixture.daemon.take().unwrap();
        let Request::Message { id, .. } = acp_daemon_request(&mut reader).await else {
            panic!("expected prompt")
        };
        acp_daemon_event(&mut writer, json!({"type":"ack", "id":id})).await;
        drop((reader, writer));
        let failed = fixture.response(41).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("daemon disconnected")
        );
        assert!(failed.get("result").is_none());
        fixture.prompt(42).await;
        let (mut reader, mut writer) = fixture.reattach(None).await;
        let Request::Message { id, content, .. } = acp_daemon_request(&mut reader).await else {
            panic!("expected new prompt, not replay")
        };
        assert_eq!(content, "continue");
        acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
        assert_eq!(
            fixture.response(42).await["result"]["stopReason"],
            "end_turn"
        );
    })
    .await
    .expect("interrupted ACP turn did not complete");
}

#[tokio::test]
async fn acp_configured_model_route_survives_reconnect() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut fixture = AcpReconnectFixture::new(SessionEnvMode::Inherit, Some("openai:launch-model")).await;
        let runtime = fixture.runtime.clone();
        let session_id = fixture.session_id.clone();
        let setting = tokio::spawn(async move {
            runtime.handle_message(JsonRpcMessage { id: Some(json!(20)), method: Some("session/set_config_option".into()), params: json!({"sessionId":session_id,"configId":CONFIG_ID_MODEL,"value":"openai:chosen-model"}) }).await.unwrap();
        });
        let (mut reader, mut writer) = fixture.daemon.take().unwrap();
        let Request::SetModel { id, .. } = acp_daemon_request(&mut reader).await else { panic!("expected configured model") };
        acp_daemon_event(&mut writer, json!({"type":"model_changed","id":id,"model":"chosen-model","provider_name":"openai"})).await;
        setting.await.unwrap();
        assert!(fixture.response(20).await.get("error").is_none());
        drop((reader, writer));
        let (_reader, _writer) = fixture.reattach(Some("openai:chosen-model")).await;
    }).await.expect("configured ACP model recovery timed out");
}

#[tokio::test]
async fn acp_failed_reconnect_errors_waiting_request_and_retries_next_request() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut fixture = AcpReconnectFixture::with_timeout(
            SessionEnvMode::Inherit,
            None,
            std::time::Duration::from_millis(500),
        )
        .await;
        drop(fixture.daemon.take());
        for attempt in 0..2 {
            let (stream, _) = fixture.listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let Request::Subscribe { id, .. } = acp_daemon_request(&mut reader).await else {
                panic!("expected reconnect subscription")
            };
            if attempt == 0 {
                fixture.prompt(41).await;
            }
            acp_daemon_event(
                &mut writer,
                json!({"type":"error", "id":id, "message":"daemon is still restarting"}),
            )
            .await;
        }
        let failed = fixture.response(41).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Failed to reconnect")
        );
        fixture.prompt(42).await;
        let (mut reader, mut writer) = fixture.reattach(None).await;
        let Request::Message { id, .. } = acp_daemon_request(&mut reader).await else {
            panic!("expected prompt after failed reconnect")
        };
        acp_daemon_event(&mut writer, json!({"type":"done", "id":id})).await;
        assert_eq!(
            fixture.response(42).await["result"]["stopReason"],
            "end_turn"
        );
    })
    .await
    .expect("ACP did not retry after a reconnect deadline");
}

#[tokio::test]
async fn acp_reload_notice_interrupts_turn_without_waiting_for_eof() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut fixture = AcpReconnectFixture::new(SessionEnvMode::Inherit, None).await;
        fixture.prompt(41).await;
        let (mut reader, mut writer) = fixture.daemon.take().unwrap();
        let Request::Message { id, .. } = acp_daemon_request(&mut reader).await else {
            panic!("expected prompt")
        };
        acp_daemon_event(&mut writer, json!({"type":"ack", "id":id})).await;
        acp_daemon_event(
            &mut writer,
            json!({"type":"reloading", "new_socket":null, "reload_id":null}),
        )
        .await;
        let failed = fixture.response(41).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("reload interrupted")
        );
        assert!(
            !fixture.runtime.sessions.lock().await[&fixture.session_id]
                .prompt_running
                .load(Ordering::SeqCst)
        );
        drop((reader, writer));
    })
    .await
    .expect("ACP reload notice did not finish the interrupted turn");
}

#[tokio::test]
async fn acp_reconnect_backoff_is_bounded_even_when_connect_hangs() {
    let attempts = AtomicU64::new(0);
    let error = reconnect::reconnect_with_backoff(std::time::Duration::from_millis(350), || {
        attempts.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>(anyhow::anyhow!("connection refused")) }
    })
    .await
    .unwrap_err();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(error.to_string().contains("connection refused"));
    reconnect::reconnect_with_backoff(std::time::Duration::from_millis(150), || async {
        std::future::pending::<Result<()>>().await
    })
    .await
    .expect_err("hung connect must respect the total deadline");
}
