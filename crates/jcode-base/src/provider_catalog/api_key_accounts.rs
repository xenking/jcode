//! Several labelled API keys per provider.
//!
//! OAuth logins already carry labels. This module gives API-key providers the
//! same shape: every built-in OpenAI-compatible profile and every
//! `[providers.<name>]` entry can hold the original unlabelled key plus any
//! number of labelled ones. Account ids match `[[routing.accounts]] id`:
//! `opencode-go` is the unlabelled key, `opencode-go:second` a labelled one.
//!
//! A labelled key comes from one of two places:
//! - the private store (`api-key-accounts.json`, mode 0600), written by
//!   `jcode login <provider> --account <label>`;
//! - `key_command` under `keys.<label>` in `config.toml`, an argv array run
//!   without a shell whose trimmed stdout is the key. Output is cached in
//!   memory and never logged.
//!
//! The store also records which key requests use right now (`active`). With no
//! active label the unlabelled key serves, so existing setups are unchanged.

use super::*;
use crate::config::{ApiKeyProfileConfig, Config, LabelledApiKeyConfig, NamedProviderConfig};
use anyhow::{Context, Result};
use jcode_routing_policy::AccountId;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

const STORE_FILE: &str = "api-key-accounts.json";
const STORE_LOCK: &str = "api-key-accounts";
const DEFAULT_KEY_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_KEY_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// A resolved credential. `api_key` stays out of `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKeyCredential {
    pub account_id: AccountId,
    pub api_key: String,
    /// Headers to send with the key: provider headers overlaid by the key's own.
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ApiKeyCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyCredential")
            .field("account_id", &self.account_id)
            .field("api_key", &"<redacted>")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Where an account's key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeySource {
    /// The original single key: environment, provider env file or inline config.
    Default,
    /// Saved by `jcode login --account`.
    Stored,
    /// Produced by `key_command`.
    KeyCommand,
}

/// Account metadata. Never carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyAccount {
    pub id: AccountId,
    pub label: Option<String>,
    pub source: ApiKeySource,
    /// Requests use this account.
    pub active: bool,
}

/// How a provider id maps to its credential sources.
enum Provider<'a> {
    Builtin(OpenAiCompatibleProfile, Option<&'a ApiKeyProfileConfig>),
    Named(&'a str, &'a NamedProviderConfig),
}

impl<'a> Provider<'a> {
    fn resolve(config: &'a Config, provider: &str) -> Option<Self> {
        let provider = provider.trim();
        AccountId::parse(provider).filter(|id| id.label().is_none())?;
        if let Some(profile) = openai_compatible_profile_by_id(provider) {
            // The generic profile's id is the ledger prefix, and the Muse
            // subscription logs in by device code, so neither takes labels.
            if profile.id == OPENAI_COMPAT_PROFILE.id || profile.id == MUSE_SUBSCRIPTION_PROFILE.id
            {
                return None;
            }
            return Some(Self::Builtin(
                profile,
                config.api_key_profiles.get(profile.id),
            ));
        }
        let (name, named) = config.providers.get_key_value(provider)?;
        // Anthropic-compatible providers take their key through a different
        // runtime path that has no labelled-key support.
        (!matches!(
            named.provider_type,
            crate::config::NamedProviderType::AnthropicCompatible
        ))
        .then_some(Self::Named(name.as_str(), named))
    }

    fn id(&self) -> &str {
        match self {
            Self::Builtin(profile, _) => profile.id,
            Self::Named(name, _) => name,
        }
    }

