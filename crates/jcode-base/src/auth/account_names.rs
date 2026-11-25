//! The names and organizations a person gave their saved logins.
//!
//! A small sidecar next to the credential stores, so the credential structs
//! keep their shape. The file holds no token material. Both provider modules
//! and the identity view depend on this leaf, never on each other.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const FILE_NAME: &str = "account-names.json";
const LOCK_NAME: &str = "account-names";
const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AccountMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Organization name from the provider profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MetaFile {
    /// `provider:label` to metadata.
    #[serde(default)]
    accounts: BTreeMap<String, AccountMeta>,
}

/// A snapshot of every saved name and organization, read once per listing.
pub struct NameBook(MetaFile);

impl NameBook {
    pub fn load() -> Self {
        Self(load())
    }

    /// The name the user gave this login.
    pub fn name(&self, provider: &str, label: &str) -> Option<&str> {
        self.0.accounts.get(&key(provider, label))?.name.as_deref()
    }

    /// The organization the provider reported for this login.
    pub fn org(&self, provider: &str, label: &str) -> Option<&str> {
        self.0.accounts.get(&key(provider, label))?.org.as_deref()
    }
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join(FILE_NAME))
}

fn key(provider: &str, label: &str) -> String {
    format!("{provider}:{label}")
}

fn load() -> MetaFile {
    path()
        .ok()
        .filter(|path| path.exists())
        .and_then(|path| crate::storage::read_json(&path).ok())
        .unwrap_or_default()
}

fn update(edit: impl FnOnce(&mut MetaFile) -> Result<()>) -> Result<()> {
    let home = crate::storage::jcode_dir()?;
    let _lock = crate::auth::account_store::lock_store(&home, LOCK_NAME)?;
    let mut file = load();
    edit(&mut file)?;
    file.accounts
        .retain(|_, meta| *meta != AccountMeta::default());
    crate::storage::write_json(&path()?, &file)
}

/// A name the user may give a login: trimmed, printable, bounded.
pub fn clean_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

/// Give a login a name, or clear it with `None`. Names are unique within a
/// provider so two rows can never read the same.
pub fn set_name(provider: &str, label: &str, name: Option<&str>) -> Result<()> {
    let name = match name {
        Some(raw) => match clean_name(raw) {
            Some(name) => Some(name),
            None => bail!("account names are 1 to {MAX_NAME_CHARS} printable characters"),
        },
        None => None,
    };
    update(|file| {
        if let Some(name) = &name {
            let prefix = format!("{provider}:");
            let taken = file.accounts.iter().any(|(other, meta)| {
                other.starts_with(&prefix)
                    && *other != key(provider, label)
                    && meta
                        .name
                        .as_deref()
                        .is_some_and(|existing| existing.eq_ignore_ascii_case(name))
            });
            if taken {
                bail!("another {provider} account is already named `{name}`");
            }
        }
        file.accounts.entry(key(provider, label)).or_default().name = name;
        Ok(())
    })
}

/// Drop what is remembered about a login that was removed.
pub fn forget(provider: &str, label: &str) {
    if let Err(error) = update(|file| {
        file.accounts.remove(&key(provider, label));
        Ok(())
    }) {
        crate::logging::debug(&format!("Could not forget account name: {error}"));
    }
}

/// Remember the organization name the provider reported for a login.
pub fn set_org(provider: &str, label: &str, org: Option<&str>) -> Result<()> {
    let org = org.map(str::trim).filter(|org| !org.is_empty());
    if load()
        .accounts
        .get(&key(provider, label))
        .and_then(|meta| meta.org.as_deref())
        == org
    {
        return Ok(());
    }
    update(|file| {
        file.accounts.entry(key(provider, label)).or_default().org = org.map(str::to_owned);
        Ok(())
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_per_provider_and_survive_in_the_sidecar() {
        let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        set_name("claude", "claude-otter", Some(" team ")).unwrap();
        assert!(set_name("claude", "claude-fox", Some("TEAM")).is_err());
        // Another provider may reuse the name.
        set_name("openai", "openai-otter", Some("team")).unwrap();
        assert_eq!(
            NameBook::load().name("claude", "claude-otter"),
            Some("team")
        );
        set_name("claude", "claude-otter", None).unwrap();
        assert_eq!(NameBook::load().name("claude", "claude-otter"), None);
        assert!(set_name("claude", "claude-fox", Some("  ")).is_err());
        assert!(set_name("claude", "claude-fox", Some(&"x".repeat(41))).is_err());
    }
}
