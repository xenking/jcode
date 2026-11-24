//! What the service reads from the outside world, so tests can stand in
//! fixture meters, egress and clocks.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use jcode_routing_policy::config::{EgressConfig, RoutingConfig};
use jcode_routing_policy::state::RoutingState;
use jcode_routing_policy::{AccountId, Availability, Policy, Snapshot};

use crate::usage::ResetMode;

/// Gives the first decision of a daemon's life this long to fetch the meters.
const WARM_TIMEOUT: Duration = Duration::from_secs(4);
/// The meter caches the snapshot reads are refreshed this often while routing is on.
const PREFETCH_EVERY: Duration = Duration::from_secs(150);

#[async_trait]
pub trait RoutingEnv: Send + Sync {
    fn now(&self) -> i64;
    /// `[routing]` and `[egress.*]` as configured right now.
    fn routing(&self) -> (RoutingConfig, BTreeMap<String, EgressConfig>);
    /// The measured meters, without network access.
    fn snapshot(&self, policy: &Policy, now: i64) -> Snapshot;
    /// Whether the account's egress proxy lets requests out.
    async fn availability(&self, account: &AccountId) -> Availability;
    /// The human intents file.
    fn state(&self) -> RoutingState;
    fn update_state(
        &self,
        now: i64,
        change: &mut (dyn FnMut(&mut RoutingState) + Send),
    ) -> anyhow::Result<()>;
    /// Where the decision log and the persisted health state live.
    fn home(&self) -> Option<PathBuf>;
    /// Runs once before the first decision.
    async fn warm(&self) {}
    /// Redeems a reset grant on `account` with the provider's own claim code
    /// and its cross-agent lock. `Ok(true)` when quota was restored.
    async fn redeem(&self, _account: &AccountId, _mode: ResetMode) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// The real daemon: config, cached meters, egress probes and `JCODE_HOME`.
pub struct DaemonEnv;

#[async_trait]
impl RoutingEnv for DaemonEnv {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn routing(&self) -> (RoutingConfig, BTreeMap<String, EgressConfig>) {
        let config = crate::config::config();
        (config.routing.clone(), config.egress.clone())
    }

    fn snapshot(&self, policy: &Policy, now: i64) -> Snapshot {
        crate::routing_meters::snapshot(policy, now)
    }

    async fn availability(&self, account: &AccountId) -> Availability {
        crate::egress::availability(account).await
    }

    fn state(&self) -> RoutingState {
        crate::routing_state::load()
    }

    fn update_state(
        &self,
        now: i64,
        change: &mut (dyn FnMut(&mut RoutingState) + Send),
    ) -> anyhow::Result<()> {
        crate::routing_state::update(now, |state| change(state))
    }

    fn home(&self) -> Option<PathBuf> {
        crate::storage::jcode_dir().ok()
    }

    async fn warm(&self) {
        let _ = tokio::time::timeout(WARM_TIMEOUT, crate::routing_meters::prefetch()).await;
        tokio::spawn(async {
            loop {
                tokio::time::sleep(PREFETCH_EVERY).await;
                if crate::config::config().routing.mode
                    == jcode_routing_policy::config::RoutingMode::Off
                {
                    continue;
                }
                crate::routing_meters::prefetch().await;
                // Grants due within ten minutes are seen at least twice.
                super::global().sweep().await;
            }
        });
    }

    async fn redeem(&self, account: &AccountId, mode: ResetMode) -> anyhow::Result<bool> {
        let Some(label) = account.label().map(str::to_string) else {
            return Ok(false);
        };
        match account.provider() {
            "openai" => crate::usage::auto_reset_weekly_usage_with(Some(label), mode).await,
            "claude" | "anthropic" => {
                crate::usage::auto_reset_fable_usage_with(Some(label), mode).await
            }
            _ => Ok(false),
        }
    }
}
