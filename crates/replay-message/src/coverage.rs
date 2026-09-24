//! Accepted sequence metadata. No operation enumerates individual sequence IDs.
use serde::{Deserialize, Serialize};

pub const MAX_SEQUENCE: i64 = 9_007_199_254_740_991;
pub const MAX_RANGES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationState {
    Open,
    Complete,
    TimedOutIncomplete,
}

impl FinalizationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::TimedOutIncomplete => "timed_out_incomplete",
        }
    }
}

impl std::str::FromStr for FinalizationState {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open" => Ok(Self::Open),
            "complete" => Ok(Self::Complete),
            "timed_out_incomplete" => Ok(Self::TimedOutIncomplete),
            _ => Err("invalid recording finalization state"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    #[default]
    LegacyContract,
    RangeLimit,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub ranges: Vec<[i64; 2]>,
    pub terminal: Option<i64>,
    pub unknown: Option<UnknownReason>,
}

impl Coverage {
    pub fn legacy() -> Self {
        Self {
            unknown: Some(UnknownReason::LegacyContract),
            ..Self::default()
        }
    }

    /// Called only after the chunk metadata (or an empty terminal) is accepted.
    /// Legacy endpoints deliberately contribute no inferred interior coverage.
    pub fn accept(&mut self, sequence: i64, version: Option<u32>, terminal: bool) {
        if terminal {
            self.terminal = Some(self.terminal.map_or(sequence, |old| old.max(sequence)));
        }
        if version != Some(1) && self.unknown.is_none() {
            self.unknown = Some(UnknownReason::LegacyContract);
        }
        if self.unknown == Some(UnknownReason::RangeLimit) {
            return;
        }
        let mut incoming = [sequence, sequence];
        let start = self.ranges.partition_point(|range| range[1] < sequence - 1);
        let mut end = start;
        while end < self.ranges.len() && self.ranges[end][0] <= incoming[1] + 1 {
            incoming[0] = incoming[0].min(self.ranges[end][0]);
            incoming[1] = incoming[1].max(self.ranges[end][1]);
            end += 1;
        }
        if start == end && self.ranges.len() == MAX_RANGES {
            // Explicitly abandon proof, not accepted metadata. Never hide gaps.
            self.ranges.clear();
            self.unknown = Some(UnknownReason::RangeLimit);
            return;
        }
        self.ranges.splice(start..end, [incoming]);
    }

    pub fn complete(&self) -> bool {
        self.unknown.is_none()
            && self
                .terminal
                .is_some_and(|terminal| self.ranges.as_slice() == [[0, terminal]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_coverage_golden_cases() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/replay-coverage-v1.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let mut coverage = Coverage::default();
            for message in case["messages"].as_array().unwrap() {
                coverage.accept(
                    message[0].as_i64().unwrap(),
                    message[1].as_u64().map(|v| v as u32),
                    message[2].as_bool().unwrap(),
                );
            }
            assert_eq!(
                coverage.complete(),
                case["complete"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::to_value(coverage).unwrap(),
                case["coverage"],
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn sparse_sequences_are_bounded_and_never_falsely_complete() {
        let mut coverage = Coverage::default();
        for sequence in 0..=MAX_RANGES {
            coverage.accept(sequence as i64 * 2, Some(1), false);
        }
        assert_eq!(coverage.unknown, Some(UnknownReason::RangeLimit));
        assert!(coverage.ranges.is_empty());
        coverage.accept(MAX_SEQUENCE, Some(1), true);
        assert!(!coverage.complete());
    }
    #[test]
    fn overlapping_and_duplicate_deliveries_are_idempotent() {
        let mut coverage = Coverage::default();
        for sequence in [3, 1, 0, 2] {
            coverage.accept(sequence, Some(1), sequence == 3);
        }
        assert!(coverage.complete());
        let before = coverage.clone();
        coverage.accept(3, Some(1), true);
        coverage.accept(2, Some(1), true);
        assert_eq!(before, coverage);
        coverage.accept(4, Some(1), false);
        assert!(!coverage.complete());
    }
}
