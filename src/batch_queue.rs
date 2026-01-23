use crate::polar::{PolarClient, UsageCounts};
use crate::tinybird::{
    ErrorRow, ErrorTrackingRow, EventRow, ReplayRow, TinybirdClient, WebVitalRow,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const BATCH_WINDOW: Duration = Duration::from_secs(5);
const BACKUP_REPLAY_INTERVAL: Duration = Duration::from_secs(600);
const MAX_REPLAY_BATCH_SIZE: i64 = 50;
const MAX_BACKUP_AGE_SECS: i64 = 86400;
const MAX_REQUEST_AGE_SECS: i64 = 86400;
const CHANNEL_CAPACITY: usize = 10_000;
const CHANNEL_BACKPRESSURE_THRESHOLD: usize = 8_000;
const MAX_BATCH_SIZE: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestType {
    Collect,
    Web,
    Vitals,
    Replay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedRequest {
    pub request_type: RequestType,
    pub token: String,
    pub body: Vec<u8>,
    pub country: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    pub origin: Option<String>,
}

/// Tracking context for billing purposes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingContext {
    pub owner_id: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QueuedEvent {
    Event {
        row: EventRow,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
    Error(ErrorRow),
    ErrorTracking {
        row: ErrorTrackingRow,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
    WebVital {
        row: WebVitalRow,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
    Replay {
        row: ReplayRow,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
}

impl QueuedEvent {
    fn datasource(&self) -> &'static str {
        match self {
            QueuedEvent::Event { .. } => "events",
            QueuedEvent::Error(_) => "error_",
            QueuedEvent::ErrorTracking { .. } => "error_tracking",
            QueuedEvent::WebVital { .. } => "web_vitals",
            QueuedEvent::Replay { .. } => "session_replays",
        }
    }
}

#[derive(Debug, Default)]
struct InMemoryBatch {
    events: Vec<(EventRow, Option<TrackingContext>)>,
    errors: Vec<ErrorRow>,
    error_trackings: Vec<(ErrorTrackingRow, Option<TrackingContext>)>,
    web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
    replays: Vec<(ReplayRow, Option<TrackingContext>)>,
}

impl InMemoryBatch {
    fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.errors.is_empty()
            && self.error_trackings.is_empty()
            && self.web_vitals.is_empty()
            && self.replays.is_empty()
    }

    fn total_count(&self) -> usize {
        self.events.len()
            + self.errors.len()
            + self.error_trackings.len()
            + self.web_vitals.len()
            + self.replays.len()
    }

    fn push(&mut self, event: QueuedEvent) {
        match event {
            QueuedEvent::Event { row, tracking } => self.events.push((row, tracking)),
            QueuedEvent::Error(e) => self.errors.push(e),
            QueuedEvent::ErrorTracking { row, tracking } => {
                self.error_trackings.push((row, tracking))
            }
            QueuedEvent::WebVital { row, tracking } => self.web_vitals.push((row, tracking)),
            QueuedEvent::Replay { row, tracking } => self.replays.push((row, tracking)),
        }
    }

    fn into_queued_events(self) -> Vec<QueuedEvent> {
        let mut result = Vec::with_capacity(self.total_count());
        result.extend(
            self.events
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::Event { row, tracking }),
        );
        result.extend(self.errors.into_iter().map(QueuedEvent::Error));
        result.extend(
            self.error_trackings
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::ErrorTracking { row, tracking }),
        );
        result.extend(
            self.web_vitals
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::WebVital { row, tracking }),
        );
        result.extend(
            self.replays
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::Replay { row, tracking }),
        );
        result
    }

    /// Aggregate usage counts by owner_id for billing
    fn aggregate_usage(&self) -> (HashMap<String, UsageCounts>, HashMap<String, String>) {
        let mut usage_by_owner: HashMap<String, UsageCounts> = HashMap::new();
        let mut token_by_owner: HashMap<String, String> = HashMap::new();

        for (_, ctx) in &self.events {
            if let Some(ctx) = ctx {
                let counts = usage_by_owner.entry(ctx.owner_id.clone()).or_default();
                counts.events += 1;
                token_by_owner
                    .entry(ctx.owner_id.clone())
                    .or_insert_with(|| ctx.token.clone());
            }
        }

        for (_, ctx) in &self.error_trackings {
            if let Some(ctx) = ctx {
                let counts = usage_by_owner.entry(ctx.owner_id.clone()).or_default();
                counts.error_tracking += 1;
                token_by_owner
                    .entry(ctx.owner_id.clone())
                    .or_insert_with(|| ctx.token.clone());
            }
        }

        for (_, ctx) in &self.web_vitals {
            if let Some(ctx) = ctx {
                let counts = usage_by_owner.entry(ctx.owner_id.clone()).or_default();
                counts.web_vitals += 1;
                token_by_owner
                    .entry(ctx.owner_id.clone())
                    .or_insert_with(|| ctx.token.clone());
            }
        }

        for (_, ctx) in &self.replays {
            if let Some(ctx) = ctx {
                let counts = usage_by_owner.entry(ctx.owner_id.clone()).or_default();
                counts.session_replays += 1;
                token_by_owner
                    .entry(ctx.owner_id.clone())
                    .or_insert_with(|| ctx.token.clone());
            }
        }

        (usage_by_owner, token_by_owner)
    }
}

