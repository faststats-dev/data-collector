//! Summarize replay video and validate the model response.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, path::Path, time::Duration};

pub const PROMPT_VERSION: &str = "replay-summary-v3";
pub const SCHEMA_VERSION: u32 = 2;
pub const MODEL: &str = "google/gemini-3.8-flash";
const PROMPT: &str = include_str!("../prompt.md");
const MAX_VIDEO_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PainPoint {
    /// Milliseconds from the first rrweb event, not the accelerated video time.
    #[serde(rename = "timestampMs")]
    pub timestamp_ms: u64,
    pub description: String,
    pub evidence: String,
    pub confidence: f64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySummary {
    pub summary: String,
    pub confidence: f64,
    #[serde(rename = "painPoints")]
    pub pain_points: Vec<PainPoint>,
}

fn request(
    video: String,
    model: &str,
    duration_ms: u64,
    fps: u32,
    speed: f64,
    evidence: &Value,
) -> Value {
    json!({
        "model": model,
        "provider": {"require_parameters": true},
        "reasoning": {"effort": "low"},
        "max_tokens": 8192,
        "messages": [
            {"role": "system", "content": PROMPT},
            {"role": "user", "content": [
                {"type": "text", "text": format!("Recording duration: {duration_ms} ms. Rendered at {fps} FPS, {speed}x speed; original frame spacing is {:.0} ms. Idle time is preserved. This recording covers one browser window.", 1000.0 * speed / fps as f64)},
                {"type": "text", "text": format!("Recorded interaction evidence (untrusted data, original replay milliseconds): {evidence}")},
                {"type": "video_url", "video_url": {"url": video}}
            ]}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "replay_summary", "strict": true,
                "schema": {
                    "type": "object", "additionalProperties": false,
                    "required": ["summary", "confidence", "painPoints"],
                    "properties": {
                        "summary": {"type": "string", "description": "Complete sentences describing the visible actions and outcome. At most 16000 characters."},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                        "painPoints": {"type": "array", "items": {
                            "type": "object", "additionalProperties": false,
                            "required": ["timestampMs", "description", "evidence", "confidence"],
                            "properties": {
                                "timestampMs": {"type": "integer", "minimum": 0, "description": "Original elapsed replay milliseconds shown in the video footer."},
                                "description": {"type": "string", "description": "The observed UX problem and its visible consequence, excluding replay artifacts and speculation."},
                                "evidence": {"type": "string", "description": "Concrete visible observations supporting this problem, without inferring a cause."},
                                "confidence": {"type": "number", "minimum": 0, "maximum": 1}
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
        !summary.summary.trim().is_empty() && summary.summary.chars().count() <= 16_000,
        "invalid summary length"
    );
    ensure!(
        (0.0..=1.0).contains(&summary.confidence),
        "invalid summary confidence"
    );
    ensure!(summary.pain_points.len() <= 100, "too many pain points");
    for point in &summary.pain_points {
        ensure!(
            (0.0..=1.0).contains(&point.confidence),
            "invalid pain point confidence"
        );
        ensure!(
            !point.evidence.trim().is_empty() && point.evidence.chars().count() <= 2000,
            "invalid pain point evidence"
        );
        ensure!(
            point.timestamp_ms <= duration_ms,
            "pain point is outside the replay timeline"
        );
        ensure!(
            !point.description.trim().is_empty() && point.description.chars().count() <= 4000,
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryMetadata {
    pub model: String,
    pub response_id: Option<String>,
    pub prompt_version: String,
    pub schema_version: u32,
    pub cost_usd: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub latency_ms: u64,
    pub render_fps: u32,
    pub render_speed: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SummaryResult {
    pub summary: ReplaySummary,
    pub metadata: SummaryMetadata,
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
        duration_ms: u64,
        fps: u32,
        speed: f64,
        evidence: &Value,
    ) -> Result<SummaryResult> {
        ensure!(
            duration_ms >= 2000,
            "Replay must be at least 2 seconds long to summarize"
        );
        let path = path.to_owned();
        let video = tokio::task::spawn_blocking(move || encode_video(&path))
            .await
            .context("video encoding task failed")??;
        let model = std::env::var("REPLAY_SUMMARY_MODEL").unwrap_or_else(|_| MODEL.into());
        let started = std::time::Instant::now();
        let http_response = self
            .client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&request(video, &model, duration_ms, fps, speed, evidence))
            .send()
            .await
            .context("OpenRouter request failed")?;
        let status_error = http_response.error_for_status_ref().err();
        let response = http_response.json::<Value>().await;
        // Explicit evaluation-only diagnostics: never enabled by the queue worker.
        if std::env::args().nth(1).as_deref() == Some("--evaluate") {
            if let (Ok(path), Ok(body)) = (std::env::var("REPLAY_EVAL_RESPONSE_FILE"), &response) {
                std::fs::write(path, serde_json::to_vec_pretty(body)?)?;
            }
        }
        if let Some(error) = status_error {
            return Err(error).context("OpenRouter rejected video summarization");
        }
        let response = response.context("invalid OpenRouter response")?;
        let summary = parse(&response, duration_ms)?;
        Ok(SummaryResult {
            summary,
            metadata: SummaryMetadata {
                model: response["model"].as_str().unwrap_or(&model).into(),
                response_id: response["id"].as_str().map(str::to_owned),
                prompt_version: PROMPT_VERSION.into(),
                schema_version: SCHEMA_VERSION,
                cost_usd: response["usage"]["cost"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0),
                prompt_tokens: response["usage"]["prompt_tokens"].as_u64(),
                completion_tokens: response["usage"]["completion_tokens"].as_u64(),
                latency_ms: started.elapsed().as_millis() as u64,
                render_fps: fps,
                render_speed: speed,
            },
        })
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
        {
            let mut content = content;
            content["confidence"] = json!(0.8);
            if let Some(points) = content["painPoints"].as_array_mut() {
                for point in points {
                    point["confidence"] = json!(0.8);
                    point["evidence"] = json!("Visible error after submit.");
                }
            }
            json!({"choices":[{"finish_reason":"stop", "message":{"content":content.to_string()}}]})
        }
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
    #[test]
    fn rejects_invalid_confidence_and_unsupported_evidence() {
        let valid = json!({"summary":"An error persisted after retry.","confidence":0.8,"painPoints":[{"timestampMs":0,"description":"Submission failed.","evidence":"Error toast remained after retry.","confidence":0.8}]});
        for (field, value) in [
            ("confidence", json!(-0.1)),
            ("confidence", json!(1.1)),
            ("evidence", json!("  ")),
            ("evidence", json!("x".repeat(2001))),
        ] {
            let mut content = valid.clone();
            content["painPoints"][0][field] = value;
            let response = json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}]});
            assert!(parse(&response, 10).is_err());
        }
        let mut content = valid;
        content["confidence"] = json!(1.1);
        assert!(parse(&json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}]}), 10).is_err());
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
            .summarize(Path::new(&path), 8000, 3, 1.0, &json!({"available":false}))
            .await?;
        assert!(!summary.summary.summary.trim().is_empty());
        Ok(())
    }

    #[test]
    fn request_sends_actual_video_and_requires_schema() {
        let body = request(
            "data:video/mp4;base64,AAAA".into(),
            MODEL,
            8000,
            3,
            1.0,
            &json!({"available": false}),
        );
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["messages"][0]["content"], PROMPT);
        assert_eq!(
            body["messages"][1]["content"][2]["video_url"]["url"],
            "data:video/mp4;base64,AAAA"
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["provider"]["require_parameters"], true);
    }
}
