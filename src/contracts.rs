use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const POLICY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    Finished,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetResult {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub schema_version: u32,
    pub state: RunState,
    pub assurance: String,
    pub message: Option<String>,
    pub target: Option<TargetResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub policy_version: u32,
    pub external_writers: String,
    pub supported_operations: Vec<String>,
    pub unsupported_features: Vec<String>,
    #[serde(default)]
    pub configured_exclusions: Vec<ConfiguredExclusion>,
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfiguredExclusion {
    pub root_id: String,
    pub path_bytes_base64: String,
    pub display_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Gap {
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRef {
    pub object_id: String,
    pub size: u64,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    Absent,
    Present { object: ObjectRef },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    ModeChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRef {
    pub kind: String,
    pub truncated: bool,
    pub patch_offset: Option<u64>,
    pub patch_bytes: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub root_id: String,
    pub path_bytes_base64: String,
    pub display_path: Option<String>,
    pub kind: ChangeKind,
    pub before: FileState,
    pub after: FileState,
    pub diff: DiffRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMetrics {
    pub elapsed_ms: u64,
    pub notifications_received: u64,
    pub queue_bypassed: u64,
    pub candidate_paths: usize,
    pub captured_bytes: u64,
    pub final_changes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub state: RunState,
    pub assurance: String,
    pub target: TargetResult,
    pub coverage: Coverage,
    pub metrics: RunMetrics,
    pub changes: Vec<Change>,
}

impl Status {
    pub fn new(state: RunState, message: Option<String>, target: Option<TargetResult>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state,
            assurance: "best_effort".into(),
            message,
            target,
        }
    }
}
