use super::*;

#[cfg(unix)]
#[tokio::test]
async fn interrupt_queued_batch_delivers_one_turn_promptly() {
    use crate::server::runtime::ServerRuntime;
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    for (name, body) in [
        ("batch-one", "First skill instructions"),
        ("batch-two", "Second skill instructions"),
    ] {
        let dir = home._home.path().join(".jcode/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\n\n{body}\n"),
        )
        .unwrap();
    }
    let provider = TerminalSocketProvider::default();
    provider.long_tool.store(true, Ordering::SeqCst);
    let socket = home._runtime.path().join("batch-interrupt.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("batch-debug.sock"),
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
        tokio::time::timeout(Duration::from_secs(2), async {
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
        json!({"type":"message","id":2,"content":"busy"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), provider.first_started.notified())
        .await
        .unwrap();
    provider.finish_first.notify_one();
    until(
        &mut reader,
        |e| matches!(e, ServerEvent::ToolStart { name, .. } if name == "bash"),
    )
    .await;
    for (id, client_id, content, skill) in [
        (3, "a", "first queued", "batch-one"),
        (4, "b", "second queued", "batch-two"),
    ] {
        send(&mut writer, json!({"type":"queue_prompt","id":id,"client_id":client_id,"content":content,"mode":"after_yield","active_skill":skill})).await;
        until(
            &mut reader,
            |e| matches!(e, ServerEvent::Ack { id: n } if *n == id),
        )
        .await;
    }
    let session_id = server.sessions.read().await.keys().next().unwrap().clone();
    let (peer_reader, mut peer_writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut peer_reader = BufReader::new(peer_reader);
    send(
        &mut peer_writer,
        json!({"type":"subscribe","id":10,"target_session_id":session_id}),
    )
    .await;
    until(&mut peer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 10 })
    })
    .await;
    send(
        &mut peer_writer,
        json!({"type":"interrupt_queued_prompts","id":5,"ids":["a","b"]}),
    )
    .await;
    until(&mut peer_reader, |e| matches!(e, ServerEvent::SoftInterruptInjected { client_ids, .. } if client_ids == &["a", "b"])).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while provider.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("batch turn was not started promptly");
    let messages = provider.messages.lock().unwrap().clone();
    assert_eq!(messages.len(), 2, "{messages:?}");
    let journal = crate::session::Session::load(&session_id).unwrap();
    assert_eq!(journal.pinned_skill.as_deref(), Some("batch-two"));
    let contents: Vec<_> = journal
        .messages
        .iter()
        .map(|message| (message.display_role, format!("{:?}", message.content)))
        .collect();
    let first = contents
        .iter()
        .position(|(role, text)| {
            *role == Some(crate::session::StoredDisplayRole::System)
                && text.contains("First skill instructions")
        })
        .unwrap();
    assert!(
        contents[first + 1].1.contains("first queued"),
        "{contents:?}"
    );
    assert!(
        contents[first + 2].0 == Some(crate::session::StoredDisplayRole::System)
            && contents[first + 2].1.contains("Second skill instructions"),
        "{contents:?}"
    );
    assert!(
        contents[first + 3].1.contains("second queued"),
        "{contents:?}"
    );
    drop(writer);
    drop(reader);
    drop(peer_writer);
    drop(peer_reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}
