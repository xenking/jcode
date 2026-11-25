//! Explicit, one-time transfer of one Jcode-owned OpenAI or Claude OAuth account.
//!
//! This is not credential discovery or synchronization. Only the selected/active
//! account in `openai-auth.json` or `auth.json` is read. External tools (including
//! previously trusted tools), keychains, environment credentials, API keys, AWS
//! credentials and general configuration are deliberately outside the supported
//! set. Callers must obtain consent before exporting and use a private transport.
//! No provider loaders are used: those may migrate, harden, refresh or discover
//! credentials. Export never writes to its source.
//!
//! Desktop import refuses *every* existing destination store, including an empty,
//! malformed, symlinked or other-provider-only store. Its no-replace publication
//! remains safe even with writers from older binaries that do not share a lock.
//! Cloud imports below use the shared account-store lock for read/merge/rename.
//! Other stores are never opened or modified. This is a copy, not a token move:
//! providers may later invalidate either copy when rotating refresh tokens.
//! OpenAI expiry prefers the stored timestamp, then the token's unverified JWT
//! `exp` claim. This is an offline expiry check, not authentication validation.
//! Opaque tokens without an explicit expiry remain supported.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

pub const MAX_TRANSFER_BYTES: usize = 64 * 1024;
const MAX_STORE_BYTES: usize = 1024 * 1024;
const TRANSFER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferProvider {
    OpenAi,
    Claude,
}

impl TransferProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
        }
    }

    fn store_name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai-auth.json",
            Self::Claude => "auth.json",
        }
    }
}

impl FromStr for TransferProvider {
    type Err = TransferError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "claude" => Ok(Self::Claude),
            _ => Err(TransferError::UnsupportedProvider),
        }
    }
}

/// Only static diagnostics. Never attach a parser/I/O error or credential value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferError {
    UnsupportedProvider,
    UnsupportedPlatform,
    Unavailable,
    InvalidStore,
    InvalidPayload,
    UnsupportedVersion,
    ProviderMismatch,
    TooLarge,
    Expired,
    ExistingStore,
    UnsafePath,
    Io,
    LabelMismatch,
    AccountConflict,
    ConcurrentUpdate,
}

impl TransferError {
    pub fn message(self) -> &'static str {
        match self {
            Self::UnsupportedProvider => {
                "Local login import supports only OpenAI and Claude OAuth."
            }
            Self::UnsupportedPlatform => {
                "Secure local login import is not supported on this platform."
            }
            Self::Unavailable => {
                "No selected Jcode-owned OAuth login is available for transfer. Use /login instead."
            }
            Self::InvalidStore => "The local OAuth store is malformed. Use /login instead.",
            Self::InvalidPayload => "The credential transfer payload is invalid.",
            Self::UnsupportedVersion => "The credential transfer version is not supported.",
            Self::ProviderMismatch => {
                "The credential transfer does not match the selected provider."
            }
            Self::TooLarge => "The credential transfer or source store exceeds the size limit.",
            Self::Expired => "The selected OAuth login has expired. Use /login instead.",
            Self::ExistingStore => {
                "The selected provider's destination store already exists. It was not changed. Use /login instead."
            }
            Self::UnsafePath => {
                "The credential store path is not a safe regular file or private directory."
            }
            Self::LabelMismatch => {
                "The cloud credential label does not match the requested account."
            }
            Self::AccountConflict => {
                "The account label belongs to a different or unverifiable identity."
            }
            Self::ConcurrentUpdate => "The account changed since export. It was not replaced.",
            Self::Io => "The credential store could not be accessed securely.",
        }
    }
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for TransferError {}

/// Secret bytes, intentionally without Debug, Display, Clone or Serialize.
/// Do not log these bytes, put them in argv, or persist a transport copy.
pub struct CredentialTransfer {
    bytes: Vec<u8>,
}