    fn headers(&self) -> Option<&'a BTreeMap<String, String>> {
        match self {
            Self::Builtin(_, config) => config.map(|config| &config.headers),
            Self::Named(_, config) => Some(&config.headers),
        }
    }

    fn keys(&self) -> Option<&'a BTreeMap<String, LabelledApiKeyConfig>> {
        match self {
            Self::Builtin(_, config) => config.map(|config| &config.keys),
            Self::Named(_, config) => Some(&config.keys),
        }
    }

    fn account(&self, label: Option<&str>) -> Option<AccountId> {
        AccountId::parse(&match label {
            Some(label) => format!("{}:{label}", self.id()),
            None => self.id().to_string(),
        })
    }

    /// Where the unlabelled key comes from, when one is configured.
    fn default_source(&self) -> Option<ApiKeySource> {
        match self {
            Self::Builtin(profile, _) => {
                let resolved = resolve_openai_compatible_profile_endpoint(*profile, None);
                load_api_key_from_env_or_config(&resolved.api_key_env, &resolved.env_file)
                    .map(|_| ApiKeySource::Default)
            }
            Self::Named(_, config) => {
                if !config.key_command.is_empty() {
                    return Some(ApiKeySource::KeyCommand);
                }
                named_provider_static_key(config).map(|_| ApiKeySource::Default)
            }
        }
    }
}

fn named_provider_static_key(config: &NamedProviderConfig) -> Option<String> {
    let key_env = config
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let from_env = key_env.and_then(|name| {
        match config
            .env_file
            .as_deref()
            .map(str::trim)
            .filter(|file| !file.is_empty())
        {
            Some(file) => load_api_key_from_env_or_config(name, file),
            None => std::env::var(name)
                .ok()
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty()),
        }
    });
    from_env.or_else(|| {
        config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(ToString::to_string)
    })
}

// ---------------------------------------------------------------------------
// Private store
// ---------------------------------------------------------------------------

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Store {
    #[serde(default)]
    providers: BTreeMap<String, StoredProvider>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StoredProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

fn store_path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::app_config_dir()?.join(STORE_FILE))
}

fn load_store() -> Store {
    let Ok(path) = store_path() else {
        return Store::default();
    };
    crate::storage::harden_secret_file_permissions(&path);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Store::default();
    };
    serde_json::from_str(&raw).unwrap_or_else(|_| {
        // Never include file content: it holds secrets.
        crate::logging::warn("Ignoring unreadable api-key-accounts.json");
        Store::default()
    })
}

/// Read-modify-write under the cross-process store lock.
fn update_store<T>(change: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let dir = crate::storage::app_config_dir()?;
    let _lock = crate::auth::account_store::lock_store(&dir, STORE_LOCK)?;
    let mut store = load_store();
    let value = change(&mut store)?;
    store
        .providers
        .retain(|_, entry| !entry.keys.is_empty() || entry.active.is_some());
    crate::storage::write_json_secret(&store_path()?, &store)?;
    Ok(value)
}

