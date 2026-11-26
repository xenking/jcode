//! Account routing policy: which credential serves each request, under which
//! limits, through which egress proxy.
//!
//! [`config`] holds the raw `config.toml` shapes, [`Policy::from_config`] turns
//! them into validated types, and [`decide`] is a pure function of the policy,
//! the measured [`Snapshot`], the human [`Intents`] and the [`Request`].

pub mod config;
mod economics;
mod engine;
mod policy;
pub mod state;
mod strategy;

pub use economics::{
    CacheContext, CacheFactors, Grant, Redemption, SWEEP_BEFORE_EXPIRY_SECS, Seam,
};
pub use engine::{
    AccountState, Availability, Decision, GateOption, Intents, PaidIntent, Reading, Request, Role,
    Snapshot, Timed, decide, sweep,
};
pub use policy::{
    Account, AccountId, Budget, Egress, ModelGlob, Policy, PolicyError, Priority, Scope, UNRANKED,
    Window,
};
pub use strategy::{BurnRing, BurnSample, HealthTracker};

#[cfg(test)]
mod economics_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod state_tests;
