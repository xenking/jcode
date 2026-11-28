//! Process-wide counter that says "provider catalogs changed out-of-band".
//!
//! It sits below `bus` and `provider` so the bus can announce a catalog change
//! without depending on the provider module, and the provider's memoized route
//! catalogs can compare against it.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bumped whenever provider catalogs change out-of-band (prefetch completion,
/// forced catalog refresh, auth changes). Invalidates every shared memo entry.
static CATALOG_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current generation. A memo built under an older value is stale.
pub fn current() -> u64 {
    CATALOG_GENERATION.load(Ordering::Relaxed)
}

/// Invalidate every memoized route catalog in the process.
pub fn bump() {
    CATALOG_GENERATION.fetch_add(1, Ordering::Relaxed);
}