fn valid_label(provider: &str, label: &str) -> Result<()> {
    let label = label.trim();
    anyhow::ensure!(!label.is_empty(), "The key label cannot be empty.");
    anyhow::ensure!(
        AccountId::parse(&format!("{provider}:{label}"))
            .is_some_and(|id| id.label() == Some(label)),
        "Invalid key label '{label}'. Use letters, digits, '-', '_', '.' or '@'."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// key_command
// ---------------------------------------------------------------------------

type KeyCommandCache = Mutex<BTreeMap<Vec<String>, String>>;
static KEY_COMMAND_CACHE: LazyLock<KeyCommandCache> = LazyLock::new(Default::default);
static CLEAR_ON_CONFIG_RELOAD: Once = Once::new();

/// Forget every `key_command` result, so the next request runs the command again.
pub fn clear_key_command_cache() {
    KEY_COMMAND_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Run `argv` without a shell and return its trimmed stdout. Results are cached
/// in memory by argv, so an edited command runs again. Neither the output nor
/// the arguments are logged or put into errors.
fn run_key_command(account: &str, argv: &[String], timeout: Option<u64>) -> Result<String> {
    CLEAR_ON_CONFIG_RELOAD.call_once(|| crate::config::on_config_reloaded(clear_key_command_cache));
    if let Some(key) = KEY_COMMAND_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(argv)
    {
        return Ok(key.clone());
    }
    let program = argv
        .first()
        .map(|program| program.trim())
        .filter(|program| !program.is_empty())
        .with_context(|| format!("key_command for {account} is empty"))?;
    let timeout = timeout
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_KEY_COMMAND_TIMEOUT)
        .clamp(Duration::from_secs(1), MAX_KEY_COMMAND_TIMEOUT);
    let label = std::path::Path::new(program).file_name().map_or_else(
        || program.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );

    let mut child = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("key_command `{label}` for {account} could not start: {error}")
        })?;
    let mut stdout_pipe = child.stdout.take().context("capture key_command stdout")?;
    let mut stderr_pipe = child.stderr.take().context("capture key_command stderr")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout_pipe
            .by_ref()
            .take(64 * 1024)
            .read_to_string(&mut out);
        out
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stderr_pipe.by_ref().take(4 * 1024).read_to_string(&mut out);
        out
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "key_command `{label}` for {account} timed out after {}s",
                    timeout.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let key = jcode_provider_env::sanitize_secret_value(&stdout).to_string();

    if !status.success() {
        let detail = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| {
                if key.is_empty() {
                    line.to_string()
                } else {
                    line.replace(&key, "<redacted>")
                }
            })
            .map(|line| line.chars().take(200).collect::<String>())
            .map(|line| format!(": {line}"))
            .unwrap_or_default();
        anyhow::bail!(
            "key_command `{label}` for {account} failed ({status}){detail}",
            status = status.code().map_or_else(
                || "killed by signal".to_string(),
                |code| format!("exit {code}")
            )
        );
    }
    anyhow::ensure!(
        !key.is_empty(),
        "key_command `{label}` for {account} printed no key"
    );
    anyhow::ensure!(
        !key.contains(['\n', '\r']),
        "key_command `{label}` for {account} printed more than one line"
    );
    KEY_COMMAND_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(argv.to_vec(), key.clone());
    Ok(key)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

fn merged_headers(
    provider: &Provider<'_>,
    key: Option<&LabelledApiKeyConfig>,
) -> BTreeMap<String, String> {
    let mut headers = provider.headers().cloned().unwrap_or_default();
    if let Some(key) = key {
        headers.extend(key.headers.clone());
    }
    headers
}

/// Labels the provider knows: configured under `keys` or saved in the store.
fn labels(provider: &Provider<'_>, store: &Store) -> Vec<String> {
    let mut labels: Vec<String> = provider
        .keys()
        .into_iter()
        .flat_map(|keys| keys.keys().cloned())
        .collect();
    if let Some(stored) = store.providers.get(provider.id()) {
        labels.extend(stored.keys.keys().cloned());
    }
    labels.retain(|label| provider.account(Some(label)).is_some());
    labels.sort();
    labels.dedup();
    labels
}

fn credential_with(
    config: &Config,
    store: &Store,
    account: &AccountId,
) -> Result<Option<ApiKeyCredential>> {
    let Some(provider) = Provider::resolve(config, account.provider()) else {
        return Ok(None);
    };
    credential_for(&provider, store, account)
}

fn credential_for(
    provider: &Provider<'_>,
    store: &Store,
    account: &AccountId,
) -> Result<Option<ApiKeyCredential>> {
    let (api_key, key_config) = match account.label() {
        None => {
            let key = match provider {
                Provider::Builtin(profile, _) => {
                    let resolved = resolve_openai_compatible_profile_endpoint(*profile, None);
                    load_api_key_from_env_or_config(&resolved.api_key_env, &resolved.env_file)
                }
                Provider::Named(_, named) if !named.key_command.is_empty() => {
                    Some(run_key_command(
                        account.as_str(),
                        &named.key_command,
                        named.key_command_timeout_secs,
                    )?)
                }
                Provider::Named(_, named) => named_provider_static_key(named),
            };
            (key, None)
        }
        Some(label) => {
            let key_config = provider.keys().and_then(|keys| keys.get(label));
            let key = match key_config.filter(|config| !config.key_command.is_empty()) {
                Some(config) => Some(run_key_command(
                    account.as_str(),
                    &config.key_command,
                    config.key_command_timeout_secs,
                )?),
                None => store
                    .providers
                    .get(provider.id())
                    .and_then(|stored| stored.keys.get(label))
                    .map(|key| jcode_provider_env::sanitize_secret_value(key).to_string())
                    .filter(|key| !key.is_empty()),
            };
            (key, key_config)
        }
    };
    Ok(api_key.map(|api_key| ApiKeyCredential {
        account_id: account.clone(),
        api_key,
        headers: merged_headers(provider, key_config),
    }))
}

