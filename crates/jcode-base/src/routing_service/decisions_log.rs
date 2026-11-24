//! `routing-decisions.jsonl`: one line per routing decision, shadow or auto.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

const FILE: &str = "routing-decisions.jsonl";
const ROTATE_AT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(super) struct Entry<'a> {
    pub at: i64,
    pub time: String,
    pub mode: &'a str,
    pub session: &'a str,
    pub model: &'a str,
    pub role: &'a str,
    pub seam: &'a str,
    pub current: Option<&'a str>,
    /// `stay`, `move`, `pending`, `gate` or `block`.
    pub decision: &'a str,
    pub account: Option<&'a str>,
    pub reason: &'a str,
    pub warnings: &'a [String],
    pub pending: Option<&'a str>,
    /// Whether the daemon acted on it. Always false in shadow mode.
    pub applied: bool,
}

pub(super) fn append(home: Option<&Path>, entry: &Entry<'_>) {
    let Some(home) = home else { return };
    let path = home.join(FILE);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > ROTATE_AT_BYTES) {
        let _ = std::fs::rename(&path, home.join(format!("{FILE}.1")));
    }
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    if std::fs::create_dir_all(home).is_err() {
        return;
    }
    // One write of the whole line keeps concurrent sessions from interleaving.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&line));
}