#[derive(Debug, Default)]
struct BatchSendResult {
    failed_events: Vec<(EventRow, Option<TrackingContext>)>,
    failed_errors: Vec<ErrorRow>,
    failed_error_trackings: Vec<(ErrorTrackingRow, Option<TrackingContext>)>,
    failed_web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
    failed_replays: Vec<(ReplayRow, Option<TrackingContext>)>,
    had_permanent_failure: bool,
}

impl BatchSendResult {
    fn has_failures(&self) -> bool {
        !self.failed_events.is_empty()
            || !self.failed_errors.is_empty()
            || !self.failed_error_trackings.is_empty()
            || !self.failed_web_vitals.is_empty()
            || !self.failed_replays.is_empty()
    }

    fn into_in_memory_batch(self) -> InMemoryBatch {
        InMemoryBatch {
            events: self.failed_events,
            errors: self.failed_errors,
            error_trackings: self.failed_error_trackings,
            web_vitals: self.failed_web_vitals,
            replays: self.failed_replays,
        }
    }

    fn failure_count(&self) -> usize {
        self.failed_events.len()
            + self.failed_errors.len()
            + self.failed_error_trackings.len()
            + self.failed_web_vitals.len()
            + self.failed_replays.len()
    }
}

pub struct BackupStore {
    pool: SqlitePool,
}