fn active_label(store: &Store, provider: &Provider<'_>) -> Option<String> {
    let active = store.providers.get(provider.id())?.active.as_deref()?;
    labels(provider, store)
        .into_iter()
        .find(|label| label == active)
}

/// Labelled keys that have a source, with where each comes from.
fn labelled_sources(provider: &Provider<'_>, store: &Store) -> Vec<(String, ApiKeySource)> {
    labels(provider, store)
        .into_iter()
        .filter_map(|label| {
            let commanded = provider
                .keys()
                .and_then(|keys| keys.get(&label))
                .is_some_and(|key| !key.key_command.is_empty());
            let stored = store
                .providers
                .get(provider.id())
                .is_some_and(|stored| stored.keys.contains_key(&label));
            let source = if commanded {
                ApiKeySource::KeyCommand
            } else if stored {
                ApiKeySource::Stored
            } else {
                return None;
            };
            Some((label, source))
        })
        .collect()
}

fn accounts_with(config: &Config, store: &Store, provider: &str) -> Vec<ApiKeyAccount> {
    let Some(provider) = Provider::resolve(config, provider) else {
        return Vec::new();
    };
    let active = active_label(store, &provider);
    let mut accounts = Vec::new();
    if let (Some(source), Some(id)) = (provider.default_source(), provider.account(None)) {
        accounts.push(ApiKeyAccount {
            id,
            label: None,
            source,
            active: active.is_none(),
        });
    }
    for (label, source) in labelled_sources(&provider, store) {
        let Some(id) = provider.account(Some(&label)) else {
            continue;
        };
        accounts.push(ApiKeyAccount {
            id,
            source,
            active: active.as_deref() == Some(label.as_str()),
            label: Some(label),
        });
    }
    accounts
}

/// The key and headers for one account. `Ok(None)` when the account has no
/// credential. A failing `key_command` is an error, never a silent fallback to
/// another key.
pub fn api_key_credential_for_account(account: &AccountId) -> Result<Option<ApiKeyCredential>> {
    credential_with(crate::config::config(), &load_store(), account)
}

pub(crate) fn api_key_credential_for_account_with(
    config: &Config,
    account: &AccountId,
) -> Result<Option<ApiKeyCredential>> {
    credential_with(config, &load_store(), account)
}

/// Every credential of an API-key provider, metadata only. The unlabelled key
/// comes first.
pub fn api_key_accounts(provider: &str) -> Vec<ApiKeyAccount> {
    api_key_accounts_with(crate::config::config(), provider)
}

pub(crate) fn api_key_accounts_with(config: &Config, provider: &str) -> Vec<ApiKeyAccount> {
    accounts_with(config, &load_store(), provider)
}

/// The account requests use right now: the active labelled key, else the
/// unlabelled one, else `None` when the provider has no key at all.
pub fn active_api_key_account(provider: &str) -> Option<AccountId> {
    accounts_with(crate::config::config(), &load_store(), provider)
        .into_iter()
        .find(|account| account.active)
        .map(|account| account.id)
}

/// The credential requests use right now.
pub fn active_api_key_credential(provider: &str) -> Result<Option<ApiKeyCredential>> {
    active_api_key_credential_with(crate::config::config(), provider)
}

