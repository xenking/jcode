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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSpeedStatus {
    pub requested: SpeedMode,
    pub effective: SpeedMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl std::fmt::Display for WorkerSpeedStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Speed requested: {:?}. Effective speed: {:?}.",
            self.requested, self.effective
        )?;
        if let Some(reason) = &self.reason {
            write!(f, " {reason}")?;
        }
        Ok(())
    }
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
