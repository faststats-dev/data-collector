pub mod coverage;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Environment variable used by both the producer and consumer to select the topic.
pub const TOPIC_ENV: &str = "REPLAY_KAFKA_TOPIC";
/// Topic used when [`TOPIC_ENV`] is not set.
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
    /// None denotes the legacy contract, whose initial sequence is unknown.
    #[serde(default)]
    pub sequence_contract_version: Option<u32>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_generation: Option<i32>,
    pub project_id: Uuid,
    pub session_id: String,
    pub window_id: String,
    #[serde(default)]
    pub has_errors: bool,
    #[serde(default)]
    pub has_poor_vitals: bool,
}

impl ReplayChunk {
    pub fn validate_sequence(&self) -> Result<(), &'static str> {
        if !(0..=coverage::MAX_SEQUENCE).contains(&self.sequence) {
            return Err("sequence must be a nonnegative JavaScript safe integer");
        }
        let first = self.first_sequence.unwrap_or(self.sequence);
        let last = self.last_sequence.unwrap_or(self.sequence);
        if first < 0
            || last > coverage::MAX_SEQUENCE
            || first > last
            || self.sequence < first
            || self.sequence > last
        {
            return Err("invalid inclusive sequence range");
        }
        if let Some(version) = self.sequence_contract_version {
            if version != 1 {
                return Err("unsupported sequence contract");
            }
            if self.first_sequence.is_some_and(|s| s != self.sequence)
                || self.last_sequence.is_some_and(|s| s != self.sequence)
                || self.client_batch_count != 1
            {
                return Err("contract v1 requires one immutable client batch per envelope");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn chunk() -> ReplayChunk {
        serde_json::from_value(json!({"project_id":Uuid::nil(),"storage_generation":1,"session_id":"s","window_id":"w","sequence":0,"sequence_contract_version":1,"is_final":false,"events":[],"client_batch_count":1})).unwrap()
    }
    #[test]
    fn legacy_coalescing_is_not_a_versioned_coverage_proof() {
        let mut chunk = chunk();
        chunk.first_sequence = Some(0);
        chunk.last_sequence = Some(2);
        chunk.client_batch_count = 3;
        assert!(chunk.validate_sequence().is_err());
        chunk.sequence_contract_version = None;
        assert!(chunk.validate_sequence().is_ok());
        chunk.first_sequence = Some(3);
        assert!(chunk.validate_sequence().is_err());
    }
    #[test]
    fn sequence_boundaries_are_safe_on_both_sides_of_the_wire() {
        let mut chunk = chunk();
        for sequence in [0, 1, coverage::MAX_SEQUENCE] {
            chunk.sequence = sequence;
            assert!(chunk.validate_sequence().is_ok());
        }
        for sequence in [-1, coverage::MAX_SEQUENCE + 1, i64::MAX] {
            chunk.sequence = sequence;
            assert!(chunk.validate_sequence().is_err());
        }
        chunk.sequence = 0;
        chunk.sequence_contract_version = Some(2);
        assert!(chunk.validate_sequence().is_err());
    }
}
