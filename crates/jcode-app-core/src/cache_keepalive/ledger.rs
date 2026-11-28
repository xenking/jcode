use anyhow::{Context, Result};
use jcode_message_types::{CacheKeepaliveHit, CacheKeepaliveUsage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    day: i64,
    daily_micros: u64,
    usage: CacheKeepaliveUsage,
}

fn path(session_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !session_id.is_empty()
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "Invalid session ID for cache keepalive usage"
    );
    Ok(crate::storage::runtime_dir()
        .join("cache-keepalive")
        .join(format!("{session_id}.json")))
}

pub(super) fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_millis()).ok())
        .unwrap_or(0)
}

pub(crate) fn load_usage(session_id: &str) -> Option<CacheKeepaliveUsage> {
    crate::storage::read_json::<Ledger>(&path(session_id).ok()?)
        .ok()
        .map(|ledger| ledger.usage)
}

fn update<T>(session_id: &str, f: impl FnOnce(&mut Ledger) -> Option<T>) -> Result<Option<T>> {
    let path = path(session_id)?;
    std::fs::create_dir_all(
        path.parent()
            .context("Missing cache keepalive usage directory")?,
    )?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.try_lock()
        .context("Cache keepalive usage is being updated")?;
    let mut ledger = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
        Err(error) => return Err(error.into()),
    };
    let result = f(&mut ledger);
    if result.is_some() {
        crate::storage::write_json(&path, &ledger)?;
    }
    Ok(result)
}

pub(super) struct Reservation {
    session_id: String,
    day: i64,
    reserved: u64,
}

impl Reservation {
    pub(super) fn reserve(session_id: &str, reserved: u64, cap: u64) -> Result<Option<Self>> {
        let today = unix_ms(SystemTime::now()).div_euclid(86_400_000);
        update(session_id, |ledger| {
            if today > ledger.day {
                ledger.day = today;
                ledger.daily_micros = 0;
            }
            if reserved > cap.saturating_sub(ledger.daily_micros) {
                return None;
            }
            ledger.daily_micros += reserved;
            ledger.usage.attempted = ledger.usage.attempted.saturating_add(1);
            ledger.usage.unconfirmed_micros =
                ledger.usage.unconfirmed_micros.saturating_add(reserved);
            Some(Self {
                session_id: session_id.into(),
                day: ledger.day,
                reserved,
            })
        })
    }

    pub(super) fn settle(self, cost: u64, hit: Option<CacheKeepaliveHit>) {
        if let Err(error) = update(&self.session_id, |ledger| {
            if self.day == ledger.day {
                ledger.daily_micros = ledger
                    .daily_micros
                    .saturating_sub(self.reserved)
                    .saturating_add(cost);
            }
            ledger.usage.unconfirmed_micros = ledger
                .usage
                .unconfirmed_micros
                .saturating_sub(self.reserved);
            ledger.usage.cost_micros = ledger.usage.cost_micros.saturating_add(cost);
            if let Some(hit) = hit {
                ledger.usage.warmed = ledger.usage.warmed.saturating_add(1);
                ledger.usage.last_hit = Some(hit);
            }
            Some(())
        }) {
            crate::logging::warn(&format!("Could not settle cache keepalive usage: {error}"));
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(usage) = load_usage(&self.session_id) {
            crate::bus::Bus::global().publish(crate::bus::BusEvent::CacheKeepalive {
                session_id: self.session_id.clone(),
                usage,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keepalive_daily_cap_counts_unknown_attempts_across_openers() {
        let _guard = crate::storage::lock_test_env();
        let id = format!("cache-cap-{}", uuid::Uuid::new_v4());
        let cap = 10_000_000;
        let initial = Reservation::reserve(&id, 9_000_000, cap).unwrap().unwrap();
        assert!(Reservation::reserve(&id, 9_000_000, cap).unwrap().is_none());
        initial.settle(450_000, None);
        let lost = Reservation::reserve(&id, 9_000_000, cap).unwrap().unwrap();
        drop(lost);
        assert!(Reservation::reserve(&id, 9_000_000, cap).unwrap().is_none());
        let usage = load_usage(&id).unwrap();
        assert_eq!(usage.attempted, 2);
        assert_eq!(usage.cost_micros, 450_000);
        assert_eq!(usage.unconfirmed_micros, 9_000_000);
    }
}
