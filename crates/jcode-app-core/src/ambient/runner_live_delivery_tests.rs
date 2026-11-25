use super::*;
use crate::protocol::ServerEvent;
use crate::server::{Client, Server};

// Keep the environment alive until the entire Tokio runtime has been dropped,
// including the daemon's background tasks. No shared daemon or credentials are used.
#[test]
fn scheduled_live_delivery_reaches_subscribed_client() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("isolated server directory");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let _runtime_dir = EnvVarGuard::set_path("JCODE_RUNTIME_DIR", temp.path());
    let socket = temp.path().join("schedule.sock");
    let _socket = EnvVarGuard::set_path("JCODE_SOCKET", &socket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            let streaming = StreamingTestProvider::default();
            streaming.queue_response(vec![
                StreamEvent::TextDelta("Scheduled live output.".to_string()),
                StreamEvent::MessageEnd { stop_reason: None },
            ]);
            let provider: Arc<dyn Provider> = Arc::new(streaming.clone());
            let server = Server::new_with_paths(
                provider.clone(),
                socket.clone(),
                temp.path().join("schedule-debug.sock"),
            );
            let server_task = tokio::spawn(async move { server.run().await });
            let mut attached = loop {
                if let Ok(client) = Client::connect_with_path(socket.clone()).await {
                    break client;
                }
                assert!(
                    !server_task.is_finished(),
                    "isolated server exited at startup"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let mut session = Session::create(None, Some("live schedule regression".to_string()));
            session.set_working_dir(Some(temp.path().display().to_string()));
            assert!(session.ensure_initial_session_context_message());
            session.save().expect("save target session");
            let subscribe_id = attached
                .subscribe_with_info(
                    Some(temp.path().display().to_string()),
                    Some(false),
                    Some(session.id().to_owned()),
                    false,
                    false,
                )
                .await
                .expect("subscribe observer");
            AmbientRunnerHandle::wait_for_request_done(&mut attached, subscribe_id)
                .await
                .expect("observer subscription ready");
            let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
            let item = ScheduledItem {
                id: "scheduled-live-regression".to_string(),
                scheduled_for: chrono::Utc::now(),
                context: "Report scheduled live output".to_string(),
                priority: Priority::Normal,
                target: ScheduleTarget::Session {
                    session_id: session.id().to_owned(),
                },
                created_by_session: session.id().to_owned(),
                created_at: chrono::Utc::now(),
                working_dir: Some(temp.path().display().to_string()),
                task_description: None,
                relevant_files: vec![],
                git_branch: None,
                additional_context: None,
            };
            runner
                .deliver_scheduled_direct_item(&provider, &item)
                .await
                .expect("deliver scheduled task");
            let mut streamed = String::new();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match attached.read_event().await.expect("attached client event") {
                        ServerEvent::TextDelta { text } => {
                            streamed.push_str(&text);
                        }
                        ServerEvent::Done { id: 0 } => break,
                        ServerEvent::Error { message, .. } => panic!("live turn error: {message}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("scheduled output and Done must reach the original attachment");
            assert_eq!(
                streamed, "Scheduled live output.",
                "live attachment must receive the scheduled response"
            );
            assert_eq!(
                streaming.supplied_tools.lock().unwrap().len(),
                1,
                "only the scheduled turn may call the provider"
            );
            let error = runner
                .notify_live_session("missing-scheduled-target", "not delivered")
                .await
                .expect_err("unknown target must remain an error");
            assert!(
                error.to_string().contains("not currently live"),
                "{error:#}"
            );
            drop(attached);
            server_task.abort();
            let _ = server_task.await;
        })
        .await
        .expect("isolated scheduled delivery test deadline");
    });
}
