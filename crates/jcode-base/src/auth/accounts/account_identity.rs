//! Who a saved login is, in words a person recognises.
//!
//! Internal labels (`claude-fox`, `openai-otter`) are stable ids, not names.
//! The name shown for a login is the one the user gave it, else the
//! organization or masked email, plus the plan: `team · Max 20x`.
//!
//! Names and organizations come from the `account_names` sidecar.

use super::account_names::NameBook;
pub use super::account_names::{clean_name, forget, set_name, set_org};
use jcode_usage_types::{AccountIdentity, CredentialKind};
use std::collections::BTreeMap;

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
    names: &NameBook,
) -> AccountIdentity {
    let name = names.name(provider, &label).map(str::to_owned);
    let org = names.org(provider, &label).map(str::to_owned);
    let email = email.as_deref().map(crate::usage::mask_email);
    let plan = raw_plan.as_deref().and_then(plan_display);
    AccountIdentity {
        icon: None,
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
    let names = NameBook::load();
    let mut identities = Vec::new();
    let claude_active = crate::auth::claude::active_account_label();
    for account in crate::auth::claude::list_accounts().unwrap_or_default() {
        identities.push(identity(
            "claude",
            account.label.clone(),
            account.email,
            account.subscription_type,
            claude_active.as_deref() == Some(account.label.as_str()),
            &names,
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
            &names,
        ));
    }
    tell_apart(&mut identities);
    give_icons(&mut identities);
    identities
}

/// The default icon only where a provider has several logins; an icon the user
/// configured stays. Counted here, not via the routing list, which is built
/// from these identities.
fn give_icons(identities: &mut [AccountIdentity]) {
    let routing = &crate::config::config().routing;
    for index in 0..identities.len() {
        let provider = identities[index].provider.clone();
        let siblings = identities
            .iter()
            .filter(|other| other.provider == provider)
            .count();
        let id = format!("{provider}:{}", identities[index].label);
        let configured = routing.account(&id, |raw| Some(raw.icon.clone())).flatten();
        identities[index].icon = jcode_session_types::account_icon::disambiguating_icon(
            &id,
            configured.as_deref(),
            siblings,
        );
    }
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

/// The saved Cursor login, named by its masked email and plan like the other logins.
pub fn cursor_identity(raw_plan: Option<&str>) -> Option<AccountIdentity> {
    let login = crate::auth::cursor::login::load_login()?;
    Some(identity(
        "cursor",
        login.label,
        login.email,
        raw_plan.map(str::to_owned),
        true,
        &NameBook::load(),
    ))
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
    fn the_saved_cursor_login_is_named_by_its_masked_email_and_plan() {
        let _guard = crate::storage::lock_test_env();
        let home = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        assert!(cursor_identity(Some("pro")).is_none());
        let tokens = crate::auth::cursor::CursorDirectTokens {
            access_token: "access".into(),
            refresh_token: None,
            source: crate::auth::cursor::login::CURSOR_LOGIN_SOURCE,
        };
        crate::auth::cursor::login::save_login_for_tests(
            None,
            &tokens,
            Some("developer@example.com".into()),
        )
        .unwrap();
        let identity = cursor_identity(Some("pro")).unwrap();
        match previous {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        assert_eq!(identity.label, "cursor-otter");
        assert_eq!(identity.plan.as_deref(), Some("Pro"));
        assert!(
            identity.display_name.ends_with("· Pro"),
            "{}",
            identity.display_name
        );
        assert!(
            !identity.display_name.contains("developer@"),
            "{}",
            identity.display_name
        );
    }
}
