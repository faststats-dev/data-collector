//! Summarize replay video and validate the model response.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, path::Path, time::Duration};

pub const MODEL: &str = "z-ai/glm-5.3-flash";
const PROMPT: &str = include_str!("../prompt.md");
const MAX_VIDEO_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PainPoint {
    /// Milliseconds from the first rrweb event, not the accelerated video time.
    #[serde(rename = "timestampMs")]
    pub timestamp_ms: u64,
    pub description: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySummary {
    pub summary: String,
    #[serde(rename = "painPoints")]
    pub pain_points: Vec<PainPoint>,
}

fn request(video: String, start_ms: u64, duration_ms: u64) -> Value {
    json!({
        "model": MODEL,
        "provider": {"require_parameters": true},
        "reasoning": {"effort": "low"},
        "max_tokens": 4096,
        "messages": [
            {"role": "system", "content": PROMPT},
            {"role": "user", "content": [
                {"type": "text", "text": format!("Recording start (Unix epoch ms): {start_ms}. Recording duration (ms): {duration_ms}.")},
                {"type": "video_url", "video_url": {"url": video}}
            ]}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "replay_summary", "strict": true,
                "schema": {
                    "type": "object", "additionalProperties": false,
                    "required": ["summary", "painPoints"],
                    "properties": {
                        "summary": {"type": "string"},
                        "painPoints": {"type": "array", "items": {
                            "type": "object", "additionalProperties": false,
                            "required": ["timestampMs", "description"],
                            "properties": {
                                "timestampMs": {"type": "integer", "description": "Original elapsed replay milliseconds shown in the video footer."},
                                "description": {"type": "string", "description": "The supported failure and any directly observed consequence, excluding replay artifacts and speculation."}
                            }
                        }}
                    }
                }
            }
        }
    })
}

fn parse(response: &Value, duration_ms: u64) -> Result<ReplaySummary> {
    let choice = &response["choices"][0];
    ensure!(
        choice["finish_reason"] == "stop",
        "model did not finish a complete summary"
    );
    let content = choice["message"]["content"]
        .as_str()
        .context("model returned no summary")?;
    let mut summary: ReplaySummary =
        serde_json::from_str(content).context("invalid summary JSON")?;
    ensure!(
        !summary.summary.trim().is_empty() && summary.summary.len() <= 16_000,
        "invalid summary length"
    );
    ensure!(summary.pain_points.len() <= 100, "too many pain points");
    for point in &summary.pain_points {
        ensure!(
            point.timestamp_ms <= duration_ms,
            "pain point is outside the replay timeline"
        );
        ensure!(
            !point.description.trim().is_empty() && point.description.len() <= 4000,
            "invalid pain point description"
        );
    }
    summary.pain_points.sort_by_key(|point| point.timestamp_ms);
    Ok(summary)
}

fn encode_video(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).context("open rendered video")?;
    let size = file.metadata()?.len();
    ensure!(size > 0, "rendered video is empty");
    ensure!(
        size <= MAX_VIDEO_BYTES,
        "rendered video exceeds the 64 MiB model input limit"
    );
    // Bound the actual read too, even if the file grows after checking metadata.
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_VIDEO_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(!bytes.is_empty(), "rendered video is empty");
    ensure!(
        bytes.len() as u64 <= MAX_VIDEO_BYTES,
        "rendered video exceeds the 64 MiB model input limit"
    );
    let mut video = String::from("data:video/mp4;base64,");
    STANDARD.encode_string(bytes, &mut video);
    Ok(video)
}

pub struct Summarizer {
    client: reqwest::Client,
    api_key: String,
}

impl Summarizer {
    pub fn new(api_key: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(300))
                .build()?,
            api_key,
        })
    }

    pub async fn summarize(
        &self,
        path: &Path,
        start_ms: u64,
        duration_ms: u64,
    ) -> Result<ReplaySummary> {
        let path = path.to_owned();
        let video = tokio::task::spawn_blocking(move || encode_video(&path))
            .await
            .context("video encoding task failed")??;
        let response = self
            .client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&request(video, start_ms, duration_ms))
            .send()
            .await
            .context("OpenRouter request failed")?
            .error_for_status()
            .context("OpenRouter rejected video summarization")?
            .json::<Value>()
            .await
            .context("invalid OpenRouter response")?;
        parse(&response, duration_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_input_is_encoded_and_bounded() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        assert!(
            encode_video(file.path())
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
        std::fs::write(file.path(), b"video bytes")?;
        assert_eq!(
            encode_video(file.path())?,
            "data:video/mp4;base64,dmlkZW8gYnl0ZXM="
        );
        // A sparse file exercises the size check without allocating a large video.
        file.as_file().set_len(MAX_VIDEO_BYTES + 1)?;
        assert!(
            encode_video(file.path())
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
        Ok(())
    }

    fn response(content: Value) -> Value {
        json!({"choices":[{"finish_reason":"stop", "message":{"content":content.to_string()}}]})
    }
    #[test]
    fn validates_model_output_and_orders_original_timestamps() {
        let valid = json!({"summary":"Checkout failed.","painPoints":[{"timestampMs":8000,"description":"Error shown."},{"timestampMs":1000,"description":"Repeated submit."}]});
        assert_eq!(
            parse(&response(valid.clone()), 8000).unwrap().pain_points[0].timestamp_ms,
            1000
        );
        assert!(parse(&response(valid), 7999).is_err());
        for invalid in [
            json!({"summary":"", "painPoints":[]}),
            json!({"summary":"OK", "painPoints":[{"timestampMs":-1,"description":"Error"}]}),
            json!({"summary":"OK", "painPoints":[{"timestampMs":0,"description":""}]}),
            json!({"summary":"OK"}),
            json!({"summary":"OK", "painPoints":[],"extra":true}),
        ] {
            assert!(parse(&response(invalid), 8000).is_err());
        }
        let mut truncated = response(json!({"summary":"OK", "painPoints":[]}));
        truncated["choices"][0]["finish_reason"] = json!("length");
        assert!(parse(&truncated, 8000).is_err());
        assert!(
            parse(
                &response(json!({"summary":"No issue observed.","painPoints":[]})),
                0
            )
            .is_ok()
        );
    }
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and REPLAY_TEST_VIDEO; makes a paid API request"]
    async fn real_openrouter_video_returns_valid_summary() -> Result<()> {
        if let Ok(path) = std::env::var("OPENROUTER_ENV_FILE") {
            dotenvy::from_path(path)?;
        }
        let path = std::env::var("REPLAY_TEST_VIDEO")?;
        let summarizer = Summarizer::new(crate::config::required("OPENROUTER_API_KEY")?)?;
        let summary = summarizer
            .summarize(Path::new(&path), 1_700_000_000_000, 8000)
            .await?;
        assert!(!summary.summary.trim().is_empty());
        Ok(())
    }

    #[test]
    fn request_sends_actual_video_and_requires_schema() {
        let body = request("data:video/mp4;base64,AAAA".into(), 123, 8000);
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["messages"][0]["content"], PROMPT);
        assert_eq!(
            body["messages"][1]["content"][1]["video_url"]["url"],
            "data:video/mp4;base64,AAAA"
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["provider"]["require_parameters"], true);
    }
}
