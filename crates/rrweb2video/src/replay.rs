use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::value::RawValue;

/// Validated replay envelope. Event payloads remain raw to avoid building a DOM in Rust.
#[derive(Debug)]
pub struct Replay {
    pub(crate) events: Vec<Box<RawValue>>,
    pub start_ms: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
}

impl Replay {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        Self::from_events(serde_json::from_slice(bytes).context("expected an rrweb event array")?)
    }

    /// Take ownership of raw events without serializing and reparsing the recording.
    pub fn from_events(events: Vec<Box<RawValue>>) -> Result<Self> {
        #[derive(Deserialize)]
        struct Event<'a> {
            #[serde(rename = "type")]
            kind: u8,
            timestamp: u64,
            #[serde(borrow)]
            data: &'a RawValue,
        }
        #[derive(Deserialize)]
        struct Viewport {
            width: u32,
            height: u32,
        }
        ensure!(events.len() >= 2, "at least two events are required");
        let (mut first, mut last, mut width, mut height, mut snapshot) = (0, 0, 0, 0, false);
        for (index, raw) in events.iter().enumerate() {
            let e: Event = serde_json::from_str(raw.get())
                .with_context(|| format!("invalid event {index}"))?;
            ensure!(
                e.timestamp <= 9_007_199_254_740_991,
                "timestamp exceeds JavaScript precision"
            );
            if index == 0 {
                first = e.timestamp;
            }
            ensure!(
                e.timestamp >= last,
                "events must be ordered by timestamp (event {index})"
            );
            last = e.timestamp;
            if e.kind == 2 {
                snapshot = true;
            }
            if e.kind == 4 {
                let v: Viewport =
                    serde_json::from_str(e.data.get()).context("invalid metadata viewport")?;
                ensure!(
                    v.width > 0 && v.height > 0 && v.width <= 16384 && v.height <= 16384,
                    "invalid viewport dimensions"
                );
                width = width.max(v.width);
                height = height.max(v.height);
            }
        }
        ensure!(snapshot, "replay has no full snapshot");
        ensure!(width > 0 && height > 0, "replay has no viewport metadata");
        Ok(Self {
            events,
            start_ms: first,
            duration_ms: last - first,
            width,
            height,
        })
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

/// Frame timestamps are computed from an integer index, never accumulated wall time.
#[derive(Debug, Clone, Copy)]
pub struct FramePlan {
    pub fps: u32,
    pub speed: f64,
    pub duration_ms: u64,
    pub frame_count: u64,
}
impl FramePlan {
    pub fn new(duration_ms: u64, fps: u32, speed: f64) -> Result<Self> {
        ensure!((1..=120).contains(&fps), "fps must be between 1 and 120");
        ensure!(
            speed.is_finite() && (0.1..=64.0).contains(&speed),
            "speed must be between 0.1 and 64"
        );
        ensure!(
            duration_ms <= 86_400_000,
            "render duration exceeds 24 hours"
        );
        let frame_count = (duration_ms as f64 * fps as f64 / (1000.0 * speed)).ceil() as u64 + 1;
        Ok(Self {
            fps,
            speed,
            duration_ms,
            frame_count,
        })
    }
    pub fn replay_time_ms(&self, index: u64) -> f64 {
        (index as f64 * 1000.0 * self.speed / self.fps as f64).min(self.duration_ms as f64)
    }
}
