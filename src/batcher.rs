use chrono::{DateTime, Utc};
use clickhouse::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::interval;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, clickhouse::Row)]
pub struct EventRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub project_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub server_id: Uuid,
    pub data: String, // JSON type in ClickHouse expects a JSON string
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize, clickhouse::Row)]
pub struct ErrorRow {
    pub id: u32,
    pub name: String,
    pub message: String,
    pub stack: Vec<String>,
    pub cause_id: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, clickhouse::Row)]
pub struct ErrorTrackingRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub project_id: Uuid,
    pub hash: String,
    pub error_id: u32,
    pub count: u32,
    #[serde(with = "clickhouse::serde::uuid")]
    pub data_entry_id: Uuid,
}

#[derive(Debug, Clone, serde::Serialize, clickhouse::Row)]
pub struct WebVitalRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub project_id: Uuid,
    pub metric: String,
    pub value: f64,
    pub label: String,
    pub device: Option<String>,
    pub country: Option<String>,
    pub os: Option<String>,
    pub browser: Option<String>,
    pub url: String, // NOT nullable in ClickHouse
    pub attributes: String,
}

pub struct Batcher {
    client: Client,
    events: Arc<Mutex<Vec<EventRow>>>,
    errors: Arc<Mutex<Vec<ErrorRow>>>,
    error_tracking: Arc<Mutex<Vec<ErrorTrackingRow>>>,
    web_vitals: Arc<Mutex<Vec<WebVitalRow>>>,
    batch_interval: Duration,
}

impl Batcher {
    pub fn new(client: Client, batch_interval_secs: u64) -> Self {
        Self {
            client,
            events: Arc::new(Mutex::new(Vec::new())),
            errors: Arc::new(Mutex::new(Vec::new())),
            error_tracking: Arc::new(Mutex::new(Vec::new())),
            web_vitals: Arc::new(Mutex::new(Vec::new())),
            batch_interval: Duration::from_secs(batch_interval_secs),
        }
    }

    pub async fn add_event(&self, event: EventRow) {
        let mut events = self.events.lock().await;
        events.push(event);
    }

    pub async fn add_error(&self, error: ErrorRow) {
        let mut errors = self.errors.lock().await;
        errors.push(error);
    }

    pub async fn add_error_tracking(&self, error_tracking: ErrorTrackingRow) {
        let mut error_tracking_vec = self.error_tracking.lock().await;
        error_tracking_vec.push(error_tracking);
    }

    pub async fn add_web_vital(&self, web_vital: WebVitalRow) {
        let mut web_vitals = self.web_vitals.lock().await;
        web_vitals.push(web_vital);
    }

    pub async fn start(self: Arc<Self>) {
        let mut ticker = interval(self.batch_interval);
        loop {
            ticker.tick().await;
            if let Err(e) = self.flush_all().await {
                eprintln!("Error flushing batches: {:?}", e);
            }
        }
    }

    async fn flush_all(&self) -> Result<(), clickhouse::error::Error> {
        // Flush events
        let events = {
            let mut events = self.events.lock().await;
            std::mem::take(&mut *events)
        };

        if !events.is_empty() {
            eprintln!("Flushing {} events to ClickHouse", events.len());
            let mut insert = self.client.insert::<EventRow>("events").await?;
            for (i, event) in events.iter().enumerate() {
                eprintln!(
                    "Writing event {}: id={}, project_id={}, server_id={}, data={:?}",
                    i, event.id, event.project_id, event.server_id, event.data
                );
                insert.write(event).await?;
            }
            insert.end().await?;
            eprintln!("Successfully flushed events");
        }

        // Flush errors
        let errors = {
            let mut errors = self.errors.lock().await;
            std::mem::take(&mut *errors)
        };

        if !errors.is_empty() {
            eprintln!("Flushing {} errors to ClickHouse", errors.len());
            let mut insert = self.client.insert::<ErrorRow>("error").await?;
            for (i, error) in errors.iter().enumerate() {
                eprintln!(
                    "Writing error {}: id={}, name={}, message={}, stack_len={}, cause_id={:?}",
                    i,
                    error.id,
                    error.name,
                    error.message,
                    error.stack.len(),
                    error.cause_id
                );
                insert.write(error).await?;
            }
            insert.end().await?;
            eprintln!("Successfully flushed errors");
        }

        // Flush error tracking
        let error_tracking = {
            let mut error_tracking = self.error_tracking.lock().await;
            std::mem::take(&mut *error_tracking)
        };

        if !error_tracking.is_empty() {
            let mut insert = self
                .client
                .insert::<ErrorTrackingRow>("error_tracking")
                .await?;
            for et in error_tracking {
                insert.write(&et).await?;
            }
            insert.end().await?;
        }

        // Flush web vitals
        let web_vitals = {
            let mut web_vitals = self.web_vitals.lock().await;
            std::mem::take(&mut *web_vitals)
        };

        if !web_vitals.is_empty() {
            let mut insert = self.client.insert::<WebVitalRow>("web_vitals").await?;
            for wv in web_vitals {
                insert.write(&wv).await?;
            }
            insert.end().await?;
        }

        Ok(())
    }
}