impl CredentialTransfer {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u32,
    provider: TransferProvider,
    credential: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiCredential {
    access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeCredential {
    access: String,
    refresh: String,
    expires: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription_type: Option<String>,
}

fn validate_tokens(access: &str, refresh: &str, expires: Option<i64>) -> Result<(), TransferError> {
    if access.trim().is_empty() || refresh.trim().is_empty() {
        return Err(TransferError::InvalidPayload);
    }
    if expires.is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis()) {
        return Err(TransferError::Expired);
    }
    Ok(())
}

fn serialize<T: Serialize>(
    provider: TransferProvider,
    credential: T,
) -> Result<CredentialTransfer, TransferError> {
    let bytes = serde_json::to_vec(&Envelope {
        version: TRANSFER_VERSION,
        provider,
        credential,
    })
    .map_err(|_| TransferError::InvalidPayload)?;
    if bytes.len() > MAX_TRANSFER_BYTES {
        return Err(TransferError::TooLarge);
    }
    Ok(CredentialTransfer { bytes })
}

/// Read-only availability, without refresh, migration or external discovery.
/// A missing/expired login is unavailable. Malformed stores remain errors.
pub fn available_local(provider: TransferProvider) -> Result<bool, TransferError> {
    match export_local(provider) {
        Ok(_) => Ok(true),
        Err(TransferError::Unavailable | TransferError::Expired) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Read-only availability in an explicitly isolated Jcode data directory.
pub fn available_at(home: &Path, provider: TransferProvider) -> Result<bool, TransferError> {
    match export_at(home, provider) {
        Ok(_) => Ok(true),
        Err(TransferError::Unavailable | TransferError::Expired) => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_labels<'a>(labels: impl Iterator<Item = &'a str>) -> Result<(), TransferError> {
    let mut seen = std::collections::HashSet::new();
    for label in labels {
        if label.trim().is_empty() || !seen.insert(label) {
            return Err(TransferError::InvalidStore);
        }
    }
    Ok(())
}

/// Export exactly the runtime-selected, stored-active, or first account, in that
/// order. An invalid explicit/active selection is refused, never substituted.
pub fn export_local(provider: TransferProvider) -> Result<CredentialTransfer, TransferError> {
    let home = crate::storage::jcode_dir().map_err(|_| TransferError::Io)?;
    let selected = super::account_store::runtime_active_override(provider.as_str());
    export_account_at(&home, provider, selected.as_deref())
}

/// Isolated-store equivalent of export, without process-global account overrides.
pub fn export_at(
    home: &Path,
    provider: TransferProvider,
) -> Result<CredentialTransfer, TransferError> {
    export_account_at(home, provider, None)
}

/// Read a specifically selected account, or the stored-active/first account.
/// `home` is a Jcode data directory, not a user home or external tool directory.
pub fn export_account_at(
    home: &Path,
    provider: TransferProvider,
    selected: Option<&str>,
) -> Result<CredentialTransfer, TransferError> {
    let bytes = read_store(&home.join(provider.store_name()))?;
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(TransferError::InvalidStore);
    }
    match provider {
        TransferProvider::OpenAi => {
            let store: super::codex::JcodeOpenAiAuthFile =
                serde_json::from_slice(&bytes).map_err(|_| TransferError::InvalidStore)?;
            validate_labels(
                store
                    .openai_accounts
                    .iter()
                    .map(|account| account.label.as_str()),
            )?;
            let label = selected.or(store.active_openai_account.as_deref());
            let account = match label {
                Some(label) => store
                    .openai_accounts
                    .iter()
                    .find(|account| account.label == label),
                None => store.openai_accounts.first(),
            }
            .ok_or(TransferError::Unavailable)?;
            let expires_at = account
                .expires_at
                .or_else(|| super::codex::expires_at_from_access_token(&account.access_token));
            validate_tokens(&account.access_token, &account.refresh_token, expires_at)?;
            serialize(
                provider,
                OpenAiCredential {
                    access_token: account.access_token.clone(),
                    refresh_token: account.refresh_token.clone(),
                    id_token: account.id_token.clone(),
                    account_id: account.account_id.clone(),
                    expires_at,
                },
            )
        }
        TransferProvider::Claude => {
            // Read legacy accounts without invoking the mutating migration loader.
            #[derive(Deserialize)]
            struct Store {
                #[serde(default)]
                anthropic_accounts: Vec<super::claude::AnthropicAccount>,
                active_anthropic_account: Option<String>,
                anthropic: Option<Legacy>,
            }
            #[derive(Deserialize)]
            struct Legacy {
                access: String,
                refresh: String,
                expires: i64,
            }
            let store: Store =
                serde_json::from_slice(&bytes).map_err(|_| TransferError::InvalidStore)?;
            validate_labels(
                store
                    .anthropic_accounts
                    .iter()
                    .map(|account| account.label.as_str()),
            )?;
            let label = selected.or(store.active_anthropic_account.as_deref());
            let credential = if store.anthropic_accounts.is_empty() && label.is_none() {
                let legacy = store.anthropic.ok_or(TransferError::Unavailable)?;
                ClaudeCredential {
                    access: legacy.access,
                    refresh: legacy.refresh,
                    expires: legacy.expires,
                    scopes: Vec::new(),
                    subscription_type: None,
                }
            } else {
                let account = match label {
                    Some(label) => store
                        .anthropic_accounts
                        .iter()
                        .find(|account| account.label == label),
                    None => store.anthropic_accounts.first(),
                }
                .ok_or(TransferError::Unavailable)?;
                ClaudeCredential {
                    access: account.access.clone(),
                    refresh: account.refresh.clone(),
                    expires: account.expires,
                    scopes: account.scopes.clone(),
                    subscription_type: account.subscription_type.clone(),
                }
            };
            validate_tokens(
                &credential.access,
                &credential.refresh,
                Some(credential.expires),
            )?;
            serialize(provider, credential)
        }
    }
}

fn read_store(path: &Path) -> Result<Vec<u8>, TransferError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TransferError::Unavailable
        } else {
            TransferError::Io
        }
    })?;
    let metadata = file.metadata().map_err(|_| TransferError::Io)?;
    if !metadata.is_file() {
        return Err(TransferError::UnsafePath);
    }
    if metadata.len() > MAX_STORE_BYTES as u64 {
        return Err(TransferError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| TransferError::Io)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(TransferError::TooLarge);
    }
    Ok(bytes)
}

