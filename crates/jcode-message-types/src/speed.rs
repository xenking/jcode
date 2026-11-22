use serde::{Deserialize, Serialize};

/// Requested execution speed, independent of reasoning effort.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedMode {
    #[default]
    Standard,
    Fast,
    Ultrafast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedAvailability {
    Available,
    Unknown,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeedOption {
    pub mode: SpeedMode,
    pub availability: SpeedAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SpeedOption {
    pub fn standard() -> Self {
        Self {
            mode: SpeedMode::Standard,
            availability: SpeedAvailability::Available,
            reason: None,
        }
    }
}
