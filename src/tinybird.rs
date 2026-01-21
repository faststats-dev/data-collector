use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
    pub id: u32,
    pub name: String,
    pub message: String,
    pub stack: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorTrackingRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub hash: String,
    pub error_id: u32,
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
    pub label: String,
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
}

impl From<reqwest::Error> for TinybirdError {
    fn from(err: reqwest::Error) -> Self {
        TinybirdError::Request(err)
    }
}

impl std::fmt::Display for TinybirdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TinybirdError::Request(e) => write!(f, "Request error: {}", e),
            TinybirdError::Api { status, message } => {
                write!(f, "Tinybird API error ({}): {}", status, message)
            }
        }
    }
}

impl TinybirdError {
    /// Returns true if this error is transient and the request should be retried.
    pub fn is_transient(&self) -> bool {
        match self {
            TinybirdError::Request(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            TinybirdError::Api { status, .. } => *status == 429 || *status >= 500,
        }
    }
}

impl TinybirdClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            client: Client::new(),
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

        let body = rows
            .iter()
            .map(|row| serde_json::to_string(row).expect("Failed to serialize row"))
            .collect::<Vec<_>>()
            .join("\n");

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/x-ndjson")
            .body(body)
            .send()
            .await?;

        let status = response.status().as_u16();
        let response_body = response.text().await.unwrap_or_default();

        if status != 200 && status != 202 {
            return Err(TinybirdError::Api {
                status,
                message: response_body,
            });
        }

        Ok(())
    }

    pub async fn insert_events(&self, events: &[EventRow]) -> Result<(), TinybirdError> {
        self.send_batch("events", events).await
    }

    pub async fn insert_errors(&self, errors: &[ErrorRow]) -> Result<(), TinybirdError> {
        self.send_batch("error_", errors).await
    }

    pub async fn insert_error_trackings(
        &self,
        error_trackings: &[ErrorTrackingRow],
    ) -> Result<(), TinybirdError> {
        self.send_batch("error_tracking", error_trackings).await
    }

    pub async fn insert_web_vitals(&self, web_vitals: &[WebVitalRow]) -> Result<(), TinybirdError> {
        self.send_batch("web_vitals", web_vitals).await
    }

    pub async fn insert_replays(&self, replays: &[ReplayRow]) -> Result<(), TinybirdError> {
        self.send_batch("session_replays", replays).await
    }
}
