use super::ToolOutput;
use jcode_session_types::{
    CodemodeCall, CodemodeCallStatus, CodemodeDirection, CodemodeStatus, CodemodeSummary,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

const MAX_CALLS: usize = 40;
const METADATA_KEY: &str = "codemode";

#[derive(Default)]
pub(super) struct CallLedger(Mutex<Ledger>);

#[derive(Default)]
struct Ledger {
    calls: BTreeMap<usize, (Instant, CodemodeCall)>,
    omitted: usize,
}

impl CallLedger {
    pub(super) fn dispatched(
        &self,
        seq: usize,
        name: &str,
        input: &Value,
        session: &str,
        started: Instant,
        builtin: bool,
    ) {
        let mut ledger = self.0.lock().expect("codemode ledger lock");
        if ledger.calls.len() == MAX_CALLS {
            ledger.omitted += 1;
            if ledger
                .calls
                .last_key_value()
                .is_some_and(|(last, _)| seq > *last)
            {
                return;
            }
            ledger.calls.pop_last();
        }

        let mut call = CodemodeCall {
            tool: safe_tool_name(name).to_string(),
            action: None,
            actor: None,
            target: None,
            direction: None,
            external: false,
            status: CodemodeCallStatus::Interrupted,
            duration_ms: 0,
            preview: None,
            diagnostic: None,
        };
        if builtin && name == "swarm" {
            let raw = input["action"]
                .as_str()
                .filter(|s| s.len() <= 48)
                .unwrap_or_default();
            let action = super::communicate::canonical_swarm_action(raw);
            let to = |key: &str| input[key].as_str();
            let recipient = || to("to_session").or_else(|| to("target_session"));
            let peer = || to("target_session").or_else(|| to("to_session"));
            let direct = || recipient().and_then(peer_name);
            let (target, direction) = match action {
                "message" if to("to_swarm").is_some_and(|s| !s.trim().is_empty()) => (
                    direct().or(Some("swarm")),
                    Some(CodemodeDirection::Outbound),
                ),
                "message" if recipient().is_some() => (direct(), Some(CodemodeDirection::Outbound)),
                "message" if to("channel").is_some() => {
                    (Some("channel"), Some(CodemodeDirection::Outbound))
                }
                "message" | "broadcast" => (Some("swarm"), Some(CodemodeDirection::Outbound)),
                "channel" => (Some("channel"), Some(CodemodeDirection::Outbound)),
                "answer" => (direct(), Some(CodemodeDirection::Outbound)),
                "spawn" => (Some("worker"), None),
                "status" | "summary" | "focus" | "stop" | "resume" | "retry" | "wake" | "start"
                | "start_task" | "replace" | "reassign" | "assign_task" | "assign_role"
                | "assign_next" => (peer().and_then(peer_name), None),
                _ => (None, None),
            };
            call.action = super::communicate::display_swarm_action(action).map(str::to_owned);
            call.actor = peer_name(session).map(str::to_owned);
            call.target = target.map(str::to_owned);
            call.direction = direction;
        }
        ledger.calls.insert(seq, (started, call));
    }

    pub(super) fn finished(
        &self,
        seq: usize,
        latency_ms: u64,
        input: &Value,
        result: &anyhow::Result<ToolOutput>,
    ) {
        let mut ledger = self.0.lock().expect("codemode ledger lock");
        let Some((_, call)) = ledger.calls.get_mut(&seq) else {
            return;
        };
        call.duration_ms = latency_ms;
        match result {
            Ok(output) => {
                call.status = CodemodeCallStatus::Success;
                if call.action.as_deref() == Some("message") {
                    call.preview = input["message"].as_str().and_then(message_preview);
                    if let Some(target) = output
                        .metadata
                        .as_ref()
                        .and_then(|meta| meta["external_dm_target"].as_str())
                    {
                        call.external = true;
                        call.target = peer_name(target).map(str::to_owned);
                    }
                }
            }
            Err(error) => {
                call.status = CodemodeCallStatus::Error;
                call.diagnostic = Some(safe_diagnostic(error).to_string());
            }
        }
        if call.action.as_deref() == Some("spawn")
            && let Ok(output) = result
            && let Some(name) = output
                .metadata
                .as_ref()
                .and_then(|meta| meta["session_id"].as_str())
                .and_then(peer_name)
        {
            call.target = Some(name.to_string());
        }
    }

    pub(super) fn summary(&self, status: CodemodeStatus) -> CodemodeSummary {
        let mut ledger = self.0.lock().expect("codemode ledger lock");
        let calls = std::mem::take(&mut ledger.calls)
            .into_values()
            .map(|(started, mut call)| {
                if call.status == CodemodeCallStatus::Interrupted {
                    call.duration_ms = started.elapsed().as_millis() as u64;
                }
                call
            })
            .collect();
        CodemodeSummary {
            status,
            calls,
            calls_omitted: ledger.omitted,
            report_body_bytes: None,
        }
    }
}

fn message_preview(message: &str) -> Option<String> {
    const MAX_REDACTION_BYTES: usize = 4096;
    const MAX_PREVIEW_BYTES: usize = 160;
    if message.len() > MAX_REDACTION_BYTES {
        return None;
    }
    let visible: String = message
        .chars()
        .filter(|&ch| {
            (ch.is_whitespace() || !ch.is_control())
                && !matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .collect();
    let redacted = crate::message::redact_preview_secrets(&visible);
    let line = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        None
    } else if line.len() <= MAX_PREVIEW_BYTES {
        Some(line)
    } else {
        let cut = crate::util::truncate_str(&line, MAX_PREVIEW_BYTES - '…'.len_utf8());
        Some(format!("{cut}…"))
    }
}

fn safe_diagnostic(error: &anyhow::Error) -> &'static str {
    use std::io::ErrorKind;
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<std::io::Error>() {
            return match error.kind() {
                ErrorKind::PermissionDenied => "Permission denied",
                ErrorKind::NotFound => "Resource not found",
                ErrorKind::TimedOut => "Tool timed out",
                ErrorKind::ConnectionRefused => "Connection refused",
                _ => "Tool I/O failed",
            };
        }
        if source.downcast_ref::<serde_json::Error>().is_some() {
            return "Invalid JSON data";
        }
    }
    "Tool execution failed"
}

