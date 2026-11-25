use super::*;
use crate::message::Role;

#[cfg(unix)]
#[path = "client_lifecycle_socket_batch_tests.rs"]
mod batch_tests;

#[cfg(unix)]
#[path = "client_idle_unload_tests.rs"]
mod idle_unload_tests;

#[cfg(unix)]
#[tokio::test]
async fn queue_mutations_survive_daemon_restart_over_socket() {
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    let session_id = server.sessions.read().await.keys().next().unwrap().clone();
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
    let runtime2 = ServerRuntime::from_server(&server2);
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
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    let session_id = server.sessions.read().await.keys().next().unwrap().clone();

    // A wake turn the daemon starts on its own, like a swarm report.
    let swarm = crate::server::live_turn::LiveTurnSwarmContext::new(
        &server.swarm_state.members,
        &server.swarm_state.swarms_by_id,
        &server.event_history,
        &server.event_counter,
        &server.swarm_event_tx,
    );
    assert!(
        crate::server::live_turn::run_live_turn_if_idle(
            &session_id,
            "worker reported",
            None,
            &server.sessions,
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
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    let session_id = server.sessions.read().await.keys().next().unwrap().clone();
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
    let runtime2 = ServerRuntime::from_server(&server2);
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
        format!("{:?}", journal.message_at(skill_index + 1).expect("transcript message").content).contains("survive restart"),
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
    use crate::server::runtime::ServerRuntime;
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
        let runtime = ServerRuntime::from_server(&server);
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
            let session_id = server.sessions.read().await.keys().next().unwrap().clone();
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
        let session_id = server.sessions.read().await.keys().next().unwrap().clone();
        let journal = crate::session::Session::load(&session_id).unwrap();
        let skill = journal
            .iter_messages()
            .position(|m| {
                m.display_role == Some(crate::session::StoredDisplayRole::System)
                    && format!("{:?}", m.content).contains("Modes skill instructions")
            })
            .expect("skill entry");
        assert!(
            format!("{:?}", journal.message_at(skill + 1).expect("transcript message").content).contains("follow-up"),
            "{mode}"
        );
        drop(writer);
        drop(reader);
        runtime.shutdown().await;
        handle.await.unwrap();
    }
}

#[derive(Clone, Default)]
struct SdkCallbackProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct NeverReturningTool(Arc<tokio::sync::Notify>);

#[async_trait]
impl crate::tool::Tool for NeverReturningTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "A test tool that never returns"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[async_trait]
impl Provider for SdkCallbackProvider {
    async fn complete(
        &self,
        _: &[Message],
        tools: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read");
        assert_eq!(tools[0].description, "SDK read callback");
        let events = if self.calls.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            vec![
                StreamEvent::ToolUseStart {
                    id: "model-call".into(),
                    name: "read".into(),
                },
                StreamEvent::ToolInputDelta("{}".into()),
                StreamEvent::ToolUseEnd,
                StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                },
            ]
        } else {
            vec![
                StreamEvent::TextDelta("callback completed".into()),
                StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                },
            ]
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
    fn name(&self) -> &str {
        "sdk-callback-test"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

type CapturedImages = Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;

#[tokio::test]
async fn attachments_reach_the_provider_over_daemon_socket() {
    use crate::server::runtime::ServerRuntime;
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
    let runtime = ServerRuntime::from_server(&server);
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
    use crate::server::{reload_recovery, runtime::ServerRuntime};
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
    let runtime = ServerRuntime::from_server(&server);
    let daemon = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let recovery = reload_recovery::RecoveryOwner::new(&server);

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

        let agent = server.sessions.read().await.get(&id).unwrap().clone();
        {
            let agent = agent.lock().await;
            let last = agent.messages().last().unwrap();
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
            assert!(session.last_message().is_some_and(|last| {
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

#[derive(Clone, Default)]
struct TerminalSocketProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    messages: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    images: CapturedImages,
    first_started: Arc<tokio::sync::Notify>,
    finish_first: Arc<tokio::sync::Notify>,
    fail_catalog: Arc<std::sync::atomic::AtomicBool>,
    churn_catalog: Arc<std::sync::atomic::AtomicBool>,
    catalog_revision: Arc<std::sync::atomic::AtomicUsize>,
    ask_after_reattach: Arc<std::sync::atomic::AtomicBool>,
    steer_tool: Arc<std::sync::atomic::AtomicBool>,
    long_tool: Arc<std::sync::atomic::AtomicBool>,
    effort: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait]
impl Provider for TerminalSocketProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        self.images.lock().unwrap().push(
            messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Image { media_type, data } => {
                        Some((media_type.clone(), data.clone()))
                    }
                    _ => None,
                })
                .collect(),
        );
        self.messages.lock().unwrap().push(
            messages
                .iter()
                .filter(|message| message.role == Role::User)
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        );
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.finish_first.notified().await;
            if self.long_tool.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "long-tool".into(),
                        name: "bash".into(),
                    }),
                    Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"sleep 30","intent":"test interrupting a blocked tool"}"#
                            .into(),
                    )),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".into()),
                    }),
                ])));
            }
            if self.steer_tool.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "steer-tool".into(),
                        name: "bash".into(),
                    }),
                    Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"printf queue-boundary","intent":"test queue boundary"}"#
                            .into(),
                    )),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".into()),
                    }),
                ])));
            }
            if self.ask_after_reattach.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart { id: "ask-after-reattach".into(), name: "ask".into() }),
                    Ok(StreamEvent::ToolInputDelta(r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#.into())),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd { stop_reason: Some("tool_use".into()) }),
                ])));
            }
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::TextDelta("completed".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "terminal-socket-test"
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.effort.lock().unwrap().clone()
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        *self.effort.lock().unwrap() = Some(effort.to_owned());
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }

    fn available_models_display(&self) -> Vec<String> {
        assert!(
            !self.fail_catalog.load(Ordering::SeqCst),
            "injected catalog worker failure"
        );
        let mut models = vec![self.model()];
        if self.catalog_revision.load(Ordering::SeqCst) > 0 {
            models.push("fixture-future-model".into());
        }
        models
    }

    fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
        self.available_models_display()
            .into_iter()
            .map(|model| crate::provider::ModelRoute {
                speed_options: Vec::new(),
                reasoning_efforts: None,
                model,
                provider: "fixture".into(),
                api_method: "fixture".into(),
                available: true,
                detail: String::new(),
                usage: None,
                cheapness: None,
            })
            .collect()
    }

    async fn refresh_model_catalog(&self) -> Result<crate::provider::ModelCatalogRefreshSummary> {
        // Only explicit refresh may bypass the fresh cached fallback.
        self.catalog_revision.fetch_add(1, Ordering::SeqCst);
        Ok(crate::provider::ModelCatalogRefreshSummary::default())
    }

    fn catalog_sharing_identity(&self) -> Option<String> {
        if self.churn_catalog.load(Ordering::SeqCst) {
            self.catalog_revision.fetch_add(1, Ordering::SeqCst);
        }
        Some(format!(
            "terminal-socket/{:p}/{}",
            Arc::as_ptr(&self.calls),
            self.catalog_revision.load(Ordering::SeqCst)
        ))
    }
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

