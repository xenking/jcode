use super::Agent;
use crate::kv_cache_monitor::RequestCacheDomain;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

const EXPENSIVE_TOKENS: u64 = 50_000;
const CONSECUTIVE_REQUESTS: u8 = 3;
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CachePause {
    provider: String,
    model: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    requests: u8,
    uncached_input: u64,
}

impl CachePause {
    fn message(&self) -> String {
        format!(
            "Paused by cache protection: {} / {} sent {} uncached input tokens across {} consecutive requests. No further model requests were sent. Use /cache resume to allow a new attempt.",
            self.provider, self.model, self.uncached_input, self.requests
        )
    }
}

#[derive(Default)]
pub(super) struct CacheBreaker {
    previous: Option<Sample>,
    paused: Option<(String, CachePause)>,
}

pub(super) struct CacheRoute {
    domain: RequestCacheDomain,
    effort: Option<String>,
}

struct Sample {
    route: CacheRoute,
    at: Instant,
    prompt: u64,
    requests: u8,
    uncached: u64,
}

impl CacheBreaker {
    fn observe(
        &mut self,
        route: CacheRoute,
        usage: Option<(u64, u64, Option<u64>)>,
        now: Instant,
    ) -> Option<CachePause> {
        let previous = self.previous.take();
        let (input, read, write) = usage?;
        let (prompt, uncached, expensive) = match route.domain.provider.to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => {
                let prompt = input
                    .saturating_add(read)
                    .saturating_add(write.unwrap_or(0));
                (prompt, input, input >= EXPENSIVE_TOKENS && write == Some(0))
            }
            "openai" | "openai-api" | "openai-oauth" if read <= input => {
                let uncached = input - read;
                (
                    input,
                    uncached,
                    uncached >= EXPENSIVE_TOKENS && read < input - input / 5,
                )
            }
            _ => return None,
        };
        let same_run = previous.filter(|old| {
            old.route.domain == route.domain
                && old.route.effort == route.effort
                && now.saturating_duration_since(old.at) <= MAX_SAMPLE_GAP
                && prompt >= old.prompt - old.prompt / 5
        });
        let (requests, uncached) = match (expensive, same_run) {
            (true, Some(old)) => (
                old.requests.saturating_add(1),
                old.uncached.saturating_add(uncached),
            ),
            (true, None) => (1, uncached),
            (false, _) => (0, 0),
        };
        let pause = (requests >= CONSECUTIVE_REQUESTS).then(|| CachePause {
            provider: route.domain.provider.clone(),
            model: route.domain.model.clone(),
            timestamp: chrono::Utc::now(),
            requests,
            uncached_input: uncached,
        });
        self.previous = Some(Sample {
            route,
            at: now,
            prompt,
            requests,
            uncached,
        });
        pause
    }
}

fn pause_path(id: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !id.is_empty() && !id.starts_with('.') && !id.contains(['/', '\\']),
        "invalid session ID for cache protection"
    );
    Ok(crate::session::session_path(id)?.with_extension("cache-paused"))
}

impl Agent {
    pub(super) fn cache_breaker_route(&self) -> CacheRoute {
        CacheRoute {
            domain: self.request_cache_domain(),
            effort: self.provider.reasoning_effort(),
        }
    }

