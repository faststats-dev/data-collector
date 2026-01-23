use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const POLAR_API_URL: &str = "https://sandbox-api.polar.sh";

#[derive(Clone)]
pub struct PolarClient {
    client: Client,
    token: String,
}

#[derive(Debug, Serialize)]
struct EventCreateExternalCustomer {
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    name: String,
    external_customer_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct EventsIngest {
    events: Vec<EventCreateExternalCustomer>,
}

#[derive(Debug, Deserialize)]
pub struct EventsIngestResponse {
    pub inserted: i64,
    #[serde(default)]
    pub duplicates: i64,
}

#[derive(Debug)]
pub enum PolarError {
    Request(reqwest::Error),
    Api { status: u16, message: String },
}

impl From<reqwest::Error> for PolarError {
    fn from(err: reqwest::Error) -> Self {
        PolarError::Request(err)
    }
}

impl std::fmt::Display for PolarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolarError::Request(e) => write!(f, "Request error: {}", e),
            PolarError::Api { status, message } => {
                write!(f, "Polar API error ({}): {}", status, message)
            }
        }
    }
}

/// Aggregated usage counts for a specific owner (customer)
#[derive(Debug, Default, Clone)]
pub struct UsageCounts {
    pub error_tracking: u64,
    pub web_vitals: u64,
    pub session_replays: u64,
}

impl PolarClient {
    pub fn new(token: String) -> Self {
        Self {
            client: Client::new(),
            token,
        }
    }

    /// Ingest usage events to Polar for billing
    ///
    /// # Arguments
    /// * `usage_by_owner` - Map of owner_id (external_customer_id) to their usage counts
    /// * `token_by_owner` - Map of owner_id to the project token used (for metadata)
    pub async fn ingest_usage(
        &self,
        usage_by_owner: &HashMap<String, UsageCounts>,
        token_by_owner: &HashMap<String, String>,
    ) -> Result<EventsIngestResponse, PolarError> {
        let mut events = Vec::new();
        let timestamp = Utc::now();

        for (owner_id, counts) in usage_by_owner {
            let token = token_by_owner.get(owner_id).cloned();
            let metadata = token.map(|t| {
                let mut map = HashMap::new();
                map.insert("token".to_string(), serde_json::Value::String(t));
                map
            });

            if counts.error_tracking > 0 {
                events.push(EventCreateExternalCustomer {
                    timestamp: Some(timestamp),
                    name: "error_tracking".to_string(),
                    external_customer_id: owner_id.clone(),
                    metadata: metadata.clone().map(|mut m| {
                        m.insert(
                            "count".to_string(),
                            serde_json::Value::Number(counts.error_tracking.into()),
                        );
                        m
                    }),
                });
            }

            if counts.web_vitals > 0 {
                events.push(EventCreateExternalCustomer {
                    timestamp: Some(timestamp),
                    name: "web_vitals".to_string(),
                    external_customer_id: owner_id.clone(),
                    metadata: metadata.clone().map(|mut m| {
                        m.insert(
                            "count".to_string(),
                            serde_json::Value::Number(counts.web_vitals.into()),
                        );
                        m
                    }),
                });
            }

            if counts.session_replays > 0 {
                events.push(EventCreateExternalCustomer {
                    timestamp: Some(timestamp),
                    name: "session_replays".to_string(),
                    external_customer_id: owner_id.clone(),
                    metadata: metadata.map(|mut m| {
                        m.insert(
                            "count".to_string(),
                            serde_json::Value::Number(counts.session_replays.into()),
                        );
                        m
                    }),
                });
            }
        }

        if events.is_empty() {
            return Ok(EventsIngestResponse {
                inserted: 0,
                duplicates: 0,
            });
        }

        let body = EventsIngest { events };

        let response = self
            .client
            .post(format!("{}/v1/events/ingest", POLAR_API_URL))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();

        if status != 200 {
            let message = response.text().await.unwrap_or_default();
            return Err(PolarError::Api { status, message });
        }

        let result: EventsIngestResponse = response.json().await?;
        Ok(result)
    }
}