#[path = "client_lifecycle_socket_tests/desktop_capabilities.rs"]
mod desktop_capabilities;

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

async fn sdk_socket_workflow(response_only: bool, terminal_race: bool, identity_capability: bool) {
    sdk_socket_cancel_workflow(
        response_only,
        terminal_race,
        identity_capability,
        false,
        false,
        false,
        false,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn sdk_socket_cancel_workflow(
    response_only: bool,
    terminal_race: bool,
    identity_capability: bool,
    cancel_first: bool,
    idle_soft_interrupt: bool,
    catalog_failure: bool,
    ask_reattach: bool,
    queued_test: Option<&str>,
) {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let terminal_provider = TerminalSocketProvider::default();
    terminal_provider.ask_after_reattach.store(
        ask_reattach || queued_test == Some("force_stop"),
        Ordering::SeqCst,
    );
    terminal_provider
        .steer_tool
        .store(queued_test == Some("steer"), Ordering::SeqCst);
    terminal_provider
        .long_tool
        .store(queued_test == Some("force_stop_tool"), Ordering::SeqCst);
    let provider_template: Arc<dyn Provider> = if terminal_race || response_only {
        Arc::new(terminal_provider.clone())
    } else {
        Arc::new(SdkCallbackProvider::default())
    };

    let sessions: SessionAgents =
        Arc::new(crate::server::SessionManager::from_agents(HashMap::new()));
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let client_count = Arc::new(RwLock::new(0usize));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let shared_context = Arc::new(RwLock::new(HashMap::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::new()));
    let file_touch = FileTouchService::new();
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::new()));
    let client_debug_state = Arc::new(RwLock::new(ClientDebugState::default()));
    let (_debug_response_tx, _) = broadcast::channel(8);
    let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let (_global_event_tx, _) = broadcast::channel(8);
    let global_is_processing = Arc::new(RwLock::new(false));
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues: SessionInterruptQueues = Arc::new(RwLock::new(HashMap::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    let connect = || {
        let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
        let server_task = tokio::spawn(handle_client(
            server_stream,
            Arc::clone(&sessions),
            _global_event_tx.clone(),
            provider_template.clone(),
            global_is_processing.clone(),
            global_session_id.clone(),
            client_count.clone(),
            Arc::clone(&client_connections),
            swarm_members.clone(),
            swarms_by_id.clone(),
            shared_context.clone(),
            swarm_plans.clone(),
            swarm_coordinators.clone(),
            file_touch.clone(),
            channel_subscriptions.clone(),
            channel_subscriptions_by_session.clone(),
            client_debug_state.clone(),
            _debug_response_tx.clone(),
            event_history.clone(),
            event_counter.clone(),
            swarm_event_tx.clone(),
            "jcode-test".to_string(),
            "🧪".to_string(),
            mcp_pool.clone(),
            shutdown_signals.clone(),
            soft_interrupt_queues.clone(),
            AwaitMembersRuntime::default(),
            SwarmMutationRuntime::default(),
            None,
        ));
        (server_task, client_stream)
    };
    let (server_task, client_stream) = connect();

    let (client_reader, mut client_writer) = client_stream.into_split();
    let mut client_reader = BufReader::new(client_reader);

    use desktop_capabilities::{assert_desktop_capability_contract, send, until};
    use serde_json::json;
    send(
        &mut client_writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path(),"supports_turn_identity":identity_capability,"continue_on_disconnect":ask_reattach || queued_test == Some("reload")}),
    )
    .await;
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 1 })
    })
    .await;
    send(&mut client_writer, json!({"type":"get_history","id":99})).await;
    let saw_bootstrap = std::cell::Cell::new(false);
    until(&mut client_reader, |e| {
        if matches!(e, ServerEvent::TurnState { .. }) {
            saw_bootstrap.set(true);
        }
        matches!(e, ServerEvent::History { id: 99, .. })
    })
    .await;
    assert_eq!(
        saw_bootstrap.get(),
        identity_capability,
        "bootstrap must be negotiated"
    );
    {
        // A stored todo change pushes to the attached client without polling.
        let session_id = sessions.read().await.keys().next().unwrap().clone();
        let todos: Vec<crate::todo::TodoItem> = serde_json::from_value(json!([
            {"id":"pushed","content":"Pushed without a poll","priority":"high","status":"in_progress"}
        ]))
        .unwrap();
        crate::todo::save_todos(&session_id, &todos).unwrap();
        let pushed = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Todos { .. })
        })
        .await;
        assert!(
            matches!(&pushed, ServerEvent::Todos { session_id: id, payload, pushes: true }
                if *id == session_id && payload.contains("Pushed without a poll")),
            "unexpected todo push: {pushed:?}"
        );
        // Another session's write must not reach this client. Bus events are
        // delivered in order, so a leaked push would arrive before the push
        // for the write to this session that follows it.
        crate::todo::save_todos("some-other-session", &todos).unwrap();
        let second: Vec<crate::todo::TodoItem> = serde_json::from_value(json!([
            {"id":"pushed","content":"Second push","priority":"high","status":"completed"}
        ]))
        .unwrap();
        crate::todo::save_todos(&session_id, &second).unwrap();
        let next = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Todos { .. })
        })
        .await;
        assert!(
            matches!(&next, ServerEvent::Todos { session_id: id, payload, .. }
                if *id == session_id && payload.contains("Second push")),
            "expected only this session's second write, got: {next:?}"
        );
    }
    if catalog_failure {
        let session_id = sessions.read().await.keys().next().unwrap().clone();
        terminal_provider
            .catalog_revision
            .fetch_add(1, Ordering::SeqCst);
        terminal_provider.fail_catalog.store(true, Ordering::SeqCst);
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":100}),
        )
        .await;
        let error = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Error { id: 100, .. })
        })
        .await;
        assert!(matches!(error, ServerEvent::Error { message, .. }
            if message.contains("Could not refresh model catalog")));
        terminal_provider
            .fail_catalog
            .store(false, Ordering::SeqCst);
        terminal_provider
            .churn_catalog
            .store(true, Ordering::SeqCst);
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":102}),
        )
        .await;
        let error = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Error { id: 102, .. })
        })
        .await;
        assert!(matches!(error, ServerEvent::Error { message, .. }
            if message.contains("changed repeatedly")));
        terminal_provider
            .churn_catalog
            .store(false, Ordering::SeqCst);
        assert!(sessions.read().await.contains_key(&session_id));
        assert!(
            client_connections
                .read()
                .await
                .values()
                .any(|connection| connection.session_id == session_id)
        );
        // Reuse this exact socket, not a reconnect that could restore a closed
        // session and replay an interrupted cloud continuation. The workflow
        // below must still stream two fresh model turns from this owner.
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":101}),
        )
        .await;
        until(&mut client_reader, |event| {
            matches!(event, ServerEvent::History { id: 101, .. })
        })
        .await;
    }
    if identity_capability {
        let session_id = sessions.read().await.keys().next().unwrap().clone();
        let tool_started = Arc::new(tokio::sync::Notify::new());
        if queued_test == Some("force_stop_tool") {
            let agent = sessions.read().await.get(&session_id).unwrap().clone();
            let registry = agent.lock().await.registry();
            registry
                .register(
                    "bash".into(),
                    Arc::new(NeverReturningTool(tool_started.clone())),
                )
                .await;
        }
        let (legacy_task, legacy_stream) = connect();
        let (legacy_reader, mut legacy_writer) = legacy_stream.into_split();
        let mut legacy_reader = BufReader::new(legacy_reader);
        send(
            &mut legacy_writer,
            json!({"type":"subscribe","id":1,"target_session_id":session_id}),
        )
        .await;
        until(&mut legacy_reader, |event| {
            assert!(
                !matches!(
                    event,
                    ServerEvent::TurnState { .. }
                        | ServerEvent::TurnStarted { .. }
                        | ServerEvent::TurnEvent { .. }
                ),
                "legacy attach cannot receive identity protocol"
            );
            matches!(event, ServerEvent::Done { id: 1 })
        })
        .await;
        send(
            &mut client_writer,
            json!({"type":if idle_soft_interrupt { "soft_interrupt" } else { "message" },"id":2,"content":"one shared turn", "jarvis_voice":idle_soft_interrupt}),
        )
        .await;
        tokio::time::timeout(
            Duration::from_secs(5),
            terminal_provider.first_started.notified(),
        )
        .await
        .unwrap();
        if queued_test == Some("catalog") {
            // A second client explicitly refreshes the already-fresh catalog while
            // the long-lived session owns its agent lock. No daemon restart.
            send(
                &mut legacy_writer,
                json!({"type":"refresh_models", "id": 88}),
            )
            .await;
            until(&mut legacy_reader, |event| {
                matches!(event, ServerEvent::Done { id: 88 })
            })
            .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while terminal_provider.catalog_revision.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("explicit refresh must bypass the fixture's fresh cache");
            tokio::time::sleep(Duration::from_millis(1200)).await;
            terminal_provider.finish_first.notify_one();
            let event = until(&mut client_reader, |event| matches!(event,
                ServerEvent::AvailableModelsUpdated { available_models, available_model_routes, .. }
                if available_models.iter().any(|m| m == "fixture-future-model")
                    && available_model_routes.iter().any(|r| r.model == "fixture-future-model")
            )).await;
            assert!(matches!(event, ServerEvent::AvailableModelsUpdated { .. }));
            drop(client_writer);
            drop(client_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        if matches!(queued_test, Some("force_stop" | "force_stop_tool")) {
            terminal_provider.finish_first.notify_one();
            if queued_test == Some("force_stop") {
                until(&mut legacy_reader, |event| {
                    matches!(event, ServerEvent::AskUserRequest { .. })
                })
                .await;
            } else {
                until(
                    &mut legacy_reader,
                    |event| matches!(event, ServerEvent::ToolExec { name, .. } if name == "bash"),
                )
                .await;
                tool_started.notified().await;
                let agent = sessions.read().await.get(&session_id).unwrap().clone();
                assert!(agent.try_lock().is_err(), "turn must hold the agent lock");
            }
            let start = Instant::now();
            if queued_test == Some("force_stop_tool") {
                send(
                    &mut client_writer,
                    json!({"type":"force_stop_turn","id":77,"session_id":session_id}),
                )
                .await;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut client_reader, |event| {
                        matches!(event, ServerEvent::Ack { id: 77 })
                    }),
                )
                .await
                .expect("force stop on the owner socket must reply without the agent lock");
            } else {
                send(
                    &mut legacy_writer,
                    json!({"type":"force_stop_turn","id":77,"session_id":session_id}),
                )
                .await;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut legacy_reader, |event| {
                        matches!(event, ServerEvent::Ack { id: 77 })
                    }),
                )
                .await
                .expect("force stop on an attached socket must reply without the agent lock");
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            if queued_test == Some("force_stop_tool") {
                // The same connection must be able to submit immediately after
                // ACK, before the old turn's terminal frame is consumed.
                send(
                    &mut client_writer,
                    json!({"type":"message","id":78,"content":"next message"}),
                )
                .await;
            } else {
                send(
                    &mut legacy_writer,
                    json!({"type":"message","id":78,"content":"next message"}),
                )
                .await;
            }
            tokio::time::timeout(Duration::from_secs(1), until(&mut client_reader, |event| matches!(event, ServerEvent::TurnEvent { event, .. } if matches!(event.as_ref(), ServerEvent::Done { id: 2 })))).await.expect("force stop must release the turn promptly");
            if queued_test == Some("force_stop_tool") {
                tokio::time::timeout(Duration::from_secs(1), until(&mut client_reader, |event| matches!(event, ServerEvent::TurnEvent { event, .. } if matches!(event.as_ref(), ServerEvent::Done { id: 78 })))).await.expect("next turn on the owner connection must start promptly");
            } else {
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut legacy_reader, |event| {
                        matches!(event, ServerEvent::Done { id: 78 })
                    }),
                )
                .await
                .expect("next turn on a second connection must start promptly");
            }
            let history = crate::session::Session::load(&session_id).unwrap();
            assert!(
                serde_json::to_string(&history.cloned_messages())
                    .unwrap()
                    .contains("stopped by user")
            );
            drop(client_writer);
            drop(client_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        if ask_reattach {
            drop(client_writer);
            drop(client_reader);
            tokio::time::timeout(Duration::from_secs(5), async {
                while client_connections.read().await.len() > 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("old socket must detach while turn continues");
            let (new_task, new_stream) = connect();
            let (new_reader, mut new_writer) = new_stream.into_split();
            let mut new_reader = BufReader::new(new_reader);
            send(&mut new_writer, json!({"type":"subscribe","id":41,"target_session_id":session_id,"continue_on_disconnect":true})).await;
            until(&mut new_reader, |event| {
                matches!(event, ServerEvent::Done { id: 41 })
            })
            .await;
            terminal_provider.finish_first.notify_one();
            let ServerEvent::AskUserRequest { request_id, .. } = until(&mut new_reader, |event| {
                matches!(event, ServerEvent::AskUserRequest { .. })
            })
            .await
            else {
                unreachable!()
            };
            let peer_ask = tokio::time::timeout(
                Duration::from_secs(2),
                until(&mut legacy_reader, |event| {
                    matches!(event, ServerEvent::AskUserRequest { .. })
                }),
            )
            .await
            .expect("already-attached peer must receive ask");
            assert!(
                matches!(peer_ask, ServerEvent::AskUserRequest { request_id: ref peer_id, .. } if peer_id == &request_id)
            );
            // The question is already on screen when Desktop exits. Its response
            // sender must remain owned by the session, not by this socket.
            drop(new_writer);
            drop(new_reader);
            tokio::time::timeout(Duration::from_secs(5), async {
                while client_connections.read().await.len() > 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("question socket must detach");
            let (reopened_task, reopened_stream) = connect();
            let (reopened_reader, mut reopened_writer) = reopened_stream.into_split();
            let mut reopened_reader = BufReader::new(reopened_reader);
            send(&mut reopened_writer, json!({"type":"subscribe","id":43,"target_session_id":session_id,"continue_on_disconnect":true})).await;
            let replay = until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::AskUserRequest { .. })
            })
            .await;
            assert!(
                matches!(replay, ServerEvent::AskUserRequest { request_id: ref replayed, .. } if replayed == &request_id)
            );
            send(
                &mut reopened_writer,
                json!({"type":"ask_user_response","id":42,"request_id":request_id,"response":{"answers":[{"selected":["B"],"other":"custom [#image 1]","comments":{"B":"reason [#image 1]"},"other_images":[["image/png","b3RoZXI="]],"comment_images":{"B":[["image/jpeg","Y29tbWVudA=="]]}}]}}),
            )
            .await;
            until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::Done { id: 42 })
            })
            .await;
            tokio::time::timeout(
                Duration::from_secs(2),
                until(
                    &mut legacy_reader,
                    |event| matches!(event, ServerEvent::ToolDone { name, .. } if name == "ask"),
                ),
            )
            .await
            .expect("peer must receive completion of answered ask");
            until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::Done { id: 2 })
            })
            .await;
            send(&mut reopened_writer, json!({"type":"get_history","id":44})).await;
            let history = until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::History { .. })
            })
            .await;
            let history = serde_json::to_string(&history).unwrap();
            assert!(
                history.contains("comment: reason") && history.contains("Other: custom"),
                "{history}"
            );
            assert!(
                history.contains("b3RoZXI=") && history.contains("Y29tbWVudA=="),
                "{history}"
            );
            let reloaded = crate::session::Session::load(&session_id).unwrap();
            let blocks: Vec<_> = reloaded
                .iter_messages()
                .flat_map(|message| &message.content)
                .collect();
            assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/png" && data == "b3RoZXI=")));
            assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/jpeg" && data == "Y29tbWVudA==")));
            assert_eq!(crate::session::render_images(&reloaded).len(), 2);
            let model_images = terminal_provider.images.lock().unwrap();
            let last = model_images.last().expect("provider resumed after ask");
            assert!(last.contains(&("image/png".into(), "b3RoZXI=".into())));
            assert!(last.contains(&("image/jpeg".into(), "Y29tbWVudA==".into())));
            drop(model_images);
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
            sessions.clear_for_test().await;
            drop(reopened_writer);
            drop(reopened_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            for task in [server_task, legacy_task, new_task, reopened_task] {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
            return;
        }
        if idle_soft_interrupt {
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::SoftInterruptInjected { content, point, client_ids, display_role, .. }
                    if display_role.as_deref() == Some("jarvis_user") && content == "one shared turn" && point == "turn_start" && client_ids.is_empty())
            })
            .await;
        }
        if queued_test == Some("effort") {
            send(
                &mut client_writer,
                json!({"type":"set_reasoning_effort","id":49,"effort":"high"}),
            )
            .await;
            let event = tokio::time::timeout(
                Duration::from_secs(2),
                until(&mut client_reader, |event| {
                    matches!(event, ServerEvent::ReasoningEffortChanged { id: 49, .. })
                }),
            )
            .await
            .expect("effort must be confirmed during the active turn");
            assert!(
                matches!(event, ServerEvent::ReasoningEffortChanged { effort: Some(ref effort), error: None, .. } if effort == "high")
            );
            assert_eq!(
                terminal_provider.reasoning_effort().as_deref(),
                Some("high")
            );
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 1);
        }
        if let Some(mode) = queued_test.filter(|mode| *mode != "effort") {
            let queue_mode = if mode == "steer" {
                "steer"
            } else {
                "after_yield"
            };
            let request = json!({"type":"queue_prompt","id":51,"client_id":"follow-up-1","mode":queue_mode,"content":"follow-up"});
            send(&mut client_writer, request.clone()).await;
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::Ack { id: 51 })
            })
            .await;
            // Retrying the same id after an uncertain acknowledgment is idempotent.
            send(&mut client_writer, request).await;
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::Ack { id: 51 })
            })
            .await;
            send(
                &mut client_writer,
                json!({"type":"get_queued_prompts","id":52}),
            )
            .await;
            let snapshot = until(&mut client_reader, |event| {
                matches!(event, ServerEvent::QueuedPrompts { id: 52, .. })
            })
            .await;
            assert!(
                matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. } if prompts.len() == 1 && prompts[0].content == "follow-up")
            );
            assert_eq!(
                terminal_provider.calls.load(Ordering::SeqCst),
                1,
                "queued input must wait while provider is active"
            );
            if mode == "reload" {
                let (new_task, new_stream) = connect();
                let (new_reader, mut new_writer) = new_stream.into_split();
                let mut new_reader = BufReader::new(new_reader);
                send(&mut new_writer, json!({"type":"subscribe","id":60,"target_session_id":session_id,"continue_on_disconnect":true})).await;
                until(&mut new_reader, |event| {
                    matches!(event, ServerEvent::Done { id: 60 })
                })
                .await;
                drop(client_writer);
                drop(client_reader);
                send(
                    &mut new_writer,
                    json!({"type":"get_queued_prompts","id":61}),
                )
                .await;
                let snapshot = until(&mut new_reader, |event| {
                    matches!(event, ServerEvent::QueuedPrompts { id: 61, .. })
                })
                .await;
                assert!(
                    matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. } if prompts.len() == 1 && prompts[0].client_id == "follow-up-1")
                );
                terminal_provider.finish_first.notify_one();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while terminal_provider.calls.load(Ordering::SeqCst) < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("follow-up must run after reattachment");
                assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
                let messages = terminal_provider.messages.lock().unwrap().clone();
                assert_eq!(
                    messages[1]
                        .iter()
                        .filter(|text| text.ends_with("] follow-up"))
                        .count(),
                    1,
                    "captured messages: {messages:?}"
                );
                drop(new_writer);
                drop(new_reader);
                drop(legacy_writer);
                drop(legacy_reader);
                server_task.abort();
                legacy_task.abort();
                new_task.abort();
                return;
            }
            if mode == "interrupt" {
                send(
                    &mut client_writer,
                    json!({"type":"interrupt_queued_prompt","id":53,"client_id":"follow-up-1"}),
                )
                .await;
                until(&mut client_reader, |event| {
                    matches!(event, ServerEvent::Ack { id: 53 })
                })
                .await;
            } else {
                terminal_provider.finish_first.notify_one();
                if mode == "steer" {
                    // A real tool completes before steering is delivered.
                    until(&mut client_reader, |event| match event {
                        ServerEvent::SoftInterruptInjected { point, .. } => point == "C" || point == "D",
                        ServerEvent::TurnEvent { event, .. } => matches!(event.as_ref(), ServerEvent::SoftInterruptInjected { point, .. } if point == "C" || point == "D"),
                        _ => false,
                    }).await;
                }
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while terminal_provider.calls.load(Ordering::SeqCst) < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("queued prompt must reach the model");
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
            let messages = terminal_provider.messages.lock().unwrap().clone();
            assert_eq!(
                messages[1]
                    .iter()
                    .filter(|text| text.ends_with("] follow-up"))
                    .count(),
                1,
                "captured messages: {messages:?}"
            );
            return;
        }
        if cancel_first {
            send(&mut client_writer, json!({"type":"cancel","id":3})).await;
        } else {
            terminal_provider.finish_first.notify_one();
        }
        let mut interrupted = false;
        let mut active = None;
        let mut scoped_text = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert!(client_reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                match event {
                    ServerEvent::TurnStarted { identity } => {
                        assert!(active.replace(identity).is_none());
                    }
                    ServerEvent::TurnEvent {
                        identity, event, ..
                    } => {
                        if (idle_soft_interrupt || queued_test == Some("effort"))
                            && active.is_none()
                        {
                            // The pickup wait may already have consumed the
                            // asynchronous TurnStarted notification.
                            active = Some(identity.clone());
                        }
                        assert_eq!(
                            active.as_ref(),
                            Some(&identity),
                            "every emission belongs to accepted runtime turn"
                        );
                        match *event {
                            ServerEvent::TextDelta { ref text } if text == "completed" => {
                                scoped_text = true
                            }
                            ServerEvent::Interrupted => {
                                interrupted = true;
                            }
                            ServerEvent::Done { id: 2 } => {
                                if !cancel_first || interrupted {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    ServerEvent::TextDelta { .. }
                    | ServerEvent::Done { id: 2 }
                    | ServerEvent::Error { id: 2, .. } => {
                        panic!("identity client received unscoped turn output")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        if cancel_first {
            assert!(
                interrupted,
                "cancel completion must be scoped, not lost until the UI timeout"
            );
            send(
                &mut client_writer,
                json!({"type":"message","id":4,"content":"next prompt"}),
            )
            .await;
            until(&mut client_reader, |event| match event {
                ServerEvent::TurnEvent {
                    identity, event, ..
                } => {
                    assert_ne!(Some(identity), active.as_ref());
                    assert!(!matches!(event.as_ref(), ServerEvent::Error { .. }));
                    matches!(event.as_ref(), ServerEvent::Done { id: 4 })
                }
                _ => false,
            })
            .await;
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(scoped_text);
        }
        let legacy_text = std::cell::Cell::new(false);
        if !cancel_first {
            until(&mut legacy_reader, |event| {
                assert!(
                    !matches!(
                        event,
                        ServerEvent::TurnState { .. }
                            | ServerEvent::TurnStarted { .. }
                            | ServerEvent::TurnEvent { .. }
                    ),
                    "legacy observer receives flat events only"
                );
                if matches!(event, ServerEvent::TextDelta { text } if text == "completed") {
                    legacy_text.set(true);
                }
                matches!(event, ServerEvent::Done { id: 2 })
            })
            .await;
            assert!(legacy_text.get());
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 1);
        }
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        drop(legacy_writer);
        drop(legacy_reader);
        for task in [server_task, legacy_task] {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        return;
    }
    if terminal_race {
        send(
            &mut client_writer,
            json!({"type":"message","id":2,"content":"first turn"}),
        )
        .await;
        tokio::time::timeout(
            Duration::from_secs(5),
            terminal_provider.first_started.notified(),
        )
        .await
        .unwrap();
        // Park the actor inside GetState, independently of the processing task
        // and event fanout. Its Ack proves it has already selected this request.
        let sessions_guard = sessions.write_for_test().await;
        send(&mut client_writer, json!({"type":"state","id":3})).await;
        until(&mut client_reader, |e| {
            matches!(e, ServerEvent::Ack { id: 3 })
        })
        .await;
        terminal_provider.finish_first.notify_one();
        until(&mut client_reader, |e| {
            matches!(e, ServerEvent::Done { id: 2 })
        })
        .await;
        // Queue input while the actor cannot consume completion. On release,
        // both channels are ready and biased select must choose Message first.
        send(
            &mut client_writer,
            json!({"type":"message","id":4,"content":"acceptance back on laptop"}),
        )
        .await;
        drop(sessions_guard);
        let terminal = until(&mut client_reader, |e| {
            matches!(
                e,
                ServerEvent::Done { id: 4 } | ServerEvent::Error { id: 4, .. }
            )
        })
        .await;
        assert!(
            matches!(terminal, ServerEvent::Done { id: 4 }),
            "next prompt must reach model: {terminal:?}"
        );
        assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        return;
    }
    if response_only {
        assert_desktop_capability_contract(
            &mut client_writer,
            &mut client_reader,
            &sessions,
            &terminal_provider,
            home._home.path(),
        )
        .await;
        // This test exercises tool availability, not the 30-second retention of
        // idle unsaved sessions. Remove the isolated fixture before detaching.
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        return;
    }
    // A catalog/startup task can hold the Agent without an active turn. Tool
    // registration must wait for that work, not return a spurious busy error.
    let startup_agent = sessions.read().await.values().next().unwrap().clone();
    let startup_guard = startup_agent.lock_owned().await;
    let release_startup = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(startup_guard);
    });
    send(&mut client_writer, json!({"type":"configure_tools","id":2,"tools":{"enabled":[],"custom":[{"name":"read","description":"SDK read callback","parameters":{"type":"object"}}]}})).await;
    let configured = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 2 } | ServerEvent::Error { id: 2, .. }
        )
    })
    .await;
    release_startup.await.unwrap();
    assert!(
        matches!(configured, ServerEvent::Ack { .. }),
        "{configured:?}"
    );
    send(&mut client_writer, json!({"type":"list_tools","id":3})).await;
    let ServerEvent::Tools { tools, .. } = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Tools { id: 3, .. } | ServerEvent::Error { id: 3, .. }
        )
    })
    .await
    else {
        panic!("expected tools")
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "read");
    send(
        &mut client_writer,
        json!({"type":"message","id":4,"content":"call read"}),
    )
    .await;
    let ServerEvent::ToolCall { call_id, name, .. } = until(&mut client_reader, |e| {
        matches!(e, ServerEvent::ToolCall { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(name, "read");
    send(
        &mut client_writer,
        json!({"type":"configure_tools","id":5,"tools":{}}),
    )
    .await;
    let busy = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 5 } | ServerEvent::Error { id: 5, .. }
        )
    })
    .await;
    assert!(
        matches!(busy, ServerEvent::Error { .. }),
        "must not ack invalid/busy config: {busy:?}"
    );
    send(
        &mut client_writer,
        json!({"type":"tool_result","id":6,"call_id":call_id,"output":"SDK read output"}),
    )
    .await;
    let acknowledged = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 6 } | ServerEvent::Error { id: 6, .. }
        )
    })
    .await;
    assert!(
        matches!(acknowledged, ServerEvent::Ack { .. }),
        "{acknowledged:?}"
    );
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 4 })
    })
    .await;

    let session_a = sessions.read().await.keys().next().unwrap().clone();
    let (observer_task, observer_stream) = connect();
    let (observer_reader, mut observer_writer) = observer_stream.into_split();
    let mut observer_reader = BufReader::new(observer_reader);
    send(&mut observer_writer, json!({"type":"subscribe","id":20,"working_dir":home._home.path(),"target_session_id":session_a})).await;
    until(&mut observer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 20 })
    })
    .await;

    // Explicit detach releases SDK ownership even while the socket stays open.
    send(
        &mut client_writer,
        json!({"type":"prepare_disconnect","id":21}),
    )
    .await;
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 21 })
    })
    .await;
    let tools = json!({"enabled":[],"custom":[{"name":"read","description":"SDK read callback","parameters":{"type":"object"}}]});
    send(
        &mut observer_writer,
        json!({"type":"configure_tools","id":22,"tools":tools}),
    )
    .await;
    let ack = until(&mut observer_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 22 } | ServerEvent::Error { id: 22, .. }
        )
    })
    .await;
    assert!(
        matches!(ack, ServerEvent::Ack { .. }),
        "detach must release owner: {ack:?}"
    );

    // Switching the owner to B must leave A unavailable, not route A's
    // callback to B. The first client keeps A's Agent alive throughout.
    let mut other_session =
        crate::session::Session::create_with_id("sdk_other_session".into(), None, None);
    other_session.apply_workspace(crate::session::Workspace {
        working_dir: Some(home._home.path().to_string_lossy().into_owned()),
        ..other_session.workspace()
    });
    // An existing session, not an empty panel: empty untitled sessions are
    // never written to disk and get a 30s reconnect grace on disconnect.
    other_session.apply_titles(crate::session::Titles {
        title: Some("sdk other session".into()),
        ..other_session.titles()
    });
    for index in 0..8 {
        other_session.append_stored_message(crate::session::StoredMessage {
            codemode: None,
            attachment_data: None,
            id: format!("history-{index}"),
            role: if index % 2 == 0 {
                crate::message::Role::User
            } else {
                crate::message::Role::Assistant
            },
            content: vec![ContentBlock::Text {
                text: format!("history {index}"),
                cache_control: None,
            }]
            .into(),
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    other_session.save().unwrap();
    assert!(crate::session::session_exists("sdk_other_session"));
    let other_registry = Registry::new(provider_template.clone()).await;
    sessions.write_for_test().await.insert(
        "sdk_other_session".into(),
        Arc::new(Mutex::new(Agent::new_with_session(
            provider_template.clone(),
            other_registry,
            other_session,
            None,
        ))),
    );
    if identity_capability && terminal_race {
        // Exercise actual dispatch, not just the history handlers. A subsequent
        // Subscribe is authoritative and clears an earlier opt-in for legacy clients.
        let (history_task, history_stream) = connect();
        let (reader, mut writer) = history_stream.into_split();
        let mut reader = BufReader::new(reader);
        for recent in [None, Some(2), None, Some(3)] {
            let mut subscribe =
                json!({"type":"subscribe","id":101,"working_dir":home._home.path()});
            if let Some(size) = recent {
                // Deliberately stale prefix state must not leak into later requests.
                subscribe["history_base"] =
                    json!({"count":4,"fingerprint":123,"recent_messages":size});
            }
            send(&mut writer, subscribe).await;
            until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 101 })).await;
            send(
                &mut writer,
                json!({"type":"resume_session","id":102,"session_id":"sdk_other_session"}),
            )
            .await;
            let event = until(&mut reader, |e| {
                matches!(e, ServerEvent::History { id: 102, .. })
            })
            .await;
            let expected = recent.map_or(8, |size| size + 1);
            let ServerEvent::History {
                messages,
                messages_base_count,
                ..
            } = event
            else {
                unreachable!()
            };
            assert_eq!(messages.len(), expected, "resume recent={recent:?}");
            assert_eq!(messages_base_count, None);
            assert_eq!(messages.last().unwrap().content, "history 7");
            until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 102 })).await;
            send(&mut writer, json!({"type":"get_history","id":103})).await;
            let event = until(&mut reader, |e| {
                matches!(e, ServerEvent::History { id: 103, .. })
            })
            .await;
            let ServerEvent::History {
                messages,
                messages_base_count,
                ..
            } = event
            else {
                unreachable!()
            };
            assert_eq!(messages.len(), expected, "get_history recent={recent:?}");
            assert_eq!(messages_base_count, None);
        }
        drop(writer);
        drop(reader);
        tokio::time::timeout(Duration::from_secs(10), history_task)
            .await
            .expect("history client disconnect")
            .unwrap()
            .unwrap();
    }
    send(
        &mut observer_writer,
        json!({"type":"resume_session","id":23,"session_id":"sdk_other_session"}),
    )
    .await;
    until(&mut observer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 23 })
    })
    .await;
    send(
        &mut client_writer,
        json!({"type":"message","id":24,"content":"call read after owner switch"}),
    )
    .await;
    until(&mut client_reader, |e| {
        assert!(
            !matches!(e, ServerEvent::ToolCall { .. }),
            "disconnected owner must not receive callback"
        );
        matches!(e, ServerEvent::Done { id: 24 })
    })
    .await;
    send(&mut observer_writer, json!({"type":"list_tools","id":25})).await;
    until(&mut observer_reader, |e| {
        assert!(
            !matches!(e, ServerEvent::ToolCall { .. }),
            "A callback must never reach B's owner connection"
        );
        matches!(e, ServerEvent::Tools { id: 25, .. })
    })
    .await;

    // A new owner can configure retained A after the prior owner switches.
    send(
        &mut client_writer,
        json!({"type":"configure_tools","id":26,"tools":tools}),
    )
    .await;
    let ack = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 26 } | ServerEvent::Error { id: 26, .. }
        )
    })
    .await;
    assert!(
        matches!(ack, ServerEvent::Ack { .. }),
        "session switch must release owner: {ack:?}"
    );
    send(
        &mut client_writer,
        json!({"type":"message","id":7,"content":"call read again"}),
    )
    .await;
    let event = until(&mut client_reader, |e| {
        matches!(e, ServerEvent::ToolCall { .. })
    })
    .await;
    assert!(matches!(event, ServerEvent::ToolCall { session_id, .. } if session_id == session_a));
    drop(observer_writer);
    drop(observer_reader);
    tokio::time::timeout(Duration::from_secs(5), observer_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Disconnect with a pending callback must not wait for the 120s deadline.
    drop(client_writer);
    drop(client_reader);
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