impl BackupStore {
    pub async fn new(path: &Path) -> Result<Self, sqlx::Error> {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                sqlx::Error::Io(std::io::Error::other(format!(
                    "Failed to create backup directory: {}",
                    e
                )))
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS backed_up_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_data TEXT NOT NULL,
                datasource TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_error TEXT
            )
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_backed_up_events_created_at
            ON backed_up_events(created_at)
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS failed_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_data TEXT NOT NULL,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_failed_requests_created_at
            ON failed_requests(created_at)
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn backup_events(
        &self,
        events: &[QueuedEvent],
        error_msg: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        if events.is_empty() {
            return Ok(());
        }

        let now = Utc::now().to_rfc3339();

        let mut tx = self.pool.begin().await?;

        for event in events {
            let event_data = serde_json::to_string(event).expect("Failed to serialize event");
            let datasource = event.datasource();

            sqlx::query(
                "INSERT INTO backed_up_events (event_data, datasource, created_at, last_error) VALUES (?, ?, ?, ?)",
            )
            .bind(&event_data)
            .bind(datasource)
            .bind(&now)
            .bind(error_msg)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_backed_up_events(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, QueuedEvent)>, sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, event_data FROM backed_up_events ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let events: Vec<(i64, QueuedEvent)> = rows
            .into_iter()
            .filter_map(|(id, data)| serde_json::from_str(&data).ok().map(|event| (id, event)))
            .collect();

        Ok(events)
    }

    pub async fn remove_backed_up_events(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        if ids.is_empty() {
            return Ok(());
        }

        let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
        let query = format!(
            "DELETE FROM backed_up_events WHERE id IN ({})",
            placeholders.join(", ")
        );

        let mut q = sqlx::query(&query);
        for id in ids {
            q = q.bind(id);
        }

        q.execute(&self.pool).await?;
        Ok(())
    }

    pub async fn cleanup_stale_backups(&self) -> Result<u64, sqlx::Error> {
        let cutoff = Utc::now() - chrono::Duration::seconds(MAX_BACKUP_AGE_SECS);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query("DELETE FROM backed_up_events WHERE created_at < ?")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn count_backed_up(&self) -> Result<i64, sqlx::Error> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backed_up_events")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.0)
    }

    pub async fn backup_request(&self, request: &FailedRequest) -> Result<(), sqlx::Error> {
        let data = serde_json::to_string(request).expect("Failed to serialize request");
        let now = Utc::now().to_rfc3339();

        sqlx::query("INSERT INTO failed_requests (request_data, created_at) VALUES (?, ?)")
            .bind(&data)
            .bind(&now)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn get_failed_requests(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, FailedRequest)>, sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, request_data FROM failed_requests ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, data)| {
                serde_json::from_str(&data)
                    .ok()
                    .map(|request| (id, request))
            })
            .collect())
    }

    pub async fn remove_failed_request(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM failed_requests WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_stale_requests(&self) -> Result<u64, sqlx::Error> {
        let cutoff = Utc::now() - chrono::Duration::seconds(MAX_REQUEST_AGE_SECS);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query("DELETE FROM failed_requests WHERE created_at < ?")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn count_failed_requests(&self) -> Result<i64, sqlx::Error> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM failed_requests")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.0)
    }
}

pub struct BatchQueue {
    tinybird: Arc<TinybirdClient>,
    polar: Option<Arc<PolarClient>>,
    pub(crate) backup_store: Arc<BackupStore>,
    sender: mpsc::Sender<QueuedEvent>,
    in_memory_batch: Arc<Mutex<InMemoryBatch>>,
    flush_lock: Arc<Mutex<()>>,
}

impl BatchQueue {
    fn is_channel_under_pressure(&self) -> bool {
        self.sender.capacity() < (CHANNEL_CAPACITY - CHANNEL_BACKPRESSURE_THRESHOLD)
    }
}

impl BatchQueue {
    pub async fn new(
        tinybird: Arc<TinybirdClient>,
        polar: Option<Arc<PolarClient>>,
        backup_path: &Path,
    ) -> Result<Arc<Self>, sqlx::Error> {
        let backup_store = Arc::new(BackupStore::new(backup_path).await?);
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let in_memory_batch = Arc::new(Mutex::new(InMemoryBatch::default()));
        let flush_lock = Arc::new(Mutex::new(()));

        let queue = Arc::new(Self {
            tinybird,
            polar,
            backup_store,
            sender,
            in_memory_batch,
            flush_lock,
        });

        let startup_queue = Arc::clone(&queue);
        tokio::spawn(async move {
            startup_queue.replay_backed_up_events().await;
        });

        queue.start_batch_processor(receiver);
        queue.start_batch_flusher();
        queue.start_backup_replayer();

        Ok(queue)
    }

    pub async fn queue_event(
        &self,
        event: QueuedEvent,
    ) -> Result<(), mpsc::error::SendError<QueuedEvent>> {
        self.sender.send(event).await
    }

