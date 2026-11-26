use crate::protocol::{AskOption, AskQuestion};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

const ALLOW: &str = "Allow for this sending session";
const QUEUE: &str = "Queue without waking (Recommended)";
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

type SenderLock = tokio::sync::Mutex<()>;
static APPROVALS: LazyLock<Mutex<HashMap<String, Weak<SenderLock>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static STORE: Mutex<()> = Mutex::new(());

#[derive(Default, Serialize, Deserialize)]
struct State {
    allow_cold_wake: Option<bool>,
    pending: Vec<DeferredMessage>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct DeferredMessage {
    id: String,
    pub from_session: String,
    pub from_name: Option<String>,
    pub text: String,
}

pub(super) struct Risk {
    recipient: String,
    model: String,
    tokens: u64,
    limit: u64,
    source: Option<String>,
}

impl Risk {
    fn assess(
        recipient: &str,
        asleep: bool,
        expires_at_ms: Option<i64>,
        context: Context,
    ) -> Option<Self> {
        let expired =
            expires_at_ms.is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis());
        (context.limit > 0 && context.tokens > context.limit / 2 && (asleep || expired)).then(
            || Self {
                recipient: recipient.into(),
                model: context.model,
                tokens: context.tokens,
                limit: context.limit,
                source: context.source,
            },
        )
    }

    fn question(&self) -> AskQuestion {
        let cost = self
            .source
            .as_deref()
            .and_then(|source| {
                let split_cache_usage = source.starts_with("claude:");
                let cache_miss = crate::provider::pricing::metered_usage_cost_usd(
                    source,
                    &self.model,
                    if split_cache_usage { 0 } else { self.tokens },
                    0,
                    0,
                    if split_cache_usage { self.tokens } else { 0 },
                )?;
                let warm = crate::provider::pricing::metered_usage_cost_usd(
                    source,
                    &self.model,
                    if split_cache_usage { 0 } else { self.tokens },
                    0,
                    self.tokens,
                    0,
                )?;
                Some(format!(
                    "estimated extra input cost ${:.4}",
                    (cache_miss - warm).max(0.0)
                ))
            })
            .unwrap_or_else(|| {
                format!(
                    "estimated {} uncached input tokens; plan or API price unavailable",
                    self.tokens
                )
            });
        AskQuestion {
            question: format!("{} uses {} with {:.0}% of its context occupied ({}/{} tokens). Its prompt cache is cold, with {cost}. Allow this sending session to wake expensive cold recipients?", self.recipient, self.model, (self.tokens as f64 / self.limit as f64) * 100.0, self.tokens, self.limit),
            header: Some("Cold wake".into()),
            options: vec![AskOption { label: QUEUE.into(), description: Some("Keep the message for the recipient’s next user-started turn; do not wake it now or ask again for this sender.".into()) }, AskOption { label: ALLOW.into(), description: Some("Permit this sender to wake cold large-context recipients without asking again.".into()) }],
            multi_select: false,
            allow_other: false,
        }
    }
}

struct Context {
    model: String,
    tokens: u64,
    limit: u64,
    source: Option<String>,
}

