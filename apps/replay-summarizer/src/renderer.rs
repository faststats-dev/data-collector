//! Client for the private rendering service.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use replay_render_protocol::{self as protocol, Message, RenderReport, Request};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
struct RenderFailure {
    retryable: bool,
}
impl std::fmt::Display for RenderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("isolated renderer rejected or failed the job")
    }
}
impl std::error::Error for RenderFailure {}

pub fn retryable(error: &anyhow::Error) -> bool {
    if let Some(failure) = error.downcast_ref::<RenderFailure>() {
        return failure.retryable;
    }
    !error
        .downcast_ref::<reqwest::Error>()
        .and_then(|e| e.status())
        .is_some_and(|s| s.is_client_error() && !matches!(s.as_u16(), 408 | 429))
}

pub struct Client {
    client: reqwest::Client,
    url: String,
    token: String,
}
impl Client {
    pub fn new() -> Result<Self> {
        let url = crate::config::required("REPLAY_RENDER_URL")?;
        let parsed = reqwest::Url::parse(&url)?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none(),
            "invalid REPLAY_RENDER_URL"
        );
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(protocol::JOB_SECONDS + 120))
                .build()?,
            url,
            token: crate::config::required("REPLAY_RENDER_TOKEN")?,
        })
    }
    pub async fn render(&self, request: Request, output: &Path) -> Result<RenderReport> {
        let body = serde_json::to_vec(&request)?;
        ensure!(
            body.len() <= protocol::MAX_INPUT,
            "render request exceeds limit"
        );
        let mut response = self
            .client
            .post(format!("{}/v1/render", self.url.trim_end_matches('/')))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        let mut pending = Vec::new();
        let mut file = tokio::fs::File::create(output).await?;
        let mut size = 0usize;
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), response.chunk())
            .await
            .context("renderer response stalled")??
        {
            // HTTP chunks can contain partial or multiple NDJSON messages.
            for bytes in chunk.split_inclusive(|byte| *byte == b'\n') {
                ensure!(
                    pending.len() + bytes.len() <= protocol::MAX_MESSAGE,
                    "oversized renderer response"
                );
                pending.extend_from_slice(bytes);
                if pending.last() != Some(&b'\n') {
                    continue;
                }
                let message: Message = serde_json::from_slice(&pending)?;
                pending.clear();
                match message {
                    Message::Progress => {}
                    Message::Video { data } => {
                        let bytes = STANDARD.decode(data)?;
                        ensure!(
                            bytes.len() <= 48 * 1024 && size + bytes.len() <= protocol::MAX_VIDEO,
                            "oversized renderer video"
                        );
                        file.write_all(&bytes).await?;
                        size += bytes.len();
                    }
                    Message::Complete { mut report } => {
                        ensure!(
                            size > 0
                                && report.frames > 0
                                && report.video_duration_seconds.is_finite(),
                            "invalid render completion"
                        );
                        file.flush().await?;
                        report.output = output.to_owned();
                        return Ok(report);
                    }
                    Message::Failed { retryable, .. } => {
                        return Err(RenderFailure { retryable }.into());
                    }
                }
            }
        }
        anyhow::bail!("renderer disconnected before completion")
    }
}
