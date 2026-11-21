//! Deterministic model-catalog ordering for the `/model` pickers.
//!
//! Both clients order a provider group the same way, so a row sits where the
//! eye expects it in the terminal and in Desktop:
//!
//! 1. vendor namespace (`anthropic/…`), so one vendor's models stay together;
//! 2. base releases before `~latest` aliases;
//! 3. newest release first (`claude-opus-5-5` before `claude-opus-4-8`);
//! 4. family name (`claude-opus` before `claude-sonnet`);
//! 5. `:batch` and `[1m]`-style variants right behind their base model.
//!
//! Usage never takes part: a route used last week must not jump above a newer
//! model of the same family, and the list must not reorder between sessions.

use std::cmp::Ordering;

/// How one model id sorts inside its provider group.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModelKey {
    /// Slash-qualified namespace (`anthropic` in `anthropic/claude-opus-5-5`).
    pub vendor: String,
    /// Family words with the version removed (`claude-opus`, `gpt`).
    pub family: String,
    /// Generation numbers, in id order.
    pub version: Vec<u64>,
    /// A `~`-prefixed or `latest`-named alias of the route before it.
    pub alias: bool,
    /// Trailing `:tag` or `[bracket]` marker of the same base model.
    pub variant: String,
}

impl ModelKey {
    /// A base release, not an alias or a tagged variant.
    pub fn primary(&self) -> bool {
        !self.alias && self.variant.is_empty()
    }

    /// Order inside one provider group: vendor, base before alias, newest
    /// release first, then family and variant name.
    pub fn compare(&self, other: &Self) -> Ordering {
        self.vendor
            .cmp(&other.vendor)
            .then_with(|| self.alias.cmp(&other.alias))
            .then_with(|| other.version.cmp(&self.version))
            .then_with(|| self.family.cmp(&other.family))
            .then_with(|| self.variant.cmp(&other.variant))
    }
}

/// Split a catalog model id into its sortable parts.
///
/// Route prefixes (`openai-oauth:`), account pins (`@work`) and long-context
/// markers are stripped here so a prefixed id sorts with its bare model.
pub fn model_key(raw: &str) -> ModelKey {
    let bare = crate::explicit_model_provider_prefix(raw)
        .map_or(raw, |(_, _, bare)| bare)
        .to_ascii_lowercase();
    let bare = bare.split('@').next().unwrap_or(&bare).to_string();
    // An alias marker is a leading `~` (a catalog-level alias row) or a
    // trailing `~latest` (an alias of the model named before it).
    let (bare, mut alias) = match bare.split_once('~') {
        Some((base, _marker)) => (base.to_string(), true),
        None => (bare, false),
    };
    let (vendor, name) = bare.rsplit_once('/').unwrap_or(("", bare.as_str()));
    let vendor = vendor.trim_start_matches('~').to_string();
    let mut name = name.to_string();
    let mut variant = String::new();
    if let Some(index) = name.find('[') {
        variant = name.split_off(index);
    }
    if let Some((head, tag)) = name.split_once(':') {
        variant = format!("{tag}{variant}");
        name = head.to_string();
    }
    let mut family = Vec::new();
    let mut version = Vec::new();
    for token in name.split(['-', '_']).filter(|token| !token.is_empty()) {
        let numeric = token.strip_prefix('v').unwrap_or(token);
        if token == "latest" {
            alias = true;
        } else if !numeric.is_empty()
            && numeric.chars().all(|c| c.is_ascii_digit() || c == '.')
            && numeric.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            // Release stamps (`20251001`) are longer than any generation
            // number, and would otherwise read as one.
            version.extend(
                numeric
                    .split('.')
                    .filter(|part| (1..6).contains(&part.len()))
                    .filter_map(|part| part.parse::<u64>().ok()),
            );
        } else {
            family.push(token);
        }
    }
    ModelKey {
        vendor,
        family: family.join("-"),
        version,
        alias,
        variant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(ids: &[&str]) -> Vec<String> {
        let mut keyed: Vec<(ModelKey, String)> = ids
            .iter()
            .map(|id| (model_key(id), (*id).to_string()))
            .collect();
        keyed.sort_by(|(a, a_id), (b, b_id)| a.compare(b).then_with(|| a_id.cmp(b_id)));
        keyed.into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn newest_release_first_then_family_then_alias_and_variant() {
        assert_eq!(
            sorted(&[
                "gpt-5.5",
                "gpt-6.1-sol",
                "gpt-5-mini",
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.3-codex",
                "gpt-5.6-sol:batch",
                "~latest",
            ]),
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-sol:batch",
                "gpt-5.5",
                "gpt-5.3-codex",
                "gpt-5-mini",
                "~latest",
            ]
        );
    }

    #[test]
    fn keys_split_vendor_family_version_alias_and_variant() {
        let key = model_key("anthropic/claude-opus-5-5");
        assert_eq!(key.vendor, "anthropic");
        assert_eq!(key.family, "claude-opus");
        assert_eq!(key.version, [5, 5]);
        assert!(key.primary());

        let alias = model_key("claude-opus-5-5~latest");
        assert!(alias.alias && !alias.primary());
        let variant = model_key("anthropic/claude-opus-5-5:batch");
        assert_eq!(variant.variant, "batch");
        assert!(!variant.primary());
        let bracket = model_key("claude-opus-5-5[1m]");
        assert_eq!(bracket.variant, "[1m]");

        // Route prefixes and account pins sort with their bare id.
        assert_eq!(
            model_key("openai-oauth:gpt-5.6-sol"),
            model_key("gpt-5.6-sol")
        );
        assert_eq!(
            model_key("claude-opus-5-5@work"),
            model_key("claude-opus-5-5")
        );
        // A release stamp is not a generation.
        assert_eq!(model_key("claude-haiku-4-5-20251001").version, [4, 5]);
        // A family without a separate version number is its own family.
        assert_eq!(model_key("o3").family, "o3");
        assert!(model_key("o3").version.is_empty());
        assert_eq!(model_key("gemini-3.1-pro-preview").version, [3, 1]);
    }

    #[test]
    fn aliases_and_variants_follow_their_base_and_dates_do_not_reorder() {
        let ids = sorted(&[
            "claude-haiku-4-5-20251001",
            "claude-haiku-4-5",
            "claude-opus-5-5~latest",
            "claude-opus-5-5",
            "claude-opus-5-5:batch",
            "claude-opus-4-8",
        ]);
        assert_eq!(
            ids,
            [
                // Releases newest first, variants behind their base, and every
                // `~latest` alias after the releases it aliases.
                "claude-opus-5-5",
                "claude-opus-5-5:batch",
                "claude-opus-4-8",
                "claude-haiku-4-5",
                "claude-haiku-4-5-20251001",
                "claude-opus-5-5~latest",
            ]
        );
    }

    #[test]
    fn key_order_is_unchanged_by_input_order() {
        let ids = ["gpt-5.5", "gpt-6-astra", "gpt-5.6-sol", "gpt-5.3-codex"];
        let mut reversed = ids.to_vec();
        reversed.reverse();
        assert_eq!(sorted(&ids), sorted(&reversed));
    }
}
