use super::*;

#[cfg(unix)]
#[tokio::test]
async fn queue_mutations_survive_daemon_restart_over_socket() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    for name in ["mutation-b", "mutation-c"] {
        let dir = home._home.path().join(".jcode/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\n\nInstructions.\n"),
        )
        .unwrap();
    }
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) -> ServerEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("daemon event timeout")
    }
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("mutations-1.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("mutations-debug-1.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    let session_id = server
        .handles
        .sessions
        .snapshot()
        .await
        .keys()
        .next()
        .unwrap()
        .clone();
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"busy"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    let (peer_reader, mut peer_writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut peer_reader = BufReader::new(peer_reader);
    send(&mut peer_writer, json!({"type":"subscribe","id":20,"target_session_id":session_id,"continue_on_disconnect":true})).await;
    until(&mut peer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 20 })
    })
    .await;
    for (id, prompt, skill) in [
        (3, "a", None),
        (4, "b", Some("mutation-b")),
        (5, "c", Some("mutation-c")),
    ] {
        send(&mut writer, json!({"type":"queue_prompt","id":id,"client_id":prompt,"content":prompt,"mode":"after_yield","active_skill":skill})).await;
        until(
            &mut reader,
            |e| matches!(e, ServerEvent::Ack { id: n } if *n == id),
        )
        .await;
    }
    send(
        &mut writer,
        json!({"type":"reorder_queued_prompts","id":6,"client_id":"test","order":["c","a","b"]}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 6 })).await;
    until(&mut peer_reader, |e| matches!(e, ServerEvent::QueuedPrompts { id: 0, prompts } if prompts.iter().map(|p| p.client_id.as_str()).collect::<Vec<_>>() == ["c", "a", "b"])).await;
    // Hold and edit the middle prompt in place, preserving its slot and mode.
    send(
        &mut writer,
        json!({"type":"set_queued_prompt_editing","id":60,"client_id":"test","prompt_id":"a","editing":true}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 60 })).await;
    until(&mut peer_reader, |e| matches!(e, ServerEvent::QueuedPrompts { id: 0, prompts } if prompts.get(1).is_some_and(|p| p.client_id == "a" && p.editing))).await;
    send(
        &mut writer,
        json!({"type":"edit_queued_prompt","id":61,"client_id":"test","prompt_id":"a","content":"a edited"}),
    )
    .await;
    until(&mut reader, |e| {
        matches!(
            e,
            ServerEvent::QueuedPromptEdited {
                id: 61,
                outcome: crate::protocol::QueuedPromptEditOutcome::Edited,
                ..
            }
        )
    })
    .await;
    until(&mut peer_reader, |e| matches!(e, ServerEvent::QueuedPrompts { id: 0, prompts } if prompts.get(1).is_some_and(|p| p.client_id == "a" && p.content == "a edited" && !p.editing && p.mode == crate::protocol::QueuedPromptMode::AfterYield))).await;
    send(
        &mut writer,
        json!({"type":"remove_queued_prompt","id":7,"client_id":"test","prompt_id":"a"}),
    )
    .await;
    until(&mut reader, |e| {
        matches!(
            e,
            ServerEvent::QueuedPromptRemoved {
                id: 7,
                outcome: crate::protocol::QueuedPromptRemovalOutcome::Removed,
                ..
            }
        )
    })
    .await;
    until(&mut peer_reader, |e| matches!(e, ServerEvent::QueuedPrompts { id: 0, prompts } if prompts.iter().map(|p| p.client_id.as_str()).collect::<Vec<_>>() == ["c", "b"])).await;
    drop(peer_writer);
    drop(peer_reader);
    drop(writer);
    drop(reader);
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap();
    handle.await.unwrap();
    for signal in crate::turn_cancel_registry::active_turn_signals(&session_id) {
        signal.fire();
    }
    provider.finish_first.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while crate::turn_cancel_registry::has_active_turn(&session_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old turn did not retire");
    let provider2 = TerminalSocketProvider::default();
    let socket2 = home._runtime.path().join("mutations-2.sock");
    let server2 = crate::server::Server::new_with_paths(
        Arc::new(provider2),
        socket2.clone(),
        home._runtime.path().join("mutations-debug-2.sock"),
    );
    let runtime2 = server2.runtime();
    let handle2 =
        runtime2.spawn_main_accept_loop(crate::transport::Listener::bind(&socket2).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket2)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    send(
        &mut writer,
        json!({"type":"subscribe","id":8,"target_session_id":session_id}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 8 })).await;
    send(&mut writer, json!({"type":"get_queued_prompts","id":9})).await;
    let snapshot = until(&mut reader, |e| {
        matches!(e, ServerEvent::QueuedPrompts { id: 9, .. })
    })
    .await;
    let ServerEvent::QueuedPrompts { prompts, .. } = snapshot else {
        unreachable!()
    };
    assert_eq!(
        prompts
            .iter()
            .map(|p| p.client_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c", "b"]
    );
    assert_eq!(
        prompts
            .iter()
            .map(|p| p.active_skill.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("mutation-c"), Some("mutation-b")]
    );
    drop(writer);
    drop(reader);
    runtime2.shutdown().await;
    handle2.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_accepts_queue_immediately_after_turn_done() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("turn-done-race.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("turn-done-race-debug.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                assert!(!matches!(&event, ServerEvent::Error { message, .. } if message.contains("Already processing")), "{event:?}");
                if predicate(&event) { break; }
            }
        }).await.expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    provider.finish_first.notify_one();
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 2 })).await;
    send(&mut writer, json!({"type":"queue_prompt","id":3,"client_id":"done-race","content":"next","mode":"steer"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prompt was not delivered");
    let messages = provider.messages.lock().unwrap().clone();
    assert_eq!(
        messages[1]
            .iter()
            .filter(|text| text.ends_with("] next"))
            .count(),
        1,
        "{messages:?}"
    );
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

/// A server-owned turn (swarm wake, background completion) has no client
/// processing task. When it ends, after-yield prompts the user queued during it
/// must run as their own turn, and a later interrupt of one new prompt must
/// never carry the older after-yield prompts along with it.
#[cfg(unix)]
#[tokio::test]
async fn after_yield_queue_runs_when_a_server_owned_turn_yields() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("wake-yield.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("wake-yield-debug.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    break;
                }
            }
        })
        .await
        .expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    let session_id = server
        .handles
        .sessions
        .snapshot()
        .await
        .keys()
        .next()
        .unwrap()
        .clone();

    // A wake turn the daemon starts on its own, like a swarm report.
    let swarm = crate::server::live_turn::LiveTurnSwarmContext::new(
        &server.handles.swarm_state.members,
        &server.handles.swarm_state.swarms_by_id,
        &server.handles.event_history,
        &server.handles.event_counter,
        &server.handles.swarm_event_tx,
    );
    assert!(
        crate::server::live_turn::run_live_turn_if_idle(
            &session_id,
            "worker reported",
            None,
            &server.handles.sessions,
            swarm,
        )
        .await
    );
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    for (id, client_id, content) in [(3, "q1", "held one"), (4, "q2", "held two")] {
        send(&mut writer, json!({"type":"queue_prompt","id":id,"client_id":client_id,"content":content,"mode":"after_yield"})).await;
        until(
            &mut reader,
            |e| matches!(e, ServerEvent::Ack { id: n } if *n == id),
        )
        .await;
    }
    provider.finish_first.notify_one();

    // The wake turn yields: the held prompts run as the next turn, alone.
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("after-yield prompts were not delivered when the wake turn yielded");
    tokio::time::timeout(Duration::from_secs(10), async {
        while crate::turn_cancel_registry::has_active_turn(&session_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // A brand-new prompt sent now must run without the old ones.
    send(&mut writer, json!({"type":"queue_prompt","id":5,"client_id":"fresh","content":"fresh prompt","mode":"after_yield"})).await;
    send(
        &mut writer,
        json!({"type":"interrupt_queued_prompts","id":6,"ids":["fresh"]}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fresh prompt was not delivered");
    let messages = provider.messages.lock().unwrap().clone();
    let count = |call: &[String], needle: &str| call.iter().filter(|t| t.contains(needle)).count();
    assert_eq!(count(&messages[1], "held one"), 1, "{messages:?}");
    assert_eq!(count(&messages[1], "held two"), 1, "{messages:?}");
    assert_eq!(count(&messages[1], "fresh prompt"), 0, "{messages:?}");
    // The third request repeats history, so the held prompts appear once each.
    assert_eq!(count(&messages[2], "held one"), 1, "{messages:?}");
    assert_eq!(count(&messages[2], "fresh prompt"), 1, "{messages:?}");
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_delivers_switched_steer_before_ordered_followups() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    provider.steer_tool.store(true, Ordering::SeqCst);
    let socket = home._runtime.path().join("order.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("order-debug.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    break;
                }
            }
        })
        .await
        .expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    for (id, text) in [(3, "later-one"), (4, "switch-to-steer"), (5, "later-two")] {
        send(&mut writer, json!({"type":"queue_prompt","id":id,"client_id":text,"content":text,"mode":"after_yield","system_reminder":"collection instructions"})).await;
        until(
            &mut reader,
            |event| matches!(event, ServerEvent::Ack { id: ack } if *ack == id),
        )
        .await;
    }
    send(&mut writer, json!({"type":"set_queued_prompt_mode","id":6,"client_id":"switch-to-steer","mode":"steer"})).await;
    until(&mut reader, |event| {
        matches!(event, ServerEvent::Ack { id: 6 })
    })
    .await;
    provider.finish_first.notify_one();
    until(&mut reader, |event| {
        matches!(event, ServerEvent::Done { id: 0 })
    })
    .await;
    let messages = provider.messages.lock().unwrap().clone();
    let count = |turn: usize, text: &str| {
        messages[turn]
            .iter()
            .flat_map(|message| message.split("\n\n"))
            .filter(|part| part.ends_with(text))
            .count()
    };
    assert!(
        messages[2]
            .iter()
            .any(|message| message.contains("collection instructions")),
        "queued follow-up lost its system reminder: {messages:?}"
    );
    assert_eq!(
        messages.len(),
        3,
        "follow-ups must share one queued turn: {messages:?}"
    );
    assert_eq!(
        (
            count(0, "switch-to-steer"),
            count(0, "later-one"),
            count(0, "later-two")
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            count(1, "switch-to-steer"),
            count(1, "later-one"),
            count(1, "later-two")
        ),
        (1, 0, 0)
    );
    assert_eq!(
        (
            count(2, "switch-to-steer"),
            count(2, "later-one"),
            count(2, "later-two")
        ),
        (1, 1, 1)
    );
    let followups = messages[2].join("\n");
    assert!(
        followups.contains("later-one\n\nlater-two"),
        "queue order changed: {followups:?}"
    );
    send(&mut writer, json!({"type":"remove_queued_prompt","id":7,"client_id":"test","prompt_id":"switch-to-steer"})).await;
    until(&mut reader, |event| {
        matches!(
            event,
            ServerEvent::QueuedPromptRemoved {
                id: 7,
                outcome: crate::protocol::QueuedPromptRemovalOutcome::AlreadyDelivered,
                ..
            }
        )
    })
    .await;
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_restart_restores_and_delivers_pending_prompt_once() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let skill_dir = home._home.path().join(".jcode/skills/queued-test");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: queued-test\ndescription: test\n---\n\nQueued skill instructions.\n",
    )
    .unwrap();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("restart-1.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("debug-1.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    break;
                }
            }
        })
        .await
        .expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    let session_id = server
        .handles
        .sessions
        .snapshot()
        .await
        .keys()
        .next()
        .unwrap()
        .clone();
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    send(&mut writer, json!({"type":"queue_prompt","id":30,"client_id":"unknown","content":"must not send","mode":"after_yield","active_skill":"no-such-queued-skill"})).await;
    until(&mut reader, |e| {
        matches!(
            e,
            ServerEvent::QueuedPromptRejected {
                id: 30,
                reason: crate::protocol::QueuedPromptRejection::UnknownSkill,
                ..
            }
        )
    })
    .await;
    send(&mut writer, json!({"type":"queue_prompt","id":3,"client_id":"restart-prompt","content":"survive restart","mode":"after_yield","system_reminder":"persistent collection instructions","active_skill":"queued-test"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
    drop(writer);
    drop(reader);
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap();
    handle.await.unwrap();
    // Simulate process exit by dropping the old turn without consuming its queue.
    for signal in crate::turn_cancel_registry::active_turn_signals(&session_id) {
        signal.fire();
    }
    provider.finish_first.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while crate::turn_cancel_registry::has_active_turn(&session_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old process turn did not retire");
    assert_eq!(
        crate::soft_interrupt_store::load(&session_id)
            .unwrap()
            .len(),
        1
    );

    let provider2 = TerminalSocketProvider::default();
    provider2.finish_first.notify_one();
    let socket2 = home._runtime.path().join("restart-2.sock");
    let server2 = crate::server::Server::new_with_paths(
        Arc::new(provider2.clone()),
        socket2.clone(),
        home._runtime.path().join("debug-2.sock"),
    );
    let runtime2 = server2.runtime();
    let handle2 =
        runtime2.spawn_main_accept_loop(crate::transport::Listener::bind(&socket2).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket2)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    send(
        &mut writer,
        json!({"type":"subscribe","id":4,"target_session_id":session_id}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 4 })).await;
    send(&mut writer, json!({"type":"get_queued_prompts","id":5})).await;
    until(&mut reader, |e| {
        matches!(e, ServerEvent::QueuedPrompts { id: 5, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider2.calls.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored prompt not delivered");
    let messages = provider2.messages.lock().unwrap().clone();
    let journal = crate::session::Session::load(&session_id).unwrap();
    assert_eq!(journal.pinned_skill(), Some("queued-test"));
    let skill_index = journal
        .iter_messages()
        .position(|message| {
            message.display_role == Some(crate::session::StoredDisplayRole::System)
                && format!("{:?}", message.content).contains("Queued skill instructions")
        })
        .expect("missing skill system entry");
    assert!(
        format!(
            "{:?}",
            journal
                .message_at(skill_index + 1)
                .expect("transcript message")
                .content
        )
        .contains("survive restart"),
        "skill should precede queued user text"
    );
    assert!(
        messages[0]
            .iter()
            .any(|message| message.contains("# Active Skill")
                && message.contains("Queued skill instructions")),
        "lost skill on restart: {messages:?}"
    );
    assert!(
        messages[0]
            .iter()
            .any(|message| message.contains("persistent collection instructions")),
        "lost reminder on restart: {messages:?}"
    );
    assert_eq!(
        messages[0]
            .iter()
            .filter(|s| s.ends_with("] survive restart"))
            .count(),
        1,
        "{messages:?}"
    );
    send(&mut writer, json!({"type":"queue_prompt","id":6,"client_id":"restart-prompt","content":"survive restart","mode":"after_yield"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 6 })).await;
    assert_eq!(provider2.calls.load(Ordering::SeqCst), 1);
    drop(writer);
    drop(reader);
    runtime2.shutdown().await;
    handle2.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_listener_delivers_all_queue_modes_once() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let skill_dir = home._home.path().join(".jcode/skills/modes-test");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: modes-test\ndescription: test\n---\n\nModes skill instructions.\n",
    )
    .unwrap();
    for mode in ["after_yield", "steer", "interrupt"] {
        let provider = TerminalSocketProvider::default();
        provider.steer_tool.store(mode == "steer", Ordering::SeqCst);
        let socket = home._runtime.path().join(format!("{mode}.sock"));
        let server = crate::server::Server::new_with_paths(
            Arc::new(provider.clone()),
            socket.clone(),
            home._runtime.path().join(format!("{mode}-debug.sock")),
        );
        let runtime = server.runtime();
        let handle =
            runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
        let stream = crate::transport::Stream::connect(&socket).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
            writer
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn until(
            reader: &mut BufReader<crate::transport::ReadHalf>,
            predicate: impl Fn(&ServerEvent) -> bool,
        ) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    let event: ServerEvent = serde_json::from_str(&line).unwrap();
                    if predicate(&event) {
                        break;
                    }
                }
            })
            .await
            .expect("daemon event timeout");
        }
        send(
            &mut writer,
            json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
        )
        .await;
        until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
        send(
            &mut writer,
            json!({"type":"message","id":2,"content":"first turn"}),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
            .await
            .unwrap();
        let queue_mode = if mode == "steer" {
            "steer"
        } else {
            "after_yield"
        };
        let request = json!({"type":"queue_prompt","id":3,"client_id":format!("{mode}-one"),"content":"follow-up","mode":queue_mode,"active_skill":"modes-test"});
        send(&mut writer, request.clone()).await;
        until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
        send(&mut writer, request).await;
        until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        if mode == "interrupt" {
            let session_id = server
                .handles
                .sessions
                .snapshot()
                .await
                .keys()
                .next()
                .unwrap()
                .clone();
            let (peer_reader, mut peer_writer) = crate::transport::Stream::connect(&socket)
                .await
                .unwrap()
                .into_split();
            let mut peer_reader = BufReader::new(peer_reader);
            send(
                &mut peer_writer,
                json!({"type":"subscribe", "id":10, "target_session_id":session_id}),
            )
            .await;
            until(&mut peer_reader, |e| {
                matches!(e, ServerEvent::Done { id: 10 })
            })
            .await;
            send(
                &mut peer_writer,
                json!({"type":"interrupt_queued_prompt","id":4,"client_id":format!("{mode}-one")}),
            )
            .await;
            until(&mut peer_reader, |e| {
                matches!(e, ServerEvent::Ack { id: 4 })
            })
            .await;
        } else {
            provider.finish_first.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while provider.calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued prompt not delivered");
        let messages = provider.messages.lock().unwrap().clone();
        assert_eq!(messages.len(), 2, "{mode}: {messages:?}");
        assert_eq!(
            messages[0]
                .iter()
                .filter(|s| s.ends_with("] follow-up"))
                .count(),
            0
        );
        assert_eq!(
            messages[1]
                .iter()
                .filter(|s| s.ends_with("] follow-up"))
                .count(),
            1,
            "{mode}: {messages:?}"
        );
        assert!(
            messages[1]
                .iter()
                .any(|s| s.contains("Modes skill instructions")),
            "{mode}: {messages:?}"
        );
        let session_id = server
            .handles
            .sessions
            .snapshot()
            .await
            .keys()
            .next()
            .unwrap()
            .clone();
        let journal = crate::session::Session::load(&session_id).unwrap();
        let skill = journal
            .iter_messages()
            .position(|m| {
                m.display_role == Some(crate::session::StoredDisplayRole::System)
                    && format!("{:?}", m.content).contains("Modes skill instructions")
            })
            .expect("skill entry");
        assert!(
            format!(
                "{:?}",
                journal.iter_messages().nth(skill + 1).unwrap().content
            )
            .contains("follow-up"),
            "{mode}"
        );
        drop(writer);
        drop(reader);
        runtime.shutdown().await;
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn attachments_reach_the_provider_over_daemon_socket() {
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("attachments.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("attachments-debug.sock"),
    );
    let runtime = server.runtime();
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    writer
        .write_all(
            format!(
                "{}\n",
                json!({"type":"subscribe","id":1,"working_dir":home._home.path()})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if matches!(
            serde_json::from_str::<ServerEvent>(&line).unwrap(),
            ServerEvent::Done { id: 1 }
        ) {
            break;
        }
    }
    writer
        .write_all(
            format!(
                "{}\n",
                json!({"type":"message","id":2,"content":"inspect","attachments":[
                    {"type":"file","name":"example.rs","mime":"text/plain","size":3,"data":"YWJj"},
                    {"type":"file","name":"image.png","mime":"image/png","size":3,"data":"AQID"},
                    {"type":"pasted_text","text":"one\ntwo"}
                ]})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), provider.first_started.notified())
        .await
        .unwrap();
    let messages = provider.messages.lock().unwrap().clone();
    assert!(messages[0].iter().any(|text| text.contains("example.rs")
        && text.contains("abc")
        && text.contains("one\ntwo")));
    assert!(provider.images.lock().unwrap()[0].contains(&("image/png".into(), "AQID".into())));
    provider.finish_first.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if matches!(
                serde_json::from_str::<ServerEvent>(&line).unwrap(),
                ServerEvent::Done { id: 2 }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    writer
        .write_all(b"{\"type\":\"get_history\",\"id\":3}\n")
        .await
        .unwrap();
    let history = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if let ServerEvent::History {
                id: 3,
                session_id,
                messages,
                ..
            } = serde_json::from_str::<ServerEvent>(&line).unwrap()
            {
                break (session_id, messages);
            }
        }
    })
    .await
    .unwrap();
    let user = history
        .1
        .iter()
        .find(|message| message.role == "user")
        .unwrap();
    assert_eq!(user.content, "inspect");
    assert_eq!(user.attachments.len(), 3);
    assert!(
        matches!(&user.attachments[2], jcode_session_types::StoredAttachment::PastedText { text } if text == "one\ntwo")
    );
    let persisted = crate::session::Session::load(&history.0).unwrap();
    assert!(persisted.iter_messages().any(|message| {
        message
            .attachment_data
            .as_ref()
            .is_some_and(|data| data.items.len() == 3)
    }));
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_pinned_skill_does_not_start_a_turn_but_pending_work_recovers() {
    use crate::server::reload_recovery;
    use crate::session::{Session, SessionStatus, StoredDisplayRole};
    use serde_json::json;

    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let skill = home._home.path().join(".jcode/skills/restore-probe");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: restore-probe\ndescription: Restore test\n---\n\nStay concise.\n",
    )
    .unwrap();
    let provider = TerminalSocketProvider::default();
    provider.finish_first.notify_one();
    let socket = home._runtime.path().join("pinned-restore.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("pinned-debug.sock"),
    );
    let runtime = server.runtime();
    let daemon = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let recovery = crate::server::recovery_owner::RecoveryOwner::new(&server.handles);

    for (role, content, should_resume) in [
        (
            Role::Assistant,
            ContentBlock::Text {
                text: "Finished".into(),
                cache_control: None,
            },
            false,
        ),
        (
            Role::User,
            ContentBlock::Text {
                text: "Pending prompt".into(),
                cache_control: None,
            },
            true,
        ),
        (
            Role::Assistant,
            ContentBlock::ToolUse {
                id: "pending-call".into(),
                name: "test-unknown-tool".into(),
                input: json!({}),
                thought_signature: None,
            },
            true,
        ),
    ] {
        let before = provider.calls.load(Ordering::SeqCst);
        let mut session = Session::create(None, None);
        session.apply_workspace(crate::session::Workspace {
            working_dir: Some(home._home.path().to_string_lossy().into_owned()),
            ..session.workspace()
        });
        session.set_pinned_skill_names(vec!["restore-probe".into()]);
        if role == Role::Assistant {
            session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: "Original request".into(),
                    cache_control: None,
                }],
            );
        }
        session.add_message(role, vec![content]);
        assert_eq!(session.status(), SessionStatus::Active);
        session.save().unwrap();
        let id = session.id().to_owned();

        let (reader, mut writer) = crate::transport::Stream::connect(&socket)
            .await
            .unwrap()
            .into_split();
        writer
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"type":"subscribe","id":10,"working_dir":home._home.path()}),
                    json!({"type":"resume_session","id":1,"session_id":id})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(reader);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                match event {
                    ServerEvent::Done { id: 1 } => break,
                    ServerEvent::Error {
                        id: 1 | 10,
                        message,
                        ..
                    } => panic!("resume failed: {message}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("resume did not finish");

        let agent = server
            .handles
            .sessions
            .snapshot()
            .await
            .get(&id)
            .unwrap()
            .clone();
        {
            let agent = agent.lock().await;
            let last = agent.iter_messages().last().unwrap();
            assert_eq!(last.display_role, Some(StoredDisplayRole::System));
            assert!(last.content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text.contains("# Active Skill") && text.contains("Stay concise."))));
        }
        assert_eq!(
            reload_recovery::peek_for_session(&id).unwrap().is_some(),
            should_resume,
            "a skill announcement must neither start work nor hide an unfinished turn"
        );
        let tasks = recovery.reconcile_once().await.tasks;
        assert_eq!(tasks.len(), usize::from(should_resume));
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        }
        let after = provider.calls.load(Ordering::SeqCst);
        if should_resume {
            assert!(after > before, "unfinished turn never reached the provider");
            let session = Session::load(&id).unwrap();
            assert!(session.iter_messages().last().is_some_and(|last| {
                last.role == Role::Assistant && last.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text, .. } if text == "completed")
                })
            }));
        } else {
            assert_eq!(
                after, before,
                "restoring a finished turn contacted the provider"
            );
        }
    }

    runtime.shutdown().await;
    daemon.await.unwrap();
}

