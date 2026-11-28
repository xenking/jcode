//! Where routing intents live on disk: `JCODE_HOME/routing-state.json`.
//!
//! The daemon and `jcode routing` both read-modify-write this file. Writes are
//! atomic renames, so a reader never sees a torn file.

use std::path::PathBuf;

use anyhow::Result;
use jcode_routing_policy::state::RoutingState;

fn path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("routing-state.json"))
}

/// Missing or unreadable state reads as no intents.
pub fn load() -> RoutingState {
    path()
        .ok()
        .filter(|path| path.exists())
        .and_then(|path| crate::storage::read_json(&path).ok())
        .unwrap_or_default()
}

/// Applies `change` to the stored state, drops expired entries and saves it.
pub fn update<T>(now: i64, change: impl FnOnce(&mut RoutingState) -> T) -> Result<T> {
    let mut state = load();
    let out = change(&mut state);
    state.prune(now);
    let path = path()?;
    crate::storage::assert_not_real_home(&path);
    crate::storage::write_json(&path, &state)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use jcode_routing_policy::AccountId;
    use jcode_routing_policy::state::PaidEntry;

    #[test]
    fn update_persists_and_prunes() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());

        let account = AccountId::parse("claude:a").unwrap();
        super::update(100, |state| {
            state.set_hold(Some((&account, 200)));
            state.set_paid(Some(PaidEntry::Off));
        })
        .unwrap();
        assert_eq!(super::load().hold.map(|hold| hold.until), Some(200));
        super::update(300, |_| ()).unwrap();
        let state = super::load();
        assert!(state.hold.is_none(), "expired hold pruned on save");
        assert_eq!(state.paid, Some(PaidEntry::Off));

        match previous {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}
