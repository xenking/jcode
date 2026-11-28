//! One stable emoji per account, shown first wherever the account is named.
//!
//! Account labels are animal names (`claude-otter`, `openai-fox`), so the
//! default is the animal's emoji. A label that names no known animal gets an
//! emoji picked by a stable hash of the label from the same palette, so the
//! icon never changes between runs or machines. `[[routing.accounts]] icon`
//! overrides either.
//!
//! An icon exists to tell accounts of one provider apart, so the default is
//! shown only for a provider with more than one account ([`disambiguating_icon`]);
//! a lone account shows an icon only when its owner configured one.

/// `(animal, emoji)` for the animals accounts are usually named after.
const ANIMALS: &[(&str, &str)] = &[
    ("otter", "🦦"),
    ("fox", "🦊"),
    ("panda", "🐼"),
    ("wolf", "🐺"),
    ("bear", "🐻"),
    ("cat", "🐱"),
    ("dog", "🐶"),
    ("lion", "🦁"),
    ("tiger", "🐯"),
    ("owl", "🦉"),
    ("eagle", "🦅"),
    ("hawk", "🦅"),
    ("whale", "🐋"),
    ("dolphin", "🐬"),
    ("shark", "🦈"),
    ("octopus", "🐙"),
    ("penguin", "🐧"),
    ("rabbit", "🐰"),
    ("koala", "🐨"),
    ("monkey", "🐵"),
    ("frog", "🐸"),
    ("turtle", "🐢"),
    ("snake", "🐍"),
    ("horse", "🐴"),
    ("cow", "🐮"),
    ("pig", "🐷"),
    ("sheep", "🐑"),
    ("goat", "🐐"),
    ("deer", "🦌"),
    ("camel", "🐫"),
    ("elephant", "🐘"),
    ("giraffe", "🦒"),
    ("zebra", "🦓"),
    ("butterfly", "🦋"),
    ("bee", "🐝"),
    ("ant", "🐜"),
    ("crab", "🦀"),
    ("fish", "🐟"),
    ("dragon", "🐉"),
    ("raccoon", "🦝"),
    ("squirrel", "🐿️"),
    ("hedgehog", "🦔"),
    ("badger", "🦡"),
    ("sloth", "🦥"),
    ("llama", "🦙"),
    ("parrot", "🦜"),
    ("flamingo", "🦩"),
    ("swan", "🦢"),
    ("duck", "🦆"),
    ("chicken", "🐔"),
    ("buffalo", "🦬"),
    ("hippo", "🦛"),
    ("rhino", "🦏"),
    ("kangaroo", "🦘"),
    ("bat", "🦇"),
    ("mouse", "🐭"),
    ("beaver", "🦫"),
    ("lobster", "🦞"),
    ("squid", "🦑"),
    ("scorpion", "🦂"),
    ("peacock", "🦚"),
    ("crocodile", "🐊"),
    ("clover", "🍀"),
];

/// The icon of `account` (`provider:label`, or just a label). A non-blank
/// `configured` icon wins.
pub fn account_icon(account: &str, configured: Option<&str>) -> String {
    if let Some(icon) = configured.map(str::trim).filter(|icon| !icon.is_empty()) {
        return icon.to_owned();
    }
    let label = account.rsplit(':').next().unwrap_or(account).to_lowercase();
    if let Some((_, icon)) = label
        .split(['-', '_', ' ', '.'])
        .rev()
        .find_map(|word| ANIMALS.iter().find(|(animal, _)| *animal == word))
    {
        return (*icon).to_owned();
    }
    // FNV-1a: fixed constants, so the pick is the same on every machine.
    let hash = label.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    ANIMALS[(hash % ANIMALS.len() as u64) as usize].1.to_owned()
}

/// The provider of an account id (`claude:claude-fox` -> `claude`; `openrouter` -> `openrouter`).
pub fn provider_of(account: &str) -> &str {
    account
        .split_once(':')
        .map_or(account, |(provider, _)| provider)
}

/// The icon to show for `account` when its provider has `provider_accounts`
/// accounts (itself included): the configured icon always, the default only
/// where it tells accounts apart. Every surface that names an account uses this.
pub fn disambiguating_icon(
    account: &str,
    configured: Option<&str>,
    provider_accounts: usize,
) -> Option<String> {
    let configured = configured.map(str::trim).filter(|icon| !icon.is_empty());
    (configured.is_some() || provider_accounts > 1).then(|| account_icon(account, configured))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_animals_map_to_their_emoji_whatever_the_provider_prefix() {
        assert_eq!(account_icon("claude:claude-otter", None), "🦦");
        assert_eq!(account_icon("claude-fox", None), "🦊");
        assert_eq!(account_icon("claude:claude-panda", None), "🐼");
        assert_eq!(account_icon("openai:openai-otter", None), "🦦");
    }

    #[test]
    fn an_unknown_label_gets_a_stable_palette_icon() {
        let first = account_icon("claude:team-acme", None);
        assert_eq!(first, account_icon("claude:team-acme", None));
        assert!(ANIMALS.iter().any(|(_, icon)| *icon == first));
        assert_ne!(account_icon("a-b-c", None), "");
    }

    #[test]
    fn a_configured_icon_overrides_the_default_and_a_blank_one_does_not() {
        assert_eq!(account_icon("claude:claude-otter", Some(" 🚀 ")), "🚀");
        assert_eq!(account_icon("claude:claude-otter", Some("  ")), "🦦");
    }

    #[test]
    fn a_default_icon_only_tells_accounts_of_one_provider_apart() {
        assert_eq!(disambiguating_icon("claude:claude-fox", None, 1), None);
        assert_eq!(disambiguating_icon("claude:claude-fox", None, 0), None);
        assert_eq!(
            disambiguating_icon("claude:claude-fox", None, 2),
            Some("🦊".into())
        );
        assert_eq!(
            disambiguating_icon("claude:claude-fox", Some("🚀"), 1),
            Some("🚀".into()),
            "an icon its owner set stays"
        );
        assert_eq!(disambiguating_icon("claude:claude-fox", Some(" "), 1), None);
        assert_eq!(provider_of("claude:claude-fox"), "claude");
        assert_eq!(provider_of("openrouter"), "openrouter");
    }
}
