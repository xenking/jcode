use super::*;
use chrono::Datelike;
pub(super) use jcode_provider_core::service_tier::AnthropicFastMode as FastMode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BETA: &str = "fast-mode-2026-02-01";

pub(super) fn capability(model: &str, oauth: bool) -> Option<FastMode> {
    capability_on(model, oauth, chrono::Utc::now().date_naive())
}

fn capability_on(model: &str, oauth: bool, today: chrono::NaiveDate) -> Option<FastMode> {
    jcode_provider_core::service_tier::anthropic_fast_mode(
        model,
        oauth,
        (today.year(), today.month(), today.day()),
    )
}

pub(super) fn append_beta(headers: &mut String, fast: Option<FastMode>) {
    if fast == Some(FastMode::Beta) && !headers.split(',').any(|item| item.trim() == BETA) {
        headers.push(',');
        headers.push_str(BETA);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServedSpeed {
    Standard,
    Fast,
}

impl ServedSpeed {
    pub(super) fn from_usage(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(Self::Standard),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Credential {
    OAuth(String),
    ApiKey(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) model: String,
    pub(super) credential: Credential,
}

pub(super) fn pricing_notice(target: &Target) -> String {
    let billing = match target.credential {
        Credential::OAuth(_) => "Uses paid Anthropic usage credits, even when plan quota remains",
        Credential::ApiKey(_) => "Billed to the Anthropic API organization",
    };
    let price =
        jcode_provider_core::pricing::anthropic_api_fast_pricing(&target.model).and_then(|p| {
            Some((
                p.input_price_per_mtok_micros? / 1_000_000,
                p.output_price_per_mtok_micros? / 1_000_000,
            ))
        });
    match price {
        Some((input, output)) => format!(
            "{billing}. Fast list price ${input} input / ${output} output per million tokens."
        ),
        None => format!("{billing} at premium rates."),
    }
}

#[derive(Clone, Debug)]
pub(super) enum FastBlock {
    Cooldown(Instant),
    Denied(&'static str),
}

impl std::fmt::Display for FastBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl FastBlock {
    pub(super) fn classify(
        status: reqwest::StatusCode,
        headers: &HeaderMap,
        body: &str,
        oauth: bool,
    ) -> Option<Self> {
        if status.as_u16() == 429
            && oauth
            && let Some(reason) = headers.get("anthropic-ratelimit-unified-overage-disabled-reason")
        {
            return Some(Self::Denied(match reason.to_str() {
                Ok("out_of_credits") => "out of Anthropic usage credits",
                Ok("org_spend_cap_reached") => "Anthropic usage credit spend cap reached",
                _ => "Anthropic usage credits are not available for this account",
            }));
        }
        if matches!(status.as_u16(), 400 | 403) {
            let body = body.to_ascii_lowercase();
            if body.contains("fast mode is not enabled")
                || body.contains("fast mode is not supported")
                || body.contains("fast mode is not available")
                || body.contains("does not support fast mode")
            {
                return Some(Self::Denied(
                    "Anthropic rejected Fast for this model or account",
                ));
            }
        }
        if matches!(status.as_u16(), 429 | 529) {
            // Claude Code 2.1.325 uses 30 minutes when no explicit reset is supplied.
            let delay = jcode_provider_core::retry_after::retry_after_raw_delay(headers)
                .unwrap_or(Duration::from_secs(30 * 60))
                .min(Duration::from_secs(24 * 60 * 60));
            return Some(Self::Cooldown(Instant::now() + delay));
        }
        None
    }

    pub(super) fn detail(&self) -> String {
        match self {
            Self::Cooldown(until) => format!(
                "Standard speed: Anthropic Fast is cooling down ({}s remaining)",
                until
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .saturating_add(1),
            ),
            Self::Denied(reason) => format!("Standard speed: {reason}"),
        }
    }

    fn active(&self) -> bool {
        match self {
            Self::Cooldown(until) => *until > Instant::now(),
            Self::Denied(_) => true,
        }
    }
}

#[derive(Clone)]
struct Selection {
    target: Target,
    generation: Arc<()>,
    block: Option<FastBlock>,
    served: Option<ServedSpeed>,
}

#[derive(Clone, Default)]
pub(super) struct FastState(Arc<Mutex<Option<Selection>>>);

impl FastState {
    pub(super) fn select(&self, target: Option<Target>) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.as_ref().map(|selected| &selected.target) == target.as_ref() {
            return;
        }
        *state = target.map(|target| Selection {
            target,
            generation: Arc::new(()),
            block: None,
            served: None,
        });
    }

    pub(super) fn selected(&self, target: &Target) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|selected| selected.target == *target)
    }

    pub(super) fn status(&self, target: &Target) -> (Option<FastBlock>, Option<ServedSpeed>) {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match state.as_ref().filter(|selected| selected.target == *target) {
            Some(selected) => (
                selected.block.clone().filter(FastBlock::active),
                selected.served,
            ),
            None => (None, None),
        }
    }

    pub(super) fn requested_for(&self, model: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|selected| selected.target.model == model)
    }

    pub(super) fn request(&self, target: Target, mode: FastMode) -> Option<Request> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let selected = state
            .as_mut()
            .filter(|selection| selection.target == target)?;
        if selected.block.as_ref().is_some_and(|block| !block.active()) {
            selected.block = None;
        }
        selected.served = None;
        Some(Request {
            state: self.clone(),
            generation: Arc::clone(&selected.generation),
            mode,
            block: selected.block.clone(),
        })
    }

    pub(super) fn fork(&self) -> Self {
        Self(Arc::new(Mutex::new(
            self.0.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        )))
    }
}

pub(super) struct Request {
    state: FastState,
    generation: Arc<()>,
    mode: FastMode,
    block: Option<FastBlock>,
}

impl Request {
    pub(super) fn mode(&self) -> Option<FastMode> {
        self.block.is_none().then_some(self.mode)
    }

    pub(super) fn block(&self) -> Option<&FastBlock> {
        self.block.as_ref()
    }

    pub(super) fn fallback(&mut self, block: FastBlock) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(selection) = state
            .as_mut()
            .filter(|selected| Arc::ptr_eq(&selected.generation, &self.generation))
        {
            selection.block = Some(block.clone());
            selection.served = None;
        }
        self.block = Some(block);
    }

    pub(super) fn confirm(&self, speed: ServedSpeed) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(selection) = state
            .as_mut()
            .filter(|selected| Arc::ptr_eq(&selected.generation, &self.generation))
        {
            selection.served = Some(speed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_48_beta_ends_on_the_documented_date_without_switching_models() {
        let before = chrono::NaiveDate::from_ymd_opt(2026, 11, 29).unwrap();
        let after = before.succ_opt().unwrap();
        for oauth in [false, true] {
            assert_eq!(
                capability_on("claude-opus-4-8", oauth, before),
                Some(FastMode::Beta)
            );
            assert_eq!(capability_on("claude-opus-4-8", oauth, after), None);
            assert_eq!(
                capability_on("claude-opus-5", oauth, after),
                Some(FastMode::Beta)
            );
        }
    }

    #[test]
    fn overload_cooldown_is_not_the_ordinary_sixty_second_retry_cap() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("600"));
        let block = FastBlock::classify(
            reqwest::StatusCode::from_u16(529).unwrap(),
            &headers,
            "overloaded",
            false,
        )
        .unwrap();
        assert!(
            matches!(block, FastBlock::Cooldown(until) if until > Instant::now() + Duration::from_secs(599))
        );
    }
}
