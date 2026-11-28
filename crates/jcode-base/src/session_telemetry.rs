//! Session-addressed provider context measurements for read-only resume previews.
//! No credentials, cumulative token counts, or process-global account state.
use crate::persistence_batch::{Batch, keep_latest};
use crate::session::Session;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTelemetry {
    pub session_id: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub measured_at: chrono::DateTime<chrono::Utc>,
    /// Normalized prompt size for one provider request, excluding output.
    pub context_tokens: u64,
    pub context_limit: Option<usize>,
    pub compaction: Option<std::sync::Arc<jcode_session_types::StoredCompactionState>>,
    #[serde(default)]
    pub route_api_method: Option<String>,
    #[serde(default)]
    pub serving_provider: Option<String>,
    #[serde(default)]
    pub credential: Option<jcode_provider_core::ResolvedCredential>,
    #[serde(default)]
    pub account_label: Option<String>,
    #[serde(default)]
    pub account_usage: Option<jcode_protocol::AccountUsageSnapshot>,
    #[serde(default)]
    pub context_policy: Option<jcode_session_types::ContextMeter>,
}

static WRITES: Batch<Arc<SessionTelemetry>> = Batch::new(persist_context, keep_latest);

pub(crate) fn flush_pending() {
    WRITES.flush();
}

fn persist_context(path: &Path, snapshot: &Arc<SessionTelemetry>) -> anyhow::Result<()> {
    if let Some(mut previous) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SessionTelemetry>(&bytes).ok())
    {
        // Match identical measurements without manufacturing a write solely
        // for the timestamp. Still refresh age-based quota previews once a minute.
        let elapsed = snapshot.measured_at - previous.measured_at;
        if elapsed >= chrono::TimeDelta::zero() && elapsed < chrono::TimeDelta::minutes(1) {
            previous.measured_at = snapshot.measured_at;
            if &previous == snapshot.as_ref() {
                return Ok(());
            }
        }
    }
    crate::storage::write_json_cache(path, snapshot.as_ref())
}

fn path(session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return None;
    }
    Some(
        crate::storage::durable_state_dir()
            .join("session-telemetry")
            .join(format!("{session_id}.json")),
    )
}

pub fn save_context(
    session: &Session,
    context_tokens: u64,
    context_limit: Option<usize>,
    serving_provider: &str,
    credential: Option<jcode_provider_core::ResolvedCredential>,
    account_label: Option<String>,
    context_policy: Option<jcode_session_types::ContextMeter>,
) {
    if session.is_ephemeral() {
        return;
    }
    let Some(path) = path(session.id()) else {
        return;
    };
    let account_usage = if matches!(
        credential,
        Some(jcode_provider_core::ResolvedCredential::Oauth)
    ) {
        let provider = match serving_provider.to_ascii_lowercase().as_str() {
            "openai" => Some(crate::usage::MultiAccountProviderKind::OpenAI),
            "claude" | "anthropic" => Some(crate::usage::MultiAccountProviderKind::Anthropic),
            _ => None,
        };
        provider
            .zip(account_label.as_deref())
            .and_then(|(provider, label)| {
                crate::usage::cached_account_usage_snapshot(provider, label)
                    .filter(|usage| usage.label == label)
                    .map(|usage| jcode_protocol::AccountUsageSnapshot {
                        additional_windows: usage.additional_windows,
                        label: usage.label,
                        email: None,
                        exhausted: usage.exhausted,
                        primary_label: usage.primary_label,
                        five_hour_ratio: usage.five_hour_ratio,
                        secondary_label: usage.secondary_label,
                        seven_day_ratio: usage.seven_day_ratio,
                        resets_at: usage.resets_at,
                        error: usage.error,
                        banked_resets: None,
                        banked_usable: None,
                        banked_cached_source: None,
                        banked_expires_at: None,
                        banked_next_at: None,
                    })
            })
    } else {
        None
    };
    let snapshot = SessionTelemetry {
        session_id: session.id().to_owned(),
        model: session.model().map(str::to_owned),
        provider: session.provider_key().map(str::to_owned),
        measured_at: chrono::Utc::now(),
        context_tokens,
        context_limit,
        compaction: session.compaction(),
        route_api_method: session.route_api_method().map(str::to_owned),
        serving_provider: Some(serving_provider.to_ascii_lowercase()),
        credential,
        account_label,
        account_usage,
        context_policy,
    };
    // Best-effort UI metadata must never fail a provider turn.
    if let Err(error) = WRITES.submit(path, Arc::new(snapshot)) {
        crate::logging::warn(&format!("Could not save session telemetry: {error:#}"));
    }
}