pub(crate) fn active_api_key_credential_with(
    config: &Config,
    provider: &str,
) -> Result<Option<ApiKeyCredential>> {
    match Provider::resolve(config, provider) {
        Some(provider) => active_credential_of(&provider),
        None => Ok(None),
    }
}

fn active_credential_of(provider: &Provider<'_>) -> Result<Option<ApiKeyCredential>> {
    let store = load_store();
    let account = match active_label(&store, provider) {
        Some(label) => provider.account(Some(&label)),
        None => provider.account(None),
    };
    match account {
        Some(account) => credential_for(provider, &store, &account),
        None => Ok(None),
    }
}

/// The credential a `[providers.<name>]` entry serves requests with right now.
/// Takes the entry itself, so it also works for a profile not yet in the
/// loaded config.
pub fn active_named_provider_credential(
    name: &str,
    profile: &NamedProviderConfig,
) -> Result<Option<ApiKeyCredential>> {
    if matches!(
        profile.provider_type,
        crate::config::NamedProviderType::AnthropicCompatible
    ) {
        return Ok(None);
    }
    let provider = Provider::Named(name, profile);
    if provider.account(None).is_none() {
        return Ok(None);
    }
    active_credential_of(&provider)
}

/// Headers configured for a provider regardless of which key serves.
pub fn api_key_provider_headers(provider: &str) -> BTreeMap<String, String> {
    let config = crate::config::config();
    Provider::resolve(config, provider)
        .and_then(|provider| provider.headers().cloned())
        .unwrap_or_default()
}

/// Whether the provider has any key, labelled or not. Cheap enough for auth status.
pub fn api_key_provider_has_labelled_accounts(provider: &str) -> bool {
    Provider::resolve(crate::config::config(), provider)
        .is_some_and(|provider| !labelled_sources(&provider, &load_store()).is_empty())
}

/// Whether `provider` (a built-in profile id or a `[providers.<name>]` entry)
/// can hold labelled API keys.
pub fn api_key_provider_takes_labels(provider: &str) -> bool {
    Provider::resolve(crate::config::config(), provider).is_some()
}

/// Built-in profile that owns this credential binding, for callers that only
/// know the env var and file name.
pub fn api_key_profile_id_for_env(key_name: &str, env_file: &str) -> Option<&'static str> {
    openai_compatible_profiles()
        .iter()
        .find(|profile| profile.api_key_env == key_name && profile.env_file == env_file)
        .map(|profile| profile.id)
}

/// Ledger key for the account requests of `source_key` bill to: the exact
/// account id when a labelled key is active, otherwise `source_key` unchanged.
pub fn spend_ledger_key(source_key: &str) -> String {
    spend_ledger_key_with(crate::config::config(), source_key)
}

pub(crate) fn spend_ledger_key_with(config: &Config, source_key: &str) -> String {
    let provider = source_key
        .strip_prefix("openai-compatible:")
        .unwrap_or(source_key);
    let Some(provider) = Provider::resolve(config, provider) else {
        return source_key.to_string();
    };
    let store = load_store();
    active_label(&store, &provider)
        .and_then(|label| provider.account(Some(&label)))
        .map_or_else(|| source_key.to_string(), |id| id.to_string())
}

/// Ledger key under which spend for this exact account is recorded. The
/// unlabelled key keeps its original source key.
pub fn ledger_key_for_account(account: &AccountId) -> String {
    match account.label() {
        Some(_) => account.to_string(),
        None if openai_compatible_profile_by_id(account.provider()).is_some() => {
            format!("openai-compatible:{}", account.provider())
        }
        None => account.provider().to_string(),
    }
}

/// Display name for a labelled account's ledger key, e.g. `opencode-go:second`.
pub fn labelled_account_display_name(ledger_key: &str) -> Option<String> {
    let account = AccountId::parse(ledger_key)?;
    let label = account.label()?;
    let config = crate::config::config();
    let provider = Provider::resolve(config, account.provider())?;
    let name = match &provider {
        Provider::Builtin(profile, _) => format!("{} (API key)", profile.display_name),
        Provider::Named(name, _) => format!("{name} (API key)"),
    };
    Some(format!("{name} [{label}]"))
}