/// Validate a bounded versioned payload and atomically create the selected
/// provider's store. Never merges with or replaces an existing destination.
pub fn import_local(provider: TransferProvider, bytes: &[u8]) -> Result<(), TransferError> {
    let home = crate::storage::jcode_dir().map_err(|_| TransferError::Io)?;
    import_at(&home, provider, bytes)?;
    super::AuthStatus::invalidate_cache();
    Ok(())
}

pub fn import_at(
    home: &Path,
    provider: TransferProvider,
    bytes: &[u8],
) -> Result<(), TransferError> {
    if bytes.len() > MAX_TRANSFER_BYTES {
        return Err(TransferError::TooLarge);
    }
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(TransferError::InvalidPayload);
    }
    let envelope: Envelope<Box<serde_json::value::RawValue>> =
        serde_json::from_slice(bytes).map_err(|_| TransferError::InvalidPayload)?;
    if envelope.version != TRANSFER_VERSION {
        return Err(TransferError::UnsupportedVersion);
    }
    if envelope.provider != provider {
        return Err(TransferError::ProviderMismatch);
    }
    if !envelope.credential.get().starts_with('{') {
        return Err(TransferError::InvalidPayload);
    }
    let store = match provider {
        TransferProvider::OpenAi => {
            let mut credential: OpenAiCredential = serde_json::from_str(envelope.credential.get())
                .map_err(|_| TransferError::InvalidPayload)?;
            credential.expires_at = credential
                .expires_at
                .or_else(|| super::codex::expires_at_from_access_token(&credential.access_token));
            validate_tokens(
                &credential.access_token,
                &credential.refresh_token,
                credential.expires_at,
            )?;
            serde_json::to_vec(&super::codex::JcodeOpenAiAuthFile {
                openai_accounts: vec![super::codex::OpenAiAccount {
                    label: "openai-otter".into(),
                    access_token: credential.access_token,
                    refresh_token: credential.refresh_token,
                    id_token: credential.id_token,
                    account_id: credential.account_id,
                    expires_at: credential.expires_at,
                    email: None,
                }],
                active_openai_account: Some("openai-otter".into()),
            })
        }
        TransferProvider::Claude => {
            let credential: ClaudeCredential = serde_json::from_str(envelope.credential.get())
                .map_err(|_| TransferError::InvalidPayload)?;
            validate_tokens(
                &credential.access,
                &credential.refresh,
                Some(credential.expires),
            )?;
            #[derive(Serialize)]
            struct Store {
                anthropic_accounts: Vec<super::claude::AnthropicAccount>,
                active_anthropic_account: &'static str,
            }
            serde_json::to_vec(&Store {
                anthropic_accounts: vec![super::claude::AnthropicAccount {
                    label: "claude-otter".into(),
                    access: credential.access,
                    refresh: credential.refresh,
                    expires: credential.expires,
                    email: None,
                    scopes: credential.scopes,
                    subscription_type: credential.subscription_type,
                }],
                active_anthropic_account: "claude-otter",
            })
        }
    }
    .map_err(|_| TransferError::InvalidPayload)?;
    secure_publish(home, provider.store_name(), &store)
}