    pub(crate) fn cache_pause_status(&self) -> anyhow::Result<Option<CachePause>> {
        let id = self.session.id();
        if let Some((owner, pause)) = &self.cache_breaker.paused
            && owner == id
        {
            return Ok(Some(pause.clone()));
        }
        if self.session.is_ephemeral() {
            return Ok(None);
        }
        let raw = match std::fs::read(pause_path(id)?) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(serde_json::from_slice(&raw)?))
    }

    pub(crate) fn resume_cache_protection(&mut self) -> anyhow::Result<bool> {
        let paused = self.cache_pause_status()?.is_some();
        if !self.session.is_ephemeral() {
            match std::fs::remove_file(pause_path(self.session.id())?) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.cache_breaker = CacheBreaker::default();
        Ok(paused)
    }

    pub(crate) fn ensure_cache_protection_allows_request(&self) -> anyhow::Result<()> {
        if let Some(pause) = self.cache_pause_status()? {
            anyhow::bail!(pause.message());
        }
        Ok(())
    }

    pub(super) fn observe_cache_protection(
        &mut self,
        route: CacheRoute,
        invalidated: bool,
    ) -> anyhow::Result<()> {
        let observed_route = self.cache_breaker_route();
        let usage = (!invalidated
            && route.domain == observed_route.domain
            && route.effort == observed_route.effort)
            .then(|| {
                self.last_usage.cache_read_input_tokens.map(|read| {
                    (
                        self.last_usage.input_tokens,
                        read,
                        self.last_usage.cache_creation_input_tokens,
                    )
                })
            })
            .flatten();
        let Some(pause) = self.cache_breaker.observe(route, usage, Instant::now()) else {
            return Ok(());
        };
        self.cache_breaker.paused = Some((self.session.id().to_owned(), pause.clone()));
        let persist_result = if self.session.is_ephemeral() {
            Ok(())
        } else {
            pause_path(self.session.id())
                .and_then(|path| crate::storage::write_json_secret_sidecar(&path, &pause))
        };
        let message = pause.message();
        crate::logging::event(
            crate::logging::LogLevel::Warn,
            "CACHE_PROTECTION_PAUSED",
            [
                ("session_id", self.session.id().to_owned()),
                ("provider", pause.provider.clone()),
                ("model", pause.model.clone()),
                ("requests", pause.requests.to_string()),
                ("uncached_input", pause.uncached_input.to_string()),
                ("persisted", persist_result.is_ok().to_string()),
            ],
        );
        self.session
            .record_replay_display_message("system", None, &message);
        self.persist_session_best_effort("cache protection pause");
        if let Err(error) = persist_result {
            anyhow::bail!("{message} Could not persist pause: {error:#}");
        }
        anyhow::bail!(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        message::{Message, StreamEvent, ToolDefinition},
        provider::{EventStream, Provider},
        tool::Registry,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn route(provider: &str, account: &str) -> CacheRoute {
        CacheRoute {
            domain: RequestCacheDomain {
                provider: provider.into(),
                model: "test".into(),
                credential: None,
                account: Some(account.into()),
            },
            effort: None,
        }
    }

    #[test]
    fn warming_route_changes_compaction_and_missing_usage_do_not_trip() {
        let mut breaker = CacheBreaker::default();
        let now = Instant::now();
        for usage in [
            Some((120_000, 18_304, None)),
            Some((120_000, 18_304, None)),
            None,
            Some((120_000, 18_304, None)),
            Some((120_000, 110_000, None)),
        ] {
            assert!(breaker.observe(route("openai", "a"), usage, now).is_none());
        }
        let expensive = Some((120_000, 18_304, None));
        assert!(
            breaker
                .observe(route("openai", "a"), expensive, now)
                .is_none()
        );
        assert!(
            breaker
                .observe(route("openai", "b"), expensive, now)
                .is_none()
        );
        assert!(
            breaker
                .observe(route("openai", "b"), Some((72_000, 2_000, None)), now)
                .is_none()
        );
        assert!(
            breaker
                .observe(route("openai", "b"), expensive, now)
                .is_none()
        );
        let pause = breaker
            .observe(route("openai", "b"), expensive, now)
            .unwrap();
        assert_eq!(pause.requests, 3);
        assert_eq!(pause.uncached_input, 273_392);
    }

    #[derive(Clone)]
    struct StuckAnthropic(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Provider for StuckAnthropic {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<EventStream> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::TokenUsage {
                    input_tokens: Some(200_000),
                    output_tokens: Some(5),
                    cache_read_input_tokens: Some(130_721),
                    cache_creation_input_tokens: Some(0),
                }),
                Ok(StreamEvent::TextDelta("partial work preserved".into())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                }),
            ])))
        }

        fn name(&self) -> &str {
            "anthropic"
        }
        fn model(&self) -> String {
            "cache-stuck-test".into()
        }
        fn context_window(&self) -> usize {
            1_000_000
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    #[tokio::test]
    async fn cache_breaker_blocks_the_fourth_request_even_after_restore() {
        let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(StuckAnthropic(calls.clone()));
        let registry = Registry::empty();
        let mut agent = Agent::new(provider.clone(), registry);
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        agent.run_turn_streaming_mpsc(tx.clone()).await.unwrap();
        agent.run_turn(false).await.unwrap();
        let error = agent.run_turn_streaming_mpsc(tx.clone()).await.unwrap_err();
        assert!(
            error.to_string().contains("600000 uncached input"),
            "{error:#}"
        );
        let id = agent.session_id().to_owned();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        drop(agent);

        let mut restored = Agent::new(provider, Registry::empty());
        restored.restore_session(&id).unwrap();
        assert!(restored.run_turn(false).await.is_err());
        assert!(restored.run_turn_streaming_mpsc(tx).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(restored.resume_cache_protection().unwrap());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "clearing the pause sends nothing"
        );
        restored.run_turn(false).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
