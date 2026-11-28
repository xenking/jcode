//! A one-shot client asks this daemon to renew a provider login it owns.
use super::*;

pub(in super::super) async fn handle_refresh_auth(
    id: u64,
    provider: String,
    account_label: Option<String>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let result = match provider.as_str() {
        "claude" | "anthropic" => {
            crate::auth::oauth::refresh_claude_login_for_client(account_label.as_deref()).await
        }
        other => Err(anyhow::anyhow!(
            "Unsupported auth refresh provider: {other}"
        )),
    };
    let _ = client_event_tx.send(match result {
        Ok(()) => ServerEvent::Done { id },
        Err(error) => ServerEvent::Error {
            id,
            message: format!("{error:#}"),
            retry_after_secs: None,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_an_error_instead_of_done_when_it_cannot_renew() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // A test process never owns the login, so it must not claim success.
        handle_refresh_auth(71, "claude".into(), None, &tx).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::Error { id: 71, .. })
        ));
        handle_refresh_auth(72, "no-such-provider".into(), None, &tx).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::Error { id: 72, .. })
        ));
    }
}
