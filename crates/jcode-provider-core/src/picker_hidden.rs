//! Which model routes a picker leaves out.
//!
//! `[provider] model_picker_hidden` is one list shared by the terminal `/model`
//! picker and the Desktop model picker, so hiding a route in one client hides
//! it in the other. Both clients ask this module rather than comparing strings
//! themselves, because two spellings name the same route:
//!
//! * the routed model spec the picker prints (`openai-oauth:gpt-5.6-sol`,
//!   `openai-api:gpt-5.6-sol`) hides one route;
//! * the bare model id (`gpt-5.6-sol`) hides every route of that model.

use crate::{ModelRoute, RouteSelection};

/// Trim and lowercase an entry or spec, so a hand-edited config still matches.
fn key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// The routed model spec for a route, as the pickers print it.
pub fn route_spec(route: &ModelRoute) -> String {
    RouteSelection::from_model_route(route).routed_model_spec()
}

/// Whether `route` is hidden by `hidden`.
///
/// An entry hides a route when it names that route's spec or its bare model id.
/// Empty entries are ignored rather than hiding everything.
pub fn route_is_hidden(hidden: &[String], route: &ModelRoute) -> bool {
    if hidden.is_empty() {
        return false;
    }
    let spec = key(&route_spec(route));
    let model = key(&route.model);
    hidden.iter().any(|entry| {
        let entry = key(entry);
        !entry.is_empty() && (entry == spec || entry == model)
    })
}

/// Sorted, deduplicated, non-empty hidden entries. The pickers write this back,
/// so a repeated toggle cannot grow the list.
pub fn normalized(hidden: &[String]) -> Vec<String> {
    let mut entries: Vec<String> = hidden
        .iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    entries.sort();
    entries.dedup();
    entries
}

/// `hidden` after flipping whether `spec` is hidden.
pub fn toggled(hidden: &[String], spec: &str) -> Vec<String> {
    let spec = spec.trim();
    let mut entries: Vec<String> = hidden
        .iter()
        .filter(|entry| key(entry) != key(spec))
        .cloned()
        .collect();
    if entries.len() == hidden.len() {
        entries.push(spec.to_string());
    }
    normalized(&entries)
}

/// Whether `entry` (as written in config) hides `spec`.
pub fn matches(entry: &str, spec: &str) -> bool {
    !entry.trim().is_empty() && key(entry) == key(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(model: &str, provider: &str, api_method: &str) -> ModelRoute {
        ModelRoute {
            speed_options: Vec::new(),
            reasoning_efforts: None,
            model: model.into(),
            provider: provider.into(),
            api_method: api_method.into(),
            available: true,
            detail: String::new(),
            usage: None,
            cheapness: None,
        }
    }

    #[test]
    fn a_spec_hides_one_route_and_a_bare_id_hides_every_route_of_that_model() {
        let oauth = route("gpt-5.6-sol", "OpenAI", "openai-oauth");
        let api = route("gpt-5.6-sol", "OpenAI", "openai-api-key");
        let spec = route_spec(&oauth);
        assert_eq!(spec, "openai-oauth:gpt-5.6-sol");
        assert!(!route_is_hidden(&[], &oauth));

        // Model-level entry: both routes of that model go away.
        assert!(route_is_hidden(&["gpt-5.6-sol".into()], &oauth));
        assert!(route_is_hidden(&["gpt-5.6-sol".into()], &api));

        // Route-level entry: only the named route goes away.
        assert!(route_is_hidden(&[spec], &oauth));
        assert!(!route_is_hidden(&["openai-oauth:gpt-5.6-sol".into()], &api));

        // Hand-edited casing and padding still match, and unrelated routes stay.
        assert!(route_is_hidden(
            &[" OpenAI-OAuth:GPT-5.6-Sol ".into()],
            &oauth
        ));
        assert!(!route_is_hidden(&["".into()], &oauth));
        assert!(!route_is_hidden(
            &["claude-oauth:claude-opus-5-5".into()],
            &oauth
        ));
    }

    #[test]
    fn toggling_is_symmetric_normalized_and_case_insensitive() {
        assert_eq!(toggled(&[], " b:m "), ["b:m"]);
        assert_eq!(toggled(&["b:m".into()], "B:M"), Vec::<String>::new());
        assert_eq!(
            toggled(&["z:1".into(), "a:1".into()], "a:1"),
            ["z:1"],
            "removal keeps the written spelling of the others"
        );
        assert_eq!(
            normalized(&[" b ".into(), "a".into(), "".into(), "a".into()]),
            ["a", "b"]
        );
        assert!(matches("a:1", " A:1 "));
        assert!(!matches("", "a:1"));
    }
}
