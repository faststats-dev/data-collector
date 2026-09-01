use crate::error_tracking::mapping::MappingResolver;
use crate::error_tracking::{ErrorLanguage, ProjectGrouping};
use crate::polar::{PolarClient, UsageCounts};
use crate::tinybird::{
    ErrorOccurrenceV3Row, ModsEventRow, TinybirdClient, WebEventRow, WebVitalRow,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const BATCH_WINDOW: Duration = Duration::from_secs(5);
const CHANNEL_CAPACITY: usize = 2_000;
const CHANNEL_BACKPRESSURE_THRESHOLD: usize = 1_600;
const MAX_BATCH_SIZE: usize = 5000;

pub struct OwnerUsage {
    pub counts: UsageCounts,
    pub token: Arc<str>,
    pub org: Option<Arc<str>>,
}

pub type AggregatedUsage = HashMap<Arc<str>, OwnerUsage>;

/// Tracking context for billing purposes
#[derive(Debug, Clone)]
pub struct TrackingContext {
    pub owner_id: Arc<str>,
    pub token: Arc<str>,
    pub organization_id: Option<Arc<str>>,
}

#[derive(Debug)]
pub enum QueuedEvent {
    WebEvent {
        row: Box<WebEventRow>,
        tracking: Option<TrackingContext>,
    },
    ModsEvent {
        row: ModsEventRow,
        tracking: Option<TrackingContext>,
    },
    ErrorOccurrenceV3 {
        row: Box<ErrorOccurrenceV3Row>,
        language: ErrorLanguage,
        grouping: ProjectGrouping,
        tracking: Option<TrackingContext>,
    },
    WebVital {
        row: WebVitalRow,
        tracking: Option<TrackingContext>,
    },
}

pub enum QueueError {
    Full,
    Closed,
}

#[derive(Debug, Default)]
struct InMemoryBatch {
    web_events: Vec<(WebEventRow, Option<TrackingContext>)>,
    mods_events: Vec<(ModsEventRow, Option<TrackingContext>)>,
    error_occurrences_v3: Vec<(
        ErrorOccurrenceV3Row,
        ErrorLanguage,
        ProjectGrouping,
        Option<TrackingContext>,
    )>,
    web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
}

impl InMemoryBatch {
    fn is_empty(&self) -> bool {
        self.web_events.is_empty()
            && self.mods_events.is_empty()
            && self.error_occurrences_v3.is_empty()
            && self.web_vitals.is_empty()
    }

    fn total_count(&self) -> usize {
        self.web_events.len()
            + self.mods_events.len()
            + self.error_occurrences_v3.len()
            + self.web_vitals.len()
    }

    fn push(&mut self, event: QueuedEvent) {
        match event {
            QueuedEvent::WebEvent { row, tracking } => self.web_events.push((*row, tracking)),
            QueuedEvent::ModsEvent { row, tracking } => self.mods_events.push((row, tracking)),
            QueuedEvent::ErrorOccurrenceV3 {
                row,
                language,
                grouping,
                tracking,
            } => self
                .error_occurrences_v3
                .push((*row, language, grouping, tracking)),
            QueuedEvent::WebVital { row, tracking } => self.web_vitals.push((row, tracking)),
        }
    }

    fn aggregate_usage(&self) -> AggregatedUsage {
        let estimated_owners = self.total_count().min(100);

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
        for (_, _, _, ctx) in &self.error_occurrences_v3 {
            if let Some(ctx) = ctx {
                usage
                    .entry(Arc::clone(&ctx.owner_id))
                    .or_insert_with(|| OwnerUsage {
                        counts: UsageCounts::default(),
                        token: Arc::clone(&ctx.token),
                        org: ctx.organization_id.as_ref().map(Arc::clone),
                    })
                    .counts
                    .error_tracking += 1;
            }
        }
        count_usage!(&self.web_vitals, web_vitals);
        usage
    }
}

#[derive(Debug, Default)]
struct BatchSendResult {
    failed_web_events: Vec<(WebEventRow, Option<TrackingContext>)>,
    failed_mods_events: Vec<(ModsEventRow, Option<TrackingContext>)>,
    failed_error_occurrences_v3: Vec<(
        ErrorOccurrenceV3Row,
        ErrorLanguage,
        ProjectGrouping,
        Option<TrackingContext>,
    )>,
    failed_web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
    had_permanent_failure: bool,
    errors: Vec<String>,
}

impl BatchSendResult {
    fn has_failures(&self) -> bool {
        self.failure_count() > 0
    }

    fn into_in_memory_batch(self) -> InMemoryBatch {
        InMemoryBatch {
            web_events: self.failed_web_events,
            mods_events: self.failed_mods_events,
            error_occurrences_v3: self.failed_error_occurrences_v3,
            web_vitals: self.failed_web_vitals,
        }
    }

    fn failure_count(&self) -> usize {
        self.failed_web_events.len()
            + self.failed_mods_events.len()
            + self.failed_error_occurrences_v3.len()
            + self.failed_web_vitals.len()
    }

    fn error_summary(&self) -> String {
        if self.errors.is_empty() {
            "unknown error".to_string()
        } else {
            self.errors.join("; ")
        }
    }
}

fn record_batch_error(
    result: &mut BatchSendResult,
    datasource: &'static str,
    rows: usize,
    error: &crate::tinybird::TinybirdError,
) {
    let permanence = if error.is_transient() {
        "transient"
    } else {
        "permanent"
    };
    result
        .errors
        .push(format!("{datasource} rows={rows} {permanence}: {error}"));
}

fn record_kafka_error(result: &mut BatchSendResult, datasource: &str, rows: usize, error: &str) {
    result
        .errors
        .push(format!("{datasource} rows={rows} transient Kafka: {error}"));
}

pub struct BatchQueue {
    tinybird: Arc<TinybirdClient>,
    polar: Option<Arc<PolarClient>>,
    mappings: Option<Arc<MappingResolver>>,
    event_publisher: Arc<crate::kafka::EventPublisher>,
    sender: mpsc::Sender<QueuedEvent>,
    in_memory_batch: Arc<Mutex<InMemoryBatch>>,
    flush_lock: Arc<Mutex<()>>,
}

impl BatchQueue {
    pub fn new(
        tinybird: Arc<TinybirdClient>,
        polar: Option<Arc<PolarClient>>,
        mappings: Option<Arc<MappingResolver>>,
        event_publisher: Arc<crate::kafka::EventPublisher>,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let in_memory_batch = Arc::new(Mutex::new(InMemoryBatch::default()));
        let flush_lock = Arc::new(Mutex::new(()));

        let queue = Arc::new(Self {
            tinybird,
            polar,
            mappings,
            event_publisher,
            sender,
            in_memory_batch,
            flush_lock,
        });

        queue.start_batch_processor(receiver);
        queue.start_batch_flusher();

        queue
    }

    pub fn queue_event(&self, event: QueuedEvent) -> Result<(), QueueError> {
        self.sender.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => QueueError::Full,
            mpsc::error::TrySendError::Closed(_) => QueueError::Closed,
        })
    }

    pub fn track_replay_usage(&self, session_id: &str, tracking: &TrackingContext) {
        let Some(polar) = &self.polar else {
            return;
        };

        let mut usage = AggregatedUsage::new();
        let mut owner_usage = OwnerUsage {
            counts: UsageCounts::default(),
            token: Arc::clone(&tracking.token),
            org: tracking.organization_id.as_ref().map(Arc::clone),
        };
        owner_usage
            .counts
            .session_replay_ids
            .insert(session_id.to_string());
        usage.insert(Arc::clone(&tracking.owner_id), owner_usage);

        let polar = Arc::clone(polar);
        tokio::spawn(async move {
            if let Err(error) = polar.ingest_usage(&usage).await {
                error!("Failed to ingest replay usage to Polar: {}", error);
            }
        });
    }

    pub fn channel_capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub async fn current_batch_size(&self) -> usize {
        self.in_memory_batch.lock().await.total_count()
    }

    fn is_channel_under_pressure(&self) -> bool {
        self.sender.capacity() < (CHANNEL_CAPACITY - CHANNEL_BACKPRESSURE_THRESHOLD)
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
                let error_summary = result.error_summary();
                error!(
                    errors = %error_summary,
                    "Dropping {} events after a permanent delivery failure",
                    result.failure_count(),
                );
                return;
            }

            retry_count += 1;

            if retry_count >= MAX_RETRIES {
                let error_summary = result.error_summary();
                error!(
                    errors = %error_summary,
                    "Dropping {} events after {} delivery attempts",
                    result.failure_count(),
                    retry_count
                );
                return;
            }

            let error_summary = result.error_summary();
            current_batch = result.into_in_memory_batch();

            let delay = Self::calculate_retry_delay(retry_count);
            warn!(
                errors = %error_summary,
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
            error_occurrences_v3,
            web_vitals,
        } = batch;

        let web_event_rows: Vec<_> = web_events.iter().map(|(e, _)| e).collect();
        let mods_event_rows: Vec<_> = mods_events.iter().map(|(e, _)| e).collect();
        let error_occurrences_v3 = self.enrich_error_occurrences_v3(error_occurrences_v3).await;
        let error_occurrence_v3_rows: Vec<_> =
            error_occurrences_v3.iter().map(|(e, _, _, _)| e).collect();
        let web_vital_rows: Vec<_> = web_vitals.iter().map(|(e, _)| e).collect();

        // This runs after sourcemap enrichment, so error messages contain the final mapped
        // stacktrace and the group hash recalculated from it.
        let (web_events_kafka, mods_events_kafka, errors_kafka, vitals_kafka) = tokio::join!(
            self.publish_web_events(&web_event_rows),
            self.publish_mods_events(&mods_event_rows),
            self.publish_errors(&error_occurrence_v3_rows),
            self.publish_vitals(&web_vital_rows),
        );

        let (web_events_res, mods_events_res, error_occurrences_v3_res, web_vitals_res) = tokio::join!(
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
                if error_occurrence_v3_rows.is_empty() {
                    Ok(())
                } else {
                    self.tinybird
                        .insert_error_occurrences_v3(&error_occurrence_v3_rows)
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
        );

        if web_events_res.is_err() || web_events_kafka.is_err() {
            if let Err(e) = web_events_res {
                record_batch_error(&mut result, "web_events", web_events.len(), &e);
                result.had_permanent_failure |= !e.is_transient();
            }
            if let Err(e) = web_events_kafka {
                record_kafka_error(&mut result, "web_events", web_events.len(), &e);
            }
            result.failed_web_events = web_events;
        }

        if mods_events_res.is_err() || mods_events_kafka.is_err() {
            if let Err(e) = mods_events_res {
                record_batch_error(&mut result, "mods_events", mods_events.len(), &e);
                result.had_permanent_failure |= !e.is_transient();
            }
            if let Err(e) = mods_events_kafka {
                record_kafka_error(&mut result, "mods_events", mods_events.len(), &e);
            }
            result.failed_mods_events = mods_events;
        }

        if error_occurrences_v3_res.is_err() || errors_kafka.is_err() {
            if let Err(e) = error_occurrences_v3_res {
                record_batch_error(
                    &mut result,
                    "error_tracking_v3",
                    error_occurrences_v3.len(),
                    &e,
                );
                result.had_permanent_failure |= !e.is_transient();
            }
            if let Err(e) = errors_kafka {
                record_kafka_error(
                    &mut result,
                    "error_tracking_v3",
                    error_occurrences_v3.len(),
                    &e,
                );
            }
            result.failed_error_occurrences_v3 = error_occurrences_v3;
        }

        if web_vitals_res.is_err() || vitals_kafka.is_err() {
            if let Err(e) = web_vitals_res {
                record_batch_error(&mut result, "web_vitals", web_vitals.len(), &e);
                result.had_permanent_failure |= !e.is_transient();
            }
            if let Err(e) = vitals_kafka {
                record_kafka_error(&mut result, "web_vitals", web_vitals.len(), &e);
            }
            result.failed_web_vitals = web_vitals;
        }

        result
    }

    async fn publish_web_events(&self, rows: &[&WebEventRow]) -> Result<(), String> {
        self.event_publisher
            .publish_all(
                rows.iter()
                    .map(|row| collector_message::Payload::WebEvent((**row).clone()))
                    .collect(),
            )
            .await
    }

    async fn publish_mods_events(&self, rows: &[&ModsEventRow]) -> Result<(), String> {
        self.event_publisher
            .publish_all(
                rows.iter()
                    .map(|row| collector_message::Payload::ModsEvent((**row).clone()))
                    .collect(),
            )
            .await
    }

    async fn publish_errors(&self, rows: &[&ErrorOccurrenceV3Row]) -> Result<(), String> {
        self.event_publisher
            .publish_all(
                rows.iter()
                    .map(|row| collector_message::Payload::ErrorOccurrence((**row).clone()))
                    .collect(),
            )
            .await
    }

    async fn publish_vitals(&self, rows: &[&WebVitalRow]) -> Result<(), String> {
        self.event_publisher
            .publish_all(
                rows.iter()
                    .map(|row| collector_message::Payload::WebVital((**row).clone()))
                    .collect(),
            )
            .await
    }

    async fn enrich_error_occurrences_v3(
        &self,
        rows: Vec<(
            ErrorOccurrenceV3Row,
            ErrorLanguage,
            ProjectGrouping,
            Option<TrackingContext>,
        )>,
    ) -> Vec<(
        ErrorOccurrenceV3Row,
        ErrorLanguage,
        ProjectGrouping,
        Option<TrackingContext>,
    )> {
        if rows.is_empty() {
            return rows;
        }

        let Some(resolver) = self.mappings.as_deref() else {
            return rows;
        };
        let mut enriched = Vec::with_capacity(rows.len());
        for (row, language, grouping, tracking) in rows {
            enriched.push((
                crate::error_tracking::v3::enrich_with_mapping(resolver, row, language, &grouping)
                    .await,
                language,
                grouping,
                tracking,
            ));
        }
        enriched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod retry_delay {
        use super::*;

        #[test]
        fn grows_exponentially_and_stays_capped() {
            let initial = BatchQueue::calculate_retry_delay(0);
            let next = BatchQueue::calculate_retry_delay(1);
            let later = BatchQueue::calculate_retry_delay(2);
            let capped = BatchQueue::calculate_retry_delay(10);

            assert!(initial >= Duration::from_millis(750));
            assert!(initial <= Duration::from_millis(1_250));
            assert!(next > initial);
            assert!(later > next);
            assert!(capped <= MAX_RETRY_DELAY + Duration::from_millis(100));
        }
    }
}
