mod backup_store;
pub use backup_store::BackupStore;

use crate::polar::{PolarClient, UsageCounts};
use crate::tinybird::{
    ErrorRow, ErrorTrackingRow, ModsEventRow, ReplayRow, TinybirdClient, WebEventRow, WebVitalRow,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const BATCH_WINDOW: Duration = Duration::from_secs(5);
const BACKUP_REPLAY_INTERVAL: Duration = Duration::from_secs(600);
const MAX_REPLAY_BATCH_SIZE: i64 = 50;
const MAX_BACKUP_AGE_SECS: i64 = 86400;
const MAX_REQUEST_AGE_SECS: i64 = 86400;
const CHANNEL_CAPACITY: usize = 2_000;
const CHANNEL_BACKPRESSURE_THRESHOLD: usize = 1_600;
const MAX_BATCH_SIZE: usize = 5000;

pub struct OwnerUsage {
    pub counts: UsageCounts,
    pub token: Arc<str>,
    pub org: Option<Arc<str>>,
}

pub type AggregatedUsage = HashMap<Arc<str>, OwnerUsage>;

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
    pub owner_id: Arc<str>,
    pub token: Arc<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<Arc<str>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QueuedEvent {
    WebEvent {
        row: Box<WebEventRow>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
    ModsEvent {
        row: ModsEventRow,
        #[serde(skip_serializing_if = "Option::is_none")]
        tracking: Option<TrackingContext>,
    },
    Error(ErrorRow),
    ErrorTracking {
        row: Box<ErrorTrackingRow>,
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
            QueuedEvent::WebEvent { .. } => "web_events",
            QueuedEvent::ModsEvent { .. } => "mods_events",
            QueuedEvent::Error(_) => "errors",
            QueuedEvent::ErrorTracking { .. } => "error_occurences_v2",
            QueuedEvent::WebVital { .. } => "web_vitals",
            QueuedEvent::Replay { .. } => "session_replays",
        }
    }
}

const INITIAL_BATCH_CAPACITY: usize = 64;

#[derive(Debug)]
struct InMemoryBatch {
    web_events: Vec<(WebEventRow, Option<TrackingContext>)>,
    mods_events: Vec<(ModsEventRow, Option<TrackingContext>)>,
    errors: Vec<ErrorRow>,
    error_trackings: Vec<(ErrorTrackingRow, Option<TrackingContext>)>,
    web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
    replays: Vec<(ReplayRow, Option<TrackingContext>)>,
}

impl Default for InMemoryBatch {
    fn default() -> Self {
        Self {
            web_events: Vec::with_capacity(INITIAL_BATCH_CAPACITY),
            mods_events: Vec::with_capacity(INITIAL_BATCH_CAPACITY),
            errors: Vec::new(),
            error_trackings: Vec::new(),
            web_vitals: Vec::with_capacity(INITIAL_BATCH_CAPACITY / 4),
            replays: Vec::new(),
        }
    }
}

impl InMemoryBatch {
    fn is_empty(&self) -> bool {
        self.web_events.is_empty()
            && self.mods_events.is_empty()
            && self.errors.is_empty()
            && self.error_trackings.is_empty()
            && self.web_vitals.is_empty()
            && self.replays.is_empty()
    }

    fn total_count(&self) -> usize {
        self.web_events.len()
            + self.mods_events.len()
            + self.errors.len()
            + self.error_trackings.len()
            + self.web_vitals.len()
            + self.replays.len()
    }

    fn push(&mut self, event: QueuedEvent) {
        match event {
            QueuedEvent::WebEvent { row, tracking } => self.web_events.push((*row, tracking)),
            QueuedEvent::ModsEvent { row, tracking } => self.mods_events.push((row, tracking)),
            QueuedEvent::Error(e) => self.errors.push(e),
            QueuedEvent::ErrorTracking { row, tracking } => {
                self.error_trackings.push((*row, tracking))
            }
            QueuedEvent::WebVital { row, tracking } => self.web_vitals.push((row, tracking)),
            QueuedEvent::Replay { row, tracking } => self.replays.push((row, tracking)),
        }
    }

    fn into_queued_events(self) -> Vec<QueuedEvent> {
        let mut result = Vec::with_capacity(self.total_count());
        result.extend(
            self.web_events
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::WebEvent {
                    row: Box::new(row),
                    tracking,
                }),
        );
        result.extend(
            self.mods_events
                .into_iter()
                .map(|(row, tracking)| QueuedEvent::ModsEvent { row, tracking }),
        );
        result.extend(self.errors.into_iter().map(QueuedEvent::Error));
        result.extend(self.error_trackings.into_iter().map(|(row, tracking)| {
            QueuedEvent::ErrorTracking {
                row: Box::new(row),
                tracking,
            }
        }));
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

    fn aggregate_usage(&self) -> AggregatedUsage {
        let estimated_owners = (self.web_events.len()
            + self.mods_events.len()
            + self.error_trackings.len()
            + self.web_vitals.len()
            + self.replays.len())
        .min(100);

        let mut usage: AggregatedUsage = HashMap::with_capacity(estimated_owners);

        macro_rules! count_usage {
            ($iter:expr, $field:ident) => {
                for (_, ctx) in $iter {
                    if let Some(ctx) = ctx {
                        usage
                            .entry(Arc::clone(&ctx.owner_id))
                            .or_insert_with(|| OwnerUsage {
                                counts: UsageCounts::default(),
                                token: Arc::clone(&ctx.token),
                                org: ctx.organization_id.as_ref().map(Arc::clone),
                            })
                            .counts
                            .$field += 1;
                    }
                }
            };
        }

        count_usage!(&self.web_events, events);
        count_usage!(&self.mods_events, events);
        count_usage!(&self.error_trackings, error_tracking);
        count_usage!(&self.web_vitals, web_vitals);
        for (row, ctx) in &self.replays {
            if let Some(ctx) = ctx {
                let entry = usage
                    .entry(Arc::clone(&ctx.owner_id))
                    .or_insert_with(|| OwnerUsage {
                        counts: UsageCounts::default(),
                        token: Arc::clone(&ctx.token),
                        org: ctx.organization_id.as_ref().map(Arc::clone),
                    });
                entry.counts.session_replays += 1;
                entry
                    .counts
                    .session_replay_ids
                    .insert(row.session_id.clone());
            }
        }

        usage
    }
}

#[derive(Debug, Default)]
struct BatchSendResult {
    failed_web_events: Vec<(WebEventRow, Option<TrackingContext>)>,
    failed_mods_events: Vec<(ModsEventRow, Option<TrackingContext>)>,
    failed_errors: Vec<ErrorRow>,
    failed_error_trackings: Vec<(ErrorTrackingRow, Option<TrackingContext>)>,
    failed_web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
    failed_replays: Vec<(ReplayRow, Option<TrackingContext>)>,
    had_permanent_failure: bool,
}

impl BatchSendResult {
    fn has_failures(&self) -> bool {
        !self.failed_web_events.is_empty()
            || !self.failed_mods_events.is_empty()
            || !self.failed_errors.is_empty()
            || !self.failed_error_trackings.is_empty()
            || !self.failed_web_vitals.is_empty()
            || !self.failed_replays.is_empty()
    }

    fn into_in_memory_batch(self) -> InMemoryBatch {
        InMemoryBatch {
            web_events: self.failed_web_events,
            mods_events: self.failed_mods_events,
            errors: self.failed_errors,
            error_trackings: self.failed_error_trackings,
            web_vitals: self.failed_web_vitals,
            replays: self.failed_replays,
        }
    }

    fn failure_count(&self) -> usize {
        self.failed_web_events.len()
            + self.failed_mods_events.len()
            + self.failed_errors.len()
            + self.failed_error_trackings.len()
            + self.failed_web_vitals.len()
            + self.failed_replays.len()
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
    pub fn new(
        tinybird: Arc<TinybirdClient>,
        polar: Option<Arc<PolarClient>>,
        backup_path: &Path,
    ) -> Arc<Self> {
        let backup_store = Arc::new(BackupStore::new(backup_path));
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

        queue
    }

    pub async fn queue_event(
        &self,
        event: QueuedEvent,
    ) -> Result<(), mpsc::error::SendError<QueuedEvent>> {
        self.sender.send(event).await
    }

    pub fn channel_capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub async fn current_batch_size(&self) -> usize {
        self.in_memory_batch.lock().await.total_count()
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
                    warn!("Batch size limit or backpressure detected, triggering early flush");
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
        info!("Flushing in-memory batch of {} events", total);

        let usage = batch.aggregate_usage();

        self.send_batch_with_retry(batch).await;

        if let Some(polar) = &self.polar
            && !usage.is_empty()
        {
            let polar = Arc::clone(polar);
            tokio::spawn(async move {
                match polar.ingest_usage(&usage).await {
                    Ok(response) => {
                        info!(
                            "Polar usage ingested: {} inserted, {} duplicates",
                            response.inserted, response.duplicates
                        );
                    }
                    Err(e) => {
                        error!("Failed to ingest usage to Polar: {}", e);
                    }
                }
            });
        }
    }

    fn calculate_retry_delay(retry_count: u32) -> Duration {
        let base = INITIAL_RETRY_DELAY.as_millis() as u64;
        let capped =
            (base * 2u64.saturating_pow(retry_count)).min(MAX_RETRY_DELAY.as_millis() as u64);
        let jitter = (capped / 4).saturating_sub((retry_count as u64 * 7919) % (capped / 2).max(1));
        Duration::from_millis(capped.saturating_sub(jitter))
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
                error!(
                    "Permanent failure, backing up {} events",
                    result.failure_count()
                );
                self.backup_events(result.into_in_memory_batch(), "Permanent API error")
                    .await;
                return;
            }

            retry_count += 1;

            if retry_count >= MAX_RETRIES {
                error!(
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
            warn!(
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
            web_events,
            mods_events,
            errors,
            error_trackings,
            web_vitals,
            replays,
        } = batch;

        let web_event_rows: Vec<_> = web_events.iter().map(|(e, _)| e).collect();
        let mods_event_rows: Vec<_> = mods_events.iter().map(|(e, _)| e).collect();
        let error_tracking_rows: Vec<_> = error_trackings.iter().map(|(e, _)| e).collect();
        let web_vital_rows: Vec<_> = web_vitals.iter().map(|(e, _)| e).collect();
        let replay_rows: Vec<_> = replays.iter().map(|(e, _)| e).collect();

        let (
            web_events_res,
            mods_events_res,
            errors_res,
            error_trackings_res,
            web_vitals_res,
            replays_res,
        ) = tokio::join!(
            async {
                if web_event_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_web_events(&web_event_rows).await
                }
            },
            async {
                if mods_event_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird.insert_mods_events(&mods_event_rows).await
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

        if let Err(e) = web_events_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_web_events = web_events;
        }

        if let Err(e) = mods_events_res {
            if !e.is_transient() {
                result.had_permanent_failure = true;
            }
            result.failed_mods_events = mods_events;
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
        warn!("Backing up {} events: {}", events.len(), error_msg);
        if let Err(e) = self
            .backup_store
            .backup_events(&events, Some(error_msg))
            .await
        {
            error!("CRITICAL: Failed to backup {} events: {}", events.len(), e);
        } else {
            info!("Successfully backed up {} events", events.len());
        }
    }

    async fn replay_backed_up_events(&self) {
        match self.backup_store.cleanup_stale_backups().await {
            Ok(count) if count > 0 => {
                info!("Cleaned up {} stale backups", count);
            }
            Err(e) => {
                error!("Failed to cleanup stale backups: {}", e);
            }
            _ => {}
        }

        let backed_up_count = match self.backup_store.count_backed_up().await {
            Ok(count) => count,
            Err(e) => {
                error!("Failed to count backed up events: {}", e);
                return;
            }
        };

        if backed_up_count == 0 {
            return;
        }

        info!("Replaying {} backed up events", backed_up_count);

        let events = match self
            .backup_store
            .get_backed_up_events(MAX_REPLAY_BATCH_SIZE)
            .await
        {
            Ok(events) => events,
            Err(e) => {
                error!("Failed to get backed up events: {}", e);
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        info!("Restoring {} events from backup", events.len());

        let event_ids: Vec<i64> = events.iter().map(|(id, _)| *id).collect();

        let mut batch = InMemoryBatch::default();
        for (_id, event) in events {
            batch.push(event);
        }

        let result = self.send_grouped_batch(batch).await;

        if !result.has_failures() {
            if let Err(e) = self.backup_store.remove_backed_up_events(&event_ids).await {
                error!("Failed to remove events after successful replay: {}", e);
            } else {
                info!("Successfully restored {} events", event_ids.len());
            }
        } else {
            let failed_count = result.failure_count();
            let succeeded_count = event_ids.len() - failed_count;

            if succeeded_count > 0 {
                // Remove only the IDs for events that succeeded (first N events)
                let succeeded_ids = &event_ids[..succeeded_count];
                if let Err(e) = self
                    .backup_store
                    .remove_backed_up_events(succeeded_ids)
                    .await
                {
                    error!(
                        "Failed to remove {} succeeded events: {}",
                        succeeded_count, e
                    );
                }
            }

            warn!(
                "Replay partially failed: {} succeeded, {} failed (kept in backup)",
                succeeded_count, failed_count
            );
        }
    }

    pub(crate) async fn replay_failed_requests(&self, pool: &sqlx::PgPool) {
        match self.backup_store.cleanup_stale_requests().await {
            Ok(count) if count > 0 => {
                info!("Cleaned up {} stale failed requests", count);
            }
            Err(e) => {
                error!("Failed to cleanup stale requests: {}", e);
            }
            _ => {}
        }

        let failed_count = match self.backup_store.count_failed_requests().await {
            Ok(count) => count,
            Err(e) => {
                error!("Failed to count failed requests: {}", e);
                return;
            }
        };

        if failed_count == 0 {
            return;
        }

        info!("Replaying {} failed requests", failed_count);

        let requests = match self.backup_store.get_failed_requests(100).await {
            Ok(requests) => requests,
            Err(e) => {
                error!("Failed to get failed requests: {}", e);
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
                        error!("Failed to remove replayed request: {}", e);
                    } else {
                        info!("Successfully replayed request {}", id);
                    }
                }
                Err(e) => {
                    warn!("Failed to replay request {}: {}", id, e);
                    if (e.contains("Unauthorized") || e.contains("Invalid"))
                        && let Err(e) = self.backup_store.remove_failed_request(id).await
                    {
                        error!("Failed to remove invalid request: {}", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn create_test_mods_event() -> ModsEventRow {
        ModsEventRow {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            server_id: Uuid::new_v4(),
            player_count: None,
            online_mode: None,
            plugin_version: None,
            minecraft_version: None,
            server_type: None,
            java_version: None,
            java_vendor: None,
            os_name: None,
            os_arch: None,
            os_version: None,
            core_count: None,
            country: None,
            custom: r#"{"test": "data"}"#.to_string(),
            created_at: Utc::now(),
        }
    }

    fn create_test_queued_event() -> QueuedEvent {
        QueuedEvent::ModsEvent {
            row: create_test_mods_event(),
            tracking: None,
        }
    }

    mod backup_store_tests {
        use super::*;

        #[tokio::test]
        async fn test_backup_and_restore_events() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path);

            let event = create_test_queued_event();
            store.backup_events(&[event], None).await.unwrap();

            let events = store.get_backed_up_events(10).await.unwrap();
            assert_eq!(events.len(), 1);

            if let QueuedEvent::ModsEvent { row, .. } = &events[0].1 {
                assert!(row.custom.contains("test"));
            } else {
                panic!("Expected Event variant");
            }
        }

        #[tokio::test]
        async fn test_remove_multiple_events() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path);

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
            let store = BackupStore::new(&db_path);

            assert_eq!(store.count_backed_up().await.unwrap(), 0);

            let events: Vec<QueuedEvent> = (0..5).map(|_| create_test_queued_event()).collect();
            store.backup_events(&events, None).await.unwrap();

            assert_eq!(store.count_backed_up().await.unwrap(), 5);
        }

        #[tokio::test]
        async fn test_backup_different_event_types() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path);

            let event = create_test_queued_event();
            store.backup_events(&[event], None).await.unwrap();

            let error = QueuedEvent::Error(ErrorRow {
                hash: "error-hash".to_string(),
                name: "TestError".to_string(),
                message: "Test message".to_string(),
                stack: vec!["line1".to_string()],
                cause_hash: None,
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
                    os_version: Some("10".to_string()),
                    browser: Some("Chrome".to_string()),
                    browser_version: Some("120.0".to_string()),
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

            assert!(matches!(events[0].1, QueuedEvent::ModsEvent { .. }));
            assert!(matches!(events[1].1, QueuedEvent::Error(_)));
            assert!(matches!(events[2].1, QueuedEvent::WebVital { .. }));
        }

        #[tokio::test]
        async fn test_events_retrieved_in_order() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path);

            for i in 0..5 {
                let mut row = create_test_mods_event();
                row.custom = format!(r#"{{"order": {}}}"#, i);
                store
                    .backup_events(
                        &[QueuedEvent::ModsEvent {
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
                if let QueuedEvent::ModsEvent { row, .. } = event {
                    assert!(row.custom.contains(&format!("\"order\": {}", i)));
                }
            }
        }

        #[tokio::test]
        async fn test_get_backed_up_events_limit() {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("test.db");
            let store = BackupStore::new(&db_path);

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
            let store = BackupStore::new(&db_path);

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
                hash: "error-hash".to_string(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_hash: None,
            }));

            assert_eq!(batch.total_count(), 3);
        }

        #[test]
        fn test_push_groups_correctly() {
            let mut batch = InMemoryBatch::default();

            batch.push(create_test_queued_event());
            batch.push(QueuedEvent::Error(ErrorRow {
                hash: "error-hash".to_string(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_hash: None,
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
                    os_version: None,
                    browser: None,
                    browser_version: None,
                    url: "https://example.com".to_string(),
                    attributes: "{}".to_string(),
                    session_id: None,
                    created_at: Utc::now(),
                },
                tracking: None,
            });

            assert_eq!(batch.mods_events.len(), 1);
            assert_eq!(batch.errors.len(), 1);
            assert_eq!(batch.web_vitals.len(), 1);
            assert!(batch.web_events.is_empty());
            assert!(batch.error_trackings.is_empty());
            assert!(batch.replays.is_empty());
        }

        #[test]
        fn test_into_queued_events() {
            let mut batch = InMemoryBatch::default();
            batch.push(create_test_queued_event());
            batch.push(QueuedEvent::Error(ErrorRow {
                hash: "error-hash".to_string(),
                name: "E".to_string(),
                message: "M".to_string(),
                stack: vec![],
                cause_hash: None,
            }));

            let queued = batch.into_queued_events();
            assert_eq!(queued.len(), 2);
            assert!(matches!(queued[0], QueuedEvent::ModsEvent { .. }));
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
            assert_eq!(create_test_queued_event().datasource(), "mods_events");
            assert_eq!(
                QueuedEvent::Error(ErrorRow {
                    hash: "error-hash".to_string(),
                    name: "E".to_string(),
                    message: "M".to_string(),
                    stack: vec![],
                    cause_hash: None,
                })
                .datasource(),
                "errors"
            );
            assert_eq!(
                QueuedEvent::ErrorTracking {
                    row: Box::new(ErrorTrackingRow {
                        id: Uuid::new_v4(),
                        project_id: Uuid::new_v4(),
                        hash: "hash".to_string(),
                        error_hash: "error-hash".to_string(),
                        count: 3,
                        data_entry_id: Some(Uuid::new_v4()),
                        session_id: None,
                        identity_key: None,
                        build_id: None,
                        plugin_version: String::new(),
                        source_kind: "error".to_string(),
                        entry_session_id: String::new(),
                        entry_country: String::new(),
                        entry_browser: String::new(),
                        entry_device: String::new(),
                        entry_os: String::new(),
                        player_count: None,
                        online_mode: None,
                        minecraft_version: String::new(),
                        server_type: String::new(),
                        java_version: String::new(),
                        java_vendor: String::new(),
                        os_version: String::new(),
                        os_arch: String::new(),
                        core_count: None,
                        entry_data: "{}".to_string(),
                        stack_placeholders: "{}".to_string(),
                        context: None,
                        handled: None,
                        created_at: Utc::now(),
                    }),
                    tracking: None,
                }
                .datasource(),
                "error_occurences_v2"
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
                        os_version: None,
                        browser: None,
                        browser_version: None,
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
                        identifier: Some(Uuid::new_v4().to_string()),
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

            if let (
                QueuedEvent::ModsEvent { row: orig, .. },
                QueuedEvent::ModsEvent { row: deser, .. },
            ) = (&event, &deserialized)
            {
                assert_eq!(orig.id, deser.id);
                assert_eq!(orig.project_id, deser.project_id);
                assert_eq!(orig.custom, deser.custom);
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