#[cfg(not(unix))]
fn secure_publish(_: &Path, _: &str, _: &[u8]) -> Result<(), TransferError> {
    // Do not pretend Unix permission bits enforce a private Windows ACL.
    Err(TransferError::UnsupportedPlatform)
}

#[cfg(unix)]
fn secure_directory(home: &Path, create: bool) -> Result<std::fs::File, TransferError> {
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    // Walk directory descriptors, never following symlinks, including ancestors.
    // Publication remains anchored even if a parent path is renamed concurrently.
    let start = if home.is_absolute() { c"/" } else { c"." };
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(start.as_ptr(), flags) };
    if fd < 0 {
        return Err(TransferError::Io);
    }
    let mut directory = unsafe { File::from_raw_fd(fd) };
    let mut saw_component = false;
    for component in home.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            _ => return Err(TransferError::UnsafePath),
        };
        saw_component = true;
        let component =
            CString::new(component.as_bytes()).map_err(|_| TransferError::UnsafePath)?;
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), component.as_ptr(), flags) };
        if create
            && fd < 0
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            let created =
                unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o700) };
            if created < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(TransferError::Io);
            }
            fd = unsafe { libc::openat(directory.as_raw_fd(), component.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(
                if !create && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
                {
                    TransferError::Unavailable
                } else {
                    TransferError::UnsafePath
                },
            );
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    if !saw_component {
        return Err(TransferError::UnsafePath);
    }
    let metadata = directory.metadata().map_err(|_| TransferError::Io)?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TransferError::UnsafePath);
    }
    Ok(directory)
}

#[cfg(unix)]
fn secure_publish(home: &Path, name: &str, bytes: &[u8]) -> Result<(), TransferError> {
    let directory = secure_directory(home, true)?;
    publish_in(&directory, name, bytes, false)
}

#[cfg(unix)]
fn publish_in(
    directory: &std::fs::File,
    name: &str,
    bytes: &[u8],
    replace: bool,
) -> Result<(), TransferError> {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    let target = CString::new(name).map_err(|_| TransferError::UnsafePath)?;
    // lstat-style check rejects dangling symlinks, directories and malformed files
    // without reading any existing credentials. linkat below is the race guard.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let exists = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            target.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if exists == 0 && !replace {
        return Err(TransferError::ExistingStore);
    }
    if exists != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
        return Err(TransferError::Io);
    }
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(TransferError::Io);
    }

    struct Staged<'a> {
        directory: &'a File,
        name: CString,
    }
    impl Drop for Staged<'_> {
        fn drop(&mut self) {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
    let temporary = CString::new(format!(".oauth-transfer-{}", uuid::Uuid::new_v4()))
        .map_err(|_| TransferError::Io)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(TransferError::Io);
    }
    let staged = Staged {
        directory,
        name: temporary,
    };
    let mut file = unsafe { File::from_raw_fd(fd) };
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(TransferError::Io);
    }
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| TransferError::Io)?;
    // Hard-link creation is atomic and cannot replace an existing destination.
    let result = if replace {
        unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                staged.name.as_ptr(),
                directory.as_raw_fd(),
                target.as_ptr(),
            )
        }
    } else {
        unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                staged.name.as_ptr(),
                directory.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        }
    };
    if result != 0 {
        return Err(
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
                TransferError::ExistingStore
            } else {
                TransferError::Io
            },
        );
    }
    drop(staged);
    // Publication has already committed. A directory sync failure must not imply
    // the credential was not installed, encouraging a retry or exposing a token.
    let _ = directory.sync_all();
    Ok(())
}

