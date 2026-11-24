//! `jcode auth keys`: several labelled API keys per provider.
//!
//! The unlabelled key is whatever `jcode login` saved before and keeps working.
//! A labelled key is added with `jcode login <provider> --account <label>` or
//! `jcode auth keys add`, and `jcode auth keys use` picks the one requests use.
//! Keys are stored with the other API keys (mode 0600) and never printed.

use super::*;
use crate::provider_catalog::{
    ApiKeyAccount, ApiKeySource, api_key_accounts, openai_compatible_profiles,
    remove_labelled_api_key, save_labelled_api_key, set_active_api_key_account,
};
use serde::Serialize;

#[derive(Serialize)]
struct KeyRow {
    provider: String,
    account: String,
    label: Option<String>,
    source: &'static str,
    active: bool,
}

fn source_name(source: ApiKeySource) -> &'static str {
    match source {
        ApiKeySource::Default => "default",
        ApiKeySource::Stored => "stored",
        ApiKeySource::KeyCommand => "key_command",
    }
}

fn rows_for(provider: &str) -> Vec<KeyRow> {
    api_key_accounts(provider)
        .into_iter()
        .map(|account: ApiKeyAccount| KeyRow {
            provider: provider.to_string(),
            account: account.id.to_string(),
            label: account.label,
            source: source_name(account.source),
            active: account.active,
        })
        .collect()
}

/// Every provider that can hold labelled keys and has at least one key.
fn provider_ids() -> Vec<String> {
    let mut ids: Vec<String> = openai_compatible_profiles()
        .iter()
        .map(|profile| profile.id.to_string())
        .collect();
    ids.extend(crate::config::config().providers.keys().cloned());
    ids.sort();
    ids.dedup();
    ids
}

pub(crate) fn run_keys_command(action: super::super::args::KeysCommand) -> Result<()> {
    use super::super::args::KeysCommand;
    match action {
        KeysCommand::List { provider, json } => {
            let providers = match provider {
                Some(provider) => vec![provider],
                None => provider_ids(),
            };
            let rows: Vec<KeyRow> = providers
                .iter()
                .flat_map(|provider| rows_for(provider))
                .collect();
            // A provider with only its default key is not worth a line.
            let several: Vec<&KeyRow> = rows
                .iter()
                .filter(|row| {
                    rows.iter()
                        .filter(|other| other.provider == row.provider)
                        .any(|other| other.label.is_some())
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&several)?);
                return Ok(());
            }
            if several.is_empty() {
                println!(
                    "No labelled API keys. Add one with `jcode login <provider> --account <label>`."
                );
                return Ok(());
            }
            for row in several {
                println!(
                    "{} {:<28} {}",
                    if row.active { "*" } else { " " },
                    row.account,
                    row.source
                );
            }
            Ok(())
        }
        KeysCommand::Add {
            provider,
            label,
            api_key,
            api_key_env,
        } => {
            let options = LoginOptions {
                openai_compatible_api_key: api_key,
                openai_compatible_api_key_env: api_key_env,
                ..Default::default()
            };
            let key = obtain_api_key(&options, &provider)?;
            let account = save_labelled_api_key(&provider, &label, &key)?;
            println!("Saved key {account}.");
            print_active_hint(&provider);
            Ok(())
        }
        KeysCommand::Remove { provider, label } => {
            if remove_labelled_api_key(&provider, &label)? {
                println!("Removed {provider}:{label}.");
                print_active_hint(&provider);
            } else {
                println!(
                    "No stored key {provider}:{label}. A key defined by `key_command` is removed from config.toml."
                );
            }
            Ok(())
        }
        KeysCommand::Use { provider, label } => {
            let account = set_active_api_key_account(&provider, label.as_deref())?;
            println!("Requests to {provider} now use {account}.");
            Ok(())
        }
    }
}

fn print_active_hint(provider: &str) {
    if let Some(active) = crate::provider_catalog::active_api_key_account(provider) {
        println!("Active: {active}");
    }
}

/// Save the key for `jcode login <provider> --account <label>`.
pub(super) fn save_login_labelled_key(
    profile_id: &str,
    display_name: &str,
    label: &str,
    key: &str,
) -> Result<()> {
    let account = save_labelled_api_key(profile_id, label, key)?;
    eprintln!("\nSaved {display_name} API key as {account}.");
    if crate::provider_catalog::active_api_key_account(profile_id).as_ref() == Some(&account) {
        eprintln!("Requests use it now.");
    } else {
        eprintln!("Switch to it with `jcode auth keys use {profile_id} {label}`.");
    }
    Ok(())
}
