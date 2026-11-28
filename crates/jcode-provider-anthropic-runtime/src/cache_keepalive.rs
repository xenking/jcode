//! Replay only first-party Messages requests with a measured cache breakpoint.

use super::*;
use jcode_provider_anthropic::{ApiContentBlock, ApiThinking};
use jcode_provider_core::cache_keepalive::{Cache, Capture, Rates, Replay, Tier, Usage};
use std::time::{Duration, SystemTime};

pub(super) struct Prepared {
    capture: Capture,
    request: reqwest::RequestBuilder,
    tier: Tier,
    rates: Rates,
}

pub(super) fn prepare(
    builder: &reqwest::RequestBuilder,
    request: &ApiRequest,
    first_party: bool,
) -> Option<Prepared> {
    let capture = Capture::current()?;
    if !first_party
        || request.speed.is_some()
        || matches!(request.thinking, Some(ApiThinking::Enabled { .. }))
        || request.messages.last()?.role != "user"
        // The currently shared price table does not express its >100k tier.
        || request.model == "claude-haiku-5-5"
    {
        return None;
    }
    let pricing = jcode_provider_core::pricing::anthropic_api_pricing(&request.model)?;
    let breakpoint = request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .find_map(|block| match block {
            ApiContentBlock::Text { cache_control, .. }
            | ApiContentBlock::ToolUse { cache_control, .. }
            | ApiContentBlock::ToolResult { cache_control, .. }
            | ApiContentBlock::Image { cache_control, .. } => cache_control.as_ref(),
            _ => None,
        })?;
    let tier = match breakpoint.ttl {
        Some("1h") => Tier::OneHour,
        Some("5m") | None => Tier::FiveMinutes,
        _ => return None,
    };
    if !jcode_base::config::config().cache.permits_tier(tier) {
        return None;
    }
    Some(Prepared {
        capture,
        request: builder.try_clone()?,
        tier,
        rates: Rates {
            input: pricing.input_price_per_mtok_micros?,
            read: pricing.cache_read_price_per_mtok_micros?,
            output: pricing.output_price_per_mtok_micros?,
        },
    })
}

impl Prepared {
    pub(super) fn capture(
        self,
        request: &ApiRequest,
        state: &SseStreamState,
        touched_at: SystemTime,
        usage_label: Option<&str>,
    ) {
        let cached_tokens = state
            .cache_read_input_tokens
            .unwrap_or_default()
            .saturating_add(state.cache_creation_input_tokens.unwrap_or_default());
        let Some(uncached_tokens) = state.input_tokens else {
            return;
        };
        if cached_tokens < jcode_base::config::config().cache.min_cached_tokens
            || cached_tokens == 0
            || (self.tier == Tier::OneHour && state.cache_creation_5m.unwrap_or(0) > 0)
            || state.served_model.as_deref() != Some(request.model.as_str())
        {
            return;
        }
        let cache = Cache {
            touched_at,
            tier: self.tier,
            cached_tokens,
            uncached_tokens,
            rates: self.rates,
        };
        let request = ping_request(request);
        let model = request.model.clone();
        let builder = self.request.header("accept", "application/json");
        let label = usage_label.map(str::to_owned);
        self.capture.record(Replay {
            cache,
            model,
            send: Arc::new(move || {
                let builder = builder.try_clone();
                let request = Arc::clone(&request);
                let label = label.clone();
                Box::pin(async move {
                    send(
                        builder.context("Cannot clone cache ping request")?,
                        &request,
                        label.as_deref(),
                    )
                    .await
                })
            }),
        });
    }
}

fn ping_request(original: &ApiRequest) -> Arc<ApiRequest> {
    let mut ping = original.clone();
    // Anthropic cache pre-warming produces no output with these two changes.
    // Leave thinking mode and effort alone: both can change cache identity.
    ping.max_tokens = 0;
    ping.stream = false;
    Arc::new(ping)
}

async fn send(
    builder: reqwest::RequestBuilder,
    request: &ApiRequest,
    label: Option<&str>,
) -> Result<Usage> {
    #[derive(serde::Deserialize)]
    struct Response {
        model: String,
        usage: sse_types::UsageInfo,
    }
    let response: Response = builder
        .timeout(Duration::from_secs(20))
        .json(request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut state = SseStreamState {
        served_model: Some(response.model),
        ..SseStreamState::default()
    };
    usage_recording::update_usage(&mut state, response.usage);
    usage_recording::record(label, &request.model, &state).await;
    anyhow::ensure!(
        state.served_model.as_deref() == Some(&request.model),
        "Cache ping model changed"
    );
    Ok(Usage {
        input: state
            .input_tokens
            .context("Missing cache ping input usage")?,
        read: state
            .cache_read_input_tokens
            .context("Missing cache ping read usage")?,
        write: state
            .cache_creation_input_tokens
            .context("Missing cache ping write usage")?,
        output: state
            .output_tokens
            .context("Missing cache ping output usage")?,
    })
}
