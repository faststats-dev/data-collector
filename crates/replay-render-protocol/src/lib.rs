use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::path::PathBuf;

pub const MAX_INPUT: usize = 64 * 1024 * 1024;
pub const MAX_VIDEO: usize = 64 * 1024 * 1024;
pub const MAX_MESSAGE: usize = 128 * 1024;
pub const JOB_SECONDS: u64 = 1800;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol: u8,
    pub events: Vec<Box<RawValue>>,
    pub fps: u32,
    pub speed: f64,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.protocol == 1, "unsupported render protocol");
        ensure!((1..=120).contains(&self.fps), "invalid FPS");
        ensure!(
            self.speed.is_finite() && (0.1..=64.0).contains(&self.speed),
            "invalid speed"
        );
        Ok(())
    }
}

/// One bounded JSON message per line; video chunks are at most 48 KiB decoded.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Progress,
    Video { data: String },
    Complete { report: RenderReport },
    Failed { message: String, retryable: bool },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenderReport {
    pub output: PathBuf,
    pub frames: u64,
    pub video_duration_seconds: f64,
    pub stats: RenderStats,
}

/// Wall-clock stage timings. Encoder work overlaps replay/capture; `encoder_wait`
/// measures backpressure, not total FFmpeg CPU time.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RenderStats {
    pub screenshots: u64,
    pub packets: u64,
    pub setup: f64,
    pub advance: f64,
    pub capture: f64,
    pub encoder_wait: f64,
    pub finalize: f64,
    pub total: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_render_settings_and_unknown_fields() {
        let mut req = Request {
            protocol: 1,
            events: vec![],
            fps: 3,
            speed: 1.0,
        };
        assert!(req.validate().is_ok());
        req.speed = f64::NAN;
        assert!(req.validate().is_err());
        req.speed = 1.0;
        req.fps = 0;
        assert!(req.validate().is_err());
        assert!(
            serde_json::from_str::<Request>(
                r#"{"protocol":1,"events":[],"fps":3,"speed":1,"url":"http://metadata"}"#
            )
            .is_err()
        );
    }
}