    fn start_batch_processor(self: &Arc<Self>, mut receiver: mpsc::Receiver<QueuedEvent>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let should_flush = {
                    let mut batch = queue.in_memory_batch.lock().await;
                    batch.push(event);
                    batch.total_count() >= MAX_BATCH_SIZE
                        || (queue.is_channel_under_pressure() && batch.total_count() >= 100)
                };

                if should_flush {
                    eprintln!("Batch size limit or backpressure detected, triggering early flush");
                    queue.flush_in_memory_batch().await;
                }
            }
        });
    }

    fn start_batch_flusher(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(BATCH_WINDOW).await;
                queue.flush_in_memory_batch().await;
            }
        });
    }

    fn start_backup_replayer(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(BACKUP_REPLAY_INTERVAL).await;
                queue.replay_backed_up_events().await;
            }
        });
    }

    pub(crate) async fn flush_in_memory_batch(&self) {
        let _flush_guard = self.flush_lock.lock().await;

        let batch = {
            let mut current = self.in_memory_batch.lock().await;
            if current.is_empty() {
                return;
            }
            std::mem::take(&mut *current)
        };

        let total = batch.total_count();
        eprintln!("Flushing in-memory batch of {} events", total);

        // Aggregate usage before sending (we need the data before batch is consumed)
        let (usage_by_owner, token_by_owner) = batch.aggregate_usage();

        self.send_batch_with_retry(batch).await;

        // Send usage to Polar for billing (fire and forget, don't block on billing)
        if let Some(polar) = &self.polar
            && !usage_by_owner.is_empty()
        {
            let polar = Arc::clone(polar);
            tokio::spawn(async move {
                match polar.ingest_usage(&usage_by_owner, &token_by_owner).await {
                    Ok(response) => {
                        eprintln!(
                            "Polar usage ingested: {} inserted, {} duplicates",
                            response.inserted, response.duplicates
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to ingest usage to Polar: {}", e);
                    }
                }
            });
        }
    }

    fn calculate_retry_delay(retry_count: u32) -> Duration {
        let base_delay = INITIAL_RETRY_DELAY.as_millis() as u64;
        let exponential_delay = base_delay * 2u64.saturating_pow(retry_count);
        let capped_delay = exponential_delay.min(MAX_RETRY_DELAY.as_millis() as u64);

        let jitter_range = capped_delay / 4;
        let jitter = if jitter_range > 0 {
            (retry_count as u64 * 7919) % (jitter_range * 2)
        } else {
            0
        };
        let delay_with_jitter = capped_delay
            .saturating_sub(jitter_range)
            .saturating_add(jitter);

        Duration::from_millis(delay_with_jitter)
    }

    async fn send_batch_with_retry(&self, batch: InMemoryBatch) {
        let mut retry_count = 0u32;
        let mut current_batch = batch;

        loop {
            let result = self.send_grouped_batch(current_batch).await;

            if !result.has_failures() {
                return;
            }

            if result.had_permanent_failure {
                eprintln!(
                    "Permanent failure, backing up {} events",
                    result.failure_count()
                );
                self.backup_events(result.into_in_memory_batch(), "Permanent API error")
                    .await;
                return;
            }

            retry_count += 1;

            if retry_count >= MAX_RETRIES {
                eprintln!(
                    "Batch failed after {} retries, backing up {} events",
                    retry_count,
                    result.failure_count()
                );
                self.backup_events(result.into_in_memory_batch(), "Max retries exceeded")
                    .await;
                return;
            }

            current_batch = result.into_in_memory_batch();

            let delay = Self::calculate_retry_delay(retry_count);
            eprintln!(
                "Batch send failed (attempt {}), retrying {} events in {:?}",
                retry_count,
                current_batch.total_count(),
                delay
            );

            tokio::time::sleep(delay).await;
        }
    }

    async fn send_grouped_batch(&self, batch: InMemoryBatch) -> BatchSendResult {
        let mut result = BatchSendResult::default();

        let InMemoryBatch {
            events,
            errors,
            error_trackings,
            web_vitals,
            replays,
        } = batch;

        // Extract just the rows for Tinybird (without tracking context)
        let event_rows: Vec<_> = events.iter().map(|(e, _)| e.clone()).collect();
        let error_tracking_rows: Vec<_> = error_trackings.iter().map(|(e, _)| e.clone()).collect();
        let web_vital_rows: Vec<_> = web_vitals.iter().map(|(e, _)| e.clone()).collect();
        let replay_rows: Vec<_> = replays.iter().map(|(e, _)| e.clone()).collect();

        let (events_res, errors_res, error_trackings_res, web_vitals_res, replays_res) = tokio::join!(
            async {
                if event_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_events(&event_rows).await
                }
            },
            async {
                if errors.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_errors(&errors).await
                }
            },
            async {
                if error_tracking_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird
                        .insert_error_trackings(&error_tracking_rows)
                        .await
                }
            },
            async {
                if web_vital_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_web_vitals(&web_vital_rows).await
                }
            },
            async {
                if replay_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_replays(&replay_rows).await
                }
            },
        );

        if let Err(e) = events_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_events = events;
        }

        if let Err(e) = errors_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_errors = errors;
        }

        if let Err(e) = error_trackings_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_error_trackings = error_trackings;
        }

        if let Err(e) = web_vitals_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_web_vitals = web_vitals;
        }

        if let Err(e) = replays_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_replays = replays;
        }

        result
    }

    async fn backup_events(&self, batch: InMemoryBatch, error_msg: &str) {
        let events = batch.into_queued_events();
        eprintln!("Backing up {} events: {}", events.len(), error_msg);
        if let Err(e) = self
            .backup_store
            .backup_events(&events, Some(error_msg))
            .await
        {
            eprintln!("CRITICAL: Failed to backup {} events: {}", events.len(), e);
        } else {
            eprintln!("Successfully backed up {} events", events.len());
        }
    }

    async fn replay_backed_up_events(&self) {
        match self.backup_store.cleanup_stale_backups().await {
            Ok(count) if count > 0 => {
                eprintln!("Cleaned up {} stale backups", count);
            }
            Err(e) => {
                eprintln!("Failed to cleanup stale backups: {}", e);
            }
            _ => {}
        }

        let backed_up_count = match self.backup_store.count_backed_up().await {
            Ok(count) => count,
            Err(e) => {
                eprintln!("Failed to count backed up events: {}", e);
                return;
            }
        };

        if backed_up_count == 0 {
            return;
        }

        eprintln!("Replaying {} backed up events", backed_up_count);

        let events = match self
            .backup_store
            .get_backed_up_events(MAX_REPLAY_BATCH_SIZE)
            .await
        {
            Ok(events) => events,
            Err(e) => {
                eprintln!("Failed to get backed up events: {}", e);
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        eprintln!("Restoring {} events from backup", events.len());

        let event_ids: Vec<i64> = events.iter().map(|(id, _)| *id).collect();

        if let Err(e) = self.backup_store.remove_backed_up_events(&event_ids).await {
            eprintln!("Failed to remove events before replay: {}", e);
            return;
        }

        let mut batch = InMemoryBatch::default();
        for (_id, event) in events {
            batch.push(event);
        }

        let result = self.send_grouped_batch(batch).await;

        if !result.has_failures() {
            eprintln!("Successfully restored {} events", event_ids.len());
        } else {
            self.backup_events(
                result.into_in_memory_batch(),
                "Replay failed, re-backing up",
            )
            .await;
        }
    }

    pub(crate) async fn replay_failed_requests(&self, pool: &sqlx::PgPool) {
        match self.backup_store.cleanup_stale_requests().await {
            Ok(count) if count > 0 => {
                eprintln!("Cleaned up {} stale failed requests", count);
            }
            Err(e) => {
                eprintln!("Failed to cleanup stale requests: {}", e);
            }
            _ => {}
        }

        let failed_count = match self.backup_store.count_failed_requests().await {
            Ok(count) => count,
            Err(e) => {
                eprintln!("Failed to count failed requests: {}", e);
                return;
            }
        };

        if failed_count == 0 {
            return;
        }

        eprintln!("Replaying {} failed requests", failed_count);

        let requests = match self.backup_store.get_failed_requests(100).await {
            Ok(requests) => requests,
            Err(e) => {
                eprintln!("Failed to get failed requests: {}", e);
                return;
            }
        };

        if requests.is_empty() {
            return;
        }

        for (id, request) in requests {
            let result = super::handler::process_failed_request(self, pool, &request).await;

            match result {
                Ok(()) => {
                    if let Err(e) = self.backup_store.remove_failed_request(id).await {
                        eprintln!("Failed to remove replayed request: {}", e);
                    } else {
                        eprintln!("Successfully replayed request {}", id);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to replay request {}: {}", id, e);
                    if (e.contains("Unauthorized") || e.contains("Invalid"))
                        && let Err(e) = self.backup_store.remove_failed_request(id).await
                    {
                        eprintln!("Failed to remove invalid request: {}", e);
                    }
                }
            }
        }
    }

    #[allow(dead_code)]
    pub async fn force_flush(&self) {
        self.flush_in_memory_batch().await;
    }

    #[allow(dead_code)]
    pub async fn backup_count(&self) -> Result<i64, sqlx::Error> {
        self.backup_store.count_backed_up().await
    }

    #[allow(dead_code)]
    pub async fn failed_request_count(&self) -> Result<i64, sqlx::Error> {
        self.backup_store.count_failed_requests().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn create_test_event() -> EventRow {
        EventRow {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            server_id: Uuid::new_v4(),
            data: r#"{"test": "data"}"#.to_string(),
            created_at: Utc::now(),
        }
    }

    fn create_test_queued_event() -> QueuedEvent {
        QueuedEvent::Event {
            row: create_test_event(),
            tracking: None,
        }
    }

    mod backup_store_tests {
        use super::*;

        #[tokio::test]
        async fn test_backup_and_restore_events() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            let event = create_test_queued_event();
            store.backup_events(&[event], None).await.unwrap();

            let events = store.get_backed_up_events(10).await.unwrap();
            assert_eq!(events.len(), 1);

            if let QueuedEvent::Event { row, .. } = &events[0].1 {
                assert!(row.data.contains("test"));
            } else {
                panic!("Expected Event variant");
            }
        }

        #[tokio::test]
        async fn test_remove_multiple_events() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            let events: Vec<QueuedEvent> = (0..5).map(|_| create_test_queued_event()).collect();
            store.backup_events(&events, None).await.unwrap();

            let events = store.get_backed_up_events(10).await.unwrap();
            assert_eq!(events.len(), 5);

            let ids_to_remove: Vec<i64> = events.iter().take(3).map(|(id, _)| *id).collect();
            store.remove_backed_up_events(&ids_to_remove).await.unwrap();

            let remaining = store.get_backed_up_events(10).await.unwrap();
            assert_eq!(remaining.len(), 2);
        }

        #[tokio::test]
        async fn test_count_backed_up() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            assert_eq!(store.count_backed_up().await.unwrap(), 0);

            let events: Vec<QueuedEvent> = (0..5).map(|_| create_test_queued_event()).collect();
            store.backup_events(&events, None).await.unwrap();

            assert_eq!(store.count_backed_up().await.unwrap(), 5);
        }

        #[tokio::test]
        async fn test_backup_different_event_types() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            let event = create_test_queued_event();
            store.backup_events(&[event], None).await.unwrap();

            let error = QueuedEvent::Error(ErrorRow {
                id: Uuid::new_v4(),
                name: "TestError".to_string(),
                message: "Test message".to_string(),
                stack: vec!["line1".to_string()],
                cause_id: None,
            });
            store.backup_events(&[error], None).await.unwrap();

            let vital = QueuedEvent::WebVital {
                row: WebVitalRow {
                    id: Uuid::new_v4(),
                    project_id: Uuid::new_v4(),
                    metric: "LCP".to_string(),
                    value: 2500.0,
                    device: Some("desktop".to_string()),
                    country: Some("US".to_string()),
                    os: Some("Windows".to_string()),
                    browser: Some("Chrome".to_string()),
                    url: "https://example.com".to_string(),
                    attributes: "{}".to_string(),
                    session_id: None,
                    created_at: Utc::now(),
                },
                tracking: None,
            };
            store.backup_events(&[vital], None).await.unwrap();

            assert_eq!(store.count_backed_up().await.unwrap(), 3);

            let events = store.get_backed_up_events(10).await.unwrap();
            assert_eq!(events.len(), 3);

            assert!(matches!(events[0].1, QueuedEvent::Event { .. }));
            assert!(matches!(events[1].1, QueuedEvent::Error(_)));
            assert!(matches!(events[2].1, QueuedEvent::WebVital { .. }));
        }

        #[tokio::test]
        async fn test_events_retrieved_in_order() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            for i in 0..5 {
                let mut row = create_test_event();
                row.data = format!(r#"{{"order": {}}}"#, i);
                store
                    .backup_events(
                        &[QueuedEvent::Event {
                            row,
                            tracking: None,
                        }],
                        None,
                    )
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let events = store.get_backed_up_events(10).await.unwrap();

            for (i, (_, event)) in events.into_iter().enumerate() {
                if let QueuedEvent::Event { row, .. } = event {
                    assert!(row.data.contains(&format!("\"order\": {}", i)));
                }
            }
        }

        #[tokio::test]
        async fn test_get_backed_up_events_limit() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            let events: Vec<QueuedEvent> = (0..10).map(|_| create_test_queued_event()).collect();
            store.backup_events(&events, None).await.unwrap();

            let events = store.get_backed_up_events(3).await.unwrap();
            assert_eq!(events.len(), 3);

            assert_eq!(store.count_backed_up().await.unwrap(), 10);
        }

        #[tokio::test]
        async fn test_backup_events_bulk() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path).await.unwrap();

            let events: Vec<QueuedEvent> = (0..10).map(|_| create_test_queued_event()).collect();

            store
                .backup_events(&events, Some("Test error"))
                .await
                .unwrap();

            assert_eq!(store.count_backed_up().await.unwrap(), 10);
        }
    }

    mod in_memory_batch_tests {
        use super::*;

        #[test]
        fn test_is_empty() {
            let batch = InMemoryBatch::default();
            assert!(batch.is_empty());

            let mut batch = InMemoryBatch::default();
            batch.push(create_test_queued_event());
            assert!(!batch.is_empty());
        }

        #[test]
        fn test_total_count() {
            let mut batch = InMemoryBatch::default();
            assert_eq!(batch.total_count(), 0);

            batch.push(create_test_queued_event());
            batch.push(create_test_queued_event());
            batch.push(QueuedEvent::Error(ErrorRow {
                id: Uuid::new_v4(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_id: None,
            }));

            assert_eq!(batch.total_count(), 3);
        }

        #[test]
        fn test_push_groups_correctly() {
            let mut batch = InMemoryBatch::default();

            batch.push(create_test_queued_event());
            batch.push(QueuedEvent::Error(ErrorRow {
                id: Uuid::new_v4(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_id: None,
            }));
            batch.push(QueuedEvent::WebVital {
                row: WebVitalRow {
                    id: Uuid::new_v4(),
                    project_id: Uuid::new_v4(),
                    metric: "LCP".to_string(),
                    value: 2500.0,
                    device: None,
                    country: None,
                    os: None,
                    browser: None,
                    url: "https://example.com".to_string(),
                    attributes: "{}".to_string(),
                    session_id: None,
                    created_at: Utc::now(),
                },
                tracking: None,
            });

            assert_eq!(batch.events.len(), 1);
            assert_eq!(batch.errors.len(), 1);
            assert_eq!(batch.web_vitals.len(), 1);
            assert!(batch.error_trackings.is_empty());
            assert!(batch.replays.is_empty());
        }

        #[test]
        fn test_into_queued_events() {
            let mut batch = InMemoryBatch::default();
            batch.push(create_test_queued_event());
            batch.push(QueuedEvent::Error(ErrorRow {
                id: Uuid::new_v4(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_id: None,
            }));

            let queued = batch.into_queued_events();
            assert_eq!(queued.len(), 2);
            assert!(matches!(queued[0], QueuedEvent::Event { .. }));
            assert!(matches!(queued[1], QueuedEvent::Error(_)));
        }
    }

    mod retry_delay_tests {
        use super::*;

        #[test]
        fn test_initial_delay_is_1_second() {
            let delay = BatchQueue::calculate_retry_delay(0);
            assert!(delay >= Duration::from_millis(750));
            assert!(delay <= Duration::from_millis(1250));
        }

        #[test]
        fn test_exponential_growth() {
            let delay_0 = BatchQueue::calculate_retry_delay(0);
            let delay_1 = BatchQueue::calculate_retry_delay(1);
            let delay_2 = BatchQueue::calculate_retry_delay(2);

            assert!(delay_1 > delay_0);
            assert!(delay_2 > delay_1);
        }

        #[test]
        fn test_max_delay_cap() {
            let delay = BatchQueue::calculate_retry_delay(10);
            assert!(delay <= MAX_RETRY_DELAY + Duration::from_millis(100));
        }
    }

    mod queued_event_tests {
        use super::*;

        #[test]
        fn test_datasource_names() {
            assert_eq!(create_test_queued_event().datasource(), "events");
            assert_eq!(
                QueuedEvent::Error(ErrorRow {
                    id: Uuid::new_v4(),
                    name: "E".to_string(),
                    message: "M".to_string(),
                    stack: vec![],
                    cause_id: None,
                })
                .datasource(),
                "error_"
            );
            let error_id = Uuid::new_v4();
            assert_eq!(
                QueuedEvent::ErrorTracking {
                    row: ErrorTrackingRow {
                        id: Uuid::new_v4(),
                        project_id: Uuid::new_v4(),
                        hash: "hash".to_string(),
                        error_id,
                        count: 1,
                        data_entry_id: Uuid::new_v4(),
                        session_id: None,
                        created_at: Utc::now(),
                    },
                    tracking: None,
                }
                .datasource(),
                "error_tracking"
            );
            assert_eq!(
                QueuedEvent::WebVital {
                    row: WebVitalRow {
                        id: Uuid::new_v4(),
                        project_id: Uuid::new_v4(),
                        metric: "LCP".to_string(),
                        value: 2500.0,
                        device: None,
                        country: None,
                        os: None,
                        browser: None,
                        url: "https://example.com".to_string(),
                        attributes: "{}".to_string(),
                        session_id: None,
                        created_at: Utc::now(),
                    },
                    tracking: None,
                }
                .datasource(),
                "web_vitals"
            );
            assert_eq!(
                QueuedEvent::Replay {
                    row: ReplayRow {
                        id: Uuid::new_v4(),
                        project_id: Uuid::new_v4(),
                        session_id: "session".to_string(),
                        events: "[]".to_string(),
                        created_at: Utc::now(),
                    },
                    tracking: None,
                }
                .datasource(),
                "session_replays"
            );
        }

        #[test]
        fn test_serialization_roundtrip() {
            let event = create_test_queued_event();
            let json = serde_json::to_string(&event).unwrap();
            let deserialized: QueuedEvent = serde_json::from_str(&json).unwrap();

            if let (QueuedEvent::Event { row: orig, .. }, QueuedEvent::Event { row: deser, .. }) =
                (&event, &deserialized)
            {
                assert_eq!(orig.id, deser.id);
                assert_eq!(orig.project_id, deser.project_id);
                assert_eq!(orig.data, deser.data);
            } else {
                panic!("Deserialization changed event type");
            }
        }
    }

    mod constants_tests {
        use super::*;

        #[test]
        fn test_max_retries_is_5() {
            assert_eq!(MAX_RETRIES, 5);
        }

        #[test]
        fn test_initial_retry_delay_is_1_second() {
            assert_eq!(INITIAL_RETRY_DELAY, Duration::from_secs(1));
        }

        #[test]
        fn test_max_retry_delay_is_30_seconds() {
            assert_eq!(MAX_RETRY_DELAY, Duration::from_secs(30));
        }

        #[test]
        fn test_batch_window_is_5_seconds() {
            assert_eq!(BATCH_WINDOW, Duration::from_secs(5));
        }

        #[test]
        fn test_backup_replay_interval_is_600_seconds() {
            assert_eq!(BACKUP_REPLAY_INTERVAL, Duration::from_secs(600));
        }

        #[test]
        fn test_max_backup_age_is_24_hours() {
            assert_eq!(MAX_BACKUP_AGE_SECS, 86400);
        }
    }
}
