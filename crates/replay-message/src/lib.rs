use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const DEFAULT_TOPIC: &str = "replay-snapshot";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplayCommand {
    Snapshot(Box<ReplayChunk>),
    SessionPatch(ReplaySessionPatch),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplayChunk {
    pub project_id: Uuid,
    pub storage_generation: i32,
    pub session_id: String,
    pub window_id: String,
    pub view_id: Option<String>,
    pub session_start_ms: Option<i64>,
    pub is_final: bool,
    pub flush_reason: Option<String>,
    pub batch_id: Option<String>,
    pub sequence: i64,
    pub first_sequence: Option<i64>,
    pub last_sequence: Option<i64>,
    pub client_batch_count: i32,
    pub identifier: Option<String>,
    pub browser: Option<String>,
    pub country: Option<String>,
    pub os: Option<String>,
    pub url: Option<String>,
    pub events: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplaySessionPatch {
    pub project_id: Uuid,
    pub session_id: String,
    pub window_id: String,
    #[serde(default)]
    pub has_errors: bool,
    #[serde(default)]
    pub has_poor_vitals: bool,
}