#[tokio::test]
async fn busy_long_lived_session_receives_catalog_update_after_turn() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("catalog"),
    )
    .await;
}

#[tokio::test]
async fn queued_follow_up_runs_only_after_the_current_turn_over_socket() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("after_yield"),
    )
    .await;
}

#[tokio::test]
async fn queued_steer_is_injected_without_cancelling_over_socket() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, false, Some("steer")).await;
}

#[tokio::test]
async fn busy_session_sets_effort_over_socket_before_turn_finishes() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("effort"),
    )
    .await;
}

#[tokio::test]
async fn queued_prompt_interrupts_current_turn_over_socket() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("interrupt"),
    )
    .await;
}

#[tokio::test]
async fn queued_prompt_survives_socket_reattach_without_duplication() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("reload"),
    )
    .await;
}

#[tokio::test]
async fn terminal_done_then_immediate_message_socket_dispatches_next_turn() {
    sdk_socket_workflow(false, true, false).await;
}

#[tokio::test]
async fn sdk_socket_configure_list_callback_and_busy_rejection() {
    sdk_socket_workflow(false, false, false).await;
}

#[tokio::test]
async fn scoped_and_legacy_socket_subscribers_receive_compatible_turn_streams() {
    sdk_socket_workflow(false, true, true).await;
}

