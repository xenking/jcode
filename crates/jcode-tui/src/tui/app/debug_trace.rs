use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
struct DebugEvent {
    at_ms: u64,
    kind: String,
    detail: String,
}

pub(super) struct DebugTrace {
    enabled: bool,
    started_at: Instant,
    events: Vec<DebugEvent>,
}

impl DebugTrace {
    pub(super) fn new() -> Self {
        Self {
            enabled: false,
            started_at: Instant::now(),
            events: Vec::new(),
        }
    }

    pub(super) fn start(&mut self) {
        self.enabled = true;
        self.started_at = Instant::now();
        self.events.clear();
    }

    pub(super) fn stop(&mut self) {
        self.enabled = false;
    }

    pub(super) fn profile(&self) -> (bool, usize, usize) {
        let payload_bytes = self
            .events
            .iter()
            .map(|event| event.kind.capacity() + event.detail.capacity())
            .sum();
        (self.enabled, self.events.len(), payload_bytes)
    }

    pub(super) fn export(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.events)
    }

    pub(super) fn record(&mut self, kind: &str, detail: String) {
        if !self.enabled {
            return;
        }
        let at_ms = self.started_at.elapsed().as_millis() as u64;
        self.events.push(DebugEvent {
            at_ms,
            kind: kind.to_string(),
            detail,
        });
    }
}
