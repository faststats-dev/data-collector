use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tracing::debug;
use uuid::Uuid;

#[derive(Clone)]
pub struct TinybirdClient {
    client: Client,
    base_url: String,
    bearer_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebEventRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub user_id: Option<String>,
    pub person_id: Option<String>,
    pub external_id: Option<String>,
    pub is_identified: bool,
    pub session_id: Option<String>,
    pub event: Option<String>,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub device: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub referrer: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_term: Option<String>,
    pub utm_content: Option<String>,
    pub title: Option<String>,
    pub page: Option<String>,
    pub url: Option<String>,
    pub outbound_link: Option<String>,
    pub country: Option<String>,
    pub custom: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModsEventRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub server_id: Uuid,
    pub player_count: Option<f64>,
    pub online_mode: Option<bool>,
    pub plugin_version: Option<String>,
    pub minecraft_version: Option<String>,
    pub server_type: Option<String>,
    pub java_version: Option<String>,
    pub java_vendor: Option<String>,
    pub os_name: Option<String>,
    pub os_arch: Option<String>,
    pub os_version: Option<String>,
    pub core_count: Option<u16>,
    pub country: Option<String>,
    pub custom: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRow {
    pub hash: String,
    pub name: String,
    pub message: String,
    pub stack: Vec<String>,
    pub cause_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorTrackingRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub group_hash: String,
    pub hash: String,
    pub error_hash: String,
    pub count: u32,
    pub data_entry_id: Option<Uuid>,
    pub session_id: Option<String>,
    pub identity_key: Option<String>,
    pub build_id: Option<String>,
    pub sdk_name: Option<String>,
    pub sdk_version: Option<String>,
    pub plugin_version: String,
    pub source_kind: String,
    pub entry_session_id: String,
    pub entry_country: String,
    pub entry_browser: String,
    pub entry_device: String,
    pub entry_os: String,
    pub player_count: Option<f64>,
    pub online_mode: Option<bool>,
    pub minecraft_version: String,
    pub server_type: String,
    pub java_version: String,
    pub java_vendor: String,
    pub os_version: String,
    pub os_arch: String,
    pub core_count: Option<u16>,
    pub entry_data: String,
    pub context: Option<String>,
    pub handled: Option<bool>,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebVitalRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub metric: String,
    pub value: f64,
    pub device: Option<String>,
    pub country: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
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
    pub identifier: Option<String>,
    pub events: String,
    pub has_full_snapshot: u8,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum TinybirdError {
    Request(reqwest::Error),
    Api { status: u16, message: String },
    Compression(std::io::Error),
    Serialization(serde_json::Error),
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

impl From<serde_json::Error> for TinybirdError {
    fn from(err: serde_json::Error) -> Self {
        TinybirdError::Serialization(err)
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
            TinybirdError::Serialization(e) => write!(f, "Serialization error: {}", e),
        }
    }
}

impl TinybirdError {
    pub fn is_transient(&self) -> bool {
        match self {
            TinybirdError::Request(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            TinybirdError::Api { status, .. } => *status == 429 || *status >= 500,
            TinybirdError::Compression(_) | TinybirdError::Serialization(_) => false,
        }
    }
}

impl TinybirdClient {
    pub fn new(base_url: String, token: String) -> Self {
        let mut bearer_token = String::with_capacity(7 + token.len());
        bearer_token.push_str("Bearer ");
        bearer_token.push_str(&token);

        Self {
            client: Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(15))
                .pool_max_idle_per_host(2)
                .build()
                .unwrap_or_else(|_| Client::new()),
            base_url,
            bearer_token,
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

        let mut encoder = GzEncoder::new(Vec::with_capacity(rows.len() * 256), Compression::fast());

        for row in rows {
            serde_json::to_writer(&mut encoder, row)?;
            encoder.write_all(b"\n")?;
        }

        let compressed = encoder.finish()?;

        let response = self
            .client
            .post(&url)
            .header("Authorization", &self.bearer_token)
            .header("Content-Type", "application/x-ndjson")
            .header("Content-Encoding", "gzip")
            .body(compressed)
            .send()
            .await?;

        let status = response.status().as_u16();

        debug!("Tinybird request status: {}", status);

        if status != 200 && status != 202 {
            let message = response.text().await.unwrap_or_default();
            return Err(TinybirdError::Api { status, message });
        }

        let _ = response.bytes().await;

        Ok(())
    }

    pub async fn insert_web_events(&self, events: &[&WebEventRow]) -> Result<(), TinybirdError> {
        self.send_batch("web_events", events).await
    }

    pub async fn insert_mods_events(&self, events: &[&ModsEventRow]) -> Result<(), TinybirdError> {
        self.send_batch("mods_events", events).await
    }

    pub async fn insert_errors(&self, errors: &[ErrorRow]) -> Result<(), TinybirdError> {
        self.send_batch("errors", errors).await
    }

    pub async fn insert_error_trackings(
        &self,
        rows: &[&ErrorTrackingRow],
    ) -> Result<(), TinybirdError> {
        self.send_batch("error_occurences_v2", rows).await
    }

    pub async fn insert_web_vitals(&self, rows: &[&WebVitalRow]) -> Result<(), TinybirdError> {
        self.send_batch("web_vitals", rows).await
    }

    pub async fn insert_replays(&self, rows: &[&ReplayRow]) -> Result<(), TinybirdError> {
        self.send_batch("session_replays", rows).await
    }
}
