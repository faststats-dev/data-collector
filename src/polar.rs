use crate::batch_queue::AggregatedUsage;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

#[cfg(debug_assertions)]
const POLAR_API_URL: &str = "https://sandbox-api.polar.sh";
#[cfg(not(debug_assertions))]
const POLAR_API_URL: &str = "https://api.polar.sh";

const MAX_EVENTS_PER_REQUEST: usize = 500;

#[derive(Clone)]
pub struct PolarClient {
    client: Client,
    bearer_token: String,
}

#[derive(Debug, Clone, Serialize)]
struct EventMetadata {
    count: u64,
    token: Arc<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization_id: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EventCreateExternalCustomer {
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    name: &'static str,
    external_customer_id: Arc<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<EventMetadata>,
}

#[derive(Debug, Serialize)]
struct EventsIngest<'a> {
    events: &'a [EventCreateExternalCustomer],
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
    pub session_replay_ids: HashSet<String>,
}

impl PolarClient {
    pub fn new(token: String) -> Self {
        let mut bearer_token = String::with_capacity("Bearer ".len() + token.len());
        bearer_token.push_str("Bearer ");
        bearer_token.push_str(&token);

        Self {
            client: Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(15))
                .pool_max_idle_per_host(1)
                .build()
                .unwrap_or_else(|_| Client::new()),
            bearer_token,
        }
    }

    pub async fn ingest_usage(
        &self,
        usage: &AggregatedUsage,
    ) -> Result<EventsIngestResponse, PolarError> {
        let mut events = Vec::with_capacity(usage.len() * 4);
        let base_timestamp = Utc::now();

        for (owner_id, owner_usage) in usage {
            let counts = [
                ("events", owner_usage.counts.events),
                ("error_tracking", owner_usage.counts.error_tracking),
                ("web_vitals", owner_usage.counts.web_vitals),
            ];

            for (i, (name, count)) in counts.into_iter().enumerate() {
                if count == 0 {
                    continue;
                }

                events.push(EventCreateExternalCustomer {
                    timestamp: Some(base_timestamp + chrono::Duration::microseconds(i as i64)),
                    name,
                    external_customer_id: Arc::clone(owner_id),
                    metadata: Some(EventMetadata {
                        count,
                        token: Arc::clone(&owner_usage.token),
                        organization_id: owner_usage.org.as_ref().map(Arc::clone),
                        session_id: None,
                    }),
                });
            }

            for (i, session_id) in owner_usage.counts.session_replay_ids.iter().enumerate() {
                events.push(EventCreateExternalCustomer {
                    timestamp: Some(
                        base_timestamp + chrono::Duration::microseconds((3 + i) as i64),
                    ),
                    name: "session_replays",
                    external_customer_id: Arc::clone(owner_id),
                    metadata: Some(EventMetadata {
                        count: 1,
                        token: Arc::clone(&owner_usage.token),
                        organization_id: owner_usage.org.as_ref().map(Arc::clone),
                        session_id: Some(session_id.clone()),
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

        let mut total_inserted = 0;
        let mut total_duplicates = 0;

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

    async fn send_events(
        &self,
        events: &[EventCreateExternalCustomer],
    ) -> Result<EventsIngestResponse, PolarError> {
        let body = EventsIngest { events };

        let response = self
            .client
            .post(format!("{}/v1/events/ingest", POLAR_API_URL))
            .header("Authorization", &self.bearer_token)
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
