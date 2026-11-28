//! With `[routing] mode = "auto"` the service alone moves a session between
//! two saved logins when one is spent: one switch, then every request on the
//! second, whatever the client asked of server failover.

use super::*;
use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider, RouteSelection};
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

/// Two OAuth logins of one provider. `spent` answers every request with a
/// usage limit; a session serves the login it was bound to, else `global`.
struct Logins {
    global: &'static str,
    spent: &'static str,
    bound: StdMutex<Option<String>>,
    served: Arc<StdMutex<Vec<String>>>,
}

impl Logins {
    fn serving(&self) -> String {
        self.bound
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.global.to_string())
    }
}

#[async_trait]
impl Provider for Logins {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        let login = self.serving();
        self.served.lock().unwrap().push(login.clone());
        if login == self.spent {
            anyhow::bail!("HTTP 429: usage_limit_reached for this account");
        }
        let events = vec![
            Ok(StreamEvent::TextDelta(format!("served by {login}"))),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        "Claude"
    }

    fn model(&self) -> String {
        "claude-sonnet-4-6".into()
    }

    fn oauth_account_label(&self) -> Option<String> {
        self.bound.lock().unwrap().clone()
    }

    fn set_oauth_account(&self, label: Option<&str>) -> anyhow::Result<()> {
        *self.bound.lock().unwrap() = label.map(str::to_string);
        Ok(())
    }

    fn serving_oauth_account_label(&self) -> Option<String> {
        Some(self.serving())
    }

    fn set_route_selection(&self, selection: &RouteSelection) -> anyhow::Result<()> {
        *self.bound.lock().unwrap() =
            jcode_provider_core::oauth_account_label(&selection.api_method).map(str::to_string);
        Ok(())
    }

    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            "title".to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                requested_model: None,
                resolved_model: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        ))
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Logins {
            global: self.global,
            spent: self.spent,
            bound: StdMutex::new(self.bound.lock().unwrap().clone()),
            served: self.served.clone(),
        })
    }
}

#[test]
fn a_spent_login_moves_the_session_once_and_every_later_request_follows() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            std::fs::write(
                home.path().join("config.toml"),
                "[routing]\nmode = \"auto\"\n\
                 [[routing.accounts]]\nid = \"claude:primary\"\npriority = 1\n\
                 [[routing.accounts]]\nid = \"claude:spare\"\npriority = 2\n",
            )
            .unwrap();
            crate::config::Config::invalidate_cache();

            let served = Arc::new(StdMutex::new(Vec::new()));
            let provider: Arc<dyn Provider> = Arc::new(Logins {
                global: "primary",
                spent: "primary",
                bound: StdMutex::new(None),
                served: served.clone(),
            });
            let registry = crate::tool::Registry::new(Arc::clone(&provider)).await;
            let session =
                crate::session::Session::create_with_id("session_one_decider".into(), None, None);
            let agent = Arc::new(Mutex::new(Agent::new_with_session(
                provider, registry, session, None,
            )));
            let (tx, mut rx) = mpsc::unbounded_channel();

            // The client did not ask for server failover: routing alone retries.
            for turn in ["first", "second"] {
                process_message_with_server_failover(
                    Arc::clone(&agent),
                    turn,
                    Vec::new(),
                    None,
                    tx.clone(),
                    false,
                )
                .await
                .unwrap_or_else(|error| panic!("{turn} turn: {error:#}"));
            }

            let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            match prev_home {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();

            let turn_requests: Vec<_> = served.lock().unwrap().iter().take(3).cloned().collect();
            assert_eq!(
                turn_requests,
                ["primary", "spare", "spare"],
                "one request to the spent login, then every request on the second"
            );
            let moves = events
                .iter()
                .filter(|event| matches!(event, ServerEvent::ModelChanged { error: None, .. }))
                .count();
            assert_eq!(moves, 1, "exactly one switch: {events:?}");
            let decided: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    ServerEvent::RoutingDecision { account, .. } => Some(account.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                decided,
                ["claude:primary", "claude:spare", "claude:spare"],
                "{events:?}"
            );
        });
}
