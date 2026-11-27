//! Which switches wait for a turn, and that a switch the user made never reaches the chat.
use super::*;
use crate::message::{ContentBlock, Role};

async fn after_first_prompt(agent: &Arc<Mutex<Agent>>) {
    agent.lock().await.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first prompt".into(),
            cache_control: None,
        }],
    );
}

async fn chat_setting_rows(agent: &Arc<Mutex<Agent>>) -> Vec<String> {
    agent
        .lock()
        .await
        .get_history()
        .into_iter()
        .filter(|row| row.role == "setting_change")
        .map(|row| row.content)
        .collect()
}

#[tokio::test]
async fn a_held_agent_lock_without_a_running_turn_applies_the_switch_now() {
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedRuntimeDir::new();
    let (provider, agent, tx, mut rx) = test_agent("session_attach_set_model").await;
    after_first_prompt(&agent).await;

    // Attach and history building hold the agent briefly; no turn runs.
    let holder = Arc::clone(&agent);
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _held = holder.lock().await;
        let _ = locked_tx.send(());
        tokio::time::sleep(Duration::from_millis(150)).await;
    });
    locked_rx.await.unwrap();

    handle_set_model(5, "test-model-b".to_string(), false, &agent, &tx).await;

    assert_eq!(provider.model(), "test-model-b");
    assert!(
        matches!(rx.try_recv(), Ok(ServerEvent::ModelChanged { id: 5, .. })),
        "no 'queued' answer when no turn runs"
    );
    assert!(chat_setting_rows(&agent).await.is_empty());
}

#[tokio::test]
async fn a_switch_deferred_behind_a_running_turn_applies_at_its_end_without_a_chat_row() {
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedRuntimeDir::new();
    let (provider, agent, tx, mut rx) = test_agent("session_turn_set_model").await;
    after_first_prompt(&agent).await;
    let turn = agent.lock().await;

    handle_set_model(6, "test-model-b".to_string(), true, &agent, &tx).await;
    assert!(matches!(
        rx.try_recv(),
        Ok(ServerEvent::ModelChangePending { id: 6, .. })
    ));
    assert_eq!(provider.model(), "test-model-a");

    drop(turn);
    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("the switch applies when the turn ends");
    assert!(matches!(
        event,
        Some(ServerEvent::ModelChanged { id: 6, .. })
    ));
    assert_eq!(provider.model(), "test-model-b");
    assert!(chat_setting_rows(&agent).await.is_empty());
}
