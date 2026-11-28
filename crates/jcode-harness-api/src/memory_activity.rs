//! Display-only memory pipeline activity.
//!
//! Mirrors the daemon's snapshot rather than reusing it, so the API crate keeps
//! its small dependency surface. The bridge converts the wire snapshot into
//! these types; clients only read them to draw a status line.

use serde::{Deserialize, Serialize};

/// What the memory pipeline is doing right now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    Idle,
    Embedding,
    SidecarChecking { count: usize },
    FoundRelevant { count: usize },
    Extracting { reason: String },
    Maintaining { phase: String },
    ToolAction { action: String, detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStepStatus {
    Pending,
    Running,
    Done,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryStepResult {
    pub summary: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPipeline {
    pub search: MemoryStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_result: Option<MemoryStepResult>,
    pub verify: MemoryStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_result: Option<MemoryStepResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_progress: Option<(usize, usize)>,
    pub inject: MemoryStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_result: Option<MemoryStepResult>,
    pub maintain: MemoryStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintain_result: Option<MemoryStepResult>,
}

/// One complete memory activity snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryActivitySnapshot {
    pub state: MemoryState,
    #[serde(default)]
    pub state_age_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<MemoryPipeline>,
}

impl MemoryActivitySnapshot {
    /// The activity is over and a client can drop its status line.
    pub fn is_idle(&self) -> bool {
        self.state == MemoryState::Idle
    }

    /// One short line naming the current step, or `None` when idle.
    pub fn status_label(&self) -> Option<String> {
        let label = match &self.state {
            MemoryState::Idle => return None,
            MemoryState::Embedding => "Recalling memory".to_string(),
            MemoryState::SidecarChecking { count } => format!("Reading memories · {count}"),
            MemoryState::FoundRelevant { count } => format!("Found {count} memories"),
            MemoryState::Extracting { reason } => format!("Verifying memories · {reason}"),
            MemoryState::Maintaining { phase } => format!("Maintaining memory · {phase}"),
            MemoryState::ToolAction { action, detail } => {
                if detail.is_empty() {
                    format!("Memory {action}")
                } else {
                    format!("Memory {action} · {detail}")
                }
            }
        };
        Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_has_no_status_and_working_states_do() {
        let idle = MemoryActivitySnapshot {
            state: MemoryState::Idle,
            state_age_ms: 0,
            pipeline: None,
        };
        assert_eq!(idle.status_label(), None);
        let found = MemoryActivitySnapshot {
            state: MemoryState::FoundRelevant { count: 3 },
            state_age_ms: 10,
            pipeline: None,
        };
        assert_eq!(found.status_label().as_deref(), Some("Found 3 memories"));
    }

    #[test]
    fn pipeline_snapshots_round_trip() {
        let activity = MemoryActivitySnapshot {
            state: MemoryState::FoundRelevant { count: 2 },
            state_age_ms: 12,
            pipeline: Some(MemoryPipeline {
                search: MemoryStepStatus::Done,
                search_result: Some(MemoryStepResult {
                    summary: "2 matches".into(),
                    latency_ms: 4,
                }),
                verify: MemoryStepStatus::Running,
                verify_result: None,
                verify_progress: Some((1, 2)),
                inject: MemoryStepStatus::Pending,
                inject_result: None,
                maintain: MemoryStepStatus::Skipped,
                maintain_result: None,
            }),
        };
        let wire = serde_json::to_value(&activity).unwrap();
        assert_eq!(
            serde_json::from_value::<MemoryActivitySnapshot>(wire).unwrap(),
            activity
        );
    }
}
