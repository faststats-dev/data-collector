use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone)]
pub struct TinybirdClient {
    client: Client,
    base_url: String,
    token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub server_id: Uuid,
    pub data: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorRow {
    pub id: u32,
    pub name: String,
    pub message: String,
    pub stack: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
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

impl TinybirdClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
            token,
        }
    }

    async fn send_event<T: Serialize + std::fmt::Debug>(
        &self,
        datasource: &str,
        row: &T,
    ) -> Result<(), TinybirdError> {
        let url = format!("{}/v0/events?name={}&wait=true", self.base_url, datasource);
        let body = serde_json::to_string(row).expect("Failed to serialize row");

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
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

    pub async fn insert_event(&self, event: EventRow) -> Result<(), TinybirdError> {
        self.send_event("events", &event).await
    }

    pub async fn insert_error(&self, error: ErrorRow) -> Result<(), TinybirdError> {
        self.send_event("error_", &error).await
    }

    pub async fn insert_error_tracking(
        &self,
        error_tracking: ErrorTrackingRow,
    ) -> Result<(), TinybirdError> {
        self.send_event("error_tracking", &error_tracking).await
    }

    pub async fn insert_web_vital(&self, web_vital: WebVitalRow) -> Result<(), TinybirdError> {
        self.send_event("web_vitals", &web_vital).await
    }

    pub async fn insert_replay(&self, replay: ReplayRow) -> Result<(), TinybirdError> {
        self.send_event("session_replays", &replay).await
    }
}
