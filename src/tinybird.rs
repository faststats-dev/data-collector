use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::Write;
use uuid::Uuid;

#[derive(Clone)]
pub struct TinybirdClient {
    client: Client,
    base_url: String,
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub server_id: Uuid,
    pub data: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRow {
    pub id: Uuid,
    pub name: String,
    pub message: String,
    pub stack: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorTrackingRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub hash: String,
    pub error_id: Uuid,
    pub count: u32,
    pub data_entry_id: Uuid,
    pub session_id: Option<String>,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebVitalRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub metric: String,
    pub value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    pub url: String,
    pub attributes: String,
    pub session_id: Option<String>,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub session_id: String,
    pub events: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum TinybirdError {
    Request(reqwest::Error),
    Api { status: u16, message: String },
    Compression(std::io::Error),
}

impl From<reqwest::Error> for TinybirdError {
    fn from(err: reqwest::Error) -> Self {
        TinybirdError::Request(err)
    }
}

impl From<std::io::Error> for TinybirdError {
    fn from(err: std::io::Error) -> Self {
        TinybirdError::Compression(err)
    }
}

impl std::fmt::Display for TinybirdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TinybirdError::Request(e) => write!(f, "Request error: {}", e),
            TinybirdError::Api { status, message } => {
                write!(f, "Tinybird API error ({}): {}", status, message)
            }
            TinybirdError::Compression(e) => write!(f, "Compression error: {}", e),
        }
    }
}

impl TinybirdError {
    pub fn is_transient(&self) -> bool {
        match self {
            TinybirdError::Request(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            TinybirdError::Api { status, .. } => *status == 429 || *status >= 500,
            TinybirdError::Compression(_) => false,
        }
    }
}

fn gzip_compress(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 4), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

impl TinybirdClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            client: Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .pool_max_idle_per_host(5)
                .build()
                .expect("Failed to build HTTP client"),
            base_url,
            token,
        }
    }

    async fn send_batch<T: Serialize>(
        &self,
        datasource: &str,
        rows: &[T],
    ) -> Result<(), TinybirdError> {
        if rows.is_empty() {
            return Ok(());
        }

        let url = format!("{}/v0/events?name={}&wait=true", self.base_url, datasource);

        let mut ndjson = Vec::with_capacity(rows.len() * 256);
        for row in rows {
            serde_json::to_writer(&mut ndjson, row).expect("Failed to serialize row");
            ndjson.push(b'\n');
        }

        let compressed = gzip_compress(&ndjson)?;
        drop(ndjson);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/x-ndjson")
            .header("Content-Encoding", "gzip")
            .body(compressed)
            .send()
            .await?;

        let status = response.status().as_u16();

        if status != 200 && status != 202 {
            let message = response.text().await.unwrap_or_default();
            return Err(TinybirdError::Api { status, message });
        }

        let _ = response.bytes().await;

        Ok(())
    }

    pub async fn insert_events_ref(&self, events: &[&EventRow]) -> Result<(), TinybirdError> {
        self.send_batch("events", events).await
    }

    pub async fn insert_errors(&self, errors: &[ErrorRow]) -> Result<(), TinybirdError> {
        self.send_batch("error_", errors).await
    }

    pub async fn insert_error_trackings_ref(
        &self,
        rows: &[&ErrorTrackingRow],
    ) -> Result<(), TinybirdError> {
        self.send_batch("error_tracking", rows).await
    }

    pub async fn insert_web_vitals_ref(&self, rows: &[&WebVitalRow]) -> Result<(), TinybirdError> {
        self.send_batch("web_vitals", rows).await
    }

    pub async fn insert_replays_ref(&self, rows: &[&ReplayRow]) -> Result<(), TinybirdError> {
        self.send_batch("session_replays", rows).await
    }
}
