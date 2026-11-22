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
            status: CodemodeCallStatus::Interrupted,
            duration_ms: 0,
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
        result: &anyhow::Result<ToolOutput>,
    ) {
        let mut ledger = self.0.lock().expect("codemode ledger lock");
        let Some((_, call)) = ledger.calls.get_mut(&seq) else {
            return;
        };
        call.duration_ms = latency_ms;
        call.status = if result.is_ok() {
            CodemodeCallStatus::Success
        } else {
            CodemodeCallStatus::Error
        };
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
        }
    }
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
            ledger.finished(seq, seq as u64, &Ok(ToolOutput::new("not retained")));
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
