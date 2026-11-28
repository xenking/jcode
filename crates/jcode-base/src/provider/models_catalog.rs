use super::*;

/// Total per-request timeout for model catalog fetches. The shared HTTP
/// client only sets a connect timeout, so without this a hung catalog request
/// keeps the scope's refresh marked in-flight and the picker stays stale.
const CATALOG_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// HTTP status carried inside model catalog fetch errors so callers can
/// distinguish auth rejections (401/403) from transient failures and recover
/// by force-refreshing OAuth tokens, mirroring the chat request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCatalogHttpStatus(pub u16);

impl std::fmt::Display for ModelCatalogHttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP status {}", self.0)
    }
}

impl std::error::Error for ModelCatalogHttpStatus {}

fn catalog_status_error(status: reqwest::StatusCode, context: String) -> anyhow::Error {
    anyhow::Error::new(ModelCatalogHttpStatus(status.as_u16())).context(context)
}

#[derive(Debug, Clone, Default)]
pub struct OpenAIModelCatalog {
    pub available_models: Vec<String>,
    pub context_limits: HashMap<String, usize>,
    /// Ordered reasoning-effort values advertised by each Codex model.
    pub reasoning_efforts: HashMap<String, Vec<String>>,
    /// Empty entries explicitly deny optional tiers, missing entries are unknown.
    pub service_tiers: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct AnthropicModelCatalog {
    pub available_models: Vec<String>,
    pub context_limits: HashMap<String, usize>,
}

pub(crate) fn parse_anthropic_model_catalog(data: &serde_json::Value) -> AnthropicModelCatalog {
    let models = data
        .get("data")
        .and_then(|value| value.as_array())
        .or_else(|| data.as_array());

    let mut available: HashSet<String> = HashSet::new();
    let mut limits: HashMap<String, usize> = HashMap::new();

    for model in models.into_iter().flatten() {
        let Some(id) = model.get("id").and_then(|value| value.as_str()) else {
            continue;
        };

        let normalized = normalize_model_id(id);
        if normalized.is_empty() {
            continue;
        }

        available.insert(normalized.clone());

        if let Some(limit) = model
            .get("max_input_tokens")
            .and_then(|value| value.as_u64())
        {
            limits.insert(normalized, limit as usize);
        }
    }

    let mut available_models: Vec<String> = available.into_iter().collect();
    available_models.sort();

    AnthropicModelCatalog {
        available_models,
        context_limits: limits,
    }
}

pub(crate) fn parse_openai_model_catalog(data: &serde_json::Value) -> OpenAIModelCatalog {
    let models = data
        .get("models")
        .and_then(|m| m.as_array())
        .or_else(|| {
            data.get("data")
                .and_then(|d| d.get("models"))
                .and_then(|m| m.as_array())
        })
        .or_else(|| data.get("data").and_then(|d| d.as_array()))
        .or_else(|| data.as_array());

    let mut available: HashSet<String> = HashSet::new();
    let mut limits: HashMap<String, usize> = HashMap::new();
    let mut service_tiers = HashMap::new();
    let mut reasoning_efforts: HashMap<String, Vec<String>> = HashMap::new();

    for model in models.into_iter().flatten() {
        let Some(slug) = model
            .get("slug")
            .or_else(|| model.get("id"))
            .or_else(|| model.get("model"))
            .and_then(|s| s.as_str())
        else {
            continue;
        };

        let slug = normalize_model_id(slug);
        if slug.is_empty() {
            continue;
        }

        available.insert(slug.clone());

        // Modern metadata wins even when malformed. Invalid data is unknown,
        // never an authoritative denial or a reason to use legacy metadata.
        if let Some(values) = model
            .get("service_tiers")
            .or_else(|| model.get("additional_speed_tiers"))
            .and_then(|value| value.as_array())
        {
            let mut tiers = Vec::new();
            let mut malformed = false;
            for value in values {
                let Some(raw) = value
                    .as_str()
                    .or_else(|| value.get("id").and_then(|id| id.as_str()))
                else {
                    malformed = true;
                    continue;
                };
                let normalized = raw.trim().to_ascii_lowercase();
                if normalized.is_empty() {
                    malformed = true;
                    continue;
                }
                let tier = if normalized == "fast" {
                    "priority".to_string()
                } else {
                    normalized
                };
                if !tiers.contains(&tier) {
                    tiers.push(tier);
                }
            }
            if !malformed {
                service_tiers.insert(slug.clone(), tiers);
            }
        }

        if let Some(ctx) = model
            .get("context_window")
            .or_else(|| model.get("context_length"))
            .and_then(|c| c.as_u64())
        {
            limits.insert(slug.clone(), ctx as usize);
        }

        if let Some(values) = model
            .get("supported_reasoning_levels")
            .or_else(|| model.get("supported_reasoning_efforts"))
            .and_then(|value| value.as_array())
        {
            let mut efforts = Vec::new();
            for value in values {
                let raw = value.as_str().or_else(|| {
                    value
                        .get("reasoning_effort")
                        .or_else(|| value.get("effort"))
                        .or_else(|| value.get("value"))
                        .and_then(|value| value.as_str())
                });
                let Some(effort) = raw.and_then(jcode_provider_core::canonical_reasoning_effort)
                else {
                    continue;
                };
                if !efforts.iter().any(|existing| existing == effort) {
                    efforts.push(effort.to_string());
                }
            }
            if values.is_empty() || !efforts.is_empty() {
                reasoning_efforts.insert(slug, efforts);
            }
        }
    }

    let mut available_models: Vec<String> = available.into_iter().collect();
    available_models.sort();

    OpenAIModelCatalog {
        available_models,
        context_limits: limits,
        reasoning_efforts,
        service_tiers,
    }
}

/// Fetch model availability and context windows from the Codex backend API.
///
/// `account_label` selects the egress of the login that owns the token.
pub async fn fetch_openai_model_catalog(
    access_token: &str,
    account_label: Option<&str>,
) -> Result<OpenAIModelCatalog> {
    fetch_openai_model_catalog_at(OPENAI_CODEX_MODELS_URL, access_token, account_label).await
}

const OPENAI_CODEX_MODELS_URL: &str =
    "https://chatgpt.com/backend-api/codex/models?client_version=1.0.0";

/// Workspace (Business/Team/Enterprise) logins share one OAuth identity across
/// several ChatGPT workspaces, and the backend decides which workspace's models
/// to list from the `chatgpt-account-id` header, exactly as it does for the
/// usage and responses calls. The token carries the same account id in its
/// claims, so it is read from there and the catalog lists the models of the
/// workspace the session will actually run on.
fn catalog_account_id(access_token: &str) -> Option<String> {
    crate::auth::codex::extract_account_id(access_token)
}

async fn fetch_openai_model_catalog_at(
    url: &str,
    access_token: &str,
    account_label: Option<&str>,
) -> Result<OpenAIModelCatalog> {
    note_openai_model_catalog_refresh_attempt();

    let client = crate::egress::client("openai", account_label).await?;
    let mut request = client
        .get(url)
        .header("Authorization", format!("Bearer {}", access_token));
    if let Some(account_id) = catalog_account_id(access_token) {
        request = request.header("chatgpt-account-id", account_id);
    }
    let resp = request.timeout(CATALOG_REQUEST_TIMEOUT).send().await?;

    if !resp.status().is_success() {
        return Err(catalog_status_error(
            resp.status(),
            format!("Failed to fetch model context limits: {}", resp.status()),
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    Ok(parse_openai_model_catalog(&data))
}

pub async fn fetch_anthropic_model_catalog(api_key: &str) -> Result<AnthropicModelCatalog> {
    fetch_anthropic_model_catalog_with_request(None, |client, after_id| {
        let mut req = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .query(&[("limit", "1000")]);

        if let Some(after) = after_id {
            req = req.query(&[("after_id", after)]);
        }

        req
    })
    .await
}

pub async fn fetch_anthropic_model_catalog_oauth(
    access_token: &str,
    account_label: Option<&str>,
) -> Result<AnthropicModelCatalog> {
    fetch_anthropic_model_catalog_with_request(account_label, |client, after_id| {
        let mut req = crate::provider::anthropic::apply_oauth_attribution_headers(
            client
                .get("https://api.anthropic.com/v1/models")
                .header("Authorization", format!("Bearer {}", access_token))
                .header(
                    "User-Agent",
                    crate::provider::anthropic::CLAUDE_CLI_USER_AGENT,
                )
                .header("anthropic-version", "2023-06-01")
                .header(
                    "anthropic-beta",
                    crate::provider::anthropic::OAUTH_BETA_HEADERS,
                )
                .query(&[("limit", "1000")]),
            &crate::provider::anthropic::new_oauth_request_id(),
        );

        if let Some(after) = after_id {
            req = req.query(&[("after_id", after)]);
        }

        req
    })
    .await
}

async fn fetch_anthropic_model_catalog_with_request<F>(
    account_label: Option<&str>,
    mut build_request: F,
) -> Result<AnthropicModelCatalog>
where
    F: FnMut(&reqwest::Client, Option<&str>) -> reqwest::RequestBuilder,
{
    let client = crate::egress::client("claude", account_label).await?;
    let mut available = HashSet::new();
    let mut limits = HashMap::new();
    let mut after_id: Option<String> = None;

    loop {
        let resp = build_request(&client, after_id.as_deref())
            .timeout(CATALOG_REQUEST_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Failed to fetch Anthropic model catalog: {}", resp.status());
        }

        let data: serde_json::Value = resp.json().await?;
        let page = parse_anthropic_model_catalog(&data);
        available.extend(page.available_models);
        limits.extend(page.context_limits);

        let has_more = data
            .get("has_more")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !has_more {
            break;
        }

        let Some(next_after) = data
            .get("last_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
        else {
            break;
        };

        after_id = Some(next_after);
    }

    let mut available_models: Vec<String> = available.into_iter().collect();
    available_models.sort();
    Ok(AnthropicModelCatalog {
        available_models,
        context_limits: limits,
    })
}

/// Fetch model availability from the OpenAI platform API using an API key.
///
/// The ChatGPT/Codex backend catalog endpoint only accepts ChatGPT OAuth
/// bearer tokens. OpenAI platform API keys return 401 there, so API-key
/// sessions must use the public platform models endpoint. That endpoint does
/// not currently expose context windows, so callers keep any built-in/cached
/// limits and only update account model availability.
pub async fn fetch_openai_api_key_model_catalog(api_key: &str) -> Result<OpenAIModelCatalog> {
    note_openai_model_catalog_refresh_attempt();

    let client = crate::egress::client("openai", None).await?;
    // Honor the same API-base override as the Responses request path so a
    // custom/proxied endpoint is probed for models instead of the real
    // api.openai.com (issue #343).
    let models_url = format!(
        "{}/models",
        crate::provider::openai::resolve_api_base().trim_end_matches('/')
    );
    let resp = client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(CATALOG_REQUEST_TIMEOUT)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(catalog_status_error(
            resp.status(),
            format!(
                "Failed to fetch OpenAI platform model catalog: {}",
                resp.status()
            ),
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut available_models: Vec<String> = data
        .get("data")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    available_models.sort();

    Ok(OpenAIModelCatalog {
        available_models,
        context_limits: HashMap::new(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_with_account(account_id: &str) -> String {
        use base64::Engine as _;
        let encode = |value: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.sig",
            encode(serde_json::json!({"alg": "none"})),
            encode(serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
            }))
        )
    }

    /// Serve one canned response and hand back the raw request it received.
    async fn serve_once(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/models", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&received).to_ascii_lowercase()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn workspace_login_catalog_fetch_names_its_workspace() {
        let (url, request) =
            serve_once(r#"{"models":[{"slug":"gpt-6.2-sol"},{"slug":"gpt-9-future"}]}"#).await;
        let catalog = fetch_openai_model_catalog_at(
            &url,
            &token_with_account("acct-business-workspace"),
            None,
        )
        .await
        .unwrap();
        assert!(
            request
                .await
                .unwrap()
                .contains("chatgpt-account-id: acct-business-workspace"),
            "the catalog request must name the workspace like every other Codex call"
        );
        // A model absent from every fallback array is still discovered.
        assert_eq!(catalog.available_models, ["gpt-6.2-sol", "gpt-9-future"]);
    }

    #[tokio::test]
    async fn personal_login_catalog_fetch_sends_no_workspace() {
        let (url, request) = serve_once(r#"{"models":[{"slug":"gpt-6.2-sol"}]}"#).await;
        fetch_openai_model_catalog_at(&url, "opaque-token", None)
            .await
            .unwrap();
        assert!(!request.await.unwrap().contains("chatgpt-account-id"));
    }

    #[test]
    fn openai_catalog_service_tiers_preserve_known_and_unknown() {
        let catalog = parse_openai_model_catalog(&serde_json::json!({
            "models": [
                {"slug":"sol", "service_tiers":[{"id":"priority"}, "fast", " PRIORITY "]},
                {"slug":"astra", "additional_speed_tiers":["fast"]},
                {"slug":"empty", "service_tiers":[], "additional_speed_tiers":["fast"]},
                {"slug":"missing"},
                {"slug":"null", "service_tiers":null, "additional_speed_tiers":["fast"]},
                {"slug":"wrong", "service_tiers":"priority"},
                {"slug":"invalid", "service_tiers":[{}, 7]},
                {"slug":"mixed", "service_tiers":["priority", {}]},
                {"slug":"blank", "service_tiers":[" "]},
                {"slug":"legacy-object", "additional_speed_tiers":[{"id":"fast"}]}
            ]
        }));
        for model in ["sol", "astra", "legacy-object"] {
            assert_eq!(
                catalog.service_tiers.get(model),
                Some(&vec!["priority".to_string()])
            );
        }
        assert_eq!(catalog.service_tiers.get("empty"), Some(&vec![]));
        for model in ["missing", "null", "wrong", "invalid", "mixed", "blank"] {
            assert!(!catalog.service_tiers.contains_key(model), "{model}");
        }
    }

    #[test]
    fn openai_catalog_parses_string_and_object_reasoning_efforts() {
        let catalog = parse_openai_model_catalog(&serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6",
                    "supported_reasoning_levels": [
                        { "reasoning_effort": "minimal" },
                        { "reasoning_effort": "high" },
                        { "reasoning_effort": "max" }
                    ]
                },
                {
                    "slug": "gpt-5.4",
                    "supported_reasoning_efforts": ["none", "low", "xhigh"]
                }
            ]
        }));

        assert_eq!(
            catalog.reasoning_efforts.get("gpt-5.6"),
            Some(&vec![
                "minimal".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );
        assert_eq!(
            catalog.reasoning_efforts.get("gpt-5.4"),
            Some(&vec![
                "none".to_string(),
                "low".to_string(),
                "xhigh".to_string()
            ])
        );
    }
}

/// Fetch context window sizes from the Codex backend API.
/// Returns a map of model slug -> context_window tokens.
pub async fn fetch_openai_context_limits(
    access_token: &str,
    account_label: Option<&str>,
) -> Result<HashMap<String, usize>> {
    Ok(fetch_openai_model_catalog(access_token, account_label)
        .await?
        .context_limits)
}

#[cfg(test)]
mod status_error_tests {
    use super::*;

    #[test]
    fn catalog_status_error_round_trips_through_anyhow_downcast() {
        let err = catalog_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "Failed to fetch model context limits: 401 Unauthorized".to_string(),
        );
        let status = err
            .downcast_ref::<ModelCatalogHttpStatus>()
            .expect("status must survive the context wrapper");
        assert_eq!(status.0, 401);
        // The human-readable context must stay the outermost message.
        assert!(
            err.to_string()
                .contains("Failed to fetch model context limits")
        );
    }
}
