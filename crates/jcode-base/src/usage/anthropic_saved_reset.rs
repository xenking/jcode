//! Manual redemption of a saved Claude reset (a `cedar_ember` grant). Reading
//! is side-effect free. Spending needs the same review-then-confirm flow as the
//! other resets, and shares the daemon's account lock and pending marker, so a
//! click can never double-spend with an automatic retry.

use super::anthropic_fable_reset::{Spend, profile_identity, redeem_with, saved_grant, status};
use super::anthropic_reset::{fresh_access_token, get_json, reset_client_for};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

const BASE: &str = "https://api.anthropic.com";

/// A confirmation pins the login, token and grant that were reviewed.
#[derive(Clone)]
pub struct PendingSavedClaudeReset {
    access_token: String,
    account_label: String,
    grant_id: String,
    grant_label: Option<String>,
    expires_at: Option<String>,
}

// Never expose bearer tokens in client state or logs.
impl std::fmt::Debug for PendingSavedClaudeReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingSavedClaudeReset")
            .field("account", &self.account_label)
            .finish_non_exhaustive()
    }
}

impl PendingSavedClaudeReset {
    pub fn account_display(&self) -> &str {
        &self.account_label
    }

    /// Review lines for graphical clients.
    pub fn confirmation_details(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Spends one saved reset: {}",
            self.grant_label.as_deref().unwrap_or(&self.grant_id)
        )];
        if let Some(at) = &self.expires_at {
            lines.push(format!(
                "This one expires in {}.",
                super::format_reset_time(at)
            ));
        }
        lines.push("It works even when no limit has been reached.".to_string());
        lines
    }
}

/// Why no saved reset can be prepared, safe to show to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedClaudeResetUnavailable {
    /// The login has no saved reset the provider reports as usable now.
    NothingUsable,
    /// An earlier claim on this account did not finish. Spending again could
    /// double-spend until the person has checked their usage page.
    Unfinished,
}

impl SavedClaudeResetUnavailable {
    pub fn message(self) -> &'static str {
        match self {
            Self::NothingUsable => "No saved Claude reset can be used right now.",
            Self::Unfinished => {
                "An earlier reset on this login did not finish. Check Claude Settings > Usage before trying again."
            }
        }
    }
}

fn marker_path(root: &std::path::Path, profile: &serde_json::Value) -> Result<std::path::PathBuf> {
    let (account, organization) = profile_identity(profile)?;
    let key = hex::encode(Sha256::digest(
        format!("{account}:{organization}").as_bytes(),
    ));
    Ok(root.join(format!("{key}.pending")))
}

/// Read-only: find the saved reset the provider would spend for one login.
/// `None` is the active login.
pub async fn prepare_saved_claude_reset(
    account_label: Option<String>,
) -> Result<std::result::Result<PendingSavedClaudeReset, SavedClaudeResetUnavailable>> {
    let label = account_label
        .or_else(crate::auth::claude::active_account_label)
        .context("Saved resets need a named Claude login. Sign in to Claude first.")?;
    let token = fresh_access_token(Some(&label)).await?;
    let client = reset_client_for(Some(&label)).await?;
    prepare_with(
        &client,
        BASE,
        token,
        label,
        &crate::storage::jcode_dir()?.join("fable-resets"),
    )
    .await
}

async fn prepare_with(
    client: &reqwest::Client,
    base: &str,
    access_token: String,
    account_label: String,
    root: &std::path::Path,
) -> Result<std::result::Result<PendingSavedClaudeReset, SavedClaudeResetUnavailable>> {
    let Some(status) = status(client, base, &access_token).await? else {
        return Ok(Err(SavedClaudeResetUnavailable::NothingUsable));
    };
    let Some(grant) = saved_grant(&status, chrono::Utc::now()) else {
        return Ok(Err(SavedClaudeResetUnavailable::NothingUsable));
    };
    let profile = get_json(client, &format!("{base}/api/oauth/profile"), &access_token).await?;
    if marker_path(root, &profile)?.exists() {
        return Ok(Err(SavedClaudeResetUnavailable::Unfinished));
    }
    Ok(Ok(PendingSavedClaudeReset {
        access_token,
        account_label,
        grant_id: grant.id.clone(),
        grant_label: grant.label.clone().filter(|label| !label.trim().is_empty()),
        expires_at: grant.ends_at.clone(),
    }))
}

/// Spend exactly the reset the user confirmed. `Ok(false)` means the provider
/// did not apply it. Errors are uncertain: the daemon's pending marker stays,
/// so a retry cannot spend a second grant.
pub async fn consume_saved_claude_reset(pending: &PendingSavedClaudeReset) -> Result<bool> {
    let client = reset_client_for(Some(&pending.account_label)).await?;
    redeem_with(
        &client,
        BASE,
        &pending.access_token,
        &pending.account_label,
        &crate::storage::jcode_dir()?.join("fable-resets"),
        Spend::Saved(&pending.grant_id),
    )
    .await
}

#[cfg(test)]
#[path = "anthropic_saved_reset_tests.rs"]
mod tests;
