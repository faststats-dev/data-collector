use crate::error_tracking::mapping::MappingResolver;
use crate::error_tracking::{ErrorLanguage, ProjectGrouping};
use crate::polar::{PolarClient, UsageCounts};
use crate::tinybird::{
    ErrorOccurrenceV3Row, ModsEventRow, TinybirdClient, WebEventRow, WebVitalRow,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const TINYBIRD_BATCH_WINDOW: Duration = Duration::from_secs(5);
const CHANNEL_CAPACITY: usize = 2_000;
const CHANNEL_BACKPRESSURE_THRESHOLD: usize = 1_600;
const TINYBIRD_MAX_BATCH_SIZE: usize = 5000;
const EVENT_PROCESSING_CONCURRENCY: usize = 100;
const KAFKA_PUBLISH_CONCURRENCY: usize = 100;

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

impl QueuedEvent {
    fn kafka_payload(&self) -> collector_message::Payload {
        match self {
            Self::WebEvent { row, .. } => collector_message::Payload::WebEvent((**row).clone()),
            Self::ModsEvent { row, .. } => collector_message::Payload::ModsEvent(row.clone()),
            Self::ErrorOccurrenceV3 { row, .. } => {
                collector_message::Payload::ErrorOccurrence((**row).clone())
            }
            Self::WebVital { row, .. } => collector_message::Payload::WebVital(row.clone()),
        }
    }
}

pub enum QueueError {
    Full,
    Closed,
}

#[derive(Debug, Default)]
struct TinybirdBatch {
    web_events: Vec<(WebEventRow, Option<TrackingContext>)>,
    mods_events: Vec<(ModsEventRow, Option<TrackingContext>)>,
    error_occurrences_v3: Vec<(ErrorOccurrenceV3Row, Option<TrackingContext>)>,
    web_vitals: Vec<(WebVitalRow, Option<TrackingContext>)>,
}

impl TinybirdBatch {
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
                language: _,
                grouping: _,
                tracking,
            } => self.error_occurrences_v3.push((*row, tracking)),
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
        for (_, ctx) in &self.error_occurrences_v3 {
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
struct TinybirdBatchSendResult {
    delivered: TinybirdBatch,
    retryable: TinybirdBatch,
    permanent_failure_count: usize,
    errors: Vec<String>,
}

impl TinybirdBatchSendResult {
    fn error_summary(&self) -> String {
        if self.errors.is_empty() {
            "unknown error".to_string()
        } else {
            self.errors.join("; ")
        }
    }
}

fn merge_usage(target: &mut AggregatedUsage, source: AggregatedUsage) {
    for (owner_id, source_usage) in source {
        let target_usage = target.entry(owner_id).or_insert_with(|| OwnerUsage {
            counts: UsageCounts::default(),
            token: Arc::clone(&source_usage.token),
            org: source_usage.org.as_ref().map(Arc::clone),
        });
        target_usage.counts.events += source_usage.counts.events;
        target_usage.counts.error_tracking += source_usage.counts.error_tracking;
        target_usage.counts.web_vitals += source_usage.counts.web_vitals;
        target_usage
            .counts
            .session_replay_ids
            .extend(source_usage.counts.session_replay_ids);
    }
}

fn classify_delivery<T>(
    outcome: Result<(), crate::tinybird::TinybirdError>,
    rows: Vec<T>,
    datasource: &'static str,
    errors: &mut Vec<String>,
) -> (Vec<T>, Vec<T>, usize) {
    let Err(error) = outcome else {
        return (rows, Vec::new(), 0);
    };
    let permanence = if error.is_transient() {
        "transient"
    } else {
        "permanent"
    };
    errors.push(format!(
        "{datasource} rows={} {permanence}: {error}",
        rows.len()
    ));
    if error.is_transient() {
        (Vec::new(), rows, 0)
    } else {
        let count = rows.len();
        (Vec::new(), Vec::new(), count)
    }
}

pub struct BatchQueue {
    tinybird: Arc<TinybirdClient>,
    polar: Option<Arc<PolarClient>>,
    mappings: Option<Arc<MappingResolver>>,
    event_publisher: Arc<crate::kafka::EventPublisher>,
    sender: mpsc::Sender<QueuedEvent>,
    tinybird_batch: Arc<Mutex<TinybirdBatch>>,
    tinybird_flush_lock: Arc<Mutex<()>>,
    pending_kafka: AtomicUsize,
    kafka_drained: Notify,
}

impl BatchQueue {
    pub fn new(
        tinybird: Arc<TinybirdClient>,
        polar: Option<Arc<PolarClient>>,
        mappings: Option<Arc<MappingResolver>>,
        event_publisher: Arc<crate::kafka::EventPublisher>,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let (kafka_sender, kafka_receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let tinybird_batch = Arc::new(Mutex::new(TinybirdBatch::default()));
        let tinybird_flush_lock = Arc::new(Mutex::new(()));

        let queue = Arc::new(Self {
            tinybird,
            polar,
            mappings,
            event_publisher,
            sender,
            tinybird_batch,
            tinybird_flush_lock,
            pending_kafka: AtomicUsize::new(0),
            kafka_drained: Notify::new(),
        });

        queue.start_event_processor(receiver, kafka_sender);
        queue.start_kafka_publisher(kafka_receiver);
        queue.start_tinybird_batch_flusher();

        queue
    }

    pub fn queue_event(&self, event: QueuedEvent) -> Result<(), QueueError> {
        self.pending_kafka.fetch_add(1, Ordering::Relaxed);
        self.sender.try_send(event).map_err(|error| {
            self.finish_kafka_event();
            match error {
                mpsc::error::TrySendError::Full(_) => QueueError::Full,
                mpsc::error::TrySendError::Closed(_) => QueueError::Closed,
            }
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
        self.tinybird_batch.lock().await.total_count()
    }

    fn finish_kafka_event(&self) {
        if self.pending_kafka.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.kafka_drained.notify_waiters();
        }
    }

    pub(crate) async fn drain(&self) {
        loop {
            let notified = self.kafka_drained.notified();
            if self.pending_kafka.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
        self.flush_tinybird_batch().await;
    }

    fn is_channel_under_pressure(&self) -> bool {
        self.sender.capacity() < (CHANNEL_CAPACITY - CHANNEL_BACKPRESSURE_THRESHOLD)
    }

    fn start_event_processor(
        self: &Arc<Self>,
        receiver: mpsc::Receiver<QueuedEvent>,
        kafka_sender: mpsc::Sender<collector_message::Payload>,
    ) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })
            .for_each_concurrent(EVENT_PROCESSING_CONCURRENCY, |event| {
                let queue = Arc::clone(&queue);
                let kafka_sender = kafka_sender.clone();
                async move {
                    let event = queue.enrich_event(event).await;
                    let payload = event.kafka_payload();
                    let should_flush = {
                        let mut batch = queue.tinybird_batch.lock().await;
                        batch.push(event);
                        batch.total_count() >= TINYBIRD_MAX_BATCH_SIZE
                            || (queue.is_channel_under_pressure() && batch.total_count() >= 100)
                    };

                    // Kafka is a secondary sink. Never let its bounded backlog apply
                    // backpressure to Tinybird ingestion.
                    if kafka_sender.try_send(payload).is_err() {
                        metrics::counter!("kafka_events_dropped_total").increment(1);
                        queue.finish_kafka_event();
                    }

                    if should_flush {
                        warn!("Tinybird batch size limit or backpressure detected, triggering early flush");
                        queue.flush_tinybird_batch().await;
                    }
                }
            })
            .await;
        });
    }

    fn start_kafka_publisher(
        self: &Arc<Self>,
        receiver: mpsc::Receiver<collector_message::Payload>,
    ) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|payload| (payload, receiver))
            })
            .for_each_concurrent(KAFKA_PUBLISH_CONCURRENCY, |payload| {
                let queue = Arc::clone(&queue);
                async move {
                    queue.publish_kafka_with_retry(payload).await;
                    queue.finish_kafka_event();
                }
            })
            .await;
        });
    }

    fn start_tinybird_batch_flusher(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TINYBIRD_BATCH_WINDOW).await;
                queue.flush_tinybird_batch().await;
            }
        });
    }

    async fn flush_tinybird_batch(&self) {
        let _flush_guard = self.tinybird_flush_lock.lock().await;

        let batch = {
            let mut current = self.tinybird_batch.lock().await;
            if current.is_empty() {
                return;
            }
            std::mem::take(&mut *current)
        };

        let total = batch.total_count();
        info!("Flushing in-memory batch of {} events", total);

        let usage = self.send_tinybird_batch_with_retry(batch).await;

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

    async fn send_tinybird_batch_with_retry(&self, batch: TinybirdBatch) -> AggregatedUsage {
        let mut retry_count = 0u32;
        let mut current_batch = batch;
        let mut delivered_usage = AggregatedUsage::new();

        loop {
            let result = self.send_tinybird_batch(current_batch).await;
            merge_usage(&mut delivered_usage, result.delivered.aggregate_usage());

            if result.permanent_failure_count > 0 {
                let error_summary = result.error_summary();
                error!(
                    errors = %error_summary,
                    "Dropping {} events after a permanent delivery failure",
                    result.permanent_failure_count,
                );
            }

            if result.retryable.is_empty() {
                return delivered_usage;
            }
            retry_count += 1;

            if retry_count >= MAX_RETRIES {
                let error_summary = result.error_summary();
                error!(
                    errors = %error_summary,
                    "Dropping {} events after {} delivery attempts",
                    result.retryable.total_count(),
                    retry_count
                );
                return delivered_usage;
            }

            let error_summary = result.error_summary();
            current_batch = result.retryable;

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

    async fn send_tinybird_batch(&self, batch: TinybirdBatch) -> TinybirdBatchSendResult {
        let mut result = TinybirdBatchSendResult::default();

        let TinybirdBatch {
            web_events,
            mods_events,
            error_occurrences_v3,
            web_vitals,
        } = batch;

        let web_event_rows: Vec<_> = web_events.iter().map(|(e, _)| e).collect();
        let mods_event_rows: Vec<_> = mods_events.iter().map(|(e, _)| e).collect();
        let error_occurrence_v3_rows: Vec<_> =
            error_occurrences_v3.iter().map(|(e, _)| e).collect();
        let web_vital_rows: Vec<_> = web_vitals.iter().map(|(e, _)| e).collect();

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

        let (delivered, retryable, permanent) =
            classify_delivery(web_events_res, web_events, "web_events", &mut result.errors);
        result.delivered.web_events = delivered;
        result.retryable.web_events = retryable;
        result.permanent_failure_count += permanent;

        let (delivered, retryable, permanent) = classify_delivery(
            mods_events_res,
            mods_events,
            "mods_events",
            &mut result.errors,
        );
        result.delivered.mods_events = delivered;
        result.retryable.mods_events = retryable;
        result.permanent_failure_count += permanent;

        let (delivered, retryable, permanent) = classify_delivery(
            error_occurrences_v3_res,
            error_occurrences_v3,
            "error_tracking_v3",
            &mut result.errors,
        );
        result.delivered.error_occurrences_v3 = delivered;
        result.retryable.error_occurrences_v3 = retryable;
        result.permanent_failure_count += permanent;

        let (delivered, retryable, permanent) =
            classify_delivery(web_vitals_res, web_vitals, "web_vitals", &mut result.errors);
        result.delivered.web_vitals = delivered;
        result.retryable.web_vitals = retryable;
        result.permanent_failure_count += permanent;

        result
    }

    async fn publish_kafka_with_retry(&self, payload: collector_message::Payload) {
        let mut retry_count = 0u32;
        loop {
            match self.event_publisher.publish(payload.clone()).await {
                Ok(()) => return,
                Err(error) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        error!(
                            %error,
                            "Dropping Kafka event after {} delivery attempts",
                            retry_count
                        );
                        return;
                    }
                    let delay = Self::calculate_retry_delay(retry_count);
                    warn!(
                        %error,
                        "Kafka delivery failed (attempt {}), retrying in {:?}",
                        retry_count,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn enrich_event(&self, event: QueuedEvent) -> QueuedEvent {
        let QueuedEvent::ErrorOccurrenceV3 {
            row,
            language,
            grouping,
            tracking,
        } = event
        else {
            return event;
        };

        let Some(resolver) = self.mappings.as_deref() else {
            return QueuedEvent::ErrorOccurrenceV3 {
                row,
                language,
                grouping,
                tracking,
            };
        };

        let row =
            crate::error_tracking::v3::enrich_with_mapping(resolver, *row, language, &grouping)
                .await;
        QueuedEvent::ErrorOccurrenceV3 {
            row: Box::new(row),
            language,
            grouping,
            tracking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_delivery_per_datasource() {
        let mut errors = Vec::new();
        let (delivered, retryable, permanent) = classify_delivery(
            Err(crate::tinybird::TinybirdError::Api {
                status: 503,
                message: "unavailable".into(),
            }),
            vec![1, 2],
            "transient_source",
            &mut errors,
        );
        assert!(delivered.is_empty());
        assert_eq!(retryable, vec![1, 2]);
        assert_eq!(permanent, 0);

        let (delivered, retryable, permanent) = classify_delivery(
            Err(crate::tinybird::TinybirdError::Api {
                status: 400,
                message: "invalid".into(),
            }),
            vec![3, 4, 5],
            "permanent_source",
            &mut errors,
        );
        assert!(delivered.is_empty());
        assert!(retryable.is_empty());
        assert_eq!(permanent, 3);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn merges_delivered_usage_counts() {
        let owner: Arc<str> = "owner".into();
        let context = TrackingContext {
            owner_id: Arc::clone(&owner),
            token: "token".into(),
            organization_id: None,
        };
        let mut target = AggregatedUsage::new();
        let source = AggregatedUsage::from([(
            owner,
            OwnerUsage {
                counts: UsageCounts {
                    events: 2,
                    error_tracking: 1,
                    ..UsageCounts::default()
                },
                token: Arc::clone(&context.token),
                org: None,
            },
        )]);
        merge_usage(&mut target, source);
        let source = AggregatedUsage::from([(
            Arc::clone(&context.owner_id),
            OwnerUsage {
                counts: UsageCounts {
                    events: 3,
                    web_vitals: 4,
                    ..UsageCounts::default()
                },
                token: context.token,
                org: None,
            },
        )]);
        merge_usage(&mut target, source);

        let counts = &target[&context.owner_id].counts;
        assert_eq!(counts.events, 5);
        assert_eq!(counts.error_tracking, 1);
        assert_eq!(counts.web_vitals, 4);
    }

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
