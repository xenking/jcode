//! Worker records: what the model, effort, spend and live phase say.

use crate::worker_meta::{worker_ids, worker_meta, worker_meta_for};
use crate::{SessionInfo, WorkerPhase};
use std::path::{Path, PathBuf};

/// A throwaway home. The crate has no tempfile dependency, so the directory
/// is named from the test's own thread and removed by the caller.
fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jcode-worker-meta-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

fn write(home: &Path, session_id: &str, snapshot: &str, journal: &str) {
    let dir = home.join("sessions");
    std::fs::write(dir.join(format!("{session_id}.json")), snapshot).unwrap();
    if !journal.is_empty() {
        std::fs::write(dir.join(format!("{session_id}.journal.jsonl")), journal).unwrap();
    }
}

const SILENT: &str = r#"{"id":"s"}"#;

#[test]
fn model_effort_and_spend_come_from_the_record() {
    let home = home("spend");
    write(
        &home,
        "session_fox_1_a",
        r#"{"model":"deepseek-v4.1-flash","provider_key":"opencode-go",
            "reasoning_effort":"high","status":"Active",
            "messages":[
              {"role":"user","content":[{"type":"text","text":"go"}]},
              {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"bash","input":{}}],
               "token_usage":{"prompt_tokens":12000,"output_tokens":3000}}
            ]}"#,
        "",
    );
    let meta = worker_meta(&home, "session_fox_1_a").unwrap();
    assert_eq!(meta.model.as_deref(), Some("deepseek-v4.1-flash"));
    assert_eq!(meta.provider.as_deref(), Some("opencode-go"));
    assert_eq!(meta.effort.as_deref(), Some("high"));
    assert_eq!(meta.input_tokens, Some(12_000));
    assert_eq!(meta.output_tokens, Some(3_000));
    assert_eq!(meta.phase, Some(WorkerPhase::Tool));
    assert_eq!(meta.tool.as_deref(), Some("bash"));
    assert!(meta.last_activity_ms.is_some());
    std::fs::remove_dir_all(home).ok();
}

/// Providers that split the cache out of `input_tokens` still add up to what
/// the model actually read, and the journal extends the snapshot.
#[test]
fn journal_lines_override_metadata_and_extend_the_totals() {
    let home = home("journal");
    write(
        &home,
        "session_rose_2_b",
        r#"{"model":"claude-opus-5-5","provider_key":"claude-oauth","status":"Active",
            "messages":[{"role":"assistant","content":[{"type":"text","text":"hi"}],
              "token_usage":{"input_tokens":100,"cache_read_input_tokens":900,
                             "cache_creation_input_tokens":50,"output_tokens":10}}]}"#,
        concat!(
            r#"{"meta":{"model":"claude-sonnet-5-5","reasoning_effort":"low"},"#,
            r#""append_messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}]}"#,
            "\n"
        ),
    );
    let meta = worker_meta(&home, "session_rose_2_b").unwrap();
    assert_eq!(meta.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(meta.effort.as_deref(), Some("low"));
    assert_eq!(meta.input_tokens, Some(1_050));
    assert_eq!(meta.output_tokens, Some(10));
    assert_eq!(meta.cache_read_tokens, Some(900));
    assert_eq!(meta.cache_creation_tokens, Some(50));
    // The newest message is a tool result, so the model is about to see it.
    assert_eq!(meta.phase, Some(WorkerPhase::Thinking));
    assert_eq!(meta.tool, None);
    std::fs::remove_dir_all(home).ok();
}

/// A record with no usage telemetry reports no spend, not zero, and a closed
/// session never claims to be working.
#[test]
fn unknown_spend_stays_absent_and_closed_sessions_are_idle() {
    let home = home("closed");
    write(&home, "session_idle_3_c", SILENT, "");
    let meta = worker_meta(&home, "session_idle_3_c").unwrap();
    assert_eq!(meta.input_tokens, None);
    assert_eq!(meta.output_tokens, None);
    assert_eq!(meta.phase, Some(WorkerPhase::Idle));

    write(
        &home,
        "session_done_4_d",
        r#"{"model":"gpt-5.6","status":"Closed",
            "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"bash","input":{}}]}]}"#,
        "",
    );
    let done = worker_meta(&home, "session_done_4_d").unwrap();
    assert_eq!(done.phase, Some(WorkerPhase::Idle));
    assert_eq!(done.tool, None);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn missing_or_unreadable_records_are_absent() {
    let home = home("missing");
    assert!(worker_meta(&home, "session_nobody_5_e").is_none());
    // Ids that are not plain session ids never reach the filesystem.
    assert!(worker_meta(&home, "../../.ssh/id_rsa").is_none());
    write(&home, "session_bad_6_f", "not json", "");
    assert!(worker_meta(&home, "session_bad_6_f").is_none());
    std::fs::remove_dir_all(home).ok();
}

/// Only swarm workers are read: an ordinary chat's own record is not a
/// worker's, and a self-parented session is not a worker either.
#[test]
fn worker_ids_are_sessions_with_another_parent() {
    let session = |id: &str, parent: Option<&str>| {
        let mut session: SessionInfo = serde_json::from_value(serde_json::json!({
            "session_id": id, "status": "idle",
        }))
        .unwrap();
        session.parent_session_id = parent.map(str::to_owned);
        session
    };
    let sessions = [
        session("session_lead_1_a", None),
        session("session_fox_2_b", Some("session_lead_1_a")),
        session("session_fox_2_b", Some("session_lead_1_a")),
        session("session_self_3_c", Some("session_self_3_c")),
    ];
    assert_eq!(worker_ids(&sessions), vec!["session_fox_2_b"]);
}

#[test]
fn batch_reads_skip_unknown_sessions() {
    let home = home("batch");
    write(
        &home,
        "session_one_1_a",
        r#"{"model":"gpt-5.6","messages":[{"role":"assistant","content":[{"type":"text","text":"done"}]}]}"#,
        "",
    );
    let ids = vec!["session_one_1_a".to_string(), "session_two_2_b".to_string()];
    let meta = worker_meta_for(&home, &ids);
    assert_eq!(meta.len(), 1);
    assert_eq!(meta["session_one_1_a"].phase, Some(WorkerPhase::Thinking));
    std::fs::remove_dir_all(home).ok();
}
