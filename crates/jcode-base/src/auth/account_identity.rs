//! Who a saved login is, in words a person recognises.
//!
//! Internal labels (`claude-fox`, `openai-otter`) are stable ids, not names.
//! The name shown for a login is the one the user gave it, else the
//! organization or masked email, plus the plan: `team · Max 20x`.
//!
//! Names and organizations live in a small sidecar next to the credential
//! stores so the credential structs keep their shape. The file holds no token
//! material.

use anyhow::{Result, bail};
use jcode_usage_types::{AccountIdentity, CredentialKind};
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

/// Human plan text for a provider's raw plan id: `max_20x` is `Max 20x`.
pub fn plan_display(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let words: Vec<String> = raw
        .split(['_', '-', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first)
                    if word.chars().any(|c| c.is_ascii_digit()) && !first.is_ascii_digit() =>
                {
                    // `20x` and `5x` stay lowercase, `claude` becomes `Claude`.
                    first.to_uppercase().chain(chars).collect()
                }
                Some(first) if first.is_ascii_digit() => word.to_ascii_lowercase(),
                Some(first) => first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect(),
                None => String::new(),
            }
        })
        .collect();
    Some(words.join(" "))
}

/// How a saved login is billed when the user has not said otherwise.
///
/// OAuth plans are subscriptions with windows that reset on their own. API
/// keys are metered. Plans funded by a credit balance (enterprise or business
/// seats) are credits.
pub fn default_kind(provider: &str, plan: Option<&str>, api_key: bool) -> CredentialKind {
    if api_key {
        return CredentialKind::Metered;
    }
    let plan = plan.unwrap_or("").trim().to_ascii_lowercase();
    match (provider, plan.as_str()) {
        ("claude", "enterprise") | ("openai", "enterprise" | "business" | "team") => {
            CredentialKind::Credits
        }
        _ => CredentialKind::Subscription,
    }
}

/// The printed name: the user's name, else organization, else masked email,
/// else the internal label, followed by the plan.
pub fn display_name(
    label: &str,
    name: Option<&str>,
    org: Option<&str>,
    email: Option<&str>,
    plan: Option<&str>,
) -> String {
    let who = name
        .or(org)
        .or(email)
        .map(str::to_owned)
        .unwrap_or_else(|| label.to_owned());
    match plan {
        Some(plan) if !who.eq_ignore_ascii_case(plan) => format!("{who} · {plan}"),
        _ => who,
    }
}

#[allow(clippy::too_many_arguments)]
fn identity(
    provider: &str,
    label: String,
    email: Option<String>,
    raw_plan: Option<String>,
    active: bool,
    meta: &MetaFile,
) -> AccountIdentity {
    let saved = meta.accounts.get(&key(provider, &label));
    let name = saved.and_then(|meta| meta.name.clone());
    let org = saved.and_then(|meta| meta.org.clone());
    let email = email.as_deref().map(crate::usage::mask_email);
    let plan = raw_plan.as_deref().and_then(plan_display);
    AccountIdentity {
        provider: provider.to_owned(),
        display_name: display_name(
            &label,
            name.as_deref(),
            org.as_deref(),
            email.as_deref(),
            plan.as_deref(),
        ),
        kind: default_kind(provider, raw_plan.as_deref(), false),
        label,
        name,
        email,
        plan,
        active,
    }
}

/// Every saved OAuth login, with the names the user gave them.
pub fn oauth_identities() -> Vec<AccountIdentity> {
    let meta = load();
    let mut identities = Vec::new();
    let claude_active = crate::auth::claude::active_account_label();
    for account in crate::auth::claude::list_accounts().unwrap_or_default() {
        identities.push(identity(
            "claude",
            account.label.clone(),
            account.email,
            account.subscription_type,
            claude_active.as_deref() == Some(account.label.as_str()),
            &meta,
        ));
    }
    let openai_active = crate::auth::codex::active_account_label();
    for account in crate::auth::codex::list_accounts().unwrap_or_default() {
        let plan = account
            .id_token
            .as_deref()
            .and_then(crate::auth::codex::extract_subscription_type)
            .or_else(|| crate::auth::codex::extract_subscription_type(&account.access_token));
        identities.push(identity(
            "openai",
            account.label.clone(),
            account.email,
            plan,
            openai_active.as_deref() == Some(account.label.as_str()),
            &meta,
        ));
    }
    tell_apart(&mut identities);
    identities
}

