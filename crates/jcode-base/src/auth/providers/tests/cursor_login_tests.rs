use super::*;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn jwt(sub: &str, exp: u64) -> String {
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{sub}","exp":{exp}}}"#));
    format!("eyJhbGciOiJub25lIn0.{payload}.sig")
}

const FAR_FUTURE: u64 = 4_000_000_000;

/// Serve scripted `(status, body)` replies, one per request, and report each request line.
async fn fake_cursor(
    replies: Vec<(u16, String)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            seen.push(request.lines().next().unwrap_or_default().to_string());
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        seen
    });
    (base, served)
}

struct TestHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _dir: TempDir,
    previous: Option<std::ffi::OsString>,
}

impl TestHome {
    fn new() -> (Self, std::path::PathBuf) {
        let guard = crate::storage::lock_test_env();
        let previous = std::env::var_os("JCODE_HOME");
        let dir = TempDir::new().unwrap();
        crate::env::set_var("JCODE_HOME", dir.path());
        let path = dir.path().to_path_buf();
        (
            Self {
                _guard: guard,
                _dir: dir,
                previous,
            },
            path,
        )
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

#[test]
fn browser_login_challenge_is_an_s256_pkce_pair_for_the_deep_control_page() {
    let challenge = begin_login().unwrap();
    let url = reqwest::Url::parse(&challenge.login_url).unwrap();
    assert_eq!(url.host_str(), Some("cursor.com"));
    assert_eq!(url.path(), "/loginDeepControl");
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(challenge.verifier.as_bytes()));
    assert_eq!(query["challenge"], expected);
    assert_eq!(query["uuid"], challenge.uuid);
    assert_eq!(query["redirectTarget"], "cli");
    assert!(!challenge.login_url.contains(&challenge.verifier));
}

#[tokio::test]
async fn polling_waits_through_404_and_returns_the_approved_tokens() {
    let (base, served) = fake_cursor(vec![
        (404, "{}".into()),
        (404, "{}".into()),
        (
            200,
            r#"{"accessToken":"acc-1","refreshToken":"ref-1"}"#.into(),
        ),
    ])
    .await;
    let challenge = begin_login().unwrap();
    let tokens = poll_tokens(
        &Client::new(),
        &base,
        &challenge,
        5,
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(tokens.access_token, "acc-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("ref-1"));
    assert_eq!(tokens.source, CURSOR_LOGIN_SOURCE);
    let seen = served.await.unwrap();
    assert!(seen.iter().all(|line| line.starts_with("GET /auth/poll?")));
    assert!(seen[2].contains(&challenge.uuid) && seen[2].contains(&challenge.verifier));
}

#[tokio::test]
async fn polling_failure_names_the_status_and_never_the_verifier() {
    let (base, _served) = fake_cursor(vec![(500, "{}".into())]).await;
    let challenge = begin_login().unwrap();
    let error = poll_tokens(
        &Client::new(),
        &base,
        &challenge,
        5,
        Duration::from_millis(1),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("500"), "{error}");
    assert!(!error.contains(&challenge.verifier));
}

#[tokio::test]
async fn polling_gives_up_when_the_browser_never_approves() {
    let (base, _served) = fake_cursor(vec![(404, "{}".into()); 3]).await;
    let challenge = begin_login().unwrap();
    let error = poll_tokens(
        &Client::new(),
        &base,
        &challenge,
        3,
        Duration::from_millis(1),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Timed out"), "{error}");
}

#[tokio::test]
async fn email_is_only_taken_from_a_profile_that_names_the_tokens_subject() {
    let token = jwt("auth0|abc", FAR_FUTURE);
    let (base, _served) = fake_cursor(vec![(
        200,
        r#"{"authId":"auth0|abc","email":"dev@example.com"}"#.into(),
    )])
    .await;
    assert_eq!(
        fetch_account_email_from(&Client::new(), &base, &token).await,
        Some("dev@example.com".into())
    );
    let (base, _served) = fake_cursor(vec![(
        200,
        r#"{"authId":"auth0|someone-else","email":"other@example.com"}"#.into(),
    )])
    .await;
    assert_eq!(
        fetch_account_email_from(&Client::new(), &base, &token).await,
        None
    );
}

#[cfg(unix)]
#[test]
fn saved_login_is_private_labelled_and_feeds_the_token_resolver() {
    use std::os::unix::fs::PermissionsExt;
    let (_home, _path) = TestHome::new();
    let tokens = CursorDirectTokens {
        access_token: jwt("auth0|abc", FAR_FUTURE),
        refresh_token: Some("refresh-secret".into()),
        source: CURSOR_LOGIN_SOURCE,
    };
    let login = save_login(None, &tokens, Some("dev@example.com".into())).unwrap();
    assert_eq!(login.label, "cursor-otter");
    let mode = std::fs::metadata(login_path().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!format!("{login:?}").contains("refresh-secret"));

    // A second login with no name logs in to the same account again.
    assert_eq!(
        save_login(None, &tokens, None).unwrap().label,
        "cursor-otter"
    );

    let resolved = load_access_token_from_env_or_file().unwrap();
    assert_eq!(resolved.source, CURSOR_LOGIN_SOURCE);
    assert_eq!(resolved.refresh_token.as_deref(), Some("refresh-secret"));
    assert!(has_cursor_native_auth());

    std::fs::remove_file(login_path().unwrap()).unwrap();
    assert!(load_login().is_none());
}

#[test]
fn refreshed_tokens_are_kept_on_the_saved_login_with_its_label() {
    let (_home, _path) = TestHome::new();
    let first = CursorDirectTokens {
        access_token: "old-access".into(),
        refresh_token: Some("old-refresh".into()),
        source: CURSOR_LOGIN_SOURCE,
    };
    save_login(Some("work"), &first, Some("dev@example.com".into())).unwrap();
    persist_refreshed_login(&CursorDirectTokens {
        access_token: "new-access".into(),
        refresh_token: None,
        source: CURSOR_LOGIN_SOURCE,
    });
    let login = load_login().unwrap();
    assert_eq!(login.access_token, "new-access");
    assert_eq!(login.refresh_token.as_deref(), Some("old-refresh"));
    assert_eq!(login.email.as_deref(), Some("dev@example.com"));
    assert_eq!(login.label, "cursor-otter");
}

#[tokio::test]
async fn importing_the_local_app_copies_its_sign_in_without_touching_the_app_files() {
    let (_home, home) = TestHome::new();
    let vscdb = cursor_vscdb_paths().into_iter().next().unwrap();
    std::fs::create_dir_all(vscdb.parent().unwrap()).unwrap();
    let access = jwt("auth0|abc", FAR_FUTURE);
    let source = create_mock_vscdb_at(
        &vscdb,
        &[
            ("cursorAuth/accessToken", access.as_str()),
            ("cursorAuth/refreshToken", "app-refresh"),
            ("cursorAuth/cachedEmail", "dev@example.com"),
        ],
    );
    let before = std::fs::read(&source).unwrap();

    let login = import_local_app_login(&Client::new(), None).await.unwrap();

    assert_eq!(login.label, "cursor-otter");
    assert_eq!(login.access_token, access);
    assert_eq!(login.refresh_token.as_deref(), Some("app-refresh"));
    assert_eq!(login.email.as_deref(), Some("dev@example.com"));
    assert!(home.join(LOGIN_FILE).exists());
    assert_eq!(std::fs::read(&source).unwrap(), before);
}

#[test]
fn importing_says_exactly_what_is_missing() {
    let (_home, _path) = TestHome::new();
    let missing = super::read_local_app_login().unwrap_err().to_string();
    assert!(missing.contains("not installed"), "{missing}");
    assert!(missing.contains("state.vscdb"), "{missing}");
    assert!(
        missing.contains("jcode login --provider cursor"),
        "{missing}"
    );

    let vscdb = cursor_vscdb_paths().into_iter().next().unwrap();
    std::fs::create_dir_all(vscdb.parent().unwrap()).unwrap();
    create_mock_vscdb_at(&vscdb, &[("other/key", "value")]);
    let signed_out = super::read_local_app_login().unwrap_err().to_string();
    assert!(signed_out.contains("not signed in"), "{signed_out}");
}

fn create_mock_vscdb_at(path: &std::path::Path, entries: &[(&str, &str)]) -> std::path::PathBuf {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);")
        .unwrap();
    for (key, value) in entries {
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                (key, value),
            )
            .unwrap();
    }
    path.to_path_buf()
}
