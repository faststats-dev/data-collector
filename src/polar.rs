use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const POLAR_API_URL: &str = "https://api.polar.sh";
const MAX_EVENTS_PER_REQUEST: usize = 500;

#[derive(Clone)]
pub struct PolarClient {
    client: Client,
    token: String,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Default, Clone)]
pub struct UsageCounts {
    pub events: u64,
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

    pub async fn ingest_usage(
        &self,
        usage_by_owner: &HashMap<String, UsageCounts>,
        token_by_owner: &HashMap<String, String>,
        org_by_owner: &HashMap<String, Option<String>>,
    ) -> Result<EventsIngestResponse, PolarError> {
        let mut events = Vec::with_capacity(usage_by_owner.len() * 4);
        let base_timestamp = Utc::now();

        for (owner_id, counts) in usage_by_owner {
            let base_metadata = Self::build_metadata(owner_id, token_by_owner, org_by_owner);

            for (i, (name, count)) in [
                ("events", counts.events),
                ("error_tracking", counts.error_tracking),
                ("web_vitals", counts.web_vitals),
                ("session_replays", counts.session_replays),
            ]
            .iter()
            .enumerate()
            {
                if *count == 0 {
                    continue;
                }

                let mut metadata = base_metadata.clone().unwrap_or_default();
                metadata.insert(
                    "count".to_string(),
                    serde_json::Value::Number((*count).into()),
                );

                events.push(EventCreateExternalCustomer {
                    timestamp: Some(base_timestamp + chrono::Duration::microseconds(i as i64)),
                    name: name.to_string(),
                    external_customer_id: owner_id.clone(),
                    metadata: Some(metadata),
                });
            }
        }

        if events.is_empty() {
            return Ok(EventsIngestResponse {
                inserted: 0,
                duplicates: 0,
            });
        }

        if events.len() <= MAX_EVENTS_PER_REQUEST {
            return self.send_events(&events).await;
        }

        let mut total_inserted = 0i64;
        let mut total_duplicates = 0i64;

        for chunk in events.chunks(MAX_EVENTS_PER_REQUEST) {
            let result = self.send_events(chunk).await?;
            total_inserted += result.inserted;
            total_duplicates += result.duplicates;
        }

        Ok(EventsIngestResponse {
            inserted: total_inserted,
            duplicates: total_duplicates,
        })
    }

    fn build_metadata(
        owner_id: &str,
        token_by_owner: &HashMap<String, String>,
        org_by_owner: &HashMap<String, Option<String>>,
    ) -> Option<HashMap<String, serde_json::Value>> {
        let token = token_by_owner.get(owner_id);
        let org_id = org_by_owner.get(owner_id).and_then(|o| o.as_ref());

        if token.is_none() && org_id.is_none() {
            return None;
        }

        let mut map = HashMap::with_capacity(2);
        if let Some(t) = token {
            map.insert("token".to_string(), serde_json::Value::String(t.clone()));
        }
        if let Some(org) = org_id {
            map.insert(
                "organization_id".to_string(),
                serde_json::Value::String(org.clone()),
            );
        }
        Some(map)
    }

    async fn send_events(
        &self,
        events: &[EventCreateExternalCustomer],
    ) -> Result<EventsIngestResponse, PolarError> {
        let body = EventsIngest {
            events: events.to_vec(),
        };

        let response = self
            .client
            .post(format!("{}/v1/events/ingest", POLAR_API_URL))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status != 200 {
            let message = response.text().await.unwrap_or_else(|e| e.to_string());
            return Err(PolarError::Api { status, message });
        }

        Ok(response.json().await?)
    }
}
