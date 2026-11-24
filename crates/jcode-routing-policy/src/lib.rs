//! Account routing policy: which credential serves each request, under which
//! limits, through which egress proxy.
//!
//! [`config`] holds the raw `config.toml` shapes, [`Policy::from_config`] turns
//! them into validated types, and [`decide`] is a pure function of the policy,
//! the measured [`Snapshot`], the human [`Intents`] and the [`Request`].

pub mod config;
mod engine;
mod policy;
pub mod state;

pub use engine::{
    AccountState, Availability, Decision, GateOption, Intents, PaidIntent, Reading, Request, Role,
    Snapshot, Timed, decide,
};
pub use policy::{
    Account, AccountId, Budget, Egress, ModelGlob, Policy, PolicyError, Priority, Scope, UNRANKED,
    Window,
};

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod state_tests;