pub fn load_context(session: &Session) -> Option<SessionTelemetry> {
    read_context(session.id()).filter(|snapshot| snapshot.matches_session(session))
}

pub fn read_context(session_id: &str) -> Option<SessionTelemetry> {
    let path = path(session_id)?;
    WRITES.inspect(&path, |pending| {
        pending
            .map(|value| value.as_ref().clone())
            .or_else(|| read_persisted_context(&path, session_id))
    })
}

fn read_persisted_context(path: &Path, session_id: &str) -> Option<SessionTelemetry> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            crate::logging::warn(&format!("Could not read session telemetry: {error}"));
            return None;
        }
    };
    let snapshot: SessionTelemetry = match serde_json::from_slice(&bytes) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            crate::logging::warn(&format!("Could not decode session telemetry: {error}"));
            return None;
        }
    };
    (snapshot.session_id == session_id).then_some(snapshot)
}

impl SessionTelemetry {
    pub fn matches_session(&self, session: &Session) -> bool {
        self.session_id == session.id()
            && self.model.as_deref() == session.model()
            && self.provider.as_deref() == session.provider_key()
            && self.route_api_method.as_deref() == session.route_api_method()
            && self.compaction == session.compaction()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomeGuard(Option<std::ffi::OsString>);

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let old = Self(std::env::var_os("JCODE_HOME"));
            crate::env::set_var("JCODE_HOME", path);
            old
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            if let Some(home) = &self.0 {
                crate::env::set_var("JCODE_HOME", home);
            } else {
                crate::env::remove_var("JCODE_HOME");
            }
        }
    }

    #[test]
    fn write_amp_identical_observations_keep_existing_telemetry() {
        let _lock = crate::storage::lock_test_env();
        let directory = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(directory.path());
        let session = Session::create_with_id("telemetry-write-amp".into(), None, None);
        let path = path(session.id()).unwrap();
        let observe = || save_context(&session, 1234, Some(200_000), "OpenAI", None, None, None);
        observe();
        let first = std::fs::read(&path).unwrap();
        for _ in 0..20 {
            observe();
        }
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert!(!path.with_extension("bak").exists());
    }

    #[test]
    fn durable_measurement_roundtrips_and_rejects_changed_identity() {
        let _lock = crate::storage::lock_test_env();
        let directory = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(directory.path());
        let mut session = Session::create_with_id("telemetry-test".into(), None, None);
        session.apply_route(crate::session::Route {
            model: Some("future-model".into()),
            ..session.route()
        });
        session.apply_route(crate::session::Route {
            provider_key: Some("openai".into()),
            ..session.route()
        });
        session.apply_route(crate::session::Route {
            route_api_method: Some("openai:account-a".into()),
            ..session.route()
        });
        save_context(
            &session,
            1234,
            Some(200_000),
            "OpenAI",
            Some(jcode_provider_core::ResolvedCredential::Oauth),
            Some("account-a".into()),
            None,
        );
        let saved = load_context(&session).unwrap();
        assert_eq!(saved.context_tokens, 1234);
        assert_eq!(saved.context_limit, Some(200_000));
        assert_eq!(saved.serving_provider.as_deref(), Some("openai"));
        assert_eq!(saved.account_label.as_deref(), Some("account-a"));
        assert!(
            saved.account_usage.is_none(),
            "missing quota cache is unknown"
        );
        session.apply_route(crate::session::Route {
            route_api_method: Some("openai:account-b".into()),
            ..session.route()
        });
        assert!(load_context(&session).is_none());
        session.apply_route(crate::session::Route {
            route_api_method: saved.route_api_method,
            ..session.route()
        });
        std::fs::write(path(session.id()).unwrap(), b"{broken").unwrap();
        assert!(load_context(&session).is_none());
    }

    #[test]
    fn session_ids_cannot_escape_snapshot_directory() {
        for id in ["", "../other", "/tmp/other", "foo/bar", "foo\\bar"] {
            assert!(path(id).is_none());
        }
        assert!(path("session_fox-123").is_some());
    }
}