/// Two logins of one provider can read the same (one email on two plans of the
/// same name). Rows that read alike must still be told apart, so the internal
/// label is appended to every duplicate.
fn tell_apart(identities: &mut [AccountIdentity]) {
    let counts: BTreeMap<(String, String), usize> =
        identities
            .iter()
            .fold(BTreeMap::new(), |mut counts, identity| {
                *counts
                    .entry((identity.provider.clone(), identity.display_name.clone()))
                    .or_default() += 1;
                counts
            });
    for identity in identities {
        if counts[&(identity.provider.clone(), identity.display_name.clone())] > 1 {
            identity.display_name = format!("{} ({})", identity.display_name, identity.label);
        }
    }
}

/// What a login request names: an existing label, an existing user name, or
/// a new name. The returned label is the one to log in to (`None` when the
/// request is new), and the name is what to remember for a new login.
pub fn resolve_request(
    provider: &str,
    requested: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(requested) = requested.and_then(clean_name) else {
        return (None, None);
    };
    let known = oauth_identities()
        .into_iter()
        .filter(|identity| identity.provider == provider)
        .find(|identity| {
            identity.label == requested
                || identity
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&requested))
        });
    match known {
        Some(identity) => (Some(identity.label), None),
        None => (None, Some(requested)),
    }
}

/// The saved OAuth login with this provider and label.
pub fn oauth_identity(provider: &str, label: &str) -> Option<AccountIdentity> {
    oauth_identities()
        .into_iter()
        .find(|identity| identity.provider == provider && identity.label == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_text_reads_like_the_plan_name() {
        for (raw, shown) in [
            ("max_20x", "Max 20x"),
            ("max_5x", "Max 5x"),
            ("max", "Max"),
            ("team", "Team"),
            ("enterprise", "Enterprise"),
            ("plus", "Plus"),
            ("PRO", "Pro"),
        ] {
            assert_eq!(plan_display(raw).as_deref(), Some(shown), "{raw}");
        }
        assert_eq!(plan_display("  "), None);
    }

    #[test]
    fn a_login_is_named_by_the_user_then_the_organization_then_the_email() {
        let show = |name, org, email| display_name("claude-fox", name, org, email, Some("Max 20x"));
        assert_eq!(
            show(Some("team"), Some("Acme"), Some("t***m@x.io")),
            "team · Max 20x"
        );
        assert_eq!(
            show(None, Some("Acme"), Some("t***m@x.io")),
            "Acme · Max 20x"
        );
        assert_eq!(show(None, None, Some("t***m@x.io")), "t***m@x.io · Max 20x");
        assert_eq!(show(None, None, None), "claude-fox · Max 20x");
        assert_eq!(
            display_name("claude-fox", None, None, None, None),
            "claude-fox"
        );
    }

    #[test]
    fn default_kinds_follow_how_the_login_is_paid() {
        assert_eq!(
            default_kind("claude", Some("max_20x"), false),
            CredentialKind::Subscription
        );
        assert_eq!(
            default_kind("claude", Some("team"), false),
            CredentialKind::Subscription
        );
        assert_eq!(
            default_kind("claude", Some("enterprise"), false),
            CredentialKind::Credits
        );
        assert_eq!(
            default_kind("openai", Some("pro"), false),
            CredentialKind::Subscription
        );
        assert_eq!(
            default_kind("openai", Some("business"), false),
            CredentialKind::Credits
        );
        assert_eq!(
            default_kind("openai", None, false),
            CredentialKind::Subscription
        );
        assert_eq!(
            default_kind("openai", Some("pro"), true),
            CredentialKind::Metered
        );
    }

    #[test]
    fn names_are_unique_per_provider_and_survive_in_the_sidecar() {
        let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        set_name("claude", "claude-otter", Some(" team ")).unwrap();
        assert!(set_name("claude", "claude-fox", Some("TEAM")).is_err());
        // Another provider may reuse the name.
        set_name("openai", "openai-otter", Some("team")).unwrap();
        assert_eq!(
            load().accounts["claude:claude-otter"].name.as_deref(),
            Some("team")
        );
        set_name("claude", "claude-otter", None).unwrap();
        assert!(!load().accounts.contains_key("claude:claude-otter"));
        assert!(set_name("claude", "claude-fox", Some("  ")).is_err());
        assert!(set_name("claude", "claude-fox", Some(&"x".repeat(41))).is_err());
    }
}
