use super::*;
use crate::usage::anthropic_reset::reset_client;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Real response of an account that has spent its November reset and still holds
/// the model-launch one, with no limit reached (ids and account details stripped).
const RECORDED: &str = include_str!("fixtures/claude_usage_real_cedar_one_spent.json");

/// The recording's grants lapse in 2027. Push them out so the test does not age.
fn status(edit: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(RECORDED).unwrap();
    for grant in value["cedar_ember"]["grants"].as_array_mut().unwrap() {
        grant["ends_at"] = "2099-03-09T07:59:00Z".into();
    }
    edit(&mut value["cedar_ember"]);
    value
}

struct Provider {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    fn claims(&self) -> Vec<serde_json::Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST "))
            .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
            .collect()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A loopback stand-in for the provider: the recorded usage body, a profile,
/// and one claim answer.
async fn provider(usage: serde_json::Value, claim_code: u16, claim: serde_json::Value) -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                bytes.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let text = String::from_utf8(bytes).unwrap();
            let (code, body) = if text.starts_with("GET /api/oauth/profile ") {
                (
                    200,
                    serde_json::json!({"account":{"uuid":"account-1"},"organization":{"uuid":"org-1"}}),
                )
            } else if text.starts_with("GET /api/oauth/usage?cedar_ember=1&skip_spend=1 ") {
                (200, usage.clone())
            } else {
                assert!(text.starts_with("POST /api/organizations/org-1/reset_rate_limits "));
                (claim_code, claim.clone())
            };
            seen.lock().unwrap().push(text);
            let body = body.to_string();
            let _ = stream
                .write_all(format!("HTTP/1.1 {code} T\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await;
        }
    });
    Provider {
        base,
        requests,
        task,
    }
}

async fn prepare(
    provider: &Provider,
    root: &std::path::Path,
) -> std::result::Result<PendingSavedClaudeReset, SavedClaudeResetUnavailable> {
    prepare_with(
        &reset_client().unwrap(),
        &provider.base,
        "pinned-token".into(),
        "claude-otter".into(),
        root,
    )
    .await
    .unwrap()
}

async fn consume(
    provider: &Provider,
    pending: &PendingSavedClaudeReset,
    root: &std::path::Path,
) -> Result<bool> {
    redeem_with(
        &reset_client().unwrap(),
        &provider.base,
        &pending.access_token,
        &pending.account_label,
        root,
        Spend::Saved(&pending.grant_id),
    )
    .await
}

fn markers(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "pending"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn the_unspent_grant_is_redeemed_with_no_limit_reached() {
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(
        status(|_| {}),
        200,
        serde_json::json!({"result":"reset","cleared":[]}),
    )
    .await;
    let pending = prepare(&provider, dir.path()).await.unwrap();
    assert_eq!(pending.grant_id, "launch-2610-fable");
    assert!(consume(&provider, &pending, dir.path()).await.unwrap());
    let claims = provider.claims();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["program"], "cedar_ember");
    assert_eq!(claims[0]["grant_id"], "launch-2610-fable");
    assert_eq!(markers(dir.path()), 0);
}

#[tokio::test]
async fn a_declined_claim_spends_nothing_and_can_be_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(
        status(|_| {}),
        200,
        serde_json::json!({"result":"cooldown","cooldown_until":"2099-01-01T00:00:00Z"}),
    )
    .await;
    let pending = prepare(&provider, dir.path()).await.unwrap();
    assert!(!consume(&provider, &pending, dir.path()).await.unwrap());
    assert_eq!(markers(dir.path()), 0);
    assert!(prepare(&provider, dir.path()).await.is_ok());
}

#[tokio::test]
async fn an_uncertain_claim_blocks_a_second_spend_until_it_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(status(|_| {}), 500, serde_json::json!({})).await;
    let pending = prepare(&provider, dir.path()).await.unwrap();
    assert!(consume(&provider, &pending, dir.path()).await.is_err());
    assert_eq!(markers(dir.path()), 1);
    assert_eq!(
        prepare(&provider, dir.path()).await.unwrap_err(),
        SavedClaudeResetUnavailable::Unfinished
    );
    assert!(!consume(&provider, &pending, dir.path()).await.unwrap());
    assert_eq!(provider.claims().len(), 1);
}

#[tokio::test]
async fn only_a_grant_the_provider_reports_usable_is_offered() {
    let dir = tempfile::tempdir().unwrap();
    let edits: [fn(&mut serde_json::Value); 6] = [
        |block| block["grants"][1]["usable_now"] = false.into(),
        |block| block["grants"][1]["paused"] = true.into(),
        |block| block["grants"][1]["resets_left"] = 0.into(),
        |block| block["grants"][1]["ends_at"] = "2020-01-01T00:00:00Z".into(),
        |block| block["eligible"] = false.into(),
        // The spent November grant is not the server's pick.
        |block| block["next_grant_id"] = "full-reset-2611".into(),
    ];
    for edit in edits {
        let provider = provider(status(edit), 200, serde_json::json!({})).await;
        assert_eq!(
            prepare(&provider, dir.path()).await.unwrap_err(),
            SavedClaudeResetUnavailable::NothingUsable
        );
    }
}
