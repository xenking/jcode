//! Cache and reset economics: shadow prices per account and the cost of each
//! way out of a warm session whose account cannot serve.
//!
//! Every cost is tokens times a cache factor times the account's shadow price
//! `λ`, in units of the model's list input price. Both sides of a comparison
//! run the same model, so the list price itself cancels out.

use crate::policy::{AccountId, Window};

/// How far a session is from losing its prompt cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Seam {
    /// The session has no history. Any account is free to pick.
    #[default]
    Start,
    /// The prompt cache is cold: TTL expired, right after a compaction or a
    /// model switch, or after a failed turn. Moving costs nothing extra.
    Cold,
    /// The cache is warm. Leaving it pays a rewrite on the target.
    Warm,
}

/// Cache pricing of one provider tier, as multiples of the input price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheFactors {
    /// Writing the context into a fresh cache.
    pub write: f64,
    /// Reading the warm context on the next turn.
    pub read: f64,
}

impl CacheFactors {
    /// Anthropic 5-minute cache: writes 1.25x, reads 0.1x.
    pub const ANTHROPIC_5M: Self = Self {
        write: 1.25,
        read: 0.1,
    };
    /// Anthropic 1-hour cache: writes 2x, reads 0.1x.
    pub const ANTHROPIC_1H: Self = Self {
        write: 2.0,
        read: 0.1,
    };
    /// No cache: every turn pays the full input price.
    pub const NONE: Self = Self {
        write: 1.0,
        read: 1.0,
    };

    /// OpenAI-style caches have no write premium and read at the catalog's
    /// cached-input ratio.
    pub fn openai(cached_input_ratio: f64) -> Self {
        Self {
            write: 1.0,
            read: cached_input_ratio,
        }
    }
}

impl Default for CacheFactors {
    fn default() -> Self {
        Self::NONE
    }
}

/// Output tokens cost this many times the input price (frontier list prices).
const OUTPUT_FACTOR: f64 = 5.0;
/// Without a measured summary, a compacted context keeps about this share.
const DEFAULT_SUMMARY_SHARE: f64 = 0.25;

/// What the request knows about its prompt cache. The default is a session
/// start with nothing cached, which leaves every economic rule inert.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheContext {
    pub seam: Seam,
    /// `C`: tokens in the cached context.
    pub context_tokens: f64,
    pub factors: CacheFactors,
    /// `S`: the context after compaction. Unknown estimates a quarter of `C`.
    pub summary_tokens: Option<f64>,
    /// Compaction can run on the warm session now. Without it a forced move
    /// is a plain move: planning a compaction that cannot run only hides it.
    pub compactable: bool,
}

impl CacheContext {
    fn summary(&self) -> f64 {
        self.summary_tokens
            .filter(|tokens| tokens.is_finite() && *tokens >= 0.0)
            .unwrap_or(self.context_tokens * DEFAULT_SUMMARY_SHARE)
            .min(self.context_tokens)
    }
}

/// A reset grant on an account: redeeming it clears `windows`.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    /// Unix seconds the grant lapses.
    pub expires_at: i64,
    /// The windows a redemption clears.
    pub windows: Vec<Window>,
    /// Only for this model class (`fable`), or every class.
    pub model_class: Option<String>,
}

/// The cost of each way out of a warm session whose account cannot serve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Options {
    /// `C x write x λnew`.
    pub moved: f64,
    /// Compaction on the warm current account, then a move with `S`.
    /// `None` when compaction cannot run: the session is not compactable or
    /// the current account is out of service.
    pub compacted: Option<f64>,
}

impl Options {
    pub(crate) fn price(cache: &CacheContext, current: Option<f64>, target: f64) -> Self {
        let c = cache.context_tokens.max(0.0);
        let s = cache.summary();
        let factors = cache.factors;
        let moved = c * factors.write * target;
        let compacted = current.filter(|_| cache.compactable).map(|lambda| {
            c * factors.read * lambda + s * OUTPUT_FACTOR * lambda + s * factors.write * target
        });
        Self { moved, compacted }
    }

    /// The cheapest way to leave, preferring a plain move on a tie: it keeps
    /// the full context.
    pub(crate) fn cheapest_move(&self) -> (Leave, f64) {
        match self.compacted {
            Some(compacted) if compacted < self.moved => (Leave::CompactThenMove, compacted),
            _ => (Leave::Move, self.moved),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leave {
    Move,
    CompactThenMove,
}

/// One pool an account spends from: a rate window, a credit grant or a ledger
/// budget. Room left at `resets_at` is lost.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pool {
    pub used: f64,
    /// What the pool holds per period (100 for a percent window).
    pub capacity: f64,
    pub resets_at: Option<i64>,
    pub window: Window,
}

impl Pool {
    fn left(&self) -> f64 {
        (self.capacity - self.used).max(0.0)
    }

    /// Share of the period still to run, from the reset and the window length.
    fn left_share(&self, now: i64) -> Option<f64> {
        self.resets_at
            .map(|at| ((at - now) as f64 / self.window.approx_secs() as f64).clamp(0.0, 1.0))
    }

    /// Room that would be lost at the reset: the pool is behind its pace line,
    /// a linear climb from 0 at the window start to `capacity` at the reset.
    pub(crate) fn behind_pace(&self, now: i64) -> bool {
        let Some(left_share) = self.left_share(now) else {
            return false;
        };
        if self.left() <= 0.0 || self.capacity <= 0.0 {
            return false;
        }
        self.used / self.capacity < 1.0 - left_share
    }

    /// `λ` of a pool ahead of pace: the steady pace to the reset over the room
    /// left.
    pub(crate) fn scarcity(&self, now: i64) -> f64 {
        let left = self.left();
        if left <= 0.0 {
            return f64::INFINITY;
        }
        match self.left_share(now) {
            Some(share) => self.capacity * share / left,
            // No reset known: the used share still rises as the pool runs out.
            None => (self.used / self.capacity).clamp(0.0, 1.0),
        }
    }

    /// When the account reaches this pool's wall again, in unix seconds: it
    /// fills over one full window after the reset.
    fn next_wall(&self) -> Option<i64> {
        let resets_at = self.resets_at?;
        Some(resets_at.saturating_add(self.window.approx_secs() as i64))
    }
}

/// A grant that will lapse before the account could hit its wall again: it is
/// free room now and free to redeem at the wall.
pub(crate) fn grant_wastes(grant: &Grant, pools: &[Pool], now: i64) -> bool {
    if grant.expires_at <= now {
        return false;
    }
    pools
        .iter()
        .filter(|pool| grant.windows.contains(&pool.window))
        .filter_map(Pool::next_wall)
        .min()
        .is_some_and(|wall| grant.expires_at < wall)
}

/// Rule 5: grants due within this many seconds are redeemed when that
/// restores any used quota.
pub const SWEEP_BEFORE_EXPIRY_SECS: i64 = 600;

/// One grant the sweep redeems now.
#[derive(Debug, Clone, PartialEq)]
pub struct Redemption {
    pub account: AccountId,
    pub grant: Grant,
}
