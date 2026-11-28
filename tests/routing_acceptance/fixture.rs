//! A fixture OpenAI-compatible provider standing in for OpenCode Go: chat,
//! models and the per-key `/usage` windows the routing meters read. It records
//! which key each chat request used. No network beyond loopback.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    /// Percent of the rolling window each key has used.
    used: HashMap<String, u32>,
    /// Keys answered with a usage-limit exhaustion (429).
    limited: HashSet<String>,
    /// Keys whose next requests get a short rate limit (429), by count.
    throttled: HashMap<String, u32>,
    /// Keys answered with a 429 for a spent weekly window.
    spent: HashSet<String>,
    /// Keys of chat requests, in order.
    seen: Vec<String>,
    /// Request bodies, same order.
    bodies: Vec<String>,
}

#[derive(Clone)]
pub struct Fixture {
    pub port: u16,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Fixture {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self { port, state }
    }

    pub fn used(&self, key: &str, percent: u32) {
        lock(&self.state).used.insert(key.into(), percent);
    }

    pub fn limit(&self, key: &str) {
        lock(&self.state).limited.insert(key.into());
    }

    pub fn throttle(&self, key: &str, requests: u32) {
        lock(&self.state).throttled.insert(key.into(), requests);
    }

    pub fn spend(&self, key: &str) {
        lock(&self.state).spent.insert(key.into());
    }

    /// Keys of the chat requests so far.
    pub fn seen(&self) -> Vec<String> {
        lock(&self.state).seen.clone()
    }

    pub fn bodies(&self) -> Vec<String> {
        lock(&self.state).bodies.clone()
    }
}

struct Request {
    path: String,
    key: String,
    body: String,
}

fn read_request(stream: &TcpStream) -> Option<(String, Request)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let (mut key, mut length) = (String::new(), 0usize);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        match name.to_ascii_lowercase().as_str() {
            "authorization" => {
                key = value.trim().trim_start_matches("Bearer").trim().to_string();
            }
            "content-length" => length = value.trim().parse().ok()?,
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    let body = String::from_utf8_lossy(&body).into_owned();
    Some((method, Request { path, key, body }))
}

fn reply(mut stream: &TcpStream, status: &str, value: &serde_json::Value) {
    let data = value.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{data}",
        data.len()
    );
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Some((method, request)) = read_request(&stream) else {
        return;
    };
    if method == "GET" {
        if request.path.trim_end_matches('/').ends_with("/usage") {
            let percent = lock(state).used.get(&request.key).copied().unwrap_or(0);
            let window = serde_json::json!({
                "status": "ok",
                "percent": percent,
                "resetsAt": "2099-01-01T00:00:00.000Z",
            });
            return reply(
                &stream,
                "200 OK",
                &serde_json::json!({"usage": {"rolling": window}}),
            );
        }
        let models = serde_json::json!({"data": [{"id": "kimi-k2.5", "object": "model"}]});
        return reply(&stream, "200 OK", &models);
    }
    let key = request.key;
    let refusal = {
        let mut state = lock(state);
        state.seen.push(key.clone());
        state.bodies.push(request.body);
        if let Some(left) = state.throttled.get_mut(&key).filter(|left| **left > 0) {
            *left -= 1;
            Some((
                "Too many requests, rate limit exceeded",
                "rate_limit_exceeded",
            ))
        } else if state.spent.contains(&key) {
            Some((
                "Weekly rate limit exceeded, resets at 2099-01-01T00:00:00Z",
                "rate_limit_exceeded",
            ))
        } else if state.limited.contains(&key) {
            Some(("usage_limit_reached", "usage_limit_reached"))
        } else {
            None
        }
    };
    if let Some((message, code)) = refusal {
        let error = serde_json::json!({"error": {"message": message, "code": code}});
        return reply(&stream, "429 Too Many Requests", &error);
    }
    let mut stream = &stream;
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
    );
    let chunk = |delta: serde_json::Value, finish: Option<&str>| {
        let choice = serde_json::json!({"index": 0, "delta": delta, "finish_reason": finish});
        let event = serde_json::json!({
            "id": "fx",
            "object": "chat.completion.chunk",
            "model": "kimi-k2.5",
            "choices": [choice],
        });
        format!("data: {event}\n\n")
    };
    let text = serde_json::json!({"role": "assistant", "content": format!("served by {key}")});
    let _ = stream.write_all(chunk(text, None).as_bytes());
    let _ = stream.write_all(chunk(serde_json::json!({}), Some("stop")).as_bytes());
    let _ = stream.write_all(b"data: [DONE]\n\n");
    let _ = stream.flush();
}
