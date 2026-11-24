//! One honest view of every routing account's usage.
//!
//! [`build`] turns the routing [`jcode_routing_policy::Policy`], the meters the
//! daemon already caches and the local priced [`Ledger`] into the
//! [`jcode_routing_policy::Snapshot`] that `decide` consumes. Nothing here
//! decides anything and nothing blocks on the network: meters come from caches
//! a [`Meters`] source reads, and a meter that is unknown, failed or rolled
//! over simply has no reading. It is never reported as zero.

mod cache;
mod labels;
mod ledger;
mod published;
mod snapshot;

pub use cache::SnapshotCache;
pub use labels::{model_class, parse_rfc3339_unix, percent_reading, window_from_label};
pub use ledger::{Ledger, SpendRow, window_reset, window_start};
pub use published::Published;
pub use snapshot::{Meters, build};

#[cfg(test)]
mod tests;