#[tokio::test]
async fn scoped_cancel_socket_immediately_dispatches_next_prompt() {
    sdk_socket_cancel_workflow(false, true, true, true, false, false, false, None).await;
}

#[tokio::test]
async fn idle_soft_interrupt_without_client_id_is_picked_up_before_cancel() {
    sdk_socket_cancel_workflow(false, true, true, true, true, false, false, None).await;
}

#[tokio::test]
async fn catalog_failure_preserves_remote_socket_owner_and_next_turn() {
    sdk_socket_cancel_workflow(false, true, true, false, false, true, false, None).await;
}

#[tokio::test]
async fn force_stop_pending_ask_over_socket_releases_session() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("force_stop"),
    )
    .await;
}

#[tokio::test]
async fn messages_sent_during_a_drain_wait_and_run_once_after_completion() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("hold_complete"),
    )
    .await;
}

#[tokio::test]
async fn messages_sent_during_a_drain_wait_and_run_once_after_rollback() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("hold_rollback"),
    )
    .await;
}

#[tokio::test]
async fn prompts_parked_during_a_drain_start_when_a_client_reattaches_to_the_new_daemon() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("hold_reattach"),
    )
    .await;
}

#[tokio::test]
async fn cancel_after_owner_reconnect_is_confirmed_to_the_new_connection() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("cancel_reconnect"),
    )
    .await;
}

#[tokio::test]
async fn force_stop_never_returning_tool_over_socket_releases_session() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("force_stop_tool"),
    )
    .await;
}

#[tokio::test]
async fn live_reattached_socket_answers_ask_in_existing_turn() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, true, None).await;
}