// ---------------------------------------------------------------------------
// Login, remove, switch
// ---------------------------------------------------------------------------

/// Save a labelled key. The first labelled key of a provider with no
/// unlabelled key becomes active, so `login --account` alone yields a working
/// setup. Otherwise the active key is unchanged.
pub fn save_labelled_api_key(provider: &str, label: &str, api_key: &str) -> Result<AccountId> {
    save_labelled_api_key_with(crate::config::config(), provider, label, api_key)
}

pub(crate) fn save_labelled_api_key_with(
    config: &Config,
    provider: &str,
    label: &str,
    api_key: &str,
) -> Result<AccountId> {
    let label = label.trim();
    let provider = Provider::resolve(config, provider)
        .with_context(|| format!("'{provider}' does not take labelled API keys."))?;
    valid_label(provider.id(), label)?;
    let key = normalize_pasted_api_key(api_key);
    anyhow::ensure!(!key.is_empty(), "API key cannot be empty.");
    let make_active = provider.default_source().is_none();
    let provider_id = provider.id().to_string();
    update_store(|store| {
        let entry = store.providers.entry(provider_id).or_default();
        entry.keys.insert(label.to_string(), key);
        if make_active && entry.active.is_none() {
            entry.active = Some(label.to_string());
        }
        Ok(())
    })?;
    crate::auth::AuthStatus::invalidate_cache();
    provider.account(Some(label)).context("invalid account id")
}

/// Remove a stored labelled key. Returns false when none was stored. A key
/// defined only by `key_command` in `config.toml` is removed there.
pub fn remove_labelled_api_key(provider: &str, label: &str) -> Result<bool> {
    remove_labelled_api_key_with(crate::config::config(), provider, label)
}

pub(crate) fn remove_labelled_api_key_with(
    config: &Config,
    provider: &str,
    label: &str,
) -> Result<bool> {
    let label = label.trim();
    let provider = Provider::resolve(config, provider)
        .with_context(|| format!("'{provider}' does not take labelled API keys."))?;
    let provider_id = provider.id().to_string();
    let removed = update_store(|store| {
        let Some(entry) = store.providers.get_mut(&provider_id) else {
            return Ok(false);
        };
        let removed = entry.keys.remove(label).is_some();
        if entry.active.as_deref() == Some(label) {
            entry.active = None;
        }
        Ok(removed)
    })?;
    crate::auth::AuthStatus::invalidate_cache();
    Ok(removed)
}

/// Choose the key requests use. `None` returns to the unlabelled key.
pub fn set_active_api_key_account(provider: &str, label: Option<&str>) -> Result<AccountId> {
    set_active_api_key_account_with(crate::config::config(), provider, label)
}

pub(crate) fn set_active_api_key_account_with(
    config: &Config,
    provider: &str,
    label: Option<&str>,
) -> Result<AccountId> {
    let provider = Provider::resolve(config, provider)
        .with_context(|| format!("'{provider}' does not take labelled API keys."))?;
    let provider_id = provider.id().to_string();
    let label = label.map(str::trim).filter(|label| !label.is_empty());
    update_store(|store| {
        if let Some(label) = label {
            anyhow::ensure!(
                labels(&provider, store).iter().any(|known| known == label),
                "No key labelled '{label}' for {provider_id}. Add it with `jcode login {provider_id} --account {label}`."
            );
        } else {
            anyhow::ensure!(
                provider.default_source().is_some(),
                "{provider_id} has no unlabelled key to return to."
            );
        }
        store
            .providers
            .entry(provider_id.clone())
            .or_default()
            .active = label.map(str::to_string);
        Ok(())
    })?;
    crate::auth::AuthStatus::invalidate_cache();
    provider.account(label).context("invalid account id")
}

#[cfg(test)]
#[path = "api_key_accounts_tests.rs"]
mod tests;
