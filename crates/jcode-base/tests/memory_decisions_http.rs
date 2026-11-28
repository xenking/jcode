//! Opt-in recall exercises the real Jcode Decisions HTTP route in isolation.
use jcode_base::memory::{MemoryCategory, MemoryEntry, MemoryManager};
use jcode_base::message::Message;
use std::io::{Read, Write};

#[tokio::test]
async fn automatic_recall_posts_decisions_and_fails_closed_offline() {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    jcode_base::env::set_var("JCODE_HOME", dir.path());
    jcode_base::env::set_var("JCODE_MEMORY_JEV_PROVIDER", "jcode");
    jcode_base::env::set_var("JCODE_MEMORY_AUTO_RECALL", "true");
    jcode_base::env::set_var("JCODE_API_BASE", &base);
    jcode_base::env::set_var("JCODE_API_KEY", "fixture-key");
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let (end, length) = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, val) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| val.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < end + length {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let response = if headers.starts_with("GET /v1/me ") {
                serde_json::json!({"capabilities":{"memory_jev":true}})
            } else {
                assert!(headers.starts_with("POST /v1/decisions "), "{headers}");
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[end..end + length]).unwrap();
                let state: serde_json::Value =
                    serde_json::from_str(request["state"].as_str().unwrap()).unwrap();
                assert!(state["query"].as_str().unwrap().contains("test"));
                let answers: serde_json::Map<String, serde_json::Value> = request["questions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|id| (id.clone(), serde_json::json!({"type":"noul","noul":0.99})))
                    .collect();
                serde_json::json!({"answers":answers})
            };
            let body = serde_json::to_vec(&response).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let manager = MemoryManager::new().with_project_dir("/fake-decisions-project");
    manager
        .remember_project(MemoryEntry::new(
            MemoryCategory::Fact,
            "Run cargo test for Rust tests",
        ))
        .unwrap();
    let messages = [Message::user("How do I test this project?")];
    let result = manager
        .get_relevant_parallel("fixture", &messages, None)
        .await
        .unwrap();
    assert!(result.prompt.unwrap().contains("cargo test"));
    server.join().unwrap();
    let error = manager
        .get_relevant_parallel("fixture-offline", &messages, None)
        .await;
    assert!(error.is_err(), "offline must not inject unjudged memories");
}