/// Cloud-only secret envelope. Deliberately has no Debug, Display, Clone or
/// Serialize implementation. Transport only over an authenticated private channel.
pub struct CloudCredentialTransfer {
    bytes: Vec<u8>,
    label: String,
    provider: TransferProvider,
}

impl CloudCredentialTransfer {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn provider(&self) -> TransferProvider {
        self.provider
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudEnvelope {
    version: u32,
    provider: TransferProvider,
    label: String,
    credential: serde_json::Value,
}

fn cloud_keys(
    provider: TransferProvider,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match provider {
        TransferProvider::OpenAi => (
            "openai_accounts",
            "active_openai_account",
            "access_token",
            "refresh_token",
        ),
        TransferProvider::Claude => (
            "anthropic_accounts",
            "active_anthropic_account",
            "access",
            "refresh",
        ),
    }
}

fn valid_cloud_label(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= 256 && !label.chars().any(char::is_control)
}

// Normalize only known account fields, never copy arbitrary store contents.
// Expired access tokens are intentional: the receiving runtime may refresh them.
fn cloud_account(
    provider: TransferProvider,
    value: &serde_json::Value,
) -> Result<serde_json::Value, TransferError> {
    let normalized = match provider {
        TransferProvider::OpenAi => {
            let account: super::codex::OpenAiAccount =
                serde_json::from_value(value.clone()).map_err(|_| TransferError::InvalidPayload)?;
            serde_json::to_value(account)
        }
        TransferProvider::Claude => {
            let account: super::claude::AnthropicAccount =
                serde_json::from_value(value.clone()).map_err(|_| TransferError::InvalidPayload)?;
            serde_json::to_value(account)
        }
    }
    .map_err(|_| TransferError::InvalidPayload)?;
    let (_, _, access, refresh) = cloud_keys(provider);
    if !normalized["label"].as_str().is_some_and(valid_cloud_label)
        || normalized[access]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
        || normalized[refresh]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(TransferError::InvalidPayload);
    }
    Ok(normalized)
}

fn parse_cloud(
    provider: TransferProvider,
    label: &str,
    bytes: &[u8],
) -> Result<CloudEnvelope, TransferError> {
    if bytes.len() > MAX_TRANSFER_BYTES {
        return Err(TransferError::TooLarge);
    }
    let mut envelope: CloudEnvelope =
        serde_json::from_slice(bytes).map_err(|_| TransferError::InvalidPayload)?;
    if envelope.version != 2 {
        return Err(TransferError::UnsupportedVersion);
    }
    if envelope.provider != provider {
        return Err(TransferError::ProviderMismatch);
    }
    if !valid_cloud_label(label) || envelope.label != label {
        return Err(TransferError::LabelMismatch);
    }
    envelope.credential = cloud_account(provider, &envelope.credential)?;
    if envelope.credential["label"].as_str() != Some(label) {
        return Err(TransferError::LabelMismatch);
    }
    Ok(envelope)
}

#[cfg(unix)]
fn cloud_store(
    bytes: &[u8],
    provider: TransferProvider,
) -> Result<serde_json::Value, TransferError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| TransferError::InvalidStore)?;
    if !value.is_object() {
        return Err(TransferError::InvalidStore);
    }
    let (accounts, active, _, _) = cloud_keys(provider);
    if let Some(selection) = value.get(active)
        && !selection.is_null()
        && !selection.as_str().is_some_and(valid_cloud_label)
    {
        return Err(TransferError::InvalidStore);
    }
    if let Some(entries) = value.get(accounts) {
        let entries = entries.as_array().ok_or(TransferError::InvalidStore)?;
        let mut labels = std::collections::HashSet::new();
        for entry in entries {
            let label = entry["label"]
                .as_str()
                .filter(|s| valid_cloud_label(s))
                .ok_or(TransferError::InvalidStore)?;
            if !labels.insert(label) {
                return Err(TransferError::InvalidStore);
            }
            cloud_account(provider, entry).map_err(|_| TransferError::InvalidStore)?;
        }
    }
    Ok(value)
}

/// Export an exact named account, or the runtime-selected/stored-active/first
/// Jcode-owned account. No external discovery, migration, refresh or writes.
pub fn export_cloud_local(
    provider: TransferProvider,
    selected: Option<&str>,
) -> Result<CloudCredentialTransfer, TransferError> {
    let home = crate::storage::jcode_dir().map_err(|_| TransferError::Io)?;
    let runtime = super::account_store::runtime_active_override(provider.as_str());
    export_cloud_account_at(&home, provider, selected.or(runtime.as_deref()))
}