pub(super) async fn assess(
    recipient: &str,
    sessions: &super::SessionAgents,
    roster_model: Option<String>,
) -> Option<Risk> {
    let agent = sessions.get(recipient).await;
    let asleep = agent.is_none();
    let use_sample = crate::session_metrics::token_totals(recipient).map(|(tokens, _)| tokens);
    let effective_limit = agent.as_ref().and_then(|agent| {
        agent
            .try_lock()
            .ok()
            .map(|agent| agent.provider_context_window() as u64)
    });
    let target = recipient.to_owned();
    tokio::task::spawn_blocking(move || {
        let stored = crate::session::Session::load(&target).ok();
        let model = roster_model.or_else(|| stored.as_ref()?.model().map(str::to_owned))?;
        let limit = effective_limit.or_else(|| {
            crate::provider::models::context_limit_for_model(&model).map(|limit| limit as u64)
        })?;
        let cap = stored
            .as_ref()
            .and_then(|session| session.context_cap())
            .map(|cap| cap as u64);
        let limit = cap.map_or(limit, |cap| cap.min(limit));
        let provider = stored
            .as_ref()
            .and_then(|session| session.route_api_method().or(session.provider_key()));
        let expires_at_ms = stored.as_ref().and_then(|session| {
            let ttl = crate::provider::cache_ttl_for_provider_model(provider?, Some(&model))?;
            let ttl = chrono::TimeDelta::try_seconds(ttl.try_into().ok()?)?;
            session
                .last_provider_request_at()?
                .checked_add_signed(ttl)
                .map(|time| time.timestamp_millis())
        });
        let tokens = use_sample.or_else(|| {
            stored
                .as_ref()?
                .iter_messages()
                .rev()
                .find_map(|message| {
                    let usage = message.token_usage.as_ref()?;
                    Some(
                        usage
                            .prompt_tokens
                            .unwrap_or(usage.input_tokens)
                            .saturating_add(usage.output_tokens),
                    )
                })
        })?;
        Risk::assess(
            &target,
            asleep,
            expires_at_ms,
            Context {
                model,
                tokens,
                limit,
                source: provider.map(str::to_owned),
            },
        )
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn approved(sender: &str, operator_chain: &[String], risk: &Risk) -> bool {
    let lock = {
        let mut locks = APPROVALS.lock().unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(sender).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(SenderLock::new(()));
            locks.insert(sender.into(), Arc::downgrade(&lock));
            lock
        }
    };
    let _approval = lock.lock().await;
    if let Ok(Some(decision)) = edit(sender, |state| (state.allow_cold_wake, false)) {
        return decision;
    }
    let question = [risk.question()];
    let request = format!("cold-wake-{}", uuid::Uuid::new_v4());
    let response = tokio::time::timeout(
        APPROVAL_TIMEOUT,
        crate::tool::ask_for_real_answer(operator_chain, &request, &question),
    )
    .await
    .ok()
    .flatten();
    let Some(answers) = response.and_then(|response| response.answers) else {
        return false;
    };
    let Some(choice) = answers.first().and_then(|answer| answer.selected.first()) else {
        return false;
    };
    let decision = match choice.as_str() {
        ALLOW => true,
        QUEUE => false,
        _ => return false,
    };
    match edit(sender, |state| {
        state.allow_cold_wake = Some(decision);
        (decision, true)
    }) {
        Ok(decision) => decision,
        Err(error) => {
            crate::logging::warn(&format!(
                "could not persist cold-wake decision for {sender}: {error}"
            ));
            false
        }
    }
}

pub(super) fn defer(
    recipient: &str,
    from_session: String,
    from_name: Option<String>,
    text: String,
) -> anyhow::Result<()> {
    edit(recipient, |state| {
        state.pending.push(DeferredMessage {
            id: uuid::Uuid::new_v4().to_string(),
            from_session,
            from_name,
            text,
        });
        ((), true)
    })
}

pub(super) fn pending_for_user_turn(recipient: &str) -> Vec<DeferredMessage> {
    match edit(recipient, |state| {
        (std::mem::take(&mut state.pending), false)
    }) {
        Ok(pending) => pending,
        Err(error) => {
            crate::logging::warn(&format!(
                "could not read deferred agent messages for {recipient}: {error}"
            ));
            Vec::new()
        }
    }
}

pub(super) fn mark_stored(recipient: &str, messages: &[DeferredMessage]) -> anyhow::Result<()> {
    let ids = messages
        .iter()
        .map(|message| message.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    edit(recipient, |state| {
        state
            .pending
            .retain(|message| !ids.contains(message.id.as_str()));
        ((), true)
    })
}

fn edit<T>(session: &str, apply: impl FnOnce(&mut State) -> (T, bool)) -> anyhow::Result<T> {
    anyhow::ensure!(
        !session.is_empty()
            && session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid session ID for cold-wake storage"
    );
    let _store = STORE.lock().unwrap_or_else(|error| error.into_inner());
    let path = crate::storage::jcode_dir()?
        .join("cold-agent-wake")
        .join(format!("{session}.json"));
    let mut state = read(&path)?;
    let (result, changed) = apply(&mut state);
    if changed {
        crate::storage::write_json_secret(&path, &state)?;
    }
    Ok(result)
}

fn read(path: &Path) -> anyhow::Result<State> {
    if !path.try_exists()? {
        return Ok(State::default());
    }
    crate::storage::read_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AskAnswer, AskResponse};

    fn context(tokens: u64) -> Context {
        Context {
            model: "test-model".into(),
            tokens,
            limit: 100,
            source: None,
        }
    }

    #[test]
    fn active_warm_and_exactly_half_full_recipients_do_not_need_approval() {
        let now = chrono::Utc::now().timestamp_millis();
        assert!(Risk::assess("warm", false, Some(now + 60_000), context(95)).is_none());
        assert!(Risk::assess("half", true, Some(now - 60_000), context(50)).is_none());
        assert!(Risk::assess("expired", false, Some(now - 60_000), context(51)).is_some());
        assert!(Risk::assess("sleeping", true, Some(now + 60_000), context(51)).is_some());
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_lock,
        reason = "serialize access to the test home"
    )]
    async fn explicit_allow_or_deny_is_remembered_per_sender() {
        let _env = crate::storage::lock_test_env();
        let risk = Risk::assess("recipient", true, None, context(80)).unwrap();
        for (label, allowed) in [(ALLOW, true), (QUEUE, false)] {
            let sender = format!("session_{}", uuid::Uuid::new_v4());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let attachment = crate::tool::interactive::attach(&sender, tx);
            let chain = [sender.clone()];
            let (decision, ()) = tokio::join!(approved(&sender, &chain, &risk), async {
                let request = rx.recv().await.unwrap();
                let questions = request.ask_questions.unwrap();
                assert!(questions[0]["question"].as_str().unwrap().contains("80%"));
                let response = serde_json::to_string(&AskResponse {
                    answers: Some(vec![AskAnswer {
                        selected: vec![label.into()],
                        ..Default::default()
                    }]),
                })
                .unwrap();
                request.response_tx.send(response).unwrap();
            });
            assert_eq!(decision, allowed);
            drop(attachment);
            assert_eq!(approved(&sender, &[], &risk).await, allowed);
        }
    }
}
