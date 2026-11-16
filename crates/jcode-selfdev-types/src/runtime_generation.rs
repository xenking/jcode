//! Wire types for runtime selection and daemon-reported generations.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredSelection {
    InstalledVersion(String),
    Path(PathBuf),
    SharedServer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub modified_ns: u128,
}

/// A resolved selection. Never resolve its original channel again at exec time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmutableTarget {
    pub binary_path: PathBuf,
    pub git_hash: Option<String>,
    pub version_label: Option<String>,
    pub file_identity: Option<FileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_identity: Option<FileIdentity>,
}

/// The process image answering a Ping, not an installation channel or marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingIdentity {
    pub pid: u32,
    pub binary_path: PathBuf,
    pub git_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_id: Option<String>,
    /// Zero means a legacy daemon which does not report exec generations.
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationVerdict {
    Ready(ServingIdentity),
    Pending { reason: String },
    Unknown { reason: String },
    Mismatch { reason: String },
}