/// Enumerate all explicitly stored cloud-transferable accounts. Missing stores
/// are normal, but malformed stores must not silently produce partial transfers.
/// This never discovers credentials from other tools or refreshes tokens.
pub fn cloud_accounts_at(home: &Path) -> Result<Vec<(TransferProvider, String)>, TransferError> {
    #[cfg(not(unix))]
    {
        let _ = home;
        Err(TransferError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    {
        let directory = match secure_directory(home, false) {
            Ok(directory) => directory,
            Err(TransferError::Unavailable) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut accounts = Vec::new();
        for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
            let bytes = match read_in(&directory, provider.store_name(), false) {
                Ok(bytes) => bytes,
                Err(TransferError::Unavailable) => continue,
                Err(error) => return Err(error),
            };
            let store = cloud_store(&bytes, provider)?;
            let (key, _, _, _) = cloud_keys(provider);
            if let Some(entries) = store.get(key).and_then(serde_json::Value::as_array) {
                for entry in entries {
                    let label = entry["label"].as_str().ok_or(TransferError::InvalidStore)?;
                    accounts.push((provider, label.to_owned()));
                }
            }
        }
        Ok(accounts)
    }
}

/// Isolated-store export, without runtime overrides. Legacy unlabeled Claude
/// credentials are refused rather than inventing a label or modifying the store.
pub fn export_cloud_account_at(
    home: &Path,
    provider: TransferProvider,
    selected: Option<&str>,
) -> Result<CloudCredentialTransfer, TransferError> {
    #[cfg(not(unix))]
    {
        let _ = (home, provider, selected);
        Err(TransferError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    {
        let directory = secure_directory(home, false)?;
        let bytes = read_in(&directory, provider.store_name(), false)?;
        let store = cloud_store(&bytes, provider)?;
        let (accounts, active, _, _) = cloud_keys(provider);
        let entries = store[accounts]
            .as_array()
            .ok_or(TransferError::Unavailable)?;
        let selection = selected.or(store[active].as_str());
        let account = match selection {
            Some(label) => entries
                .iter()
                .find(|entry| entry["label"].as_str() == Some(label)),
            None => entries.first(),
        }
        .ok_or(TransferError::Unavailable)?;
        let credential = cloud_account(provider, account)?;
        let label = credential["label"]
            .as_str()
            .ok_or(TransferError::InvalidStore)?
            .to_owned();
        let bytes = serde_json::to_vec(&CloudEnvelope {
            version: 2,
            provider,
            label: label.clone(),
            credential,
        })
        .map_err(|_| TransferError::InvalidPayload)?;
        if bytes.len() > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
        Ok(CloudCredentialTransfer {
            bytes,
            label,
            provider,
        })
    }
}

/// Merge one labeled cloud credential, preserving unrelated accounts, unknown
/// store fields and an existing active selection (including explicit null).
///
/// Read/merge/publication holds the shared account-store lock used by current
/// provider writers. Older binaries and external editors do not honor this lock:
/// quiesce them before importing. A byte recheck detects most such interference,
/// but cannot close the final check/rename race against a noncooperating writer.
///
/// Same-label updates need a matching nonempty OpenAI account_id or Claude email,
/// with all other identity hints unchanged. These are UNVERIFIED stored hints,
/// not authentication evidence: only accept trusted, authenticated transports.
/// Changed tokens also need a strictly newer known expiry, otherwise the return
/// may be stale and is refused. Expiry ordering is not proof of token lineage.
/// For stale-return protection or identity-less accounts use import_cloud_return_at.
/// No method can prove refresh-token lineage offline or prevent server-side
/// rotation conflicts. Do not keep both copies refreshing simultaneously.
pub fn import_cloud_at(
    home: &Path,
    provider: TransferProvider,
    expected_label: &str,
    bytes: &[u8],
) -> Result<(), TransferError> {
    import_cloud_inner(home, provider, expected_label, bytes, None, false)
}

/// Explicit handoff replaces a stale destination copy of the same identity.
/// Unlike a return/merge, the authenticated source is authoritative, including
/// for accounts without identity hints or with equal access-token expiries.
/// Call only after the user has requested transferring their accounts.
pub fn import_cloud_handoff_at(
    home: &Path,
    provider: TransferProvider,
    expected_label: &str,
    bytes: &[u8],
) -> Result<(), TransferError> {
    import_cloud_inner(home, provider, expected_label, bytes, None, true)
}

/// Return updated tokens only if the destination credential still exactly
/// matches the original export. This permits identity-less accounts, provided
/// identity hints remain unchanged, and refuses intervening local token rotation.
/// Retain the actual departure envelope privately until return; never log it.
/// Do NOT take a fresh local export on return as a substitute: that does not
/// establish the cloud credential's lineage and could restore a replaced login.
/// The writer-lock and authenticated-transport requirements above apply.
pub fn import_cloud_return_at(
    home: &Path,
    provider: TransferProvider,
    expected_label: &str,
    bytes: &[u8],
    original: &CloudCredentialTransfer,
) -> Result<(), TransferError> {
    let baseline = parse_cloud(provider, expected_label, original.as_bytes())?;
    import_cloud_inner(
        home,
        provider,
        expected_label,
        bytes,
        Some(baseline.credential),
        false,
    )
}

pub fn import_cloud_local(
    provider: TransferProvider,
    expected_label: &str,
    bytes: &[u8],
) -> Result<(), TransferError> {
    let home = crate::storage::jcode_dir().map_err(|_| TransferError::Io)?;
    import_cloud_at(&home, provider, expected_label, bytes)
}

#[cfg(unix)]
fn identity_hints_match(
    provider: TransferProvider,
    old: &serde_json::Value,
    new: &serde_json::Value,
) -> bool {
    let fields: &[&str] = match provider {
        TransferProvider::OpenAi => &["account_id", "email"],
        TransferProvider::Claude => &["email"],
    };
    let present = |value: &serde_json::Value| value.as_str().is_some_and(|s| !s.trim().is_empty());
    // A hint missing on either side is not evidence of a different account:
    // older refreshes dropped `email` when the grant omitted id_token. Any hint
    // present on both sides must still match exactly, and at least one hint
    // must be shared so an identity-less entry cannot absorb another account.
    let mut shared = false;
    for key in fields {
        let (a, b) = (&old[*key], &new[*key]);
        if present(a) && present(b) {
            if a != b {
                return false;
            }
            shared = true;
        }
    }
    shared || fields.iter().all(|key| old[*key] == new[*key])
}

#[cfg(unix)]
fn identity_hints_present(provider: TransferProvider, account: &serde_json::Value) -> bool {
    let fields: &[&str] = match provider {
        TransferProvider::OpenAi => &["account_id", "email"],
        TransferProvider::Claude => &["email"],
    };
    fields
        .iter()
        .any(|key| account[*key].as_str().is_some_and(|s| !s.trim().is_empty()))
}

fn cloud_expiry(provider: TransferProvider, account: &serde_json::Value) -> Option<i64> {
    match provider {
        TransferProvider::Claude => account["expires"].as_i64(),
        TransferProvider::OpenAi => account["expires_at"].as_i64().or_else(|| {
            account["access_token"]
                .as_str()
                .and_then(super::codex::expires_at_from_access_token)
        }),
    }
}

fn import_cloud_inner(
    home: &Path,
    provider: TransferProvider,
    label: &str,
    bytes: &[u8],
    baseline: Option<serde_json::Value>,
    authoritative: bool,
) -> Result<(), TransferError> {
    let envelope = parse_cloud(provider, label, bytes)?;
    #[cfg(not(unix))]
    {
        let _ = (home, envelope, baseline, authoritative);
        Err(TransferError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let directory = secure_directory(home, true)?;
        if directory.metadata().map_err(|_| TransferError::Io)?.mode() & 0o022 != 0 {
            return Err(TransferError::UnsafePath);
        }
        let _lock = super::account_store::lock_store(home, provider.store_name())
            .map_err(|_| TransferError::Io)?;
        let original = match read_in(&directory, provider.store_name(), true) {
            Ok(bytes) => Some(bytes),
            Err(TransferError::Unavailable) => None,
            Err(error) => return Err(error),
        };
        let mut store = match &original {
            Some(bytes) => cloud_store(bytes, provider)?,
            None => serde_json::json!({}),
        };
        let (accounts, active, _, _) = cloud_keys(provider);
        // A legacy Claude login has no stable original label. Do not silently
        // supersede it by selecting a new named account; let normal login migrate it.
        if provider == TransferProvider::Claude
            && !store["anthropic"].is_null()
            && store[accounts]
                .as_array()
                .is_none_or(|entries| entries.is_empty())
        {
            return Err(TransferError::InvalidStore);
        }
        let entries = store
            .as_object_mut()
            .ok_or(TransferError::InvalidStore)?
            .entry(accounts)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or(TransferError::InvalidStore)?;
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry["label"].as_str() == Some(label))
        {
            let old = cloud_account(provider, existing)?;
            // Explicit handoff: a destination copy with no identity hints at all
            // (older stores) is stale data for this label, not a different account.
            let old_identityless = !identity_hints_present(provider, &old);
            if !(authoritative && old_identityless
                || identity_hints_match(provider, &old, &envelope.credential))
            {
                return Err(TransferError::AccountConflict);
            }
            if let Some(baseline) = &baseline {
                if &old != baseline {
                    return Err(TransferError::ConcurrentUpdate);
                }
            } else if !authoritative && old != envelope.credential {
                let identity = match provider {
                    TransferProvider::OpenAi => "account_id",
                    TransferProvider::Claude => "email",
                };
                if old[identity].as_str().is_none_or(|s| s.trim().is_empty()) {
                    return Err(TransferError::AccountConflict);
                }
                let (_, _, access, refresh) = cloud_keys(provider);
                if old[access] != envelope.credential[access]
                    || old[refresh] != envelope.credential[refresh]
                {
                    match (
                        cloud_expiry(provider, &old),
                        cloud_expiry(provider, &envelope.credential),
                    ) {
                        (Some(old), Some(new)) if new > old => {}
                        _ => return Err(TransferError::ConcurrentUpdate),
                    }
                }
            }
            // Preserve unknown per-account fields too, updating only known fields.
            let object = existing
                .as_object_mut()
                .ok_or(TransferError::InvalidStore)?;
            let optional_fields: &[&str] = match provider {
                TransferProvider::OpenAi => &["id_token", "account_id", "email", "expires_at"],
                TransferProvider::Claude => &["email", "scopes", "subscription_type"],
            };
            for key in optional_fields {
                object.remove(*key);
            }
            object.extend(
                envelope
                    .credential
                    .as_object()
                    .ok_or(TransferError::InvalidPayload)?
                    .clone(),
            );
        } else {
            if baseline.is_some() {
                return Err(TransferError::ConcurrentUpdate);
            }
            entries.push(envelope.credential);
        }
        if store.get(active).is_none() {
            // Only a new provider account set receives a default active account.
            // Existing nonempty sets without an explicit selection keep first-account semantics.
            if store[accounts].as_array().is_some_and(|a| a.len() == 1) {
                store[active] = serde_json::Value::String(label.to_owned());
            }
        }
        let output = serde_json::to_vec(&store).map_err(|_| TransferError::InvalidStore)?;
        if output.len() > MAX_STORE_BYTES {
            return Err(TransferError::TooLarge);
        }
        let current = match read_in(&directory, provider.store_name(), true) {
            Ok(bytes) => Some(bytes),
            Err(TransferError::Unavailable) => None,
            Err(error) => return Err(error),
        };
        if current != original {
            return Err(TransferError::ConcurrentUpdate);
        }
        if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
            return Err(TransferError::Io);
        }
        publish_in(
            &directory,
            provider.store_name(),
            &output,
            original.is_some(),
        )?;
        super::AuthStatus::invalidate_cache();
        Ok(())
    }
}

#[cfg(unix)]
fn read_in(directory: &std::fs::File, name: &str, private: bool) -> Result<Vec<u8>, TransferError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;
    let name = std::ffi::CString::new(name).map_err(|_| TransferError::UnsafePath)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                TransferError::Unavailable
            } else {
                TransferError::UnsafePath
            },
        );
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|_| TransferError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || (private && metadata.mode() & 0o077 != 0)
    {
        return Err(TransferError::UnsafePath);
    }
    if metadata.len() > MAX_STORE_BYTES as u64 {
        return Err(TransferError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| TransferError::Io)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(TransferError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests;