fn safe_tool_name(name: &str) -> &str {
    if !name.is_empty()
        && name.len() <= 96
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        name
    } else {
        "tool"
    }
}

fn peer_name(value: &str) -> Option<&str> {
    if value.len() > 128 {
        return None;
    }
    let name = if jcode_core::id::is_generated_session_id(value) {
        jcode_core::id::extract_session_name(value)?
    } else {
        value
    };
    jcode_core::id::is_memorable_session_name(name).then_some(name)
}

pub(super) fn attach(output: ToolOutput, summary: CodemodeSummary) -> ToolOutput {
    output.with_metadata(serde_json::json!({METADATA_KEY: summary}))
}

pub(crate) fn output_summary(name: &str, output: &ToolOutput) -> Option<Box<CodemodeSummary>> {
    if name != "codemode" {
        return None;
    }
    let summary = output.metadata.as_ref()?.get(METADATA_KEY)?;
    serde::Deserialize::deserialize(summary).ok()
}

pub(crate) fn clear_report_boundary(metadata: &mut Option<Value>) {
    if let Some(meta) = metadata
        .as_mut()
        .and_then(|meta| meta.get_mut(METADATA_KEY))
        && let Some(meta) = meta.as_object_mut()
    {
        meta.remove("report_body_bytes");
    }
}

#[derive(Debug)]
pub(super) struct StoreError {
    pub error: anyhow::Error,
    pub summary: CodemodeSummary,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

pub(crate) fn error_summary(error: &anyhow::Error) -> Option<Box<CodemodeSummary>> {
    Some(Box::new(
        error.downcast_ref::<StoreError>()?.summary.clone(),
    ))
}

#[test]
fn bounded_ledger_keeps_invocation_order_even_when_dispatch_is_reversed() {
    let ledger = CallLedger::default();
    let started = Instant::now();
    for seq in (1..=MAX_CALLS + 11).rev() {
        ledger.dispatched(
            seq,
            &format!("tool_{seq}"),
            &Value::Null,
            "",
            started,
            false,
        );
        if seq != 2 {
            ledger.finished(
                seq,
                seq as u64,
                &Value::Null,
                &Ok(ToolOutput::new("not retained")),
            );
        }
    }
    let summary = ledger.summary(CodemodeStatus::Error);
    assert_eq!(summary.calls_omitted, 11);
    assert_eq!(summary.calls.len(), MAX_CALLS);
    assert_eq!(summary.calls[1].status, CodemodeCallStatus::Interrupted);
    for (index, call) in summary.calls.iter().enumerate() {
        assert_eq!(call.tool, format!("tool_{}", index + 1));
    }
}

#[test]
fn message_preview_is_bounded_utf8_single_line_and_redacted() {
    let message = format!(
        "Проверка\u{202e}\n password=fixture-credential {}",
        "ёж".repeat(200)
    );
    let preview = message_preview(&message).unwrap();
    assert!(preview.starts_with("Проверка password=[REDACTED_SECRET] "));
    assert!(preview.ends_with('…'));
    assert!(preview.len() <= 160);
    assert!(!preview.contains("fixture-credential"));
    assert!(!preview.contains('\n'));
    assert!(message_preview(&"ёж".repeat(1200)).is_none());
}
