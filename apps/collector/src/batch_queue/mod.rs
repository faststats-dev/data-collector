mod tinybird;

use crate::error_tracking::ErrorLanguage;
use crate::error_tracking::mapping::MappingResolver;
use crate::tinybird::TinybirdClient;
use collector_message::{ErrorOccurrence, ModsEvent, WebEvent, WebVital};
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tinybird::Batch;
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

#[derive(Debug)]
pub enum QueuedEvent {
    WebEvent {
        row: Box<WebEvent>,
    },
    ModsEvent {
        row: ModsEvent,
    },
    ErrorOccurrenceV3 {
        row: Box<ErrorOccurrence>,
        language: ErrorLanguage,
    },
    WebVital {
        row: WebVital,
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

pub struct BatchQueue {
    tinybird: TinybirdClient,
    mappings: Option<MappingResolver>,
    event_publisher: crate::kafka::EventPublisher,
    sender: mpsc::Sender<QueuedEvent>,
    tinybird_batch: Mutex<Batch>,
    tinybird_flush_lock: Mutex<()>,
    pending_kafka: AtomicUsize,
    kafka_drained: Notify,
}

impl BatchQueue {
    pub fn new(
        tinybird: TinybirdClient,
        mappings: Option<MappingResolver>,
        event_publisher: crate::kafka::EventPublisher,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let (kafka_sender, kafka_receiver) = mpsc::channel(CHANNEL_CAPACITY);

        let queue = Arc::new(Self {
            tinybird,
            mappings,
            event_publisher,
            sender,
            tinybird_batch: Mutex::new(Batch::default()),
            tinybird_flush_lock: Mutex::new(()),
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

        tinybird::send_with_retry(&self.tinybird, batch).await;
    }

    async fn publish_kafka_with_retry(&self, payload: collector_message::Payload) {
        let message = collector_message::Message::new(payload);
        let mut retry_count = 0u32;
        loop {
            match self.event_publisher.publish(&message).await {
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
                    let delay = calculate_retry_delay(retry_count);
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
        let Some(resolver) = self.mappings.as_ref() else {
            return event;
        };

        let QueuedEvent::ErrorOccurrenceV3 { row, language } = event else {
            return event;
        };

        let row = crate::error_tracking::v3::enrich_with_mapping(resolver, *row, language).await;
        QueuedEvent::ErrorOccurrenceV3 {
            row: Box::new(row),
            language,
        }
    }
}

fn calculate_retry_delay(retry_count: u32) -> Duration {
    let base = INITIAL_RETRY_DELAY.as_millis() as u64;
    let capped = (base * 2u64.saturating_pow(retry_count)).min(MAX_RETRY_DELAY.as_millis() as u64);
    let jitter = (capped / 4).saturating_sub((retry_count as u64 * 7919) % (capped / 2).max(1));
    Duration::from_millis(capped.saturating_sub(jitter))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod retry_delay {
        use super::*;

        #[test]
        fn grows_exponentially_and_stays_capped() {
            let initial = calculate_retry_delay(0);
            let next = calculate_retry_delay(1);
            let later = calculate_retry_delay(2);
            let capped = calculate_retry_delay(10);

            assert!(initial >= Duration::from_millis(750));
            assert!(initial <= Duration::from_millis(1_250));
            assert!(next > initial);
            assert!(later > next);
            assert!(capped <= MAX_RETRY_DELAY + Duration::from_millis(100));
        }
    }
}
